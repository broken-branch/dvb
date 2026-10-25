//! The per-task lock and merge serialization, on a forge that stalls or fails
//! on demand: a stop waits for the merge in flight to be recorded, nothing is
//! sent after it (nor after a read that was outstanding when it came), and
//! resume reads the PR's merged state instead of merging again. Also: a
//! transient failure retries after its backoff, a refusal fails the task with
//! its reason, a moved head is reviewed, and a paused project sends nothing.

use dvb_engine::owner;
mod support;
use async_trait::async_trait;
use dvb_core::forge::{
    CheckObservation, ExpectedChecks, Forge, ForgeError, MergeError, Pr, PrObservation, PrState,
    Repo,
};
use dvb_core::state::Effect;
use dvb_core::{Actor, EventKind, ProjectId, RoleId, TaskId, TaskState};
use dvb_engine::{AppState, dispatch};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use support::wait::FAST as wait;
use tokio::sync::Semaphore;

/// A forge whose merge waits for a permit, after failing `fail` times; whose
/// reads (`pr`, `find_pr`) wait for `reads` permits when `slow_reads`; and
/// whose PR head is `head`.
struct Gate {
    merges: AtomicU32,
    opened: AtomicU32,
    fail: AtomicU32,
    /// Transport errors left for `pr` to return.
    pr_fail: AtomicU32,
    retryable: bool,
    permits: Semaphore,
    reads: Semaphore,
    read_entries: AtomicU32,
    slow_reads: std::sync::atomic::AtomicBool,
    head: std::sync::Mutex<String>,
    merged: std::sync::Mutex<Option<String>>,
}

impl Gate {
    fn new(permits: usize, fail: u32, retryable: bool) -> Arc<Self> {
        Arc::new(Self {
            merges: AtomicU32::new(0),
            opened: AtomicU32::new(0),
            fail: AtomicU32::new(fail),
            pr_fail: AtomicU32::new(0),
            retryable,
            permits: Semaphore::new(permits),
            reads: Semaphore::new(0),
            read_entries: AtomicU32::new(0),
            slow_reads: std::sync::atomic::AtomicBool::new(false),
            head: std::sync::Mutex::new("h1".into()),
            merged: std::sync::Mutex::new(None),
        })
    }

    async fn read(&self) {
        self.read_entries.fetch_add(1, Ordering::SeqCst);
        if self.slow_reads.load(Ordering::SeqCst) {
            self.reads.acquire().await.unwrap().forget();
        }
    }

    fn merges(&self) -> u32 {
        self.merges.load(Ordering::SeqCst)
    }
}

fn no(what: &str) -> ForgeError {
    ForgeError {
        rate_limit: None,
        retryable: false,
        detail: format!("{what} is not used here"),
    }
}

#[async_trait]
impl Forge for Gate {
    async fn accessible_repositories(
        &self,
    ) -> Result<dvb_core::api::GithubRepositories, ForgeError> {
        Err(no("accessible_repositories"))
    }
    async fn discover_checks(&self, repo: &Repo) -> dvb_core::api::GithubCheckDiscovery {
        let _ = repo;
        dvb_core::api::GithubCheckDiscovery::Failed {
            failure: dvb_core::api::GithubFailure {
                code: dvb_core::api::GithubFailureCode::DiscoveryFailed,
                message: "unused".into(),
                retryable: false,
            },
        }
    }

    async fn mirror_project(&self, _: &Repo) -> Result<(), ForgeError> {
        Err(no("mirror_project"))
    }
    async fn mirror_goal(
        &self,
        _: &Repo,
        _: &dvb_core::forge::GoalMirror,
    ) -> Result<u64, ForgeError> {
        Err(no("mirror_goal"))
    }
    async fn mirror_task(
        &self,
        _: &Repo,
        _: &dvb_core::forge::TaskMirror,
    ) -> Result<(), ForgeError> {
        Err(no("mirror_task"))
    }

