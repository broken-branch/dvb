//! Runs that do not exit: a reviewer that gave its verdict and then hangs is
//! ended after the grace period and its task moves on; any run past its time
//! limit is stopped, recorded, and its task takes the usual retry or failure.
//! Both are read from recorded times on the engine's clock, here a fake one
//! moved by the test; the watcher is the one a restart installs.

mod support;
use dvb_core::event::{CiMode, EndReason, RunKind};
use dvb_core::launcher::Launcher;
use dvb_core::run_verbs::{Finding, Severity};
use dvb_core::state::Effect;
use dvb_core::{
    Actor, Command, EventKind, FakeClock, HireId, ProjectId, RoleId, RunAssignment, RunId, TaskId,
    TaskState, Timestamp, caps,
};
use dvb_engine::{AppState, reconcile};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use support::launcher::FakeLauncher;
use support::wait::SLOW as wait;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn builder(run: &str) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .task(Some(TaskId::new("t")))
        .hire(HireId::new("h-backend"))
        .base(Some("b".into()))
        .build()
}

fn reviewer(run: &str) -> RunAssignment {
    support::assignment::Assignment::from_assignment(builder(run))
        .role(RoleId::new("code-reviewer"))
        .hire(HireId::new("h-code-reviewer"))
        .reviews(Some(HEAD.into()))
        .base(Some(HEAD.into()))
        .build()
}

fn moved(from: TaskState, to: TaskState) -> EventKind {
    EventKind::TaskStateChanged {
        task: TaskId::new("t"),
        from,
        to,
    }
}

struct Fixture {
    e: AppState,
    stub: Arc<FakeLauncher>,
    clock: Arc<FakeClock>,
    dir: tempfile::TempDir,
}

/// A project (with `ci`), its roles and hires and one approved task, then
/// `more`. Effects after a decision (a fix round's launch, a merge) are held:
/// only what the engine decided is checked.
fn fixture(ci: CiMode, more: Vec<EventKind>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher {
        start_state: None,
        ..Default::default()
    });
    let clock = Arc::new(FakeClock::at(Timestamp(1_700_000_000_000)));
    let launcher: Arc<dyn Launcher> = stub.clone();
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher),
            clock: Some(clock.clone()),
            ..Default::default()
        },
    );
    let p = ProjectId::new("p");
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds = support::events::approved_task_events();
    kinds[0] = support::events::project_event_with_ci(&p, "p", "/nowhere", ci);
    kinds.splice(1..1, roles.into_iter().chain(hires));
    kinds.extend(more);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    e.update.close_admission();
    Fixture {
        e,
        stub,
        clock,
        dir,
    }
}

impl Fixture {
    /// The engine starts again with `run`'s unit still active: it gets a watcher.
    async fn restart_with(&self, run: &str) {
        self.stub.running(run);
        reconcile::reconcile(self.e.clone()).await.unwrap();
    }

    /// Lets the watcher look at the units a few more times.
    async fn watched(&self) {
        let seen = self.stub.queries.load(Ordering::SeqCst);
        wait.until("the watcher to look again", || {
            self.stub.queries.load(Ordering::SeqCst) >= seen + 3
        })
        .await;
    }

    fn advance(&self, d: std::time::Duration) {
        self.clock
            .advance_millis(i64::try_from(d.as_millis()).unwrap());
    }

    /// `run`'s driver reports a cut-off CLI stream, as when a stop ends the
    /// CLI before the driver sees the stop.
    fn cut_off(&self, run: &str) {
        let dir = self.dir.path().join("work").join(run);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reason"), "parse_error").unwrap();
    }

    fn ended(&self, run: &str) -> Option<EndReason> {
        self.e.proj().runs[&RunId::new(run)].ended.clone()
    }

    fn state(&self) -> TaskState {
        self.e.proj().tasks[&TaskId::new("t")].view.state.clone()
    }

    fn events(&self, kind: &str) -> Vec<EventKind> {
        support::events::all(&self.e)
            .into_iter()
            .filter(|ev| ev.kind.kind() == kind)
            .map(|ev| ev.kind)
            .collect()
    }

