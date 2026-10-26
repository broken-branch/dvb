//! The Fake forge: PRs, checks, issues and the ruleset recorded in memory for
//! tests and dev mode. No network.
//!
//! `FakeForge::default()` only records. `FakeForge::local_git()` also works on a
//! local bare repo, the `Repo` being its path: `push` is a real
//! `git push` from the engine's mirror, and `merge` writes a real squash commit
//! onto the repo's default branch with the head's author and committer.

use crate::gateway::Wait;
use crate::github::refused;
use dvb_core::forge::{
    CheckObservation, ExpectedChecks, Forge, ForgeError, Issue, IssueError, IssueState,
    IssueSummary, MergeError, Pr, PrObservation, PrState, Repo, Visibility,
};
use dvb_core::git::Git;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct RepoState {
    branches: BTreeMap<String, String>,
    prs: Vec<PrObservation>,
    bodies: BTreeMap<u64, String>,
    checks: BTreeMap<String, Vec<CheckObservation>>,
    /// The check runs whose rerun was asked, in order: head, run id.
    reruns: Vec<(String, u64)>,
    /// The heads whose CI runs were asked to be cancelled, in order.
    cancels: Vec<String>,
    /// Log excerpts by check run id; any other run's log is unreadable.
    excerpts: BTreeMap<u64, String>,
    closed_issues: Vec<u64>,
    issues: BTreeMap<u64, Issue>,
    /// When each issue was opened: the Fake's time when it first saw it.
    opened: BTreeMap<u64, dvb_core::Timestamp>,
    /// The repository is public; repositories are private by default.
    public: bool,
    ruleset: ExpectedChecks,
    /// Merges need the head up to date with the base (a strict ruleset).
    up_to_date: bool,
    created_id: Option<u64>,
    /// The repository's id, given when it is first asked for.
    id: Option<u64>,
    board: bool,
    goals: BTreeMap<String, (u64, dvb_core::forge::GoalMirror)>,
    cards: BTreeMap<String, dvb_core::forge::TaskMirror>,
}

#[derive(Debug, Default)]
pub struct FakeForge {
    repos: Mutex<BTreeMap<Repo, RepoState>>,
    // PR mutations serialize across Git work and recording the resulting state.
    pr_mutations: tokio::sync::Mutex<()>,
    wait: Wait,
    errors: Mutex<BTreeMap<String, std::collections::VecDeque<ForgeError>>>,
    calls: Mutex<Vec<String>>,
    /// Push and merge for real on the bare repo at `Repo`'s path.
    git: bool,
    /// A Free plan: GitHub refuses rulesets on private repositories.
    free_plan: std::sync::atomic::AtomicBool,
    /// The App is installed on a personal account.
    personal: std::sync::atomic::AtomicBool,
    /// Dev mode: a repository's issues are `<dir>/<repo-name>.json`, read on every issue read.
    issue_files: Option<PathBuf>,
    /// Old name -> new name of each renamed repository.
    renamed: Mutex<BTreeMap<Repo, Repo>>,
    ids: std::sync::atomic::AtomicU64,
}

impl FakeForge {
    pub fn with_clock(mut self, clock: Arc<dyn dvb_core::Clock>) -> Self {
        self.wait.clock = clock;
        self
    }

    /// Deterministic remote responses, consumed only when the shared wait permits a request.
    pub fn fail_next(&self, operation: &str, error: ForgeError) {
        self.errors
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .entry(operation.into())
            .or_default()
            .push_back(error);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }

    fn request(&self, operation: &str) -> Result<(), ForgeError> {
        self.wait.check().map_err(crate::github::api_error)?;
        self.calls
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .push(operation.into());
        if let Some(error) = self
            .errors
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .get_mut(operation)
            .and_then(std::collections::VecDeque::pop_front)
        {
            if let Some(limit) = error.rate_limit {
                self.wait.record(limit, self.wait.clock.now());
            }
            return Err(error);
        }
        Ok(())
    }

    /// A Fake forge whose repos are local bare repos (dev mode, the e2e job).
    pub fn local_git() -> Self {
        Self {
            git: true,
            ..Self::default()
        }
    }

    /// Dev mode: issues come from `<dir>/<repo-name>.json`, the repo's bare
    /// path's last part without `.git`.
    pub fn with_issue_files(mut self, dir: PathBuf) -> Self {
        self.issue_files = Some(dir);
        self
    }

    /// Replaces the repo's issues with its issues file's, keeping the ones
    /// closed here closed. A missing file means no issues.
    fn read_issue_file(&self, repo: &Repo) -> Result<(), ForgeError> {
        let Some(dir) = &self.issue_files else {
            return Ok(());
        };
        let name = Path::new(&repo.0)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.strip_suffix(".git").unwrap_or(n))
            .ok_or_else(|| refused(format!("{} has no repository name", repo.0)))?;
        let path = dir.join(format!("{name}.json"));
        let read: Vec<Issue> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| refused(format!("issues file {}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(refused(format!("issues file {}: {e}", path.display()))),
        };
        let now = self.wait.clock.now();
        self.with(repo, |r| {
            r.issues = read
                .into_iter()
                .map(|mut issue| {
                    r.opened.entry(issue.number).or_insert(now);
                    issue.url = format!("fake://{}/issues/{}", repo.0, issue.number);
                    if r.closed_issues.contains(&issue.number) {
                        issue.state = IssueState::Closed;
                    }
                    (issue.number, issue)
                })
                .collect();
        });
        Ok(())
    }

