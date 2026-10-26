//! Shared local Fake forge tracing and interruption boundaries.
#![expect(
    dead_code,
    reason = "Each integration test uses a subset of the shared fixtures"
)]

use dvb_core::forge::{
    CheckObservation, ExpectedChecks, Forge, ForgeError, MergeError, Pr, PrObservation, Repo,
};
use dvb_forge::FakeForge;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Barrier;

/// A Fake forge with interruptions at external-effect boundaries.
pub struct InterruptedForge {
    pub inner: Arc<FakeForge>,
    /// Every Forge entry point received, including attempted effects that fail.
    pub calls: Mutex<Vec<&'static str>>,
    /// All request arguments, including text and path fields, for privacy checks.
    pub requests: Mutex<Vec<(&'static str, Vec<String>)>>,
    pub before_setup: AtomicBool,
    pub after_setup: AtomicBool,
    pub initialize_during_setup: Mutex<Option<(std::path::PathBuf, String)>>,
    pub close_issue_fails: AtomicBool,
    /// Opening a PR is refused (not retryable) while set.
    pub open_pr_fails: AtomicBool,
    /// Every PR body edit fails with this classified error while set.
    pub edit_pr_body_error: Mutex<Option<ForgeError>>,
    pub move_during_checks: Mutex<Option<(std::path::PathBuf, String, String)>>,
    pub cleanup_barrier: Option<Arc<Barrier>>,
    pub mirror_barrier: Option<(&'static str, Arc<Barrier>)>,
    pub mirror_writes: Mutex<Vec<&'static str>>,
    /// Mirror writes of this operation ("project", "goal", "task" or "all"), or of the
    /// goal or task with this key, fail as retryable.
    pub mirror_failure: Mutex<Option<&'static str>>,
    /// The next PR read reports this head (a read lagging a push).
    pub stale_pr_once: Mutex<Option<String>>,
    /// The next push lands, then its reply is lost (retryable error).
    pub push_reply_lost: AtomicBool,
    /// Hold the next push reply after it lands, to cancel at the crash boundary.
    pub push_barrier: Mutex<Option<Arc<Barrier>>>,
    /// How many merges were sent to the forge.
    pub merges: std::sync::atomic::AtomicUsize,
    /// The next fetch fails (retryable error).
    pub fetch_fails_once: AtomicBool,
    /// Every fetch fails with this classified error while set.
    pub fetch_error: Mutex<Option<ForgeError>>,
    /// Hold the next fetch before Git, then release it independently of later calls.
    pub fetch_barrier: Mutex<Option<Arc<Barrier>>>,
    pub fetch_done: tokio::sync::Notify,
    pub clone_barrier: Mutex<Option<Arc<Barrier>>>,
    /// Hold the next PR, checks or merge request at its observation/effect boundary.
    pub pr_barrier: Mutex<Option<Arc<Barrier>>>,
    pub checks_barrier: Mutex<Option<Arc<Barrier>>>,
    pub main_checks_barrier: Mutex<Option<Arc<Barrier>>>,
    pub merge_barrier: Mutex<Option<Arc<Barrier>>>,
    /// Runs once when the next mirror receipts are forgotten.
    pub on_forget_mirror: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl InterruptedForge {
    pub fn new(inner: Arc<FakeForge>) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            before_setup: AtomicBool::new(false),
            after_setup: AtomicBool::new(false),
            initialize_during_setup: Mutex::new(None),
            close_issue_fails: AtomicBool::new(false),
            open_pr_fails: AtomicBool::new(false),
            edit_pr_body_error: Mutex::new(None),
            move_during_checks: Mutex::new(None),
            cleanup_barrier: None,
            mirror_barrier: None,
            mirror_writes: Mutex::new(Vec::new()),
            mirror_failure: Mutex::new(None),
            stale_pr_once: Mutex::new(None),
            push_reply_lost: AtomicBool::new(false),
            push_barrier: Mutex::new(None),
            merges: Default::default(),
            fetch_fails_once: AtomicBool::new(false),
            fetch_error: Mutex::new(None),
            fetch_barrier: Mutex::new(None),
            fetch_done: tokio::sync::Notify::new(),
            clone_barrier: Mutex::new(None),
            pr_barrier: Mutex::new(None),
            checks_barrier: Mutex::new(None),
            main_checks_barrier: Mutex::new(None),
            merge_barrier: Mutex::new(None),
            on_forget_mirror: Mutex::new(None),
        }
    }

    fn record(&self, operation: &'static str, arguments: Vec<String>) {
        // Keep the operation and its arguments in the same inspection snapshot.
        let mut requests = self.requests.lock().unwrap();
        self.calls.lock().unwrap().push(operation);
        requests.push((operation, arguments));
    }

    async fn begin_mirror(&self, operation: &'static str, key: &str) -> Result<(), ForgeError> {
        let first = {
            let mut writes = self.mirror_writes.lock().unwrap();
            let first = !writes.contains(&operation);
            writes.push(operation);
            first
        };
        if let Some((blocked, barrier)) = &self.mirror_barrier
            && *blocked == operation
            && first
        {
            barrier.wait().await;
            barrier.wait().await;
        }
        if matches!(*self.mirror_failure.lock().unwrap(), Some(failed) if [operation, key, "all"].contains(&failed))
        {
            return Err(interrupted());
        }
        Ok(())
    }
}
fn interrupted() -> ForgeError {
    ForgeError {
        rate_limit: None,
        retryable: true,
        unreachable: false,
        detail: "simulated interruption".into(),
    }
}
#[async_trait::async_trait]
impl Forge for InterruptedForge {
    async fn accessible_repositories(
        &self,
    ) -> Result<dvb_core::api::GithubRepositories, ForgeError> {
        self.inner.accessible_repositories().await
    }
    async fn discover_checks(&self, repo: &Repo) -> dvb_core::api::GithubCheckDiscovery {
        self.inner.discover_checks(repo).await
    }

