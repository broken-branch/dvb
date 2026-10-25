use super::*;
use crate::ids::GoalId;

fn ctx() -> Context {
    Context {
        waiting_for: vec![],
        task: TaskId::new("t"),
        head: None,
        goal_approved: false,
        project_paused: false,
        caps: CapsUsed::default(),
        max_runs: 3,
        ci: CiMode::NoCi,
        branch_repair: false,
        retried: false,
        reviewed: false,
        more_review: false,
        open_findings: 0,
        now: Timestamp(0),
    }
}

fn ended(reason: EndReason) -> Command {
    Command::UnitEnded {
        reason,
        detail: None,
        stderr: None,
    }
}

fn run(task: &str) -> RunAssignment {
    use crate::ids::{HireId, RoleId, RunId};
    use crate::model::{Effort, Vendor};
    RunAssignment {
        review_stage: None,
        goal_version: None,
        run: RunId::new("r1"),
        task: Some(TaskId::new(task)),
        role: RoleId::new("backend"),
        hire: HireId::new("h"),
        model: "fake-1".into(),
        vendor: Vendor::Fake,
        effort: Effort::Medium,
        reason: "skeleton".into(),
        reviews: None,
        base: None,
        routine: false,
    }
}

/// Decide, then fold the events into the state, as the engine does.
fn step(state: TaskState, cmd: Command, ctx: &Context) -> (TaskState, Vec<EventKind>) {
    let events = decide(&state, cmd, ctx).unwrap();
    let next = events.iter().fold(state, apply);
    (next, events)
}

#[test]
fn collection_during_a_lost_link_stays_unknown_until_its_real_end() {
    let c = ctx();
    let unknown = TaskState::Unknown {
        last: Box::new(TaskState::Running),
    };
    let (collected, _) = step(unknown, Command::HeadWritten, &c);
    assert_eq!(
        collected,
        TaskState::Unknown {
            last: Box::new(TaskState::Collecting)
        }
    );
    for reason in [
        EndReason::Completed,
        EndReason::Crashed,
        EndReason::ParseError,
    ] {
        let (expected, _) = step(TaskState::Collecting, ended(reason.clone()), &c);
        let (actual, events) = step(collected.clone(), ended(reason), &c);
        assert_eq!(actual, expected);
        assert!(events.iter().any(|event| {
            matches!(event, EventKind::TaskStateChanged { from, to, .. } if from == &collected && to == &expected)
        }));
        assert!(events.iter().any(|event| {
            matches!(event, EventKind::EngineReconciled { state, .. } if state == &expected)
        }));
        assert!(!matches!(
            actual,
            TaskState::Unknown { .. } | TaskState::Running | TaskState::Done
        ));
    }
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Running),
    };
    let (actual, _) = step(
        TaskState::Unknown {
            last: Box::new(stopped.clone()),
        },
        ended(EndReason::Completed),
        &c,
    );
    assert_eq!(actual, stopped);
}

#[test]
fn a_verdict_during_a_lost_link_is_kept_and_settles_on_observation() {
    let unknown = TaskState::Unknown {
        last: Box::new(TaskState::InReview),
    };
    let c = Context {
        goal_approved: true,
        head: Some("p1".into()),
        ..ctx()
    };
    let stale = decide(
        &unknown,
        Command::VerdictRecorded {
            head: "old".into(),
            open: 0,
        },
        &c,
    )
    .unwrap_err();
    assert_eq!(stale.code, Code::HeadMismatch);
    let ended = Observed {
        unit: UnitState::Inactive,
        head: None,
    };
    // Clean: merging is decided, but stays unknown until the unit is seen.
    let (s, _) = step(
        unknown.clone(),
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 0,
        },
        &c,
    );
    assert_eq!(
        s,
        TaskState::Unknown {
            last: Box::new(TaskState::Merging)
        }
    );
    let (s, _) = step(s, Command::Reconcile(ended.clone()), &c);
    assert_eq!(s, TaskState::Merging);
    // Fix-first: the review is done, so its end is not a retried review.
    let (s, events) = step(
        unknown.clone(),
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 2,
        },
        &c,
    );
    assert_eq!((&s, events.len()), (&unknown, 0));
    let fix = Context {
        open_findings: 2,
        ..c
    };
    let (s, _) = step(s, Command::Reconcile(ended), &fix);
    assert_eq!(s, TaskState::InReview);
}

#[test]
fn start_before_the_yes_is_refused() {
    let r = decide(&TaskState::Planned, Command::Start(run("t")), &ctx()).unwrap_err();
    assert_eq!(r.code, crate::RefusalCode::GoalNotApproved);
}

#[test]
fn the_happy_path_ends_done_with_no_ci() {
    let approved = Context {
        goal_approved: true,
        ..ctx()
    };
    let (s, ev) = step(TaskState::Planned, Command::Start(run("t")), &approved);
    assert_eq!(s, TaskState::Preparing);
    let kinds: Vec<&str> = ev.iter().map(EventKind::kind).collect();
    assert_eq!(kinds, ["task.state", "run.started", "task.started"]);

    let (s, _) = step(s, Command::UnitActive, &approved);
    assert_eq!(s, TaskState::Running);
    let (s, _) = step(s, Command::HeadWritten, &approved);
    assert_eq!(s, TaskState::Collecting);
    let (s, _) = step(s, ended(EndReason::Completed), &approved);
    assert_eq!(s, TaskState::Publishing);
    let (s, ev) = step(s, Command::Published, &approved);
    assert_eq!((&s, ev.len()), (&TaskState::Publishing, 0));
    let (s, _) = step(s, Command::PrOpened, &approved);
    assert_eq!(s, TaskState::InReview);

    let published = Context {
        head: Some("p1".into()),
        ..approved
    };
    let stale = decide(
        &s,
        Command::VerdictRecorded {
            head: "old".into(),
            open: 0,
        },
        &published,
    )
    .unwrap_err();
    assert_eq!(stale.code, crate::RefusalCode::HeadMismatch);
    let (s, _) = step(
        s,
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 0,
        },
        &published,
    );
    assert_eq!(s, TaskState::Merging);
    let (s, ev) = step(s, Command::MergeDone, &published);
    assert_eq!(s, TaskState::Done);
    assert_eq!(ev.last().map(EventKind::kind), Some("task.done"));
}