    /// Like GitHub, no repository token is given for a renamed repository's old name.
    fn reached(&self, repo: &Repo) -> Result<(), ForgeError> {
        if self
            .renamed
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .contains_key(repo)
        {
            return Err(ForgeError {
                rate_limit: None,
                retryable: false,
                unreachable: true,
                detail: "GitHub HTTP 422".into(),
            });
        }
        Ok(())
    }

    /// Test control: rename the repository, keeping its state; its old name
    /// still finds its id, as GitHub's redirect does.
    pub fn rename_repo(&self, from: &Repo, to: &Repo) {
        let mut repos = self.repos.lock().unwrap_or_else(dvb_core::sync::recover);
        let state = repos.remove(from).unwrap_or_default();
        repos.insert(to.clone(), state);
        drop(repos);
        self.renamed
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .insert(from.clone(), to.clone());
    }

    fn with<T>(&self, repo: &Repo, f: impl FnOnce(&mut RepoState) -> T) -> T {
        let mut repos = self.repos.lock().unwrap_or_else(dvb_core::sync::recover);
        f(repos.entry(repo.clone()).or_default())
    }

    /// Tests set the checks a head reports.
    pub fn set_checks(&self, repo: &Repo, head: &str, checks: Vec<CheckObservation>) {
        self.with(repo, |r| r.checks.insert(head.to_owned(), checks));
    }

    /// Tests set the excerpt of a check run's log.
    pub fn set_check_excerpt(&self, repo: &Repo, run: u64, excerpt: &str) {
        self.with(repo, |r| r.excerpts.insert(run, excerpt.to_owned()));
    }

    /// The ids of the check runs whose rerun was asked, in order.
    pub fn reruns(&self, repo: &Repo) -> Vec<u64> {
        self.with(repo, |r| r.reruns.iter().map(|(_, id)| *id).collect())
    }

    /// The heads whose CI runs were asked to be cancelled, in order.
    pub fn cancels(&self, repo: &Repo) -> Vec<String> {
        self.with(repo, |r| r.cancels.clone())
    }

    /// Test control: conflict evidence, or None while mergeability is unknown.
    pub fn set_conflicting(&self, repo: &Repo, pr: u64, conflicting: Option<bool>) {
        self.with(repo, |r| {
            if let Some(p) = r.prs.iter_mut().find(|p| p.number == pr) {
                p.conflicting = conflicting;
            }
        });
    }

    /// Test control: like a strict ruleset, a merge of a head behind its base
    /// is refused (local Git repos only).
    pub fn require_up_to_date(&self, repo: &Repo) {
        self.with(repo, |r| r.up_to_date = true);
    }

    /// Test control: close an open PR, waiting for any in-flight merge receipt.
    pub async fn close_pr(&self, repo: &Repo, pr: u64) {
        let _mutation = self.pr_mutations.lock().await;
        self.with(repo, |r| {
            if let Some(p) = r
                .prs
                .iter_mut()
                .find(|p| p.number == pr && p.state == PrState::Open)
            {
                p.state = PrState::Closed;
            }
        });
    }