    async fn repository_id(&self, repo: &Repo) -> Result<Option<u64>, ForgeError> {
        self.inner.repository_id(repo).await
    }
    fn repoint_mirror(&self, from: &Repo, to: &Repo) -> Result<(), ForgeError> {
        self.inner.repoint_mirror(from, to)
    }
    fn forget_mirror(&self, repo: &Repo) -> Result<(), ForgeError> {
        if let Some(hook) = self.on_forget_mirror.lock().unwrap().take() {
            hook();
        }
        self.inner.forget_mirror(repo)
    }

    async fn mirror_project(&self, repo: &Repo) -> Result<(), ForgeError> {
        self.record("mirror_project", vec![repo.0.clone()]);
        self.begin_mirror("project", &repo.0).await?;
        self.inner.mirror_project(repo).await
    }
    async fn mirror_goal(
        &self,
        repo: &Repo,
        goal: &dvb_core::forge::GoalMirror,
    ) -> Result<u64, ForgeError> {
        self.record(
            "mirror_goal",
            vec![
                repo.0.clone(),
                goal.key.clone(),
                goal.title.clone(),
                goal.description.clone(),
                goal.done.to_string(),
            ],
        );
        self.begin_mirror("goal", &goal.key).await?;
        self.inner.mirror_goal(repo, goal).await
    }
    async fn mirror_task(
        &self,
        repo: &Repo,
        task: &dvb_core::forge::TaskMirror,
    ) -> Result<(), ForgeError> {
        self.record(
            "mirror_task",
            vec![
                repo.0.clone(),
                task.key.clone(),
                task.title.clone(),
                task.role.clone(),
                task.model.clone(),
                task.state.label().into(),
                format!("{:?}", task.pr),
                task.milestone.to_string(),
            ],
        );
        self.begin_mirror("task", &task.key).await?;
        self.inner.mirror_task(repo, task).await
    }