#[test]
fn with_expected_checks_a_clean_verdict_waits_for_ci() {
    let c = Context {
        goal_approved: true,
        head: Some("p1".into()),
        ci: CiMode::Expected(vec!["rust".into()]),
        ..ctx()
    };
    let (s, _) = step(
        TaskState::InReview,
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 0,
        },
        &c,
    );
    assert_eq!(s, TaskState::AwaitingChecks);
}

#[test]
fn a_verdict_with_open_findings_never_merges() {
    let c = Context {
        goal_approved: true,
        head: Some("p1".into()),
        ..ctx()
    };
    let r = decide(
        &TaskState::InReview,
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 1,
        },
        &c,
    )
    .unwrap();
    // Fix-first: no state change here; the engine starts the fix round.
    assert!(r.is_empty());
}

#[test]
fn a_run_that_ends_without_collecting_does_not_publish() {
    let (s, ev) = step(TaskState::Running, ended(EndReason::Completed), &ctx());
    assert!(matches!(s, TaskState::Failed { .. }));
    assert_eq!(ev.last().map(EventKind::kind), Some("task.failed"));
}

#[test]
fn a_run_that_ends_badly_fails_the_task() {
    let (s, ev) = step(TaskState::Running, ended(EndReason::ParseError), &ctx());
    assert!(matches!(s, TaskState::Failed { .. }));
    assert_eq!(ev.last().map(EventKind::kind), Some("task.failed"));
}

#[test]
fn only_task_state_moves_the_state() {
    let s = apply(
        TaskState::Planned,
        &EventKind::GoalApproved {
            goal: GoalId::new("g"),
        },
    );
    assert_eq!(s, TaskState::Planned);
    let s = apply(
        s,
        &EventKind::TaskStateChanged {
            task: TaskId::new("t"),
            from: TaskState::Planned,
            to: TaskState::Stopped {
                was: Box::new(TaskState::Planned),
            },
        },
    );
    assert_eq!(
        s,
        TaskState::Stopped {
            was: Box::new(TaskState::Planned)
        }
    );
}

#[test]
fn state_wire_shape() {
    let s = TaskState::Stopped {
        was: Box::new(TaskState::RetryPending {
            effect: Effect::Push,
            attempt: 2,
        }),
    };
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        serde_json::json!({"state": "stopped", "was": {"state": "retry_pending", "effect": "push", "attempt": 2}})
    );
}

// --- State transitions, row by row ---------------------------------------------------------

use crate::event::ChecksStatus;
use crate::launcher::UnitState;
use crate::refusal::{Cap, RefusalCode as Code};

fn approved() -> Context {
    Context {
        goal_approved: true,
        head: Some("p1".into()),
        ..ctx()
    }
}

/// One of every state, nested ones included.
fn every_state() -> Vec<TaskState> {
    use TaskState as S;
    vec![
        S::Planned,
        S::Preparing,
        S::Running,
        S::Collecting,
        S::Publishing,
        S::InReview,
        S::AwaitingChecks,
        S::Merging,
        S::RetryPending {
            effect: Effect::Push,
            attempt: 1,
        },
        S::Stopped {
            was: Box::new(S::Running),
        },
        S::Unknown {
            last: Box::new(S::Running),
        },
        S::Done,
        S::Cancelled,
        S::Failed { reason: "x".into() },
    ]
}

fn terminal(s: &TaskState) -> bool {
    matches!(
        s,
        TaskState::Done | TaskState::Cancelled | TaskState::Failed { .. }
    )
}

fn code(r: Result<Vec<EventKind>, Refusal>) -> Code {
    r.unwrap_err().code
}

fn reviewer(head: &str) -> RunAssignment {
    RunAssignment {
        review_stage: None,
        goal_version: None,
        run: crate::ids::RunId::new("rv"),
        reviews: Some(head.into()),
        base: Some(head.into()),
        ..run("t")
    }
}

/// A builder run on the publication head `p1`.
fn built() -> RunAssignment {
    RunAssignment {
        base: Some("p1".into()),
        ..run("t")
    }
}

#[test]
fn stop_has_an_outcome_in_every_state() {
    for s in every_state() {
        let r = decide(&s, Command::Stop(None), &approved());
        if terminal(&s) {
            assert_eq!(code(r), Code::AlreadyTerminal);
            continue;
        }
        match &s {
            TaskState::Stopped { .. } => assert_eq!(code(r), Code::AlreadyStopped),
            TaskState::Unknown { last } => {
                let (next, _) = step(s.clone(), Command::Stop(None), &approved());
                assert_eq!(next, TaskState::Stopped { was: last.clone() });
            }
            s @ (TaskState::Planned
            | TaskState::Preparing
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::InReview
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. }) => {
                let ev = r.unwrap();
                let next = ev.iter().fold(s.clone(), apply);
                assert_eq!(
                    next,
                    TaskState::Stopped {
                        was: Box::new(s.clone())
                    }
                );
                assert_eq!(ev.last().map(EventKind::kind), Some("task.stopped"));
            }
        }
    }
}