    /// Test control: the account is on GitHub's Free plan.
    pub fn set_free_plan(&self) {
        self.free_plan
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test control: the App is installed on a personal account rather than an organization.
    pub fn set_personal_account(&self) {
        self.personal
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set_ruleset(&self, repo: &Repo, ruleset: ExpectedChecks) {
        self.with(repo, |r| r.ruleset = ruleset);
    }

    pub fn boards(&self) -> Vec<Repo> {
        self.repos
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .iter()
            .filter(|(_, state)| state.board)
            .map(|(repo, _)| repo.clone())
            .collect()
    }

    pub fn milestones(&self, repo: &Repo) -> Vec<(u64, dvb_core::forge::GoalMirror)> {
        self.with(repo, |r| r.goals.values().cloned().collect())
    }
    pub fn cards(&self, repo: &Repo) -> Vec<dvb_core::forge::TaskMirror> {
        self.with(repo, |r| r.cards.values().cloned().collect())
    }
    /// Tests set an issue with its comments and associations.
    /// It was opened now, by the Fake's clock, unless it was set before.
    pub fn set_issue(&self, repo: &Repo, issue: Issue) {
        let now = self.wait.clock.now();
        self.with(repo, |r| {
            r.opened.entry(issue.number).or_insert(now);
            r.issues.insert(issue.number, issue)
        });
    }
    /// Test control: the repository is public, so the trust rule applies to its issues.
    pub fn set_public(&self, repo: &Repo) {
        self.with(repo, |r| r.public = true);
    }
    pub fn closed_issues(&self, repo: &Repo) -> Vec<u64> {
        self.with(repo, |r| r.closed_issues.clone())
    }
    pub fn pr_body(&self, repo: &Repo, pr: u64) -> Option<String> {
        self.with(repo, |r| r.bodies.get(&pr).cloned())
    }
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn accessible_repositories(
        &self,
    ) -> Result<dvb_core::api::GithubRepositories, ForgeError> {
        self.request("accessible_repositories")?;
        let account = if self.personal.load(std::sync::atomic::Ordering::SeqCst) {
            dvb_core::api::GithubAccountType::User
        } else {
            dvb_core::api::GithubAccountType::Organization
        };
        Ok(dvb_core::api::GithubRepositories {
            organization: "fake".into(),
            repository_access_url: crate::github_setup::installation_settings_url(
                account, "fake", 1,
            ),
            repositories: self
                .repos
                .lock()
                .unwrap_or_else(dvb_core::sync::recover)
                .iter()
                .map(|(repo, r)| dvb_core::api::GithubRepository {
                    repo: repo.0.clone(),
                    private: !r.public,
                    default_branch: "main".into(),
                })
                .collect(),
        })
    }
    async fn discover_checks(&self, repo: &Repo) -> dvb_core::api::GithubCheckDiscovery {
        use dvb_core::api::{GithubCheckDiscovery, GithubFailure, GithubFailureCode};
        if let Err(error) = self.request("discover_checks") {
            return GithubCheckDiscovery::Failed {
                failure: GithubFailure {
                    code: GithubFailureCode::DiscoveryFailed,
                    message: "Fake check discovery failed".into(),
                    retryable: error.retryable,
                },
            };
        }
        GithubCheckDiscovery::Discovered {
            default_branch: "main".into(),
            head: "fake-head".into(),
            checks: self.with(repo, |r| r.checks.get("main").cloned().unwrap_or_default()),
        }
    }

    async fn repository_id(&self, repo: &Repo) -> Result<Option<u64>, ForgeError> {
        self.request("repository_id")?;
        let mut current = repo.clone();
        while let Some(next) = self
            .renamed
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .get(&current)
            .cloned()
        {
            current = next;
        }
        let mut repos = self.repos.lock().unwrap_or_else(dvb_core::sync::recover);
        Ok(repos.get_mut(&current).map(|r| {
            *r.id.get_or_insert_with(|| {
                self.ids.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            })
        }))
    }
    fn repoint_mirror(&self, _from: &Repo, _to: &Repo) -> Result<(), ForgeError> {
        // The Fake's mirror state moves with its repository on rename.
        self.request("repoint_mirror")
    }
    fn forget_mirror(&self, _repo: &Repo) -> Result<(), ForgeError> {
        // The Fake's state is the remote's, which a removal leaves as it is.
        self.request("forget_mirror")
    }

    async fn mirror_project(&self, repo: &Repo) -> Result<(), ForgeError> {
        self.request("mirror_project")?;
        self.reached(repo)?;
        self.with(repo, |r| r.board = true);
        Ok(())
    }
    async fn mirror_goal(
        &self,
        repo: &Repo,
        goal: &dvb_core::forge::GoalMirror,
    ) -> Result<u64, ForgeError> {
        self.request("mirror_goal")?;
        self.reached(repo)?;
        Ok(self.with(repo, |r| {
            let number = r
                .goals
                .get(&goal.key)
                .map_or(r.goals.len() as u64 + 1, |(n, _)| *n);
            r.goals.insert(goal.key.clone(), (number, goal.clone()));
            number
        }))
    }
    async fn mirror_task(
        &self,
        repo: &Repo,
        task: &dvb_core::forge::TaskMirror,
    ) -> Result<(), ForgeError> {
        self.request("mirror_task")?;
        self.reached(repo)?;
        self.with(repo, |r| r.cards.insert(task.key.clone(), task.clone()));
        Ok(())
    }

    async fn clone_repo(&self, repo: &Repo, mirror: &Path) -> Result<String, ForgeError> {
        self.request("clone_repo")?;
        let parent = mirror
            .parent()
            .ok_or_else(|| refused("mirror has no parent"))?;
        Git::new(parent)
            .run(
                &[
                    "clone",
                    "--quiet",
                    "--bare",
                    &repo.0,
                    &mirror.to_string_lossy(),
                ],
                &[],
            )
            .await
            .map_err(|e| refused(e.to_string()))?;
        Git::new(mirror)
            .run(&["symbolic-ref", "--short", "HEAD"], &[])
            .await
            .map_err(|e| refused(e.to_string()))
    }

    async fn commit_author(
        &self,
        repo: &Repo,
        sha: &str,
    ) -> Result<dvb_core::api::CommitIdentity, ForgeError> {
        self.request("commit_author")?;
        if !self.git {
            return Err(refused("commit author needs the local Git forge"));
        }
        let origin = Path::new(repo.0.strip_prefix("file://").unwrap_or(&repo.0));
        let who = Git::new(origin)
            .run(
                &[
                    "log",
                    "-1",
                    "--format=%an%n%ae",
                    "--end-of-options",
                    &format!("{sha}^{{commit}}"),
                ],
                &[],
            )
            .await
            .map_err(|error| refused(error.to_string()))?;
        let (name, email) = who
            .split_once('\n')
            .ok_or_else(|| refused("commit author is unreadable"))?;
        Ok(dvb_core::api::CommitIdentity {
            name: name.into(),
            email: email.into(),
        })
    }
    async fn fetch(&self, repo: &Repo, mirror: &Path, branch: &str) -> Result<String, ForgeError> {
        self.request("fetch")?;
        Git::new(mirror)
            .run(
                &[
                    "fetch",
                    "--quiet",
                    &repo.0,
                    "+refs/heads/*:refs/heads/*",
                    "+refs/tags/*:refs/tags/*",
                ],
                &[],
            )
            .await
            .map_err(|e| refused(e.to_string()))?;
        Git::new(mirror)
            .run(&["rev-parse", &format!("refs/heads/{branch}")], &[])
            .await
            .map_err(|e| refused(e.to_string()))
    }

    async fn install_ruleset(
        &self,
        repo: &Repo,
        _branch: &str,
        checks: &[String],
    ) -> Result<(), ForgeError> {
        self.request("install_ruleset")?;
        self.set_ruleset(
            repo,
            ExpectedChecks {
                pr_required: true,
                required_checks: checks.to_vec(),
                no_force_push: true,
            },
        );
        Ok(())
    }

    async fn push_setup(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        self.request("push_setup")?;
        let id = self
            .created_repo_id(repo)
            .ok_or_else(|| refused("setup push requires a recorded repository creation"))?;
        self.resume_setup(repo, id).await?;
        if self.git {
            let default = Git::new(Path::new(&repo.0))
                .run(&["symbolic-ref", "--short", "HEAD"], &[])
                .await
                .map_err(|e| refused(e.to_string()))?;
            if default != branch {
                return Err(refused("setup branch must be the default branch"));
            }
            Git::new(mirror)
                .run(
                    &[
                        "push",
                        "--quiet",
                        &format!("--force-with-lease=refs/heads/{branch}:"),
                        &repo.0,
                        &format!("{head}:refs/heads/{branch}"),
                    ],
                    &[],
                )
                .await
                .map_err(|e| refused(e.to_string()))?;
        } else if self.with(repo, |r| r.branches.contains_key(branch)) {
            return Err(refused("setup branch already exists"));
        }
        self.with(repo, |r| {
            r.branches.insert(branch.to_owned(), head.to_owned());
            r.created_id = None;
        });
        Ok(())
    }

    fn can_create_repo(&self) -> bool {
        !self.personal.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn create_repo(&self, org: &str, name: &str) -> Result<Repo, ForgeError> {
        self.request("create_repo")?;
        if !self.can_create_repo() {
            return Err(refused(
                "an App cannot create a personal account's repository",
            ));
        }
        let repo = Repo(format!("{org}/{name}"));
        let id = if self.git {
            if Path::new(&repo.0).exists() {
                return Err(refused("repository already exists"));
            }
            Git::new(Path::new(org))
                .run(&["init", "--quiet", "--bare", "-b", "main", name], &[])
                .await
                .map_err(|e| refused(e.to_string()))?;
            std::fs::metadata(&repo.0)
                .map_err(|e| refused(e.to_string()))?
                .ino()
        } else {
            let repos = self.repos.lock().unwrap_or_else(dvb_core::sync::recover);
            if repos.contains_key(&repo) {
                return Err(refused("repository already exists"));
            }
            repos.len() as u64 + 1
        };
        self.with(&repo, |r| r.created_id = Some(id));
        Ok(repo)
    }

    fn created_repo_id(&self, repo: &Repo) -> Option<u64> {
        self.with(repo, |r| r.created_id)
    }

    async fn resume_setup(&self, repo: &Repo, repository_id: u64) -> Result<(), ForgeError> {
        self.request("resume_setup")?;
        let actual = if self.git {
            std::fs::metadata(&repo.0)
                .map_err(|e| refused(e.to_string()))?
                .ino()
        } else {
            self.created_repo_id(repo)
                .ok_or_else(|| refused("no repository creation"))?
        };
        if actual != repository_id {
            return Err(refused("repository differs from the recorded creation"));
        }
        self.with(repo, |r| r.created_id = Some(repository_id));
        Ok(())
    }

    async fn push(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        self.request("push")?;
        let _mutation = self.pr_mutations.lock().await;
        if self.git {
            Git::new(mirror)
                .run(
                    &[
                        "push",
                        "--quiet",
                        &repo.0,
                        &format!("{head}:refs/heads/{branch}"),
                    ],
                    &[],
                )
                .await
                .map_err(|e| refused(e.to_string()))?;
        }
        self.with(repo, |r| {
            r.branches.insert(branch.to_owned(), head.to_owned());
            for pr in r
                .prs
                .iter_mut()
                .filter(|p| p.branch == branch && p.state == PrState::Open)
            {
                head.clone_into(&mut pr.head);
            }
        });
        Ok(())
    }

    async fn open_pr(
        &self,
        repo: &Repo,
        branch: &str,
        _title: &str,
        body: &str,
    ) -> Result<Pr, ForgeError> {
        self.request("open_pr")?;
        let _mutation = self.pr_mutations.lock().await;
        // Like GitHub: a PR targets the repo's default branch.
        let base = if self.git {
            Git::new(Path::new(&repo.0))
                .run(&["symbolic-ref", "--short", "HEAD"], &[])
                .await
                .map_err(|e| refused(e.to_string()))?
        } else {
            "main".to_owned()
        };
        self.with(repo, |r| {
            let head = r
                .branches
                .get(branch)
                .cloned()
                .ok_or_else(|| refused(format!("no branch {branch}")))?;
            if r.prs
                .iter()
                .any(|p| p.branch == branch && p.state == PrState::Open)
            {
                return Err(refused(format!("a PR for {branch} is already open")));
            }
            let number = r.prs.len() as u64 + 1;
            let url = format!("fake://{}/pull/{number}", repo.0);
            r.bodies.insert(number, body.to_owned());
            r.prs.push(PrObservation {
                number,
                url: url.clone(),
                state: PrState::Open,
                branch: branch.to_owned(),
                head,
                base,
                merge_commit: None,
                conflicting: Some(false),
            });
            Ok(Pr { number, url })
        })
    }

    async fn edit_pr_body(&self, repo: &Repo, pr: u64, body: &str) -> Result<(), ForgeError> {
        self.request("edit_pr_body")?;
        let _mutation = self.pr_mutations.lock().await;
        self.with(repo, |r| {
            if !r
                .prs
                .iter()
                .any(|p| p.number == pr && p.state == PrState::Open)
            {
                return Err(refused(format!("no open PR #{pr}")));
            }
            r.bodies.insert(pr, body.to_owned());
            Ok(())
        })
    }
    async fn checks(&self, repo: &Repo, head: &str) -> Result<Vec<CheckObservation>, ForgeError> {
        self.request("checks")?;
        Ok(self.with(repo, |r| r.checks.get(head).cloned().unwrap_or_default()))
    }

    async fn main_checks(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Vec<CheckObservation>, ForgeError> {
        self.request("main_checks")?;
        Ok(self.with(repo, |r| r.checks.get(branch).cloned().unwrap_or_default()))
    }

    /// Like GitHub's attempts, with one workflow per head: the head's
    /// interrupted runs are rerun together once; a run asked again is the
    /// rerun not reported yet, and any other run of the head is that rerun.
    async fn rerun_checks(
        &self,
        repo: &Repo,
        head: &str,
        runs: &[u64],
        recovery: bool,
    ) -> Result<(), ForgeError> {
        self.request("rerun_checks")?;
        self.with(repo, |r| {
            let asked: Vec<u64> = r
                .reruns
                .iter()
                .filter(|(h, _)| h == head)
                .map(|(_, id)| *id)
                .collect();
            if asked.is_empty() || (recovery && runs.iter().all(|id| !asked.contains(id))) {
                r.reruns
                    .extend(runs.iter().map(|id| (head.to_owned(), *id)));
                return Ok(());
            }
            let again: Vec<String> = runs
                .iter()
                .filter(|id| !asked.contains(id))
                .map(|id| {
                    r.checks
                        .get(head)
                        .and_then(|seen| seen.iter().find(|c| c.run == Some(*id)))
                        .map_or("a check".to_owned(), |c| c.name.clone())
                })
                .collect();
            if again.is_empty() {
                return Ok(());
            }
            Err(ForgeError {
                rate_limit: None,
                retryable: false,
                unreachable: false,
                detail: format!("{} failed again after its rerun", again.join(", ")),
            })
        })
    }

    async fn cancel_checks(&self, repo: &Repo, head: &str) -> Result<(), ForgeError> {
        self.request("cancel_checks")?;
        self.with(repo, |r| r.cancels.push(head.to_owned()));
        Ok(())
    }

    async fn check_excerpt(&self, repo: &Repo, run: u64) -> Result<String, ForgeError> {
        self.request("check_excerpt")?;
        self.with(repo, |r| r.excerpts.get(&run).cloned())
            .ok_or_else(|| refused(format!("no log for check run {run}")))
    }

    async fn merge(
        &self,
        repo: &Repo,
        pr: u64,
        sha: &str,
        title: &str,
    ) -> Result<String, MergeError> {
        self.request("merge").map_err(MergeError::Forge)?;
        let _mutation = self.pr_mutations.lock().await;
        let local = self.git;
        let (p, strict) = self.with(repo, |r| {
            let p = r
                .prs
                .iter()
                .find(|p| p.number == pr)
                .cloned()
                .ok_or_else(|| MergeError::Refused(format!("no PR #{pr}")))?;
            Ok((p, local && r.up_to_date))
        })?;
        match p.state {
            PrState::Open => {}
            PrState::Closed | PrState::Merged => {
                return Err(MergeError::Refused(format!("PR #{pr} is not open")));
            }
        }
        if p.head != sha {
            return Err(MergeError::HeadMoved);
        }
        if strict {
            let dir = Path::new(&repo.0);
            let base = Git::new(dir)
                .run(&["rev-parse", &format!("refs/heads/{}", p.base)], &[])
                .await
                .map_err(|e| MergeError::Forge(refused(e.to_string())))?;
            if Git::new(dir)
                .run(&["merge-base", "--is-ancestor", &base, sha], &[])
                .await
                .is_err()
            {
                return Err(MergeError::Refused("the head branch is out of date".into()));
            }
        }
        let commit = if local {
            squash(Path::new(&repo.0), &p.base, sha, pr, title)
                .await
                .map_err(MergeError::Forge)?
        } else {
            format!("squash-{pr}-{sha}")
        };
        self.with(repo, |r| {
            let current = r
                .prs
                .iter_mut()
                .find(|p| p.number == pr)
                .ok_or_else(|| MergeError::Refused(format!("no PR #{pr}")))?;
            if current.head != sha {
                return Err(MergeError::HeadMoved);
            }
            match current.state {
                PrState::Open => {}
                PrState::Closed | PrState::Merged => {
                    return Err(MergeError::Refused(format!("PR #{pr} is not open")));
                }
            }
            current.state = PrState::Merged;
            current.merge_commit = Some(commit.clone());
            Ok(commit)
        })
    }

    async fn close_issue(&self, repo: &Repo, number: u64) -> Result<(), ForgeError> {
        self.request("close_issue")?;
        self.with(repo, |r| {
            r.closed_issues.push(number);
            if let Some(issue) = r.issues.get_mut(&number) {
                issue.state = IssueState::Closed;
            }
        });
        Ok(())
    }

    async fn issue(&self, repo: &Repo, number: u64) -> Result<Option<Issue>, IssueError> {
        self.request("issue")?;
        self.reached(repo)?;
        self.read_issue_file(repo)?;
        self.with(repo, |r| {
            if r.prs.iter().any(|pr| pr.number == number) {
                return Err(IssueError::PullRequest { number });
            }
            Ok(r.issues.get(&number).cloned())
        })
    }

    async fn open_issues(&self, repo: &Repo) -> Result<Vec<IssueSummary>, ForgeError> {
        self.request("open_issues")?;
        self.reached(repo)?;
        self.read_issue_file(repo)?;
        Ok(self.with(repo, |r| {
            r.issues
                .values()
                .filter(|i| i.state == IssueState::Open)
                .map(|i| IssueSummary {
                    number: i.number,
                    url: i.url.clone(),
                    title: i.title.clone(),
                    labels: i.labels.clone(),
                    author: i.author.clone(),
                    association: i.association,
                    opened: r.opened[&i.number],
                })
                .collect()
        }))
    }

    async fn visibility(&self, repo: &Repo) -> Result<Visibility, ForgeError> {
        self.request("visibility")?;
        self.reached(repo)?;
        Ok(self.with(repo, |r| {
            if r.public {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }))
    }

    async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<(), ForgeError> {
        self.request("delete_branch")?;
        self.with(repo, |r| r.branches.remove(branch));
        Ok(())
    }

    async fn ruleset(&self, repo: &Repo) -> Result<ExpectedChecks, ForgeError> {
        self.request("ruleset")?;
        Ok(self.with(repo, |r| r.ruleset.clone()))
    }
    async fn rulesets_allowed(&self, repo: &Repo) -> Result<bool, ForgeError> {
        self.request("rulesets_allowed")?;
        let free = self.free_plan.load(std::sync::atomic::Ordering::SeqCst);
        // A read: it must not make the repository reachable to later reads.
        let public = self
            .repos
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .get(repo)
            .is_some_and(|r| r.public);
        Ok(!free || public)
    }
    async fn pr(&self, repo: &Repo, number: u64) -> Result<PrObservation, ForgeError> {
        self.request("pr")?;
        self.with(repo, |r| {
            r.prs
                .iter()
                .find(|p| p.number == number)
                .cloned()
                .ok_or_else(|| refused(format!("no PR #{number}")))
        })
    }

    async fn find_pr(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Option<PrObservation>, ForgeError> {
        self.request("find_pr")?;
        Ok(self.with(repo, |r| {
            r.prs.iter().rev().find(|p| p.branch == branch).cloned()
        }))
    }
}

/// Squash `head` onto `base` in the bare repo, as GitHub's squash merge does:
/// one new commit on `base` with the merged tree, the head's author and committer,
/// and the head's subject with the PR number.
async fn squash(
    repo: &Path,
    base: &str,
    head: &str,
    pr: u64,
    title: &str,
) -> Result<String, ForgeError> {
    let base_ref = format!("refs/heads/{base}");
    let old = Git::new(repo)
        .run(&["rev-parse", &base_ref], &[])
        .await
        .map_err(|e| refused(e.to_string()))?;
    let tree = Git::new(repo)
        .run(&["merge-tree", "--write-tree", &old, head], &[])
        .await
        .map_err(|e| refused(e.to_string()))?;
    let tree = tree.lines().next().unwrap_or_default().to_owned();
    let who = Git::new(repo)
        .run(&["log", "-1", "--format=%an%n%ae%n%cn%n%ce", head], &[])
        .await
        .map_err(|e| refused(e.to_string()))?;
    let who: Vec<&str> = who.lines().collect();
    let [an, ae, cn, ce] = who[..] else {
        return Err(refused(format!("cannot read the author of {head}")));
    };
    let message = format!("{title} (#{pr})");
    let env = [
        ("GIT_AUTHOR_NAME", an),
        ("GIT_AUTHOR_EMAIL", ae),
        ("GIT_COMMITTER_NAME", cn),
        ("GIT_COMMITTER_EMAIL", ce),
    ];
    let commit = Git::new(repo)
        .run(&["commit-tree", &tree, "-p", &old, "-m", &message], &env)
        .await
        .map_err(|e| refused(e.to_string()))?;
    Git::new(repo)
        .run(&["update-ref", &base_ref, &commit, &old], &[])
        .await
        .map_err(|e| refused(e.to_string()))?;
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;

    #[tokio::test]
    async fn a_created_public_repository_resumes() {
        for local_git in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let forge = if local_git {
                FakeForge::local_git()
            } else {
                FakeForge::default()
            };
            let repo = forge
                .create_repo(tmp.path().to_str().unwrap(), "new.git")
                .await
                .unwrap();
            let id = forge.created_repo_id(&repo).unwrap();
            forge.set_public(&repo);
            assert!(forge.resume_setup(&repo, id + 1).await.is_err());
            forge.resume_setup(&repo, id).await.unwrap();
            assert_eq!(forge.created_repo_id(&repo), Some(id));
            assert_eq!(forge.visibility(&repo).await.unwrap(), Visibility::Public);
            if local_git {
                assert!(Path::new(&repo.0).exists());
            }
        }
    }

    async fn published(forge: &FakeForge) -> Repo {
        let repo = forge
            .create_repo("example-org", "example-repo")
            .await
            .unwrap();
        forge
            .push(&repo, Path::new("/unused"), "dvb/t-1", "h1")
            .await
            .unwrap();
        repo
    }

    #[tokio::test]
    async fn a_lost_open_reply_is_recovered_by_branch() {
        let forge = FakeForge::default();
        let repo = published(&forge).await;
        let _lost = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();

        let found = forge.find_pr(&repo, "dvb/t-1").await.unwrap().unwrap();
        assert_eq!(
            (
                found.number,
                found.state,
                found.head.as_str(),
                found.base.as_str()
            ),
            (1, PrState::Open, "h1", "main")
        );
        assert!(forge.open_pr(&repo, "dvb/t-1", "t", "b").await.is_err());
        assert_eq!(forge.find_pr(&repo, "dvb/other").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_lost_merge_reply_is_recovered_by_reading_the_pr() {
        let forge = FakeForge::default();
        let repo = published(&forge).await;
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        let commit = forge.merge(&repo, pr.number, "h1", "t").await.unwrap();

        let seen = forge.pr(&repo, pr.number).await.unwrap();
        assert_eq!(seen.state, PrState::Merged);
        assert_eq!(seen.merge_commit, Some(commit));
        assert!(matches!(
            forge.merge(&repo, pr.number, "h1", "t").await,
            Err(MergeError::Refused(_))
        ));
    }

    async fn sh(dir: &Path, args: &[&str]) -> String {
        let env = [
            ("GIT_AUTHOR_NAME", "n"),
            ("GIT_AUTHOR_EMAIL", "n@example.invalid"),
            ("GIT_COMMITTER_NAME", "n"),
            ("GIT_COMMITTER_EMAIL", "n@example.invalid"),
        ];
        Git::new(dir)
            .run(args, &env)
            .await
            .map_err(|e| refused(e.to_string()))
            .unwrap()
    }

    async fn seed_repo(dir: &Path, branch: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = dir.join("origin.git");
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            dir,
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                branch,
                origin.to_str().unwrap(),
            ],
        )
        .await;
        sh(&work, &["init", "--quiet", "-b", branch]).await;
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        sh(&work, &["add", "."]).await;
        sh(&work, &["commit", "--quiet", "-m", "first"]).await;
        sh(
            &work,
            &["push", "--quiet", origin.to_str().unwrap(), branch],
        )
        .await;
        (origin, work)
    }

    #[tokio::test]
    async fn dev_issues_file_is_read_on_every_issue_read() {
        let tmp = tempfile::tempdir().unwrap();
        let forge = FakeForge::local_git().with_issue_files(tmp.path().to_owned());
        let repo = Repo("/srv/origin.git".into());
        assert!(forge.open_issues(&repo).await.unwrap().is_empty());

        let file = tmp.path().join("origin.json");
        let one = r#"{"number": 1, "title": "One", "body": "b", "labels": ["bug"],
            "author": "o", "author_association": "OWNER",
            "comments": [{"author": "m", "author_association": "MEMBER", "body": "c"}]}"#;
        let two = r#"{"number": 2, "title": "Two", "author": "x", "author_association": "NONE"}"#;
        std::fs::write(&file, format!("[{one}]")).unwrap();
        let issue = forge.issue(&repo, 1).await.unwrap().unwrap();
        assert_eq!(
            (
                issue.title.as_str(),
                issue.labels,
                issue.comments[0].body.as_str()
            ),
            ("One", vec!["bug".to_owned()], "c")
        );

        std::fs::write(&file, format!("[{one}, {two}]")).unwrap();
        let open = forge.open_issues(&repo).await.unwrap();
        assert_eq!(open.iter().map(|i| i.number).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(open[1].association, dvb_core::forge::Association::None);

        forge.close_issue(&repo, 1).await.unwrap();
        let open = forge.open_issues(&repo).await.unwrap();
        assert_eq!(open.iter().map(|i| i.number).collect::<Vec<_>>(), [2]);

        std::fs::write(&file, "[{").unwrap();
        let error = forge.open_issues(&repo).await.unwrap_err();
        assert!(
            error.detail.contains(&file.display().to_string()),
            "{error}"
        );
    }

    #[tokio::test]
    async fn local_git_pushes_and_squashes_onto_main() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "main").await;
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a"]).await;
        let head = sh(&work, &["rev-parse", "HEAD"]).await;

        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().to_owned());
        forge.push(&repo, &work, "dvb/t-1", &head).await.unwrap();
        assert_eq!(
            sh(&origin, &["rev-parse", "refs/heads/dvb/t-1"]).await,
            head
        );
        let pr = forge
            .open_pr(&repo, "dvb/t-1", "Squash a", "b")
            .await
            .unwrap();
        let merged = forge
            .merge(&repo, pr.number, &head, "Squash a")
            .await
            .unwrap();

        assert_eq!(sh(&origin, &["rev-parse", "refs/heads/main"]).await, merged);
        assert_eq!(sh(&origin, &["show", "main:a.txt"]).await, "two");
        assert_eq!(
            sh(&origin, &["log", "-1", "--format=%s", "main"]).await,
            "Squash a (#1)"
        );
        assert_eq!(sh(&origin, &["rev-list", "--count", "main"]).await, "2");
    }

    #[tokio::test]
    async fn a_pr_targets_and_merges_into_the_repo_s_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "trunk").await;
        let o = origin.to_str().unwrap();
        sh(&work, &["push", "--quiet", o, "trunk:main"]).await;
        let main_before = sh(&origin, &["rev-parse", "main"]).await;
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a"]).await;
        let head = sh(&work, &["rev-parse", "HEAD"]).await;

        let forge = FakeForge::local_git();
        let repo = Repo(o.to_owned());
        forge.push(&repo, &work, "dvb/t-1", &head).await.unwrap();
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        assert_eq!(forge.pr(&repo, pr.number).await.unwrap().base, "trunk");
        let merged = forge.merge(&repo, pr.number, &head, "t").await.unwrap();
        assert_eq!(sh(&origin, &["rev-parse", "trunk"]).await, merged);
        assert_eq!(sh(&origin, &["rev-parse", "main"]).await, main_before);
    }