    async fn clone_repo(&self, _: &Repo, _: &Path) -> Result<String, ForgeError> {
        Err(no("clone_repo"))
    }
    async fn commit_author(
        &self,
        _: &Repo,
        _: &str,
    ) -> Result<dvb_core::api::CommitIdentity, ForgeError> {
        Err(no("commit_author"))
    }
    /// The PR branch's head, as the forge has it.
    async fn fetch(&self, _: &Repo, _: &Path, _: &str) -> Result<String, ForgeError> {
        Ok(self.head.lock().unwrap().clone())
    }
    async fn install_ruleset(&self, _: &Repo, _: &str, _: &[String]) -> Result<(), ForgeError> {
        Err(no("install_ruleset"))
    }
    async fn push_setup(&self, _: &Repo, _: &Path, _: &str, _: &str) -> Result<(), ForgeError> {
        Err(no("push_setup"))
    }
    fn created_repo_id(&self, _: &Repo) -> Option<u64> {
        None
    }
    async fn resume_setup(&self, _: &Repo, _: u64) -> Result<(), ForgeError> {
        Err(no("resume_setup"))
    }
    fn can_create_repo(&self) -> bool {
        false
    }
    async fn create_repo(&self, _: &str, _: &str) -> Result<Repo, ForgeError> {
        Err(no("create_repo"))
    }
    async fn push(&self, _: &Repo, _: &Path, _: &str, _: &str) -> Result<(), ForgeError> {
        Err(no("push"))
    }
    async fn open_pr(&self, _: &Repo, _: &str, _: &str, _: &str) -> Result<Pr, ForgeError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Pr {
            number: 1,
            url: "fake://pr/1".into(),
        })
    }
    async fn edit_pr_body(&self, _: &Repo, _: u64, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
    async fn checks(&self, _: &Repo, _: &str) -> Result<Vec<CheckObservation>, ForgeError> {
        Ok(vec![])
    }
    async fn main_checks(&self, _: &Repo, _: &str) -> Result<Vec<CheckObservation>, ForgeError> {
        Ok(vec![])
    }
    async fn rerun_checks(&self, _: &Repo, _: &str, _: &[u64], _: bool) -> Result<(), ForgeError> {
        Ok(())
    }
    async fn cancel_checks(&self, _: &Repo, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
    async fn check_excerpt(&self, _: &Repo, _: u64) -> Result<String, ForgeError> {
        Err(no("check_excerpt"))
    }
    async fn merge(&self, _: &Repo, _: u64, sha: &str, _: &str) -> Result<String, MergeError> {
        self.merges.fetch_add(1, Ordering::SeqCst);
        if *self.head.lock().unwrap() != sha {
            return Err(MergeError::HeadMoved);
        }
        if self.fail.load(Ordering::SeqCst) > 0 {
            self.fail.fetch_sub(1, Ordering::SeqCst);
            return Err(if self.retryable {
                MergeError::Forge(ForgeError {
                    rate_limit: None,
                    retryable: true,
                    detail: "502".into(),
                })
            } else {
                MergeError::Refused("the branch has conflicts".into())
            });
        }
        self.permits
            .acquire()
            .await
            .map_err(|e| {
                MergeError::Forge(ForgeError {
                    rate_limit: None,
                    retryable: false,
                    detail: e.to_string(),
                })
            })?
            .forget();
        *self.merged.lock().unwrap() = Some("m1".into());
        Ok("m1".into())
    }
    async fn close_issue(&self, _: &Repo, _: u64) -> Result<(), ForgeError> {
        Ok(())
    }
    async fn issue(
        &self,
        _: &Repo,
        _: u64,
    ) -> Result<Option<dvb_core::forge::Issue>, dvb_core::forge::IssueError> {
        Err(no("issue").into())
    }
    async fn open_issues(
        &self,
        _: &Repo,
    ) -> Result<Vec<dvb_core::forge::IssueSummary>, ForgeError> {
        Err(no("open_issues"))
    }
    async fn visibility(&self, _: &Repo) -> Result<dvb_core::forge::Visibility, ForgeError> {
        Err(no("visibility"))
    }
    async fn delete_branch(&self, _: &Repo, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
    async fn ruleset(&self, _: &Repo) -> Result<ExpectedChecks, ForgeError> {
        Ok(ExpectedChecks::default())
    }
    async fn rulesets_allowed(&self, _: &Repo) -> Result<bool, ForgeError> {
        Ok(true)
    }
    async fn pr(&self, _: &Repo, number: u64) -> Result<PrObservation, ForgeError> {
        self.read().await;
        if self.pr_fail.load(Ordering::SeqCst) > 0 {
            self.pr_fail.fetch_sub(1, Ordering::SeqCst);
            return Err(ForgeError {
                rate_limit: None,
                retryable: true,
                detail: "transport error".into(),
            });
        }
        let merged = self.merged.lock().unwrap().clone();
        Ok(PrObservation {
            number,
            url: "fake://pr/1".into(),
            state: if merged.is_some() {
                PrState::Merged
            } else {
                PrState::Open
            },
            branch: "dvb/t-1".into(),
            head: self.head.lock().unwrap().clone(),
            base: "main".into(),
            merge_commit: merged,
            conflicting: Some(false),
        })
    }
    async fn find_pr(&self, _: &Repo, _: &str) -> Result<Option<PrObservation>, ForgeError> {
        self.read().await;
        Ok(None)
    }
}

/// A task reviewed clean on head `h1` with PR #1, waiting to merge.
fn merging(dir: &Path, gate: Arc<Gate>) -> (AppState, TaskId) {
    seeded(dir, gate, TaskState::Merging, true)
}

/// A task in `state` with its branch published at `h1` (by builder run `b1`)
/// and, if `pr`, PR #1.
fn seeded(dir: &Path, gate: Arc<Gate>, state: TaskState, pr: bool) -> (AppState, TaskId) {
    let e = support::engine::open_with(
        dir,
        "dvb",
        dvb_engine::EngineOptions {
            forge: Some(gate),
            launcher: None,
            ..Default::default()
        },
    );
    let (p, t) = (ProjectId::new("p"), TaskId::new("t"));
    let mut events = support::events::approved_task_events();
    events.extend([
        EventKind::RunStarted(
            support::assignment::Assignment::new(dvb_core::RunId::new("b1"))
                .task(Some(t.clone()))
                .hire(dvb_core::HireId::new("h"))
                .vendor(dvb_core::Vendor::Fake)
                .effort(dvb_core::Effort::Medium)
                .reason("x".into())
                .base(Some("b".into()))
                .build(),
        ),
        EventKind::RunEnded {
            run: dvb_core::RunId::new("b1"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
            stderr: None,
        },
        EventKind::BranchPublished {
            task: t.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: "h1".into(),
        },
        // Its reviewer's clean verdict on h1.
        EventKind::RunStarted(
            support::assignment::Assignment::new(dvb_core::RunId::new("v1"))
                .task(Some(t.clone()))
                .role(RoleId::new("code-reviewer"))
                .hire(dvb_core::HireId::new("h"))
                .vendor(dvb_core::Vendor::Fake)
                .effort(dvb_core::Effort::Medium)
                .reason("x".into())
                .reviews(Some("h1".into()))
                .base(Some("h1".into()))
                .build(),
        ),
        EventKind::VerdictRecorded {
            review_stage: None,
            verdict: dvb_core::VerdictId::new("v"),
            task: t.clone(),
            head: "h1".into(),
            run: dvb_core::RunId::new("v1"),
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            vendor: dvb_core::Vendor::Fake,
            findings: vec![],
            summary: "clean".into(),
        },
        EventKind::RunEnded {
            run: dvb_core::RunId::new("v1"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
            stderr: None,
        },
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    let mut rest = Vec::new();
    if pr {
        rest.push(EventKind::PrOpened {
            task: t.clone(),
            pr: 1,
            url: "fake://pr/1".into(),
        });
    }
    rest.push(EventKind::TaskStateChanged {
        task: t.clone(),
        from: TaskState::Planned,
        to: state,
    });
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(rest))
        .unwrap();
    (e, t)
}

fn state(e: &AppState, t: &TaskId) -> TaskState {
    e.proj().tasks[t].view.state.clone()
}

fn kinds(e: &AppState) -> Vec<&'static str> {
    support::events::all(e)
        .iter()
        .map(|ev| ev.kind.kind())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_waits_for_the_merge_in_flight_and_resume_reads_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(0, 0, true);
    let (e, t) = merging(dir.path(), gate.clone());
    dispatch::kick(&e, &t);
    wait.until("the merge to be sent", || gate.merges() == 1)
        .await;

    let stop = tokio::spawn({
        let (e, t) = (e.clone(), t.clone());
        async move { dispatch::stop(&e, &t, Actor::Owner).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The stop is recorded at once, then waits for the merge's lock.
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Merging),
    };
    assert_eq!(state(&e, &t), stopped);
    assert!(!stop.is_finished(), "stop returned with a merge in flight");
    assert!(!kinds(&e).contains(&"pr.merged"));

    gate.permits.add_permits(1);
    let after = stop.await.unwrap().unwrap();
    // The merge completed and was recorded before the stop returned; the task
    // stays stopped and nothing more is sent.
    assert_eq!(after, stopped);
    assert!(kinds(&e).contains(&"pr.merged"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gate.merges(), 1);

    // Resume reads the PR's merged state: done, never merged twice.
    let resumed = dispatch::resume(&e, &t, Actor::Owner).await.unwrap();
    assert_eq!(resumed, TaskState::Merging);
    wait.until("the task to be done", || state(&e, &t) == TaskState::Done)
        .await;
    assert_eq!(gate.merges(), 1);
    let k = kinds(&e);
    for (kind, n) in [
        ("pr.merged", 1),
        ("task.stopped", 1),
        ("task.resumed", 1),
        ("task.done", 1),
    ] {
        assert_eq!(k.iter().filter(|x| **x == kind).count(), n, "{kind}: {k:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_merge_failure_retries_after_its_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 1, true);
    let (e, t) = merging(dir.path(), gate.clone());
    dispatch::kick(&e, &t);
    wait.until("the retry to be pending", || {
        matches!(
            state(&e, &t),
            TaskState::RetryPending {
                effect: Effect::Merge,
                attempt: 1
            }
        )
    })
    .await;
    // Not before the backoff (2 s for the first retry).
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(gate.merges(), 1);
    wait.until("the task to be done", || state(&e, &t) == TaskState::Done)
        .await;
    assert_eq!(gate.merges(), 2);
    assert!(kinds(&e).contains(&"effect.retry"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_merge_fails_the_task_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 1, false);
    let (e, t) = merging(dir.path(), gate.clone());
    dispatch::kick(&e, &t);
    wait.until("the task to fail", || {
        matches!(state(&e, &t), TaskState::Failed { ref reason } if reason.contains("conflicts"))
    })
    .await;
    assert_eq!(gate.merges(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_project_sends_nothing_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 0, true);
    let (e, t) = merging(dir.path(), gate.clone());
    let p = ProjectId::new("p");
    owner::pause(&e, &p, true).unwrap();
    // Twice is fine; it is one event.
    owner::pause(&e, &p, true).unwrap();
    dispatch::kick(&e, &t);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gate.merges(), 0);
    assert_eq!(state(&e, &t), TaskState::Merging);

    owner::pause(&e, &p, false).unwrap();
    wait.until("the task to be done", || state(&e, &t) == TaskState::Done)
        .await;
    assert_eq!(gate.merges(), 1);
    let k = kinds(&e);
    assert_eq!(k.iter().filter(|x| **x == "project.paused").count(), 1);
    assert_eq!(k.iter().filter(|x| **x == "project.resumed").count(), 1);
}

/// A stop recorded while the read before the merge is outstanding lets
/// that read finish, and nothing is sent after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_the_read_before_the_merge_sends_no_merge() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 0, true);
    gate.slow_reads.store(true, Ordering::SeqCst);
    let (e, t) = merging(dir.path(), gate.clone());
    dispatch::kick(&e, &t);
    wait.until("the PR read to enter", || {
        gate.read_entries.load(Ordering::SeqCst) == 1
    })
    .await;
    let stop = tokio::spawn({
        let (e, t) = (e.clone(), t.clone());
        async move { dispatch::stop(&e, &t, Actor::Owner).await }
    });
    wait.until("the stop to be recorded", || {
        matches!(state(&e, &t), TaskState::Stopped { .. })
    })
    .await;
    assert!(!stop.is_finished(), "stop returned with a read in flight");
    gate.reads.add_permits(1);
    stop.await.unwrap().unwrap();
    assert_eq!(gate.merges(), 0);
    // Resumed, the merge goes ahead.
    gate.slow_reads.store(false, Ordering::SeqCst);
    dispatch::resume(&e, &t, Actor::Owner).await.unwrap();
    wait.until("the task to be done", || state(&e, &t) == TaskState::Done)
        .await;
    assert_eq!(gate.merges(), 1);
}

/// The same between looking for the task's PR and opening one.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_the_pr_lookup_opens_no_pr() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 0, true);
    gate.slow_reads.store(true, Ordering::SeqCst);
    let (e, t) = seeded(dir.path(), gate.clone(), TaskState::Publishing, false);
    dispatch::kick(&e, &t);
    wait.until("the PR lookup to enter", || {
        gate.read_entries.load(Ordering::SeqCst) == 1
    })
    .await;
    let stop = tokio::spawn({
        let (e, t) = (e.clone(), t.clone());
        async move { dispatch::stop(&e, &t, Actor::Owner).await }
    });
    wait.until("the stop to be recorded", || {
        matches!(state(&e, &t), TaskState::Stopped { .. })
    })
    .await;
    assert!(!stop.is_finished(), "stop returned with a read in flight");
    gate.reads.add_permits(1);
    stop.await.unwrap().unwrap();
    assert_eq!(gate.opened.load(Ordering::SeqCst), 0);
    assert_eq!(
        state(&e, &t),
        TaskState::Stopped {
            was: Box::new(TaskState::Publishing)
        }
    );
}

/// the PR's head moved, so its new head is the task's and is
/// reviewed; the old head's verdict no longer counts.
#[tokio::test(flavor = "multi_thread")]
async fn a_moved_head_is_adopted_and_reviewed_again() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 0, true);
    *gate.head.lock().unwrap() = "h2".into();
    let (e, t) = merging(dir.path(), gate.clone());
    dispatch::kick(&e, &t);
    wait.until("review to re-enter", || {
        state(&e, &t) == TaskState::InReview
    })
    .await;
    let task = e.proj().tasks[&t].clone();
    assert_eq!(task.view.head.as_deref(), Some("h2"));
    assert!(task.facts.verdict.is_none());
    // Recorded as the task's publication in the same round, and refused.
    let kinds: Vec<_> = support::events::all(&e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::BranchPublished { head, round, .. } = ev.kind {
                Some(format!("published {head} {round}"))
            } else if let EventKind::MergeRefused { .. } = ev.kind {
                Some("refused".to_owned())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(kinds, ["published h1 1", "published h2 1", "refused"]);
    // The PR was read first: no merge was asked for the old head.
    assert_eq!(gate.merges(), 0);
}

/// A step that errors is tried again after a backoff, with no kick.
#[tokio::test(start_paused = true)]
async fn a_failed_step_retries_with_backoff_and_no_kick() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::new(10, 0, true);
    gate.pr_fail.store(2, Ordering::SeqCst);
    let (e, t) = seeded(dir.path(), gate.clone(), TaskState::AwaitingChecks, true);
    // CI is expected, so the checks step reads the PR (and then polls).
    let p = ProjectId::new("p");
    let project = e.proj().projects[&p].view.clone();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![support::events::project_event_with_ci(
            &(p),
            &(project.name),
            &(project.repo),
            dvb_core::event::CiMode::Expected(vec!["rust".into()]),
        )])
    })
    .unwrap();
    dispatch::kick(&e, &t);
    wait.until("the first failed read", || {
        gate.pr_fail.load(Ordering::SeqCst) == 1
    })
    .await;
    // Two failed reads: the dispatcher waits 30 s, then 60 s.
    tokio::time::sleep(Duration::from_secs(29)).await;
    assert_eq!(gate.pr_fail.load(Ordering::SeqCst), 1);
    assert_eq!(gate.read_entries.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(gate.pr_fail.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_secs(58)).await;
    assert_eq!(gate.read_entries.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(gate.read_entries.load(Ordering::SeqCst) > 2);
}