    /// The reviewer `v1` records its verdict on the head.
    fn verdict(&self, open: bool) {
        let a = reviewer("v1");
        let finding = Finding {
            severity: Severity::High,
            file: "f".into(),
            line: None,
            title: "t".into(),
            detail: "d".into(),
            open: true,
        };
        let recorded = EventKind::VerdictRecorded {
            review_stage: None,
            verdict: dvb_core::VerdictId::new("v"),
            task: TaskId::new("t"),
            head: HEAD.into(),
            run: a.run.clone(),
            role: a.role,
            model: a.model,
            vendor: a.vendor,
            findings: if open { vec![finding] } else { vec![] },
            summary: "s".into(),
        };
        let cmd = Command::VerdictRecorded {
            head: HEAD.into(),
            open: u32::from(open),
        };
        self.e
            .run_command(
                &a.run,
                dvb_engine::Decision {
                    pre: vec![recorded],
                    ..dvb_engine::Decision::new("verdict", cmd)
                },
            )
            .unwrap()
            .unwrap();
    }
}

/// The reviewer `v1` reviewing the published head.
fn in_review() -> Vec<EventKind> {
    vec![
        EventKind::BranchPublished {
            task: TaskId::new("t"),
            branch: "dvb/t-1".into(),
            head: HEAD.into(),
            round: 1,
        },
        EventKind::RunStarted(reviewer("v1")),
        moved(TaskState::Planned, TaskState::InReview),
    ]
}