    #[tokio::test]
    async fn concurrent_local_merges_keep_both_pr_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "main").await;
        let base = sh(&work, &["rev-parse", "HEAD"]).await;
        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().to_owned());
        let mut prs = Vec::new();
        for name in ["first", "second"] {
            sh(&work, &["checkout", "--quiet", "--detach", &base]).await;
            std::fs::write(work.join(name), name).unwrap();
            sh(&work, &["add", name]).await;
            sh(&work, &["commit", "--quiet", "-m", name]).await;
            let head = sh(&work, &["rev-parse", "HEAD"]).await;
            forge.push(&repo, &work, name, &head).await.unwrap();
            let pr = forge.open_pr(&repo, name, name, "fixture").await.unwrap();
            prs.push((pr.number, head));
        }
        let (first, second) = tokio::join!(
            forge.merge(&repo, prs[0].0, &prs[0].1, "t"),
            forge.merge(&repo, prs[1].0, &prs[1].1, "t"),
        );
        first.unwrap();
        second.unwrap();
        for name in ["first", "second"] {
            assert_eq!(sh(&origin, &["show", &format!("main:{name}")]).await, name);
        }
        assert_eq!(sh(&origin, &["rev-list", "--count", "main"]).await, "3");
    }

    #[tokio::test]
    async fn push_waits_for_the_merge_receipt() {
        let forge = FakeForge::default();
        let repo = published(&forge).await;
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        // Hold exactly the lock an async squash retains while Git is pending.
        let merge = forge.pr_mutations.lock().await;
        let push = forge.push(&repo, Path::new("/unused"), "dvb/t-1", "h2");
        tokio::pin!(push);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(push.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert_eq!(forge.pr(&repo, pr.number).await.unwrap().head, "h1");
        drop(merge);
        push.await.unwrap();
        assert_eq!(forge.pr(&repo, pr.number).await.unwrap().head, "h2");
    }

    #[tokio::test]
    async fn close_waits_for_a_local_merge_and_preserves_its_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "main").await;
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a"]).await;
        let head = sh(&work, &["rev-parse", "HEAD"]).await;
        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().to_owned());
        forge.push(&repo, &work, "dvb/t-1", &head).await.unwrap();
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        let merge = forge.merge(&repo, pr.number, &head, "t");
        tokio::pin!(merge);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(merge.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let close = forge.close_pr(&repo, pr.number);
        tokio::pin!(close);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let (merged, ()) = tokio::join!(merge, close);
        let commit = merged.unwrap();
        let seen = forge.pr(&repo, pr.number).await.unwrap();
        assert_eq!(seen.state, PrState::Merged);
        assert_eq!(seen.head, head);
        assert_eq!(seen.merge_commit, Some(commit.clone()));
        assert_eq!(sh(&origin, &["rev-parse", "main"]).await, commit);
        assert_eq!(sh(&origin, &["show", "main:a.txt"]).await, "two");
    }

    #[tokio::test]
    async fn push_during_a_local_merge_keeps_the_merged_head_and_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "main").await;
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a"]).await;
        let head = sh(&work, &["rev-parse", "HEAD"]).await;
        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().to_owned());
        forge.push(&repo, &work, "dvb/t-1", &head).await.unwrap();
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        std::fs::write(work.join("a.txt"), "three\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a again"]).await;
        let newer = sh(&work, &["rev-parse", "HEAD"]).await;

        let merge = forge.merge(&repo, pr.number, &head, "t");
        tokio::pin!(merge);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(merge.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        let (pushed, merged) = tokio::join!(forge.push(&repo, &work, "dvb/t-1", &newer), merge,);
        pushed.unwrap();
        let commit = merged.unwrap();
        let seen = forge.pr(&repo, pr.number).await.unwrap();
        assert_eq!(seen.state, PrState::Merged);
        assert_eq!(seen.head, head);
        assert_eq!(seen.merge_commit, Some(commit.clone()));
        assert_eq!(sh(&origin, &["rev-parse", "main"]).await, commit);
        assert_eq!(sh(&origin, &["show", "main:a.txt"]).await, "two");
        assert_eq!(sh(&origin, &["rev-parse", "dvb/t-1"]).await, newer);
    }

    #[tokio::test]
    async fn push_or_close_before_a_local_merge_refuses_without_advancing_main() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, work) = seed_repo(tmp.path(), "main").await;
        let base = sh(&origin, &["rev-parse", "main"]).await;
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a"]).await;
        let head = sh(&work, &["rev-parse", "HEAD"]).await;
        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().to_owned());
        forge.push(&repo, &work, "dvb/t-1", &head).await.unwrap();
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        std::fs::write(work.join("a.txt"), "three\n").unwrap();
        sh(&work, &["commit", "--quiet", "-am", "Change a again"]).await;
        let newer = sh(&work, &["rev-parse", "HEAD"]).await;
        forge.push(&repo, &work, "dvb/t-1", &newer).await.unwrap();
        assert_eq!(
            forge.merge(&repo, pr.number, &head, "t").await,
            Err(MergeError::HeadMoved)
        );
        forge.close_pr(&repo, pr.number).await;
        assert!(matches!(
            forge.merge(&repo, pr.number, &newer, "t").await,
            Err(MergeError::Refused(_))
        ));
        let seen = forge.pr(&repo, pr.number).await.unwrap();
        assert_eq!(seen.state, PrState::Closed);
        assert_eq!(seen.head, newer);
        assert_eq!(seen.merge_commit, None);
        assert_eq!(sh(&origin, &["rev-parse", "main"]).await, base);
        assert_eq!(sh(&origin, &["show", "main:a.txt"]).await, "one");
    }

    #[tokio::test]
    async fn merge_is_pinned_to_the_head() {
        let forge = FakeForge::default();
        let repo = published(&forge).await;
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        forge
            .push(&repo, Path::new("/unused"), "dvb/t-1", "h2")
            .await
            .unwrap();
        assert_eq!(
            forge.merge(&repo, pr.number, "h1", "t").await,
            Err(MergeError::HeadMoved)
        );
        assert_eq!(forge.pr(&repo, pr.number).await.unwrap().head, "h2");
    }
}