#[test]
fn resume_carries_on_from_where_it_stopped() {
    use TaskState as S;
    let c = approved();
    let stopped = |s: S| S::Stopped { was: Box::new(s) };
    // No run needed: back to the state it stopped in.
    for was in [S::Planned, S::Publishing, S::AwaitingChecks, S::Merging] {
        let (s, ev) = step(stopped(was.clone()), Command::Resume(None), &c);
        assert_eq!(s, was);
        assert_eq!(ev.last().map(EventKind::kind), Some("task.resumed"));
    }
    // A retry happens on resume.
    let (s, _) = step(
        stopped(S::RetryPending {
            effect: Effect::Merge,
            attempt: 2,
        }),
        Command::Resume(None),
        &c,
    );
    assert_eq!(s, S::Merging);
    // A stopped run resumes as a new run in its preserved workspace.
    for was in [S::Preparing, S::Running, S::Collecting] {
        let (s, ev) = step(stopped(was), Command::Resume(Some(built())), &c);
        assert_eq!(s, S::Preparing);
        assert!(ev.contains(&EventKind::TaskStarted {
            branch_repair: false,
            retried: false,
            task: TaskId::new("t"),
            run: crate::ids::RunId::new("r1"),
            resume: true,
        }));
    }
    // Review starts again on the same head.
    let (s, ev) = step(
        stopped(S::InReview),
        Command::Resume(Some(reviewer("p1"))),
        &c,
    );
    assert_eq!(s, S::InReview);
    assert!(matches!(ev[1], EventKind::RunStarted(_)));
    assert_eq!(
        code(decide(
            &stopped(S::InReview),
            Command::Resume(Some(reviewer("old"))),
            &c
        )),
        Code::HeadMismatch
    );
    // The wrong shape of resume is refused, not guessed.
    assert_eq!(
        code(decide(&stopped(S::Running), Command::Resume(None), &c)),
        Code::ValueChanged
    );
    assert_eq!(
        code(decide(
            &stopped(S::Planned),
            Command::Resume(Some(built())),
            &c
        )),
        Code::ValueChanged
    );
    // A resume that starts a run waits for the project to be unpaused.
    let paused = Context {
        project_paused: true,
        ..approved()
    };
    assert_eq!(
        code(decide(
            &stopped(S::Running),
            Command::Resume(Some(built())),
            &paused
        )),
        Code::ProjectPaused
    );
    // Not stopped, or ended: nothing to resume.
    assert_eq!(
        code(decide(&S::Running, Command::Resume(None), &c)),
        Code::ValueChanged
    );
    assert_eq!(
        code(decide(&S::Done, Command::Resume(None), &c)),
        Code::AlreadyTerminal
    );
}

#[test]
fn no_observation_moves_a_stopped_task() {
    let c = approved();
    let observations = [
        Command::UnitActive,
        Command::HeadWritten,
        ended(EndReason::Crashed),
        Command::Published,
        Command::PrOpened,
        Command::ChecksObserved {
            head: "p1".into(),
            status: ChecksStatus::Success,
        },
        Command::MergeDone,
        Command::MergeRefused("moved".into()),
        Command::EffectFailed {
            rate_limit: None,
            effect: Effect::Push,
            transient: true,
            detail: "x".into(),
        },
        Command::RetryDue,
    ];
    for was in every_state().into_iter().filter(|s| {
        !terminal(s) && !matches!(s, TaskState::Stopped { .. } | TaskState::Unknown { .. })
    }) {
        let s = TaskState::Stopped { was: Box::new(was) };
        for cmd in observations.clone() {
            assert_eq!(decide(&s, cmd.clone(), &c), Ok(vec![]), "{cmd:?} in {s:?}");
        }
        assert_eq!(
            code(decide(&s, Command::Start(built()), &c)),
            Code::AlreadyStopped
        );
        assert_eq!(
            code(decide(
                &s,
                Command::VerdictRecorded {
                    head: "p1".into(),
                    open: 0
                },
                &c
            )),
            Code::AlreadyStopped
        );
    }
}