/// A reviewer that gave its verdict and then hangs (here: never exits) is
/// ended once it has been quiet for the grace period. It ends `completed`,
/// not as a crash, failure or stop, and its task moves on as after a normal
/// exit, without another review.
async fn a_hung_reviewer(ci: CiMode, open: bool) -> Fixture {
    let f = fixture(ci, in_review());
    f.restart_with("v1").await;
    f.verdict(open);
    f.advance(caps::REVIEWER_GRACE - std::time::Duration::from_secs(1));
    f.watched().await;
    assert_eq!(f.ended("v1"), None, "ended within the grace period");
    assert_eq!(f.stub.stops.load(Ordering::SeqCst), 0);

    f.advance(std::time::Duration::from_secs(2));
    wait.until("the reviewer to be ended", || f.ended("v1").is_some())
        .await;
    assert_eq!(f.ended("v1"), Some(EndReason::Completed));
    assert_eq!(f.stub.stops.load(Ordering::SeqCst), 1);
    assert!(!f.stub.is_active(&RunId::new("v1")));
    assert!(f.events("run.time_limit_reached").is_empty());
    assert!(f.events("task.failed").is_empty());
    assert!(f.events("task.stopped").is_empty());
    let facts = f.e.proj().tasks[&TaskId::new("t")].facts.clone();
    assert_eq!(facts.reviews, 1, "no review again");
    assert_eq!(facts.round, 1, "no review round spent");
    f
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_reviewer_with_open_findings_is_ended_and_the_fix_round_starts() {
    let f = a_hung_reviewer(CiMode::NoCi, true).await;
    // Let the dispatcher act on the end: the fix round, a builder on the head.
    f.e.update.reopen_admission();
    let fix = || {
        f.events("run.started").into_iter().find_map(|k| {
            if let EventKind::RunStarted(a) = k {
                a.reviews.is_none().then_some(a)
            } else {
                None
            }
        })
    };
    wait.until("the fix round", || fix().is_some()).await;
    let fix = fix().unwrap();
    assert_eq!(fix.base.as_deref(), Some(HEAD));
    assert_eq!(fix.reason, "Fix round 2.");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_reviewer_with_a_clean_verdict_is_ended_and_frees_its_slot() {
    let f = a_hung_reviewer(CiMode::Expected(vec!["ci".into()]), false).await;
    assert_eq!(f.state(), TaskState::AwaitingChecks);
    assert_eq!(f.e.proj().live_runs().count(), 0);
}

/// A reviewer that exits by itself within the grace period is not stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_reviewer_that_exits_after_its_verdict_is_not_stopped() {
    let f = fixture(CiMode::NoCi, in_review());
    f.restart_with("v1").await;
    f.verdict(true);
    f.advance(std::time::Duration::from_secs(30));
    f.stub.end(&RunId::new("v1"));
    wait.until("the reviewer's end", || f.ended("v1").is_some())
        .await;
    assert_eq!(f.ended("v1"), Some(EndReason::Completed));
    assert_eq!(f.stub.stops.load(Ordering::SeqCst), 0);
    assert!(f.events("run.time_limit_reached").is_empty());
}

/// The grace's stop also ends a hung reviewer's CLI: the stream it cut off
/// is no parse error. A reviewer that exits by itself with an unreadable
/// stream, within the grace, ends `parse_error`.
#[tokio::test(flavor = "multi_thread")]
async fn a_reviewers_stream_cut_off_by_the_graces_stop_is_no_parse_error() {
    for hung in [true, false] {
        let f = fixture(CiMode::NoCi, in_review());
        f.restart_with("v1").await;
        f.verdict(true);
        f.cut_off("v1");
        if hung {
            f.advance(caps::REVIEWER_GRACE + std::time::Duration::from_secs(1));
        } else {
            f.advance(std::time::Duration::from_secs(30));
            f.stub.end(&RunId::new("v1"));
        }
        wait.until("the reviewer's end", || f.ended("v1").is_some())
            .await;
        let expected = if hung {
            EndReason::Completed
        } else {
            EndReason::ParseError
        };
        assert_eq!(f.ended("v1"), Some(expected));
        assert_eq!(f.stub.stops.load(Ordering::SeqCst), usize::from(hung));
    }
}

/// The builder `b<n>` working, after `previous` builders of the task that
/// crashed; queued for an hour before its unit was launched.
fn building(previous: usize) -> Vec<EventKind> {
    let mut kinds = Vec::new();
    for n in 1..=previous + 1 {
        let run = format!("b{n}");
        kinds.extend([
            EventKind::RunStarted(builder(&run)),
            EventKind::TaskStarted {
                branch_repair: false,
                retried: false,
                task: TaskId::new("t"),
                run: RunId::new(&run),
                resume: false,
            },
        ]);
        if n <= previous {
            kinds.push(EventKind::RunEnded {
                run: RunId::new(&run),
                reason: EndReason::Crashed,
                detail: None,
            });
        }
    }
    kinds.push(moved(TaskState::Planned, TaskState::Running));
    kinds
}

/// A builder past its time limit: the limit counts from its launch, not
/// from its start or its wait in the queue, and holds across a restart.
/// Its unit is stopped, the limit is recorded once naming the run, task,
/// kind and limit, and the run ends `time_limit`.
async fn past_its_limit(previous: usize) -> Fixture {
    let f = fixture(CiMode::NoCi, building(previous));
    let run = format!("b{}", previous + 1);
    let queued = std::time::Duration::from_secs(60 * 60);
    f.advance(queued);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunLaunched {
            run: RunId::new(&run),
        }])
    })
    .unwrap();
    f.restart_with(&run).await;
    // The limit's stop cuts off the CLI's stream.
    f.cut_off(&run);
    let limit = caps::time_limit(RunKind::Builder);
    f.advance(limit - std::time::Duration::from_secs(1));
    f.watched().await;
    assert_eq!(f.ended(&run), None, "the queued hour does not count");

    f.advance(std::time::Duration::from_secs(2));
    wait.until("the run to be stopped", || f.ended(&run).is_some())
        .await;
    assert_eq!(f.ended(&run), Some(EndReason::TimeLimit));
    assert_eq!(f.stub.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.events("run.time_limit_reached"),
        vec![EventKind::RunTimeLimitReached {
            run: RunId::new(&run),
            task: Some(TaskId::new("t")),
            kind: RunKind::Builder,
            limit_minutes: 240,
        }]
    );
    f
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_past_its_time_limit_is_stopped_and_retried() {
    let f = past_its_limit(0).await;
    assert_eq!(
        f.state(),
        TaskState::RetryPending {
            effect: Effect::StartUnit,
            attempt: 2
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_past_its_time_limit_on_its_last_attempt_fails_naming_the_limit() {
    let f = past_its_limit(usize::from(caps::ATTEMPTS_PER_TASK) - 1).await;
    assert_eq!(
        f.state(),
        TaskState::Failed {
            reason: "3 attempts; the last ended: time limit reached (240 min)".into()
        }
    );
}

/// An orchestrator past its limit is recorded and ends `time_limit`, a
/// failed orchestrator run.
#[tokio::test(flavor = "multi_thread")]
async fn an_orchestrator_past_its_time_limit_is_stopped() {
    let planner = support::assignment::Assignment::new(RunId::new("o1"))
        .role(RoleId::new("orchestrator"))
        .hire(HireId::new("h-orchestrator"))
        .reason("x".into())
        .build();
    let f = fixture(CiMode::NoCi, vec![EventKind::RunStarted(planner)]);
    f.restart_with("o1").await;
    f.advance(caps::time_limit(RunKind::Orchestrator));
    wait.until("the run to be stopped", || f.ended("o1").is_some())
        .await;
    assert_eq!(f.ended("o1"), Some(EndReason::TimeLimit));
    assert_eq!(
        f.events("run.time_limit_reached"),
        vec![EventKind::RunTimeLimitReached {
            run: RunId::new("o1"),
            task: None,
            kind: RunKind::Orchestrator,
            limit_minutes: 45,
        }]
    );
}