    fn can_create_repo(&self) -> bool {
        self.inner.can_create_repo()
    }
    async fn create_repo(&self, org: &str, name: &str) -> Result<Repo, ForgeError> {
        self.record("create_repo", vec![org.into(), name.into()]);
        self.inner.create_repo(org, name).await
    }
    fn created_repo_id(&self, repo: &Repo) -> Option<u64> {
        self.record("created_repo_id", vec![repo.0.clone()]);
        self.inner.created_repo_id(repo)
    }
    async fn resume_setup(&self, repo: &Repo, id: u64) -> Result<(), ForgeError> {
        self.record("resume_setup", vec![repo.0.clone(), id.to_string()]);
        self.inner.resume_setup(repo, id).await
    }
    async fn clone_repo(&self, repo: &Repo, mirror: &Path) -> Result<String, ForgeError> {
        self.record(
            "clone_repo",
            vec![repo.0.clone(), mirror.to_string_lossy().into_owned()],
        );
        let barrier = self.clone_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        self.inner.clone_repo(repo, mirror).await
    }
    async fn commit_author(
        &self,
        repo: &Repo,
        sha: &str,
    ) -> Result<dvb_core::api::CommitIdentity, ForgeError> {
        self.record("commit_author", vec![repo.0.clone(), sha.into()]);
        self.inner.commit_author(repo, sha).await
    }
    async fn fetch(&self, repo: &Repo, mirror: &Path, branch: &str) -> Result<String, ForgeError> {
        self.record(
            "fetch",
            vec![
                repo.0.clone(),
                mirror.to_string_lossy().into_owned(),
                branch.into(),
            ],
        );
        let barrier = self.fetch_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        if self.fetch_fails_once.swap(false, Ordering::SeqCst) {
            return Err(interrupted());
        }
        if let Some(error) = self.fetch_error.lock().unwrap().clone() {
            return Err(error);
        }
        {
            let result = self.inner.fetch(repo, mirror, branch).await;
            self.fetch_done.notify_one();
            result
        }
    }
    async fn push_setup(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        self.record(
            "push_setup",
            vec![
                repo.0.clone(),
                mirror.to_string_lossy().into_owned(),
                branch.into(),
                head.into(),
            ],
        );
        if self.before_setup.swap(false, Ordering::SeqCst) {
            return Err(interrupted());
        }
        let initialize = self.initialize_during_setup.lock().unwrap().take();
        if let Some((mirror, head)) = initialize {
            self.inner.push(repo, &mirror, branch, &head).await?;
        }
        self.inner.push_setup(repo, mirror, branch, head).await?;
        if self.after_setup.swap(false, Ordering::SeqCst) {
            return Err(interrupted());
        }
        Ok(())
    }
    async fn install_ruleset(
        &self,
        repo: &Repo,
        branch: &str,
        checks: &[String],
    ) -> Result<(), ForgeError> {
        self.record(
            "install_ruleset",
            vec![repo.0.clone(), branch.into(), format!("{checks:?}")],
        );
        self.inner.install_ruleset(repo, branch, checks).await
    }
    async fn push(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        self.record(
            "push",
            vec![
                repo.0.clone(),
                mirror.to_string_lossy().into_owned(),
                branch.into(),
                head.into(),
            ],
        );
        self.inner.push(repo, mirror, branch, head).await?;
        let barrier = self.push_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        if self.push_reply_lost.swap(false, Ordering::SeqCst) {
            return Err(interrupted());
        }
        Ok(())
    }
    async fn open_pr(
        &self,
        repo: &Repo,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<Pr, ForgeError> {
        self.record(
            "open_pr",
            vec![repo.0.clone(), branch.into(), title.into(), body.into()],
        );
        if self.open_pr_fails.load(Ordering::SeqCst) {
            return Err(ForgeError {
                rate_limit: None,
                retryable: false,
                unreachable: false,
                detail: "open PR refused".into(),
            });
        }
        self.inner.open_pr(repo, branch, title, body).await
    }
    async fn edit_pr_body(&self, repo: &Repo, pr: u64, body: &str) -> Result<(), ForgeError> {
        self.record(
            "edit_pr_body",
            vec![repo.0.clone(), pr.to_string(), body.into()],
        );
        if let Some(error) = self.edit_pr_body_error.lock().unwrap().clone() {
            return Err(error);
        }
        self.inner.edit_pr_body(repo, pr, body).await
    }
    async fn main_checks(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Vec<CheckObservation>, ForgeError> {
        self.record("main_checks", vec![repo.0.clone(), branch.into()]);
        let seen = self.inner.main_checks(repo, branch).await;
        let barrier = self.main_checks_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        seen
    }
    async fn rerun_checks(
        &self,
        repo: &Repo,
        head: &str,
        runs: &[u64],
        recovery: bool,
    ) -> Result<(), ForgeError> {
        self.record(
            "rerun_checks",
            vec![repo.0.clone(), head.into(), format!("{runs:?}")],
        );
        self.inner.rerun_checks(repo, head, runs, recovery).await
    }
    async fn cancel_checks(&self, repo: &Repo, head: &str) -> Result<(), ForgeError> {
        self.record("cancel_checks", vec![repo.0.clone(), head.into()]);
        self.inner.cancel_checks(repo, head).await
    }
    async fn check_excerpt(&self, repo: &Repo, run: u64) -> Result<String, ForgeError> {
        self.inner.check_excerpt(repo, run).await
    }
    async fn checks(&self, repo: &Repo, head: &str) -> Result<Vec<CheckObservation>, ForgeError> {
        self.record("checks", vec![repo.0.clone(), head.into()]);
        let barrier = self.checks_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        let moved = self.move_during_checks.lock().unwrap().take();
        if let Some((mirror, branch, head)) = moved {
            self.inner.push(repo, &mirror, &branch, &head).await?;
        }
        self.inner.checks(repo, head).await
    }
    async fn merge(
        &self,
        repo: &Repo,
        pr: u64,
        head: &str,
        title: &str,
    ) -> Result<String, MergeError> {
        self.record("merge", vec![repo.0.clone(), pr.to_string(), head.into()]);
        let barrier = self.merge_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        self.merges.fetch_add(1, Ordering::SeqCst);
        self.inner.merge(repo, pr, head, title).await
    }
    async fn close_issue(&self, repo: &Repo, issue: u64) -> Result<(), ForgeError> {
        self.record("close_issue", vec![repo.0.clone(), issue.to_string()]);
        if self.close_issue_fails.load(Ordering::SeqCst) {
            return Err(interrupted());
        }
        self.inner.close_issue(repo, issue).await
    }
    async fn issue(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Option<dvb_core::forge::Issue>, dvb_core::forge::IssueError> {
        self.record("issue", vec![repo.0.clone(), number.to_string()]);
        self.inner.issue(repo, number).await
    }
    async fn open_issues(
        &self,
        repo: &Repo,
    ) -> Result<Vec<dvb_core::forge::IssueSummary>, ForgeError> {
        self.record("open_issues", vec![repo.0.clone()]);
        self.inner.open_issues(repo).await
    }
    async fn visibility(&self, repo: &Repo) -> Result<dvb_core::forge::Visibility, ForgeError> {
        self.record("visibility", vec![repo.0.clone()]);
        self.inner.visibility(repo).await
    }
    async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<(), ForgeError> {
        self.record("delete_branch", vec![repo.0.clone(), branch.into()]);
        if let Some(barrier) = &self.cleanup_barrier {
            barrier.wait().await;
            // Let the test inspect both tasks while cleanup remains blocked.
            barrier.wait().await;
        }
        self.inner.delete_branch(repo, branch).await
    }
    async fn ruleset(&self, repo: &Repo) -> Result<ExpectedChecks, ForgeError> {
        self.record("ruleset", vec![repo.0.clone()]);
        self.inner.ruleset(repo).await
    }
    async fn rulesets_allowed(&self, repo: &Repo) -> Result<bool, ForgeError> {
        self.record("rulesets_allowed", vec![repo.0.clone()]);
        self.inner.rulesets_allowed(repo).await
    }
    async fn pr(&self, repo: &Repo, pr: u64) -> Result<PrObservation, ForgeError> {
        self.record("pr", vec![repo.0.clone(), pr.to_string()]);
        let barrier = self.pr_barrier.lock().unwrap().take();
        if let Some(barrier) = barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        let mut seen = self.inner.pr(repo, pr).await?;
        if let Some(head) = self.stale_pr_once.lock().unwrap().take() {
            seen.head = head;
        }
        Ok(seen)
    }
    async fn find_pr(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Option<PrObservation>, ForgeError> {
        self.record("find_pr", vec![repo.0.clone(), branch.into()]);
        self.inner.find_pr(repo, branch).await
    }
}
