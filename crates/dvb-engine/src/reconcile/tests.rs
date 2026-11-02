use super::*;
use crate::test_support::{assignment::Assignment, engine, events, launcher::FakeLauncher};
use dvb_core::{FakeClock, ProjectId, Timestamp};
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn fixture(
    state: TaskState,
    collected: bool,
) -> (
    tempfile::TempDir,
    AppState,
    Arc<FakeLauncher>,
    Arc<FakeClock>,
) {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::default());
    let clock = Arc::new(FakeClock::at(Timestamp(0)));
    let e = engine::open_with(
        dir.path(),
        "unused",
        crate::EngineOptions {
            launcher: Some(launcher.clone()),
            clock: Some(clock.clone()),
            ..Default::default()
        },
    );
    e.manual_dispatch();
    let task = TaskId::new("t");
    let run = RunId::new("b1");
    let mut kinds = events::approved_task_events();
    kinds.extend([
        EventKind::RunStarted(
            Assignment::new(run.clone())
                .task(Some(task.clone()))
                .base(Some("b".repeat(40)))
                .build(),
        ),
        EventKind::TaskStateChanged {
            task,
            from: TaskState::Planned,
            to: state,
        },
    ]);
    if collected {
        kinds.push(EventKind::RunCollected {
            outcome: None,
            run,
            head: Some("a".repeat(40)),
            summary: None,
        });
    }
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    (dir, e, launcher, clock)
}

#[tokio::test]
async fn a_deferred_run_check_cannot_publish_a_failed_builders_collected_head() {
    for driver_failure in [false, true] {
        let (_dir, e, launcher, _clock) = fixture(TaskState::Collecting, true);
        let task = TaskId::new("t");
        let run = RunId::new("b1");
        launcher
            .states
            .lock()
            .unwrap()
            .insert(run.clone(), UnitState::Failed);
        if driver_failure {
            let dir = e.cfg.paths.work.join(run.as_str());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("reason"), "parse_error").unwrap();
        }
        mark(&e).unwrap();
        // Startup defers the first failed run observation. Drive the task's
        // independent observation before the deferred run retry, without sleeps.
        launcher.failing.store(1, Ordering::SeqCst);
        assert!(!settle(&e, &run).await);
        task_once(&e, &task).await.unwrap();
        assert_eq!(
            e.proj().runs[&run].ended,
            Some(if driver_failure {
                EndReason::ParseError
            } else {
                EndReason::Crashed
            })
        );
        let state = e.proj().tasks[&task].view.state.clone();
        if driver_failure {
            assert!(matches!(state, TaskState::Failed { .. }));
        } else {
            assert!(matches!(state, TaskState::RetryPending { .. }));
        }
        assert!(!events::all(&e).iter().any(|event| {
            matches!(
                event.kind,
                EventKind::TaskStateChanged {
                    to: TaskState::Publishing,
                    ..
                }
            )
        }));
        assert!(settle(&e, &run).await);
        assert_eq!(e.proj().tasks[&task].view.state, state);
    }
}

#[tokio::test]
async fn a_watchers_reconciliation_does_not_consume_the_durable_stop() {
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Running),
    };
    let (_dir, e, launcher, _clock) = fixture(stopped.clone(), false);
    let task = TaskId::new("t");
    let run = RunId::new("b1");
    launcher.running(run.as_str());
    // Stops must still be enforced in paused projects.
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    mark(&e).unwrap();
    // The watcher can acquire the task lock before the startup task check.
    e.run_command(
        &run,
        crate::Decision::new(
            "reconcile",
            Command::Reconcile(Observed {
                unit: UnitState::Active,
                head: None,
            }),
        ),
    )
    .unwrap();
    task_once(&e, &task).await.unwrap();
    dispatch::step_now(&e, &task).await.unwrap();
    assert_eq!(launcher.stops.load(Ordering::SeqCst), 1);
    assert!(!launcher.is_active(&run));
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Stopped));
    assert_eq!(e.proj().tasks[&task].view.state, stopped);
}

#[tokio::test]
async fn an_inactive_unit_keeps_its_durable_stop_after_restart() {
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Running),
    };
    let (_dir, e, _launcher, _clock) = fixture(stopped.clone(), false);
    let task = TaskId::new("t");
    let run = RunId::new("b1");
    mark(&e).unwrap();
    // The unit exited after the stop was sent, before its end was recorded.
    assert!(settle(&e, &run).await);
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Stopped));
    task_once(&e, &task).await.unwrap();
    assert_eq!(e.proj().tasks[&task].view.state, stopped);
}

#[tokio::test]
async fn dispatch_defers_to_an_owner_stop_even_when_it_queued_first() {
    for cancel_owner in [false, true] {
        let (_dir, e, launcher, _clock) = fixture(TaskState::Running, false);
        let task = TaskId::new("t");
        launcher.running("b1");
        launcher.fail_stops.store(true, Ordering::SeqCst);
        let lock = e.lock(&task).await;
        let mut step = Box::pin(dispatch::step_now(&e, &task));
        assert!(futures_util::poll!(&mut step).is_pending());
        let mut stop = Box::pin(dispatch::stop(&e, &task, Actor::Owner));
        assert!(futures_util::poll!(&mut stop).is_pending());
        assert!(matches!(
            e.proj().tasks[&task].view.state,
            TaskState::Stopped { .. }
        ));
        drop(lock);
        step.await.unwrap();
        assert!(e.proj().tasks[&task].facts.retries.is_empty());
        if cancel_owner {
            drop(stop);
            dispatch::step_now(&e, &task).await.unwrap();
        } else {
            assert!(stop.await.is_err());
        }
        assert_eq!(
            e.proj().tasks[&task].facts.retries[&dvb_core::state::Effect::StopUnit],
            1
        );
    }
}

#[tokio::test]
async fn a_surviving_stop_is_retried_after_its_first_launcher_failure() {
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Running),
    };
    let (_dir, e, launcher, clock) = fixture(stopped, false);
    let task = TaskId::new("t");
    let run = RunId::new("b1");
    launcher.running(run.as_str());
    launcher.fail_stops.store(true, Ordering::SeqCst);
    mark(&e).unwrap();
    task_once(&e, &task).await.unwrap();
    dispatch::step_now(&e, &task).await.unwrap();
    assert!(
        e.proj().tasks[&task]
            .facts
            .retries
            .contains_key(&dvb_core::state::Effect::StopUnit)
    );
    assert!(launcher.is_active(&run));
    launcher.fail_stops.store(false, Ordering::SeqCst);
    clock.advance_millis(2_000);
    dispatch::step_now(&e, &task).await.unwrap();
    assert_eq!(launcher.stops.load(Ordering::SeqCst), 1);
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Stopped));
}
