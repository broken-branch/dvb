//! Restart with a launcher that cannot be read at first (systemd busy, D-Bus
//! hiccup): every live run, a builder's and an orchestrator's, is asked about
//! again until it has a watcher or an end, so none is left holding a slot.

mod support;
use dvb_core::launcher::{Launcher, UnitState};
use dvb_core::{
    Actor, EventKind, GoalId, ProjectId, RoleId, RunAssignment, RunId, TaskId, TaskState,
};
use dvb_engine::{AppState, reconcile};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use support::wait::SLOW as wait;

use support::launcher::FakeLauncher;

async fn task_status(e: &AppState) -> TaskState {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let response = dvb_engine::api::router(e.clone())
        .oneshot(
            Request::builder()
                .uri("/tasks/t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    serde_json::from_slice::<dvb_core::api::TaskView>(&body)
        .unwrap()
        .state
}

async fn lost_link(end: UnitState) {
    let dir = tempfile::tempdir().unwrap();
    let flaky = Arc::new(FakeLauncher {
        start_state: None,
        stop_changes_state: false,
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(flaky.clone()),
            ..Default::default()
        },
    );
    let (p, t, r) = (ProjectId::new("p"), TaskId::new("t"), RunId::new("b1"));
    let mut events = support::events::approved_task_events();
    events.extend([
        EventKind::RunStarted(run("b1", Some("t"))),
        EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Planned,
            to: TaskState::Running,
        },
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    flaky
        .states
        .lock()
        .unwrap()
        .insert(r.clone(), UnitState::Active);
    // Hold downstream publication/retry effects while checking the watcher.
    e.update.close_admission();
    reconcile::reconcile(e.clone()).await.unwrap();
    assert_eq!(task_status(&e).await, TaskState::Running);

    flaky.failing.store(u32::MAX, Ordering::SeqCst);
    wait.until("a lost link to show unknown", || {
        matches!(e.proj().tasks[&t].view.state, TaskState::Unknown { .. })
    })
    .await;
    let first = flaky.queries.load(Ordering::SeqCst);
    // More than the old 51-error cutoff: every API read stays unknown and the
    // run remains live, rather than being declared crashed from a lost link.
    for _ in 0..140 {
        assert!(matches!(task_status(&e).await, TaskState::Unknown { .. }));
        assert!(!ended(&e, "b1"));
        if flaky.queries.load(Ordering::SeqCst) >= first + 55 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(flaky.queries.load(Ordering::SeqCst) >= first + 55);

    flaky.failing.store(0, Ordering::SeqCst);
    wait.until("an active unit to show running again", || {
        e.proj().tasks[&t].view.state == TaskState::Running
    })
    .await;
    assert_eq!(task_status(&e).await, TaskState::Running);

    // A second lost link can go straight to a confirmed end without another
    // active answer. A collection report during the outage must survive.
    flaky.failing.store(u32::MAX, Ordering::SeqCst);
    wait.until("the second lost link", || {
        matches!(e.proj().tasks[&t].view.state, TaskState::Unknown { .. })
    })
    .await;
    e.run_command(
        &r,
        dvb_engine::Decision {
            pre: vec![EventKind::RunCollected {
                outcome: None,
                run: r.clone(),
                head: Some("collected-head".into()),
                summary: None,
            }],
            ..dvb_engine::Decision::new("collected", dvb_core::Command::HeadWritten)
        },
    )
    .unwrap();
    assert!(matches!(task_status(&e).await, TaskState::Unknown { .. }));
    flaky.states.lock().unwrap().insert(r.clone(), end);
    flaky.failing.store(0, Ordering::SeqCst);
    wait.until("the confirmed end", || ended(&e, "b1")).await;
    let expected = match end {
        UnitState::Inactive => dvb_core::event::EndReason::Completed,
        UnitState::Failed => dvb_core::event::EndReason::Crashed,
        UnitState::Active => panic!("expected an end"),
    };
    assert_eq!(e.proj().runs[&r].ended, Some(expected));
    let status = task_status(&e).await;
    match end {
        UnitState::Inactive => assert_eq!(status, TaskState::Publishing),
        UnitState::Failed => assert!(matches!(status, TaskState::RetryPending { .. })),
        UnitState::Active => panic!("expected an end"),
    }
}

#[tokio::test]
async fn a_lost_link_stays_unknown_until_a_confirmed_success() {
    lost_link(UnitState::Inactive).await;
}

#[tokio::test]
async fn a_lost_link_stays_unknown_until_a_confirmed_failure() {
    lost_link(UnitState::Failed).await;
}

fn run(id: &str, task: Option<&str>) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(id))
        .task(task.map(TaskId::new))
        .role(RoleId::new(if task.is_some() {
            "backend"
        } else {
            "orchestrator"
        }))
        .reason("x".into())
        .base(Some("b".into()))
        .build()
}

fn ended(e: &AppState, id: &str) -> bool {
    e.proj().runs[&RunId::new(id)].ended.is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_live_run_is_settled_after_the_launcher_could_not_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let flaky = Arc::new(FakeLauncher {
        start_state: None,
        stop_changes_state: false,
        ..Default::default()
    });
    let launcher: Arc<dyn Launcher> = flaky.clone();
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher),
            ..Default::default()
        },
    );
    let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::project_event(&p, "p", "/nowhere"),
            support::events::goal_event(&g, "x", None),
            // The orchestrator was still at work when the engine stopped.
            EventKind::RunStarted(run("o1", None)),
            support::events::backend_plan_event(&g, &t),
            EventKind::GoalApproved { goal: g },
            EventKind::RunStarted(run("b1", Some("t"))),
            EventKind::TaskStateChanged {
                task: t.clone(),
                from: TaskState::Planned,
                to: TaskState::Running,
            },
        ])
    })
    .unwrap();
    // Both units are still running; the first reads of each fail.
    for id in ["o1", "b1"] {
        flaky
            .states
            .lock()
            .unwrap()
            .insert(RunId::new(id), UnitState::Active);
    }
    flaky.failing.store(3, Ordering::SeqCst);
    e.remember();
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    // The task's own check got through in the end, and its run is watched.
    wait.until("the task to be checked", || {
        e.proj().tasks[&t].view.state == TaskState::Running
    })
    .await;
    // Both units end now: each has a watcher that records it.
    for id in ["o1", "b1"] {
        flaky
            .states
            .lock()
            .unwrap()
            .insert(RunId::new(id), UnitState::Inactive);
    }
    wait.until("both runs to end", || ended(&e, "o1") && ended(&e, "b1"))
        .await;
    assert_eq!(e.proj().live_runs().count(), 0);
}