#[test]
fn only_a_failed_task_is_retried_as_a_new_run() {
    use TaskState as S;
    let failed = S::Failed {
        reason: "3 attempts; the last ended: crashed".into(),
    };
    // Its attempts are used up; the owner's retry is still one more run.
    let c = Context {
        caps: CapsUsed {
            attempts: caps::ATTEMPTS_PER_TASK,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, ev) = step(failed.clone(), Command::Retry(Some(built())), &c);
    assert_eq!(s, S::Preparing);
    assert!(ev.contains(&EventKind::TaskStarted {
        branch_repair: false,
        retried: true,
        task: TaskId::new("t"),
        run: crate::ids::RunId::new("r1"),
        resume: true,
    }));
    assert_eq!(ev.last().map(EventKind::kind), Some("task.resumed"));
    // Published: the head's reviewer again.
    let (s, _) = step(failed.clone(), Command::Retry(Some(reviewer("p1"))), &c);
    assert_eq!(s, S::InReview);
    // Admitted like any run: not while paused, nor without a run.
    let paused = Context {
        project_paused: true,
        ..c.clone()
    };
    assert_eq!(
        code(decide(&failed, Command::Retry(Some(built())), &paused)),
        Code::ProjectPaused
    );
    assert_eq!(
        code(decide(&failed, Command::Retry(None), &ctx())),
        Code::ValueChanged
    );
    // Pushed but no PR: back to publishing, which opens it on that branch.
    let (s, ev) = step(failed, Command::Retry(None), &c);
    assert_eq!(s, S::Publishing);
    assert_eq!(ev.last().map(EventKind::kind), Some("task.resumed"));
    // A stopped task with a head takes a builder on it; any other state is
    // refused with a code.
    for s in every_state() {
        let r = decide(&s, Command::Retry(Some(built())), &c);
        match s {
            S::Failed { .. } | S::Stopped { .. } => assert!(r.is_ok()),
            S::Done | S::Cancelled => assert_eq!(code(r), Code::AlreadyTerminal),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Unknown { .. } => assert_eq!(code(r), Code::ValueChanged),
        }
    }
}

/// A stopped task with a head, retried, gets one builder round on that
/// head, even past the review-round cap and with findings open; as a
/// reviewer or without a run it is still refused as stopped.
#[test]
fn a_stopped_task_with_a_head_is_retried_as_a_builder_on_it() {
    use TaskState as S;
    let capped = Context {
        caps: CapsUsed {
            review_rounds: caps::REVIEW_ROUNDS_PER_PR,
            ..CapsUsed::default()
        },
        open_findings: 2,
        ..approved()
    };
    for was in [S::InReview, S::AwaitingChecks, S::Merging, S::Running] {
        let stopped = S::Stopped { was: Box::new(was) };
        let (s, ev) = step(stopped, Command::Retry(Some(built())), &capped);
        assert_eq!(s, S::Preparing);
        assert!(ev.contains(&EventKind::TaskStarted {
            branch_repair: false,
            retried: true,
            task: TaskId::new("t"),
            run: crate::ids::RunId::new("r1"),
            resume: true,
        }));
        assert_eq!(ev.last().map(EventKind::kind), Some("task.resumed"));
    }
    let stopped = S::Stopped {
        was: Box::new(S::InReview),
    };
    for (cmd, c) in [
        (Command::Retry(Some(reviewer("p1"))), capped.clone()),
        (Command::Retry(None), capped.clone()),
    ] {
        assert_eq!(code(decide(&stopped, cmd, &c)), Code::AlreadyStopped);
    }
    // On another base it is refused: a builder after publication builds on the head.
    let elsewhere = RunAssignment {
        base: Some("main".into()),
        ..built()
    };
    assert_eq!(
        code(decide(&stopped, Command::Retry(Some(elsewhere)), &capped)),
        Code::ValueChanged
    );
}

/// A stopped task with no head, retried, gets one builder round on its
/// kept work; one that never started has none and is still refused as stopped.
#[test]
fn a_stopped_task_without_a_head_is_retried_as_a_builder_on_its_kept_work() {
    use TaskState as S;
    let kept = RunAssignment {
        base: Some("main".into()),
        ..run("t")
    };
    for was in [S::Publishing, S::Running, S::Collecting] {
        let stopped = S::Stopped { was: Box::new(was) };
        let (s, ev) = step(stopped, Command::Retry(Some(kept.clone())), &ctx_no_head());
        assert_eq!(s, S::Preparing);
        assert!(ev.contains(&EventKind::TaskStarted {
            branch_repair: false,
            retried: true,
            task: TaskId::new("t"),
            run: crate::ids::RunId::new("r1"),
            resume: true,
        }));
        assert!(ev.contains(&EventKind::TaskResumed {
            task: TaskId::new("t"),
            reason: Some("retried on its kept work".into()),
        }));
    }
    let planned = S::Stopped {
        was: Box::new(S::Planned),
    };
    let publishing = S::Stopped {
        was: Box::new(S::Publishing),
    };
    for (s, cmd) in [
        (&planned, Command::Retry(Some(kept))),
        (&publishing, Command::Retry(None)),
    ] {
        assert_eq!(code(decide(s, cmd, &ctx_no_head())), Code::AlreadyStopped);
    }
}

fn ctx_no_head() -> Context {
    Context {
        goal_approved: true,
        ..ctx()
    }
}

/// Past the review-round cap, the owner's retry still starts the
/// builder on the head (one more round); another start or a resume does not,
/// unless it continues that retried builder.
#[test]
fn the_owners_retry_grants_a_builder_one_round_past_the_cap() {
    use TaskState as S;
    let failed = S::Failed {
        reason: "3 review rounds used up; CI failed".into(),
    };
    let capped = Context {
        caps: CapsUsed {
            review_rounds: caps::REVIEW_ROUNDS_PER_PR,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, _) = step(failed, Command::Retry(Some(built())), &capped);
    assert_eq!(s, S::Preparing);
    let stopped = S::Stopped {
        was: Box::new(S::Running),
    };
    assert!(matches!(
        code(decide(&stopped, Command::Resume(Some(built())), &capped)),
        Code::CapReached {
            which: Cap::ReviewRoundsPerPr
        }
    ));
    let retried = Context {
        retried: true,
        ..capped
    };
    let (s, _) = step(stopped, Command::Resume(Some(built())), &retried);
    assert_eq!(s, S::Preparing);
}

/// A head every review approved, retried into its merge, goes back to its
/// checks (or its merge without CI) with no run.
#[test]
fn a_merge_retry_sends_an_approved_head_to_its_checks_and_merge() {
    use TaskState as S;
    let failed = S::Failed {
        reason: "the branch repair limit was reached".into(),
    };
    let reviewed = Context {
        reviewed: true,
        ..approved()
    };
    let (s, ev) = step(failed.clone(), Command::RetryMerge, &reviewed);
    assert_eq!(s, S::Merging);
    assert_eq!(ev.last().map(EventKind::kind), Some("task.resumed"));
    let ci = Context {
        ci: CiMode::Expected(vec!["rust".into()]),
        ..reviewed.clone()
    };
    let (s, _) = step(failed.clone(), Command::RetryMerge, &ci);
    assert_eq!(s, S::AwaitingChecks);
    // An unapproved head is not merged without a run.
    assert_eq!(
        code(decide(&failed, Command::RetryMerge, &approved())),
        Code::ValueChanged
    );
    for s in every_state() {
        let r = decide(&s, Command::RetryMerge, &reviewed);
        match s {
            S::Failed { .. } => assert!(r.is_ok()),
            S::Done | S::Cancelled => assert_eq!(code(r), Code::AlreadyTerminal),
            S::Stopped { .. } => assert_eq!(code(r), Code::AlreadyStopped),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Unknown { .. } => assert_eq!(code(r), Code::ValueChanged),
        }
    }
}

#[test]
fn an_ended_task_never_moves() {
    let c = approved();
    for s in every_state().into_iter().filter(terminal) {
        for cmd in [
            Command::Start(built()),
            ended(EndReason::Completed),
            Command::MergeDone,
            Command::Stop(None),
            Command::Resume(None),
            Command::Cancel,
            Command::RetryDue,
            Command::Restarted,
        ] {
            match decide(&s, cmd, &c) {
                Ok(ev) => assert!(ev.is_empty()),
                Err(r) => assert_eq!(r.code, Code::AlreadyTerminal),
            }
        }
    }
}

#[test]
fn a_crashed_run_retries_within_the_attempts_cap() {
    let c = Context {
        now: Timestamp(1_000),
        caps: CapsUsed {
            attempts: 1,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, ev) = step(TaskState::Running, ended(EndReason::Crashed), &c);
    assert_eq!(
        s,
        TaskState::RetryPending {
            effect: Effect::StartUnit,
            attempt: 2
        }
    );
    assert!(ev.contains(&EventKind::EffectRetry {
        rate_limit: None,
        task: TaskId::new("t"),
        effect: Effect::StartUnit,
        attempt: 2,
        at: Timestamp(1_000 + backoff_millis(2, 2_000, 300_000)),
    }));
    // A due run retry is a Start with the new run, in the preserved workspace.
    assert_eq!(code(decide(&s, Command::RetryDue, &c)), Code::ValueChanged);
    let (s, ev) = step(s, Command::Start(built()), &c);
    assert_eq!(s, TaskState::Preparing);
    assert!(matches!(ev[2], EventKind::TaskStarted { resume: true, .. }));
    // At the cap the task fails with the reason.
    let spent = Context {
        caps: CapsUsed {
            attempts: caps::ATTEMPTS_PER_TASK,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, _) = step(TaskState::Running, ended(EndReason::Oom), &spent);
    assert!(matches!(s, TaskState::Failed { ref reason } if reason.contains("out of memory")));
    let retry = TaskState::RetryPending {
        effect: Effect::StartUnit,
        attempt: 3,
    };
    let (s, _) = step(retry, Command::Start(built()), &spent);
    assert!(matches!(s, TaskState::Failed { .. }));
}

/// A run stopped at its time limit takes a crash's path: another run while
/// attempts remain, else the task fails naming the limit of the run's kind.
#[test]
fn a_run_past_its_time_limit_retries_then_fails_naming_the_limit() {
    let c = Context {
        caps: CapsUsed {
            attempts: 1,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let retry = TaskState::RetryPending {
        effect: Effect::StartUnit,
        attempt: 2,
    };
    let ended = ended(EndReason::TimeLimit);
    assert_eq!(step(TaskState::Running, ended.clone(), &c).0, retry);
    assert_eq!(step(TaskState::InReview, ended.clone(), &c).0, retry);
    let spent = Context {
        caps: CapsUsed {
            attempts: caps::ATTEMPTS_PER_TASK,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let failed = |reason: &str| TaskState::Failed {
        reason: reason.into(),
    };
    assert_eq!(
        step(TaskState::Running, ended.clone(), &spent).0,
        failed("3 attempts; the last ended: time limit reached (240 min)")
    );
    assert_eq!(
        step(TaskState::InReview, ended, &spent).0,
        failed("3 attempts; the last ended: time limit reached (90 min)")
    );
}

#[test]
fn a_run_stopped_from_outside_stops_the_task() {
    let (s, _) = step(TaskState::Running, ended(EndReason::Stopped), &approved());
    assert_eq!(
        s,
        TaskState::Stopped {
            was: Box::new(TaskState::Running)
        }
    );
}

#[test]
fn a_reviewer_that_ends_without_a_verdict_runs_again() {
    let (s, _) = step(
        TaskState::InReview,
        ended(EndReason::Completed),
        &approved(),
    );
    assert!(matches!(
        s,
        TaskState::RetryPending {
            effect: Effect::StartUnit,
            ..
        }
    ));
    let (s, ev) = step(s, Command::Start(reviewer("p1")), &approved());
    assert_eq!(s, TaskState::InReview);
    assert_eq!(ev.len(), 2, "no task.started for a reviewer: {ev:?}");
}

#[test]
fn transient_effect_failures_retry_then_fail() {
    let c = approved();
    let failed_push = |transient| Command::EffectFailed {
        rate_limit: None,
        effect: Effect::Push,
        transient,
        detail: "502".into(),
    };
    let (s, _) = step(TaskState::Publishing, failed_push(true), &c);
    assert_eq!(
        s,
        TaskState::RetryPending {
            effect: Effect::Push,
            attempt: 1
        }
    );
    let (s, _) = step(s, Command::RetryDue, &c);
    assert_eq!(s, TaskState::Publishing);
    let spent = Context {
        caps: CapsUsed {
            retries: caps::RETRIES_PER_EFFECT,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, _) = step(TaskState::Publishing, failed_push(true), &spent);
    assert!(matches!(s, TaskState::Failed { .. }));
    let (s, _) = step(TaskState::Publishing, failed_push(false), &c);
    assert!(matches!(s, TaskState::Failed { ref reason } if reason.contains("502")));
    // An effect's failure outside its state is stale.
    assert_eq!(
        decide(&TaskState::InReview, failed_push(true), &c),
        Ok(vec![])
    );
}

#[test]
fn a_fix_round_builds_on_the_head_and_stops_at_the_cap() {
    let c = approved();
    let fix = built();
    let (s, ev) = step(TaskState::InReview, Command::Start(fix.clone()), &c);
    assert_eq!(s, TaskState::Preparing);
    assert!(matches!(
        ev[2],
        EventKind::TaskStarted { resume: false, .. }
    ));
    // The orchestrator's start on the default branch is not a fix round.
    let other = RunAssignment {
        base: Some("main".into()),
        ..run("t")
    };
    assert_eq!(
        code(decide(&TaskState::InReview, Command::Start(other), &c)),
        Code::ValueChanged
    );
    let capped = Context {
        caps: CapsUsed {
            review_rounds: caps::REVIEW_ROUNDS_PER_PR,
            ..CapsUsed::default()
        },
        ..approved()
    };
    // At the cap the task stops, its PR and findings kept.
    let (s, ev) = step(
        TaskState::InReview,
        Command::VerdictRecorded {
            head: "p1".into(),
            open: 2,
        },
        &capped,
    );
    assert_eq!(
        s,
        TaskState::Stopped {
            was: Box::new(TaskState::InReview)
        }
    );
    assert_eq!(ev.last().map(EventKind::kind), Some("task.stopped"));
    assert!(matches!(
        code(decide(&TaskState::InReview, Command::Start(fix), &capped)),
        Code::CapReached {
            which: Cap::ReviewRoundsPerPr
        }
    ));
}

#[test]
fn only_success_on_the_head_merges() {
    let c = Context {
        ci: CiMode::Expected(vec!["rust".into()]),
        ..approved()
    };
    let observed = |head: &str, status| Command::ChecksObserved {
        head: head.into(),
        status,
    };
    for status in [
        ChecksStatus::Missing,
        ChecksStatus::Pending,
        ChecksStatus::Unreadable,
    ] {
        assert_eq!(
            decide(&TaskState::AwaitingChecks, observed("p1", status), &c),
            Ok(vec![])
        );
    }
    assert_eq!(
        code(decide(
            &TaskState::AwaitingChecks,
            observed("old", ChecksStatus::Success),
            &c
        )),
        Code::HeadMismatch
    );
    // A failure keeps it waiting (CI may be run again); none of it merges.
    assert_eq!(
        decide(
            &TaskState::AwaitingChecks,
            observed("p1", ChecksStatus::Failed),
            &c
        ),
        Ok(vec![])
    );
    // Seen again just before the merge, anything but success sends it back.
    let (s, _) = step(
        TaskState::Merging,
        observed("p1", ChecksStatus::Pending),
        &c,
    );
    assert_eq!(s, TaskState::AwaitingChecks);
    // An empty expected set never merges.
    let empty = Context {
        ci: CiMode::Expected(vec![]),
        ..c.clone()
    };
    assert_eq!(
        decide(
            &TaskState::AwaitingChecks,
            observed("p1", ChecksStatus::Success),
            &empty
        ),
        Ok(vec![])
    );
    let (s, _) = step(
        TaskState::AwaitingChecks,
        observed("p1", ChecksStatus::Success),
        &c,
    );
    assert_eq!(s, TaskState::Merging);
}

#[test]
fn a_refused_merge_reenters_review_a_bounded_number_of_times() {
    for from in [TaskState::Merging, TaskState::AwaitingChecks] {
        let (s, ev) = step(
            from,
            Command::MergeRefused("head moved".into()),
            &approved(),
        );
        assert_eq!(s, TaskState::InReview);
        assert_eq!(ev[1].kind(), "merge.refused");
    }
    let spent = Context {
        caps: CapsUsed {
            retries: caps::RETRIES_PER_EFFECT,
            ..CapsUsed::default()
        },
        ..approved()
    };
    let (s, _) = step(
        TaskState::Merging,
        Command::MergeRefused("head moved".into()),
        &spent,
    );
    assert!(matches!(s, TaskState::Failed { .. }));
}

#[test]
fn change_course_cancels_only_unstarted_tasks() {
    let c = approved();
    let (s, ev) = step(TaskState::Planned, Command::Cancel, &c);
    assert_eq!(s, TaskState::Cancelled);
    assert_eq!(ev.last().map(EventKind::kind), Some("task.cancelled"));
    let (s, _) = step(
        TaskState::Stopped {
            was: Box::new(TaskState::Planned),
        },
        Command::Cancel,
        &c,
    );
    assert_eq!(s, TaskState::Cancelled);
    for s in [
        TaskState::Running,
        TaskState::Stopped {
            was: Box::new(TaskState::Running),
        },
    ] {
        assert_eq!(code(decide(&s, Command::Cancel, &c)), Code::ValueChanged);
    }
}

#[test]
fn a_paused_project_starts_no_run() {
    let paused = Context {
        project_paused: true,
        ..approved()
    };
    assert_eq!(
        code(decide(
            &TaskState::Planned,
            Command::Start(built()),
            &paused
        )),
        Code::ProjectPaused
    );
}

#[test]
fn after_a_restart_a_task_in_flight_is_unknown_until_reconciled() {
    use TaskState as S;
    let c = approved();
    let seen = |unit, head: Option<&str>| Observed {
        unit,
        head: head.map(str::to_owned),
    };
    for s in [
        S::Preparing,
        S::Running,
        S::Collecting,
        S::Publishing,
        S::InReview,
        S::Merging,
    ] {
        let (u, _) = step(s.clone(), Command::Restarted, &c);
        assert_eq!(u, S::Unknown { last: Box::new(s) });
    }
    for s in [
        S::Planned,
        S::AwaitingChecks,
        S::Done,
        S::Stopped {
            was: Box::new(S::Planned),
        },
    ] {
        assert_eq!(decide(&s, Command::Restarted, &c), Ok(vec![]));
    }
    // A stopped run's unit may have outlived the stop: checked, then stopped
    // again, never resumed by the check.
    let stopped = S::Stopped {
        was: Box::new(S::Running),
    };
    let (u, _) = step(stopped.clone(), Command::Restarted, &c);
    assert_eq!(
        u,
        S::Unknown {
            last: Box::new(stopped.clone())
        }
    );
    assert_eq!(
        code(decide(&u, Command::Stop(None), &c)),
        Code::AlreadyStopped
    );
    assert_eq!(
        code(decide(&u, Command::Resume(None), &c)),
        Code::ValueChanged
    );
    let (back, _) = step(u, Command::Reconcile(seen(UnitState::Active, None)), &c);
    assert_eq!(back, stopped);
    let unknown = |s: S| S::Unknown { last: Box::new(s) };
    let reconcile = |s: S, o: Observed| {
        let (next, ev) = step(unknown(s), Command::Reconcile(o), &c);
        assert_eq!(
            ev.last(),
            Some(&EventKind::EngineReconciled {
                task: TaskId::new("t"),
                state: next.clone()
            })
        );
        next
    };
    assert_eq!(
        reconcile(S::Running, seen(UnitState::Active, None)),
        S::Running
    );
    assert_eq!(
        reconcile(S::Running, seen(UnitState::Inactive, Some("h"))),
        S::Publishing
    );
    assert!(matches!(
        reconcile(S::Running, seen(UnitState::Failed, None)),
        S::RetryPending {
            effect: Effect::StartUnit,
            ..
        }
    ));
    // A head that came during the check: collected, the unit still exiting.
    assert_eq!(
        reconcile(S::Running, seen(UnitState::Active, Some("h"))),
        S::Collecting
    );
    // Reconciliation re-enters the merge step, which reads the forge.
    assert_eq!(
        reconcile(S::Merging, seen(UnitState::Inactive, None)),
        S::Merging
    );
    assert!(matches!(
        reconcile(S::InReview, seen(UnitState::Inactive, None)),
        S::RetryPending {
            effect: Effect::StartUnit,
            ..
        }
    ));
    assert_eq!(
        reconcile(S::InReview, seen(UnitState::Active, None)),
        S::InReview
    );
    // Unknown refuses what needs the truth, and can still be stopped.
    let u = unknown(S::Running);
    assert_eq!(
        code(decide(&u, Command::Start(built()), &c)),
        Code::ValueChanged
    );
    // UnitEnded is a confirmed observation, so its actual end can settle the
    // task directly; a successful exit without collection fails as usual.
    let (ended, _) = step(u, ended(EndReason::Completed), &c);
    assert!(matches!(ended, S::Failed { .. }));
}

#[test]
fn backoff_keeps_each_policy_bounds_even_for_saturated_attempts() {
    for (initial, maximum) in [
        (1_000, 1_000),
        (5_000, 5_000),
        (30_000, 600_000),
        (10_000, 10_000),
        (60_000, 3_600_000),
        (2_000, 300_000),
    ] {
        assert_eq!(backoff_millis(0, initial, maximum), initial);
        assert_eq!(backoff_millis(1, initial, maximum), initial);
        assert_eq!(
            backoff_millis(2, initial, maximum),
            (2 * initial).min(maximum)
        );
        assert_eq!(backoff_millis(u8::MAX, initial, maximum), maximum);
    }
}

#[test]
fn backoff_doubles_and_is_bounded() {
    assert_eq!(backoff_millis(1, 2_000, 300_000), 2_000);
    assert_eq!(backoff_millis(2, 2_000, 300_000), 4_000);
    assert_eq!(backoff_millis(200, 2_000, 300_000), 300_000);
    assert_eq!(backoff_millis(0, 2_000, 300_000), 2_000);
}

#[test]
fn a_task_stopped_by_the_review_cap_cannot_resume_its_review() {
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::InReview),
    };
    let capped = |open_findings| Context {
        caps: CapsUsed {
            review_rounds: caps::REVIEW_ROUNDS_PER_PR,
            ..CapsUsed::default()
        },
        open_findings,
        ..approved()
    };
    // The cap's own stop (an open verdict on the head): stop or change course.
    assert_eq!(
        code(decide(
            &stopped,
            Command::Resume(Some(reviewer("p1"))),
            &capped(2)
        )),
        Code::ValueChanged
    );
    // A reviewer stopped before its verdict on the final head resumes as usual.
    let (s, _) = step(stopped, Command::Resume(Some(reviewer("p1"))), &capped(0));
    assert_eq!(s, TaskState::InReview);
}

#[test]
fn a_model_mismatch_fails_the_task_with_catalogue_wording() {
    let ended = decide(&TaskState::Running, ended(EndReason::Mismatch), &approved()).unwrap();
    assert!(ended.iter().any(|k| matches!(
        k,
        EventKind::TaskFailed { reason: Some(r), .. } if r.contains(crate::wording::text("end.mismatch"))
    )));
}

#[test]
fn no_changes_is_terminal_without_an_effect_retry_and_stale_outcomes_do_nothing() {
    for state in every_state() {
        let events = decide(&state, Command::NoChanges, &ctx()).unwrap();
        if state == TaskState::Publishing {
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[0], EventKind::TaskStateChanged { to: TaskState::Failed { reason }, .. } if reason == "no changes: the run changed nothing")
            );
            assert!(
                matches!(&events[1], EventKind::TaskFailed { reason, .. } if reason.as_deref() == Some("no changes: the run changed nothing"))
            );
        } else {
            assert!(events.is_empty(), "{state:?}: {events:?}");
        }
    }
}

#[test]
fn explicit_no_ci_releases_a_waiting_task_but_still_requires_the_current_head() {
    let c = approved();
    assert_eq!(c.ci, CiMode::NoCi);
    let (state, _) = step(
        TaskState::AwaitingChecks,
        Command::ChecksObserved {
            head: c.head.clone().unwrap(),
            status: ChecksStatus::Missing,
        },
        &c,
    );
    assert_eq!(state, TaskState::Merging);
    assert_eq!(
        code(decide(
            &TaskState::AwaitingChecks,
            Command::ChecksObserved {
                head: "old".into(),
                status: ChecksStatus::Success,
            },
            &c
        )),
        Code::HeadMismatch
    );
}

#[test]
fn a_clean_intermediate_review_stays_in_review_even_without_ci_or_during_a_lost_link() {
    for ci in [CiMode::NoCi, CiMode::Expected(vec!["rust".into()])] {
        let mut c = ctx();
        c.head = Some("head".into());
        c.ci = ci;
        c.more_review = true;
        for state in [
            TaskState::InReview,
            TaskState::Unknown {
                last: Box::new(TaskState::InReview),
            },
        ] {
            let cmd = Command::VerdictRecorded {
                head: "head".into(),
                open: 0,
            };
            let (after, events) = step(state.clone(), cmd.clone(), &c);
            assert_eq!(after, state);
            assert!(events.is_empty());
            c.more_review = false;
            let (after, _) = step(state.clone(), cmd, &c);
            let ready = match c.ci {
                CiMode::NoCi => TaskState::Merging,
                CiMode::Expected(_) => TaskState::AwaitingChecks,
            };
            assert_eq!(
                after,
                if matches!(state, TaskState::Unknown { .. }) {
                    TaskState::Unknown {
                        last: Box::new(ready),
                    }
                } else {
                    ready
                }
            );
            c.more_review = true;
        }
    }
}

#[test]
fn admission_uses_the_current_limit_for_builders_reviewers_resume_and_retry() {
    for limit in [1, 6, u64::MAX] {
        let mut c = approved();
        c.max_runs = limit;
        for (state, cmd) in [
            (TaskState::Planned, Command::Start(built())),
            (
                TaskState::Stopped {
                    was: Box::new(TaskState::InReview),
                },
                Command::Resume(Some(reviewer("p1"))),
            ),
            (
                TaskState::RetryPending {
                    effect: Effect::StartUnit,
                    attempt: 1,
                },
                Command::Start(reviewer("p1")),
            ),
            (
                TaskState::Stopped {
                    was: Box::new(TaskState::Running),
                },
                Command::Resume(Some(built())),
            ),
            (
                TaskState::Failed {
                    reason: "interrupted".into(),
                },
                Command::Retry(Some(built())),
            ),
        ] {
            c.caps.concurrent_runs = limit - 1;
            assert!(
                decide(&state, cmd.clone(), &c)
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, EventKind::RunStarted(_)))
            );
            c.caps.concurrent_runs = limit;
            let refusal = decide(&state, cmd, &c).unwrap_err();
            assert_eq!(
                refusal.code,
                Code::CapReached {
                    which: Cap::ConcurrentRuns
                }
            );
            assert!(
                refusal.detail.contains(&format!("maximum is {limit}")),
                "{refusal:?}"
            );
        }
    }
    let mut c = approved();
    c.max_runs = 1;
    c.caps.concurrent_runs = 6;
    assert!(
        decide(&TaskState::Running, Command::UnitActive, &c)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn all_worker_admissions_wait_for_the_prerequisite_merge() {
    let context = Context {
        goal_approved: true,
        waiting_for: vec![TaskId::new("api")],
        ..ctx()
    };
    for (state, command) in [
        (TaskState::Planned, Command::Start(run("t"))),
        (
            TaskState::Failed {
                reason: "failed".into(),
            },
            Command::Retry(Some(run("t"))),
        ),
        (
            TaskState::Stopped {
                was: Box::new(TaskState::Running),
            },
            Command::Resume(Some(run("t"))),
        ),
        (
            TaskState::Stopped {
                was: Box::new(TaskState::Publishing),
            },
            Command::Resume(Some(run("t"))),
        ),
        (
            TaskState::RetryPending {
                effect: Effect::StartUnit,
                attempt: 1,
            },
            Command::Start(run("t")),
        ),
    ] {
        assert_eq!(
            decide(&state, command, &context).unwrap_err().code,
            RefusalCode::DependencyNotMerged {
                task: TaskId::new("api")
            }
        );
    }
}

#[test]
fn a_stopped_completion_check_can_resume_as_a_builder_without_changing_its_old_publication() {
    let context = Context {
        goal_approved: true,
        head: Some("published".into()),
        ..ctx()
    };
    for was in [
        TaskState::Publishing,
        TaskState::AwaitingChecks,
        TaskState::Merging,
    ] {
        let stopped = TaskState::Stopped { was: Box::new(was) };
        let mut builder = run("t");
        builder.base.clone_from(&context.head);
        let (state, events) = step(stopped, Command::Resume(Some(builder)), &context);
        assert_eq!(state, TaskState::Preparing);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EventKind::RunStarted(a) if a.reviews.is_none()))
        );
    }
}