/// A builder whose unit failed while the engine was away, recovered at startup:
/// the run's actual end (with the driver's own account) settles the task, and
/// the check is recorded with the state it settled in.
async fn restart_after_a_failed_unit(collected: bool) -> TaskState {
    let dir = tempfile::tempdir().unwrap();
    let flaky = Arc::new(FakeLauncher {
        start_state: None,
        stop_changes_state: false,
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(flaky.clone()),
            ..Default::default()
        },
    );
    // Hold the retry/publication effects; only the decision is checked.
    e.update.close_admission();
    let (p, t, r) = (ProjectId::new("p"), TaskId::new("t"), RunId::new("b1"));
    let mut events = support::events::approved_task_events();
    events.extend([
        EventKind::RunStarted(run("b1", Some("t"))),
        EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Planned,
            to: if collected {
                TaskState::Collecting
            } else {
                TaskState::Running
            },
        },
    ]);
    if collected {
        events.push(EventKind::RunCollected {
            outcome: None,
            run: r.clone(),
            head: Some("a".repeat(40)),
            summary: None,
        });
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    flaky
        .states
        .lock()
        .unwrap()
        .insert(r.clone(), UnitState::Failed);
    let run_dir = e.cfg.paths.work.join("b1");
    std::fs::create_dir_all(&run_dir).unwrap();
    if !collected {
        // The driver could not read the model's output: not transient.
        std::fs::write(run_dir.join("reason"), "parse_error").unwrap();
    }
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    let expected = if collected {
        dvb_core::event::EndReason::Crashed
    } else {
        dvb_core::event::EndReason::ParseError
    };
    assert_eq!(e.proj().runs[&r].ended, Some(expected));
    let state = e.proj().tasks[&t].view.state.clone();
    let checked = support::events::all(&e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::EngineReconciled { state, .. } = ev.kind {
                Some(state)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(checked, vec![state.clone()]);
    state
}

#[tokio::test]
async fn a_collected_builder_that_crashed_while_away_is_retried_not_published() {
    let state = restart_after_a_failed_unit(true).await;
    assert!(matches!(state, TaskState::RetryPending { .. }), "{state:?}");
}

#[tokio::test]
async fn a_driver_failure_found_at_startup_fails_the_task_permanently() {
    let state = restart_after_a_failed_unit(false).await;
    assert!(matches!(state, TaskState::Failed { .. }), "{state:?}");
}

/// The current reviewer reports its verdict while its unit cannot be queried
/// and exits before the launcher answers again: the verdict is kept, the task
/// stays unknown meanwhile, and the confirmed end settles what it decided
/// without another review.
async fn reviewer_reports_during_an_outage(open: bool) -> TaskState {
    use dvb_core::run_verbs::{Finding, Severity};
    let dir = tempfile::tempdir().unwrap();
    let flaky = Arc::new(FakeLauncher {
        start_state: None,
        stop_changes_state: false,
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(flaky.clone()),
            ..Default::default()
        },
    );
    let (p, t, r) = (ProjectId::new("p"), TaskId::new("t"), RunId::new("v1"));
    let head = "a".repeat(40);
    let mut reviewer = run("v1", Some("t"));
    reviewer.reviews = Some(head.clone());
    let mut events = support::events::approved_task_events();
    events.extend([
        EventKind::BranchPublished {
            task: t.clone(),
            branch: "dvb/t-1".into(),
            head: head.clone(),
            round: 1,
        },
        EventKind::RunStarted(reviewer.clone()),
        EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Planned,
            to: TaskState::InReview,
        },
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    flaky
        .states
        .lock()
        .unwrap()
        .insert(r.clone(), UnitState::Active);
    // Hold the merge and fix-round effects; only the decisions are checked.
    e.update.close_admission();
    reconcile::reconcile(e.clone()).await.unwrap();
    let reviews = e.proj().tasks[&t].facts.reviews;

    flaky.failing.store(u32::MAX, Ordering::SeqCst);
    wait.until("a lost link to show unknown", || {
        matches!(e.proj().tasks[&t].view.state, TaskState::Unknown { .. })
    })
    .await;
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
        task: t.clone(),
        head: head.clone(),
        run: r.clone(),
        role: reviewer.role.clone(),
        model: reviewer.model.clone(),
        vendor: reviewer.vendor,
        findings: if open { vec![finding] } else { vec![] },
        summary: "s".into(),
    };
    let reply = e
        .run_command(
            &r,
            dvb_engine::Decision {
                pre: vec![recorded],
                ..dvb_engine::Decision::new(
                    "verdict",
                    dvb_core::Command::VerdictRecorded {
                        head: head.clone(),
                        open: u32::from(open),
                    },
                )
            },
        )
        .unwrap();
    assert!(
        matches!(reply, Some(TaskState::Unknown { .. })),
        "{reply:?}"
    );
    assert!(e.proj().tasks[&t].facts.verdict.is_some());
    assert!(matches!(task_status(&e).await, TaskState::Unknown { .. }));

    // The reviewer exits; then the launcher answers again.
    flaky
        .states
        .lock()
        .unwrap()
        .insert(r.clone(), UnitState::Inactive);
    flaky.failing.store(0, Ordering::SeqCst);
    wait.until("the confirmed end", || {
        ended(&e, "v1") && !matches!(e.proj().tasks[&t].view.state, TaskState::Unknown { .. })
    })
    .await;
    assert_eq!(
        e.proj().runs[&r].ended,
        Some(dvb_core::event::EndReason::Completed)
    );
    assert_eq!(e.proj().tasks[&t].facts.reviews, reviews, "no review again");
    task_status(&e).await
}

#[tokio::test]
async fn a_clean_verdict_during_an_outage_merges_after_the_reviewer_exits() {
    let state = reviewer_reports_during_an_outage(false).await;
    assert_eq!(state, TaskState::Merging);
}

#[tokio::test]
async fn a_fix_first_verdict_during_an_outage_is_not_reviewed_again() {
    let state = reviewer_reports_during_an_outage(true).await;
    assert_eq!(state, TaskState::InReview);
}
