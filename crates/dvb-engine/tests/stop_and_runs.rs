//! Stops that take time or fail, runs queued behind a vendor's limit or a
//! pause, the end of a run replaced meanwhile, a verdict from a replaced review,
//! and the workspace a resume carries on in. A stub launcher stands in for
//! systemd: its stops can be slow or fail.

use dvb_engine::owner;
mod support;
use axum::http::{Method, StatusCode};
use dvb_core::api::TaskView;
use dvb_core::event::EndReason;
use dvb_core::git::Git;
use dvb_core::launcher::{Launcher, UnitState};
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{
    Actor, Command, EventKind, HireId, ProjectId, RefusalCode, RoleId, RunAssignment, RunId,
    TaskId, TaskState, Vendor,
};
use dvb_engine::{AppState, Config, Engine, Fail, dispatch};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::wait::FAST as wait;

use support::launcher::FakeLauncher as Stub;

const TASK: &str = "t";

fn builder(run: &str, base: &str) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .task(Some(TaskId::new(TASK)))
        .hire(HireId::new("h-backend"))
        .base(Some(base.into()))
        .build()
}

fn reviewer(run: &str, head: &str) -> RunAssignment {
    support::assignment::Assignment::from_assignment(builder(run, head))
        .role(RoleId::new("code-reviewer"))
        .hire(HireId::new("h-code-reviewer"))
        .reviews(Some(head.into()))
        .build()
}

/// A project with its roles and hires, an approved goal and one planned task,
/// then `more`.
fn engine(dir: &Path, stub: &Arc<Stub>, more: Vec<EventKind>) -> AppState {
    engine_config(Config::dev(dir, "dvb".into()), stub, more)
}

fn engine_config(cfg: Config, stub: &Arc<Stub>, more: Vec<EventKind>) -> AppState {
    let launcher: Arc<dyn Launcher> = stub.clone();
    let e = support::engine::open_config(
        cfg,
        stub.readings.options(dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher),
            disk_reading: Some(Arc::new(|| Some(u64::MAX))),
            ..Default::default()
        }),
    );
    let p = ProjectId::new("p");
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds = support::events::approved_task_events();
    kinds.splice(1..1, roles.into_iter().chain(hires));
    kinds.extend(more);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    e
}

#[test]
fn a_stopped_run_keeps_its_end_reason_when_the_unit_link_was_lost() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &Arc::new(Stub::default()), Vec::new());
    let (t, r) = (TaskId::new(TASK), RunId::new("r"));
    e.decide(
        e.proj(),
        &t,
        Actor::Engine,
        dvb_engine::Decision::new("start", Command::Start(builder("r", "base"))),
        |_| Ok(()),
    )
    .unwrap();
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_active", Command::UnitActive),
    )
    .unwrap();
    let stopped = e
        .decide(
            e.proj(),
            &t,
            Actor::Owner,
            dvb_engine::Decision::new("stop", Command::Stop(None)),
            |_| Ok(()),
        )
        .unwrap();
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_unknown", Command::Restarted),
    )
    .unwrap();
    assert!(matches!(
        e.proj().tasks[&t].view.state,
        TaskState::Unknown { .. }
    ));
    e.end_run(&r, EndReason::Completed, None).unwrap();
    assert_eq!(e.proj().runs[&r].ended, Some(EndReason::Stopped));
    assert_eq!(e.proj().tasks[&t].view.state, stopped);
}

/// An owner's reasoned hold, and its reason, outlast a restart's
/// reconciliation of the stopped task, to `Unknown` and back.
#[test]
fn a_reasoned_hold_survives_restart_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &Arc::new(Stub::default()), Vec::new());
    let (t, r) = (TaskId::new(TASK), RunId::new("r"));
    e.decide(
        e.proj(),
        &t,
        Actor::Engine,
        dvb_engine::Decision::new("start", Command::Start(builder("r", "base"))),
        |_| Ok(()),
    )
    .unwrap();
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_active", Command::UnitActive),
    )
    .unwrap();
    let reason = Some("Hold until #442 merges".to_owned());
    e.decide(
        e.proj(),
        &t,
        Actor::Owner,
        dvb_engine::Decision::new("stop", Command::Stop(reason.clone())),
        |_| Ok(()),
    )
    .unwrap();
    let held = |e: &AppState| {
        let proj = e.proj();
        (proj.tasks[&t].held, proj.tasks[&t].view.reason.clone())
    };
    assert_eq!(held(&e), (true, reason.clone()));
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_unknown", Command::Restarted),
    )
    .unwrap();
    assert!(matches!(state(&e), TaskState::Unknown { .. }));
    assert_eq!(held(&e), (true, reason.clone()));
    e.end_run(&r, EndReason::Completed, None).unwrap();
    assert!(matches!(state(&e), TaskState::Stopped { .. }));
    assert_eq!(held(&e), (true, reason));
}

fn moved(from: TaskState, to: TaskState) -> EventKind {
    EventKind::TaskStateChanged {
        task: TaskId::new(TASK),
        from,
        to,
    }
}

/// The builder `b1` running on base `x`.
fn running() -> Vec<EventKind> {
    vec![
        EventKind::RunStarted(builder("b1", "x")),
        EventKind::TaskStarted {
            branch_repair: false,
            retried: false,
            task: TaskId::new(TASK),
            run: RunId::new("b1"),
            resume: false,
        },
        moved(TaskState::Planned, TaskState::Running),
    ]
}

fn state(e: &AppState) -> TaskState {
    e.proj().tasks[&TaskId::new(TASK)].view.state.clone()
}

fn count(e: &AppState, kind: &str) -> usize {
    support::events::all(e)
        .iter()
        .filter(|ev| ev.kind.kind() == kind)
        .count()
}

fn ended(e: &AppState, run: &str) -> Option<EndReason> {
    e.proj().runs.get(&RunId::new(run))?.ended.clone()
}

/// The Fake vendor at its limit for a moment (vendor pause).
fn limit(e: &AppState, ms: i64) {
    let resets = e.clock.now().plus_millis(ms);
    dvb_engine::vendors::record(
        e,
        Vendor::Fake,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(resets),
        },
    )
    .unwrap();
}

fn stopped(was: TaskState) -> TaskState {
    TaskState::Stopped { was: Box::new(was) }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_returns_once_the_unit_is_inactive_and_records_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    stub.linger.store(10, Ordering::SeqCst);
    let e = engine(dir.path(), &stub, running());
    stub.running("b1");
    let t = TaskId::new(TASK);
    let after = dispatch::stop(&e, &t, Actor::Owner).await.unwrap();
    assert_eq!(after, stopped(TaskState::Running));
    // Ten state reads went by before the unit was inactive; its end is recorded
    // by the stop, once.
    assert_eq!(stub.stopping.lock().unwrap()[&RunId::new("b1")], 0);
    assert_eq!(ended(&e, "b1"), Some(EndReason::Stopped));
    dvb_engine::dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Crashed)
        .await
        .unwrap();
    assert_eq!(count(&e, "run.ended"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_that_cannot_be_confirmed_is_an_error_and_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    stub.fail_stops.store(true, Ordering::SeqCst);
    let e = engine(dir.path(), &stub, running());
    stub.running("b1");
    let t = TaskId::new(TASK);
    let r = dispatch::stop(&e, &t, Actor::Owner).await;
    assert!(matches!(r, Err(Fail::Internal(_))), "{r:?}");
    assert!(
        matches!(&r, Err(Fail::Internal(dvb_engine::InternalError::Context { source, .. }))
        if matches!(source.as_ref(), dvb_engine::InternalError::Launch(_))),
        "{r:?}"
    );
    // Stopped as recorded, the run not ended, the retry recorded.
    assert_eq!(state(&e), stopped(TaskState::Running));
    assert_eq!(ended(&e, "b1"), None);
    assert_eq!(count(&e, "effect.retry"), 1);
    // No resume while the stopped unit may still run.
    match dispatch::resume(&e, &t, Actor::Owner).await {
        Err(Fail::Refused(r)) => assert_eq!(r.code, RefusalCode::ValueChanged),
        other => panic!("resumed with the unit still running: {other:?}"),
    }
    // The stop job works again: the dispatcher stops it when the retry is due.
    stub.fail_stops.store(false, Ordering::SeqCst);
    e.kick(&t);
    wait.until("the unit to be stopped", || ended(&e, "b1").is_some())
        .await;
    assert_eq!(ended(&e, "b1"), Some(EndReason::Stopped));
    let after = dispatch::resume(&e, &t, Actor::Owner).await.unwrap();
    assert_eq!(after, TaskState::Preparing);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_queued_behind_a_vendor_limit_ends_it_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    limit(&e, 1_000);
    let t = TaskId::new(TASK);
    dispatch::start_task(&e, builder("q1", "x"), Actor::Engine)
        .await
        .unwrap();
    assert_eq!(state(&e), TaskState::Preparing);
    assert_eq!(e.proj().live_runs().count(), 1);
    dispatch::stop(&e, &t, Actor::Owner).await.unwrap();
    // Ended at once: the slot is free, nothing waits for the vendor.
    assert_eq!(ended(&e, "q1"), Some(EndReason::Stopped));
    assert_eq!(e.proj().live_runs().count(), 0);
    // When the vendor reopens nothing starts, and the end is not recorded twice.
    wait.until("the vendor to reopen", || {
        !e.vendors.is_limited(Vendor::Fake)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    assert_eq!(count(&e, "run.ended"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_queued_behind_a_vendor_limit_waits_out_a_pause() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    limit(&e, 1_000);
    dispatch::start_task(&e, builder("q1", "x"), Actor::Engine)
        .await
        .unwrap();
    let p = ProjectId::new("p");
    owner::pause(&e, &p, true).unwrap();
    wait.until("the vendor to reopen", || {
        !e.vendors.is_limited(Vendor::Fake)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Held by the pause: not started, not ended.
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    assert_eq!(ended(&e, "q1"), None);
    owner::pause(&e, &p, false).unwrap();
    wait.until("the held run to start", || {
        stub.starts.load(Ordering::SeqCst) == 1 && state(&e) == TaskState::Running
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_after_a_stopped_queued_resume_carries_on_in_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.extend([
        EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Stopped,
            detail: None,
            stderr: None,
        },
        moved(TaskState::Running, stopped(TaskState::Running)),
    ]);
    let e = engine(dir.path(), &stub, more);
    // b1 prepared its workspace and edited it.
    let b1 = dir.path().join("work/b1");
    std::fs::create_dir_all(b1.join("ws")).unwrap();
    std::fs::write(b1.join("ws/edit.md"), "kept\n").unwrap();
    std::fs::write(b1.join("prepared"), "").unwrap();

    // A resume waits for the vendor, and is stopped before it starts.
    limit(&e, 1_000);
    let t = TaskId::new(TASK);
    assert_eq!(
        dispatch::resume(&e, &t, Actor::Owner).await.unwrap(),
        TaskState::Preparing
    );
    let queued = e.proj().tasks[&t].facts.run.clone().unwrap();
    dispatch::stop(&e, &t, Actor::Owner).await.unwrap();
    assert_eq!(ended(&e, queued.as_str()), Some(EndReason::Stopped));

    // The next resume carries on in b1's workspace, not a fresh clone.
    dispatch::resume(&e, &t, Actor::Owner).await.unwrap();
    let next = e.proj().tasks[&t].facts.run.clone().unwrap();
    wait.until("the resumed run to start", || {
        stub.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    let dir_next = dir.path().join("work").join(next.as_str());
    assert_eq!(
        std::fs::read_to_string(dir_next.join("ws/edit.md")).unwrap(),
        "kept\n"
    );
    assert!(dir_next.join("prepared").is_file());
    let env = std::fs::read_to_string(dir_next.join("env")).unwrap();
    assert!(env.contains("DVB_RESUME=1"), "{env}");
}

/// b1 prepared its workspace, edited it and was stopped; `secret` stands for a
/// file of the engine's that the run cannot read.
fn stopped_with_workspace(dir: &Path, stub: &Arc<Stub>) -> (AppState, std::path::PathBuf) {
    let mut more = running();
    more.extend([
        EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Stopped,
            detail: None,
            stderr: None,
        },
        moved(TaskState::Running, stopped(TaskState::Running)),
    ]);
    let e = engine(dir, stub, more);
    let b1 = dir.join("work/b1");
    std::fs::create_dir_all(b1.join("ws")).unwrap();
    std::fs::write(b1.join("ws/edit.md"), "kept\n").unwrap();
    std::fs::write(b1.join("prepared"), "").unwrap();
    let secret = dir.join("github-app.pem");
    std::fs::write(&secret, "key").unwrap();
    (e, secret)
}

/// The resumed run started fresh: no marker, no workspace, nothing of `secret`.
async fn started_afresh(e: &AppState, stub: &Stub, secret: &Path) {
    let t = TaskId::new(TASK);
    wait.until("the run to start", || {
        stub.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    let next = e.proj().tasks[&t].facts.run.clone().unwrap();
    let dir_next = secret.parent().unwrap().join("work").join(next.as_str());
    assert!(std::fs::symlink_metadata(dir_next.join("prepared")).is_err());
    assert!(std::fs::symlink_metadata(dir_next.join("ws")).is_err());
    let env = std::fs::read_to_string(dir_next.join("env")).unwrap();
    assert!(env.contains("DVB_RESUME=0"), "{env}");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "key");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_does_not_take_a_planted_link_for_the_marker_or_the_workspace() {
    for plant in ["prepared", "ws"] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let (e, secret) = stopped_with_workspace(dir.path(), &stub);
        let b1 = dir.path().join("work/b1");
        let target = if plant == "ws" {
            let elsewhere = dir.path().join("elsewhere");
            std::fs::rename(b1.join("ws"), &elsewhere).unwrap();
            elsewhere
        } else {
            std::fs::remove_file(b1.join("prepared")).unwrap();
            secret.clone()
        };
        std::os::unix::fs::symlink(&target, b1.join(plant)).unwrap();
        dispatch::resume(&e, &TaskId::new(TASK), Actor::Owner)
            .await
            .unwrap();
        started_afresh(&e, &stub, &secret).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_resume_makes_its_marker_and_never_copies_one_swapped_for_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let (e, secret) = stopped_with_workspace(dir.path(), &stub);
    // Chosen while b1's marker is real, launched after it became a link.
    limit(&e, 1_000);
    dispatch::resume(&e, &TaskId::new(TASK), Actor::Owner)
        .await
        .unwrap();
    let marker = dir.path().join("work/b1/prepared");
    std::fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink(&secret, &marker).unwrap();
    started_afresh(&e, &stub, &secret).await;
}

/// A stop also ends the vendor CLI, and the driver may report its cut-off
/// stream before it sees the stop: the run still ends stopped, however its
/// unit's end is seen. A run that ends by itself so ends `parse_error`.
#[test]
fn a_stopped_runs_cut_off_stream_is_no_parse_error() {
    for (stop, expected) in [(false, EndReason::ParseError), (true, EndReason::Stopped)] {
        for unit in [EndReason::Completed, EndReason::Crashed] {
            let dir = tempfile::tempdir().unwrap();
            let e = engine(dir.path(), &Arc::new(Stub::default()), running());
            if stop {
                e.decide(
                    e.proj(),
                    &TaskId::new(TASK),
                    Actor::Owner,
                    dvb_engine::Decision::new("stop", Command::Stop(None)),
                    |_| Ok(()),
                )
                .unwrap();
            }
            e.end_run(&RunId::new("b1"), unit.clone(), Some(EndReason::ParseError))
                .unwrap();
            assert_eq!(ended(&e, "b1"), Some(expected.clone()), "{unit:?}");
        }
    }
    // A run stopped without a task (an orchestrator's, for an update).
    for (stop, expected) in [
        (false, EndReason::ParseError),
        (true, EndReason::Interrupted),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut more = vec![EventKind::RunStarted(orchestrator("o1"))];
        if stop {
            more.push(EventKind::RunStopRequested {
                run: RunId::new("o1"),
            });
        }
        let e = engine(dir.path(), &Arc::new(Stub::default()), more);
        e.end_run(
            &RunId::new("o1"),
            EndReason::Completed,
            Some(EndReason::ParseError),
        )
        .unwrap();
        assert_eq!(ended(&e, "o1"), Some(expected));
    }
}

#[test]
fn the_end_of_a_replaced_run_does_not_move_its_successor() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.push(EventKind::RunStarted(builder("b2", "x")));
    let e = engine(dir.path(), &stub, more);
    // b1 was replaced by b2: its end is recorded and moves nothing.
    e.end_run(
        &RunId::new("b1"),
        EndReason::Crashed,
        Some(EndReason::ParseError),
    )
    .unwrap();
    assert_eq!(ended(&e, "b1"), Some(EndReason::ParseError));
    assert_eq!(state(&e), TaskState::Running);
    e.end_run(&RunId::new("b1"), EndReason::Crashed, None)
        .unwrap();
    assert_eq!(count(&e, "run.ended"), 1);
    // The current run's end is the task's.
    e.end_run(&RunId::new("b2"), EndReason::Crashed, None)
        .unwrap();
    assert!(matches!(
        state(&e),
        TaskState::RetryPending {
            effect: dvb_core::state::Effect::StartUnit,
            ..
        }
    ));
    // And a stale call from the replaced run is not the task's news.
    let r = e
        .run_command(
            &RunId::new("b1"),
            dvb_engine::Decision::new("collected", Command::HeadWritten),
        )
        .unwrap();
    assert_eq!(r, None);
}

#[test]
fn a_crashed_runs_cause_is_on_its_end_and_its_task_says_it_retries() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(dir.path(), &Arc::new(Stub::default()), running());
    let run_dir = dir.path().join("work/b1");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("cause"),
        "exit 4: exec: the CLI exited 1\nits second line",
    )
    .unwrap();
    e.end_run(&RunId::new("b1"), EndReason::Crashed, None)
        .unwrap();
    let details: Vec<_> = support::events::all(&e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::RunEnded { detail, .. } = ev.kind {
                Some(detail)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(details, [Some("exit 4: exec: the CLI exited 1".to_owned())]);
    let t = e.proj().tasks[&TaskId::new(TASK)].view.clone();
    assert!(
        matches!(t.state, TaskState::RetryPending { .. }),
        "{:?}",
        t.state
    );
    assert_eq!(
        t.reason.as_deref(),
        Some(
            "Work ended: crashed (exit 4: exec: the CLI exited 1); attempt 2 starts automatically"
        )
    );
}

#[test]
fn a_crashed_last_attempt_names_its_cause_and_clean_stderr_on_its_end_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut kinds = Vec::new();
    for n in 1..=3 {
        let run = format!("b{n}");
        kinds.extend([
            EventKind::RunStarted(builder(&run, "x")),
            EventKind::TaskStarted {
                branch_repair: false,
                retried: false,
                task: TaskId::new(TASK),
                run: RunId::new(&run),
                resume: false,
            },
        ]);
        if n < 3 {
            kinds.push(EventKind::RunEnded {
                run: RunId::new(&run),
                reason: EndReason::Crashed,
                detail: None,
                stderr: None,
            });
        }
    }
    kinds.push(moved(TaskState::Planned, TaskState::Running));
    let e = engine(dir.path(), &Arc::new(Stub::default()), kinds);
    let run_dir = dir.path().join("work/b3");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("cause"),
        "exit 4: exec: the session exited with code 1",
    )
    .unwrap();
    let token = format!("ghp_{}", "a".repeat(36));
    let mut stderr: Vec<String> = (0..20)
        .map(|n| format!("{n}: {}", "x".repeat(60)))
        .collect();
    stderr.extend([
        format!("auth: ghp_\u{1b}[31m{}\u{1b}[0m", &token[4..]),
        String::new(),
        "\u{1b}[31merror: No space left on device (os error 28)\u{1b}[0m".into(),
    ]);
    std::fs::write(run_dir.join("stderr"), stderr.join("\n")).unwrap();
    e.end_run(&RunId::new("b3"), EndReason::Crashed, None)
        .unwrap();
    let ended = support::events::all(&e)
        .into_iter()
        .find_map(|ev| {
            if let EventKind::RunEnded {
                run,
                detail,
                stderr,
                ..
            } = ev.kind
                && run.as_str() == "b3"
            {
                Some((detail, stderr))
            } else {
                None
            }
        })
        .unwrap();
    // Whole lines from the end within 400 bytes; the blank line is dropped.
    let mut tail = stderr[16..20].to_vec();
    tail.extend([
        "[line withheld by dvb's secret scan: github-token]".into(),
        "error: No space left on device (os error 28)".into(),
    ]);
    assert_eq!(
        ended,
        (
            Some("exit 4: exec: the session exited with code 1".to_owned()),
            Some(tail.join("\n"))
        )
    );
    assert!(tail.join("\n").len() <= 400 && stderr[15..].join("\n").len() > 400);
    assert_eq!(
        state(&e),
        TaskState::Failed {
            reason: format!(
                "3 attempts; the last ended: crashed (exit 4: exec: the session exited with \
                 code 1); its stderr ended: {}",
                tail.join(" | ")
            )
        }
    );
}

async fn git(dir: &Path, args: &[&str]) -> String {
    Git::new(dir)
        .run(args, &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap()
}

/// A real collected commit and its receipt, with no socket report, and the
/// project's mirror it is checked in.
async fn collection_receipt(dir: &Path) -> (String, String) {
    let run_dir = dir.join("work/b1");
    let ws = run_dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mirror = dir.join("mirrors/p.git");
    std::fs::create_dir_all(&mirror).unwrap();
    Git::new(&mirror)
        .run(&["init", "--quiet", "--bare"], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    Git::new(&ws)
        .run(&["init", "--quiet"], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    let who = [
        ("GIT_AUTHOR_NAME", "test"),
        ("GIT_AUTHOR_EMAIL", "test@dvb.invalid"),
        ("GIT_COMMITTER_NAME", "test"),
        ("GIT_COMMITTER_EMAIL", "test@dvb.invalid"),
    ];
    Git::new(&ws)
        .run(&["commit", "--quiet", "--allow-empty", "-m", "base"], &who)
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    let base = Git::new(&ws)
        .run(&["rev-parse", "HEAD"], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    std::fs::write(ws.join("work.txt"), "completed work\n").unwrap();
    Git::new(&ws)
        .run(&["add", "."], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    Git::new(&ws)
        .run(&["commit", "--quiet", "-m", "collected"], &who)
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    let head = Git::new(&ws)
        .run(&["rev-parse", "HEAD"], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    git(&ws, &["update-ref", "refs/dvb/collected", &head]).await;
    let bundle = run_dir.join(dvb_engine::publish::COLLECTED_BUNDLE);
    let bundle = bundle.to_str().unwrap();
    git(
        &ws,
        &["bundle", "create", "--quiet", bundle, "refs/dvb/collected"],
    )
    .await;
    std::fs::write(run_dir.join("head"), format!("{head}\n")).unwrap();
    (base, head)
}

#[tokio::test]
async fn a_lost_collection_report_recovers_without_another_builder_session() {
    for reported in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let (base, head) = collection_receipt(dir.path()).await;
        let mut more = running();
        more[0] = EventKind::RunStarted(builder("b1", &base));
        more.push(EventKind::RunCollected {
            outcome: None,
            run: RunId::new("b1"),
            head: None,
            summary: Some("Completed work and checked the snapshot.".into()),
        });
        if reported {
            // The event landed, but the task command or its reply was lost.
            more.push(EventKind::RunCollected {
                outcome: None,
                run: RunId::new("b1"),
                head: Some(head.clone()),
                summary: None,
            });
        }
        let e = engine(dir.path(), &stub, more);
        dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Completed)
            .await
            .unwrap();
        assert_eq!(ended(&e, "b1"), Some(EndReason::Completed));
        assert_eq!(
            e.proj().runs[&RunId::new("b1")].collected.as_deref(),
            Some(head.as_str())
        );
        assert_eq!(count(&e, "run.collected"), 2);
        assert_eq!(
            e.proj().runs[&RunId::new("b1")].summary.as_deref(),
            Some("Completed work and checked the snapshot.")
        );
        assert_eq!(state(&e), TaskState::Publishing);
        assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
        assert_eq!(count(&e, "run.started"), 1);
    }
}

#[tokio::test]
async fn an_unusable_collection_receipt_stays_a_failure() {
    for invalid in [
        "missing",
        "mismatched",
        "nonexistent",
        "expression",
        "no-bundle",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let (base, _) = collection_receipt(dir.path()).await;
        let run_dir = dir.path().join("work/b1");
        match invalid {
            "missing" => std::fs::remove_file(run_dir.join("head")).unwrap(),
            "mismatched" => std::fs::write(run_dir.join("head"), &base).unwrap(),
            "nonexistent" => std::fs::write(run_dir.join("head"), "0".repeat(40)).unwrap(),
            "expression" => std::fs::write(run_dir.join("head"), "refs/dvb/collected").unwrap(),
            "no-bundle" => {
                std::fs::remove_file(run_dir.join(dvb_engine::publish::COLLECTED_BUNDLE)).unwrap();
            }
            _ => unreachable!(),
        }
        let e = engine(dir.path(), &stub, running());
        dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Completed)
            .await
            .unwrap();
        assert_eq!(ended(&e, "b1"), Some(EndReason::Completed), "{invalid}");
        assert!(matches!(state(&e), TaskState::Failed { .. }), "{invalid}");
        assert_eq!(count(&e, "run.collected"), 0);
        let mirror = dir.path().join("mirrors/p.git");
        let runs = git(&mirror, &["for-each-ref", "refs/dvb/runs"]).await;
        assert_eq!(runs, "", "{invalid}");
    }
}

#[tokio::test]
async fn a_workspaces_own_configuration_runs_nothing_when_its_receipt_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let (_, head) = collection_receipt(dir.path()).await;
    let ws = dir.path().join("work/b1/ws");
    // A partial clone whose snapshot lacks its tree: reading it would fetch the
    // tree from the configured remote, whose upload-pack leaves a mark.
    let tree = git(&ws, &["rev-parse", &format!("{head}^{{tree}}")]).await;
    let marker = dir.path().join("marker");
    let mark = format!("touch {}; false", marker.display());
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.promisor", "true"),
        ("remote.origin.url", ws.to_str().unwrap()),
        ("remote.origin.uploadpack", &mark),
    ] {
        git(&ws, &["config", key, value]).await;
    }
    std::fs::remove_file(ws.join(".git/objects").join(&tree[..2]).join(&tree[2..])).unwrap();
    let e = engine(dir.path(), &stub, running());
    dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Completed)
        .await
        .unwrap();
    assert!(!marker.exists());
    // The receipt is read from the run's bundle, never its repository.
    assert_eq!(state(&e), TaskState::Publishing);
    assert_eq!(
        e.proj().runs[&RunId::new("b1")].collected.as_deref(),
        Some(head.as_str())
    );
}

#[tokio::test]
async fn a_linked_bundle_receipt_stays_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    collection_receipt(dir.path()).await;
    let bundle = dir
        .path()
        .join("work/b1")
        .join(dvb_engine::publish::COLLECTED_BUNDLE);
    let elsewhere = dir.path().join("elsewhere.bundle");
    std::fs::rename(&bundle, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &bundle).unwrap();
    let e = engine(dir.path(), &stub, running());
    dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Completed)
        .await
        .unwrap();
    assert!(matches!(state(&e), TaskState::Failed { .. }));
    assert_eq!(count(&e, "run.collected"), 0);
}

#[tokio::test]
async fn a_receipt_does_not_hide_a_failed_or_stopped_model_session() {
    for reason in ["", "parse_error", "mismatch"] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        collection_receipt(dir.path()).await;
        std::fs::write(dir.path().join("work/b1/reason"), reason).unwrap();
        let e = engine(dir.path(), &stub, running());
        dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Crashed)
            .await
            .unwrap();
        let expected = match reason {
            "parse_error" => EndReason::ParseError,
            "mismatch" => EndReason::Mismatch,
            _ => EndReason::Crashed,
        };
        assert_eq!(ended(&e, "b1"), Some(expected));
        assert_eq!(count(&e, "run.collected"), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    collection_receipt(dir.path()).await;
    let mut more = running();
    more.push(moved(
        TaskState::Running,
        TaskState::Stopped {
            was: Box::new(TaskState::Running),
        },
    ));
    let e = engine(dir.path(), &stub, more);
    dispatch::run_ended(&e, &RunId::new("b1"), EndReason::Completed)
        .await
        .unwrap();
    assert_eq!(ended(&e, "b1"), Some(EndReason::Stopped));
    assert!(matches!(state(&e), TaskState::Stopped { .. }));
}

#[test]
fn a_verdict_from_a_replaced_review_is_not_the_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let more = vec![
        EventKind::BranchPublished {
            task: TaskId::new(TASK),
            round: 1,
            branch: "dvb/t-1".into(),
            head: "h1".into(),
        },
        moved(TaskState::Planned, TaskState::InReview),
        EventKind::RunStarted(reviewer("v1", "h1")),
        // A stop and a resume put a new reviewer on the same head.
        EventKind::RunStarted(reviewer("v2", "h1")),
    ];
    let e = engine(dir.path(), &stub, more);
    let clean = || Command::VerdictRecorded {
        head: "h1".into(),
        open: 0,
    };
    let old = e
        .run_command(
            &RunId::new("v1"),
            dvb_engine::Decision::new("verdict", clean()),
        )
        .unwrap();
    assert_eq!(old, None);
    assert_eq!(state(&e), TaskState::InReview);
    let new = e
        .run_command(
            &RunId::new("v2"),
            dvb_engine::Decision::new("verdict", clean()),
        )
        .unwrap();
    assert_eq!(new, Some(TaskState::Merging));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_start_up_check_keeps_a_stopped_tasks_run_and_drops_its_build_output() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.extend([
        EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Stopped,
            detail: None,
            stderr: None,
        },
        moved(TaskState::Running, stopped(TaskState::Running)),
    ]);
    let run_dir = dir.path().join("work/b1");
    std::fs::create_dir_all(run_dir.join("build/debug")).unwrap();
    std::fs::create_dir_all(run_dir.join("ws/target")).unwrap();
    std::fs::write(run_dir.join("ws/target/new-source.md"), "mine\n").unwrap();
    let cache = dir.path().join("mirrors/.cache/p/cargo/registry");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("keep"), "cached").unwrap();
    let e = engine(dir.path(), &stub, more);
    dvb_engine::reconcile::reconcile(e).await.unwrap();
    wait.until("the build directory to be removed", || {
        !run_dir.join("build").exists()
    })
    .await;
    // The stopped task may resume in its workspace.
    assert!(run_dir.join("ws/target/new-source.md").exists());
    assert_eq!(
        std::fs::read_to_string(cache.join("keep")).unwrap(),
        "cached"
    );
}

fn orchestrator(run: &str) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .role(RoleId::new("orchestrator"))
        .build()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_start_up_check_removes_the_directories_nothing_reads_again() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.extend([
        EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        },
        moved(TaskState::Running, TaskState::Done),
        EventKind::RunStarted(orchestrator("o1")),
        EventKind::RunEnded {
            run: RunId::new("o1"),
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        },
        EventKind::RunStarted(orchestrator("o2")),
    ]);
    let e = engine(dir.path(), &stub, more);
    stub.running("o2");
    let work = dir.path().join("work");
    for run in ["b1", "o1", "o2"] {
        std::fs::create_dir_all(work.join(run).join("ws/src")).unwrap();
    }
    // Login runs record no run: one that wrote its result, and one still going.
    let login = |id: &str, result: bool| {
        let d = work.join(id);
        std::fs::create_dir_all(d.join("home")).unwrap();
        std::fs::write(
            d.join("env"),
            "DVB_RUN_ID=x\nDVB_LOGIN=1\nDVB_VENDOR=claude\n",
        )
        .unwrap();
        if result {
            std::fs::write(d.join("login-result"), "ok").unwrap();
        }
        d
    };
    let (ended, live) = (
        login("01M5A0000000000000000G1N01", true),
        login("01M5A0000000000000000G1N02", false),
    );
    // Not a login run, though named like one.
    let other = work.join("01M5A0000000000000000G1N03");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("login-result"), "ok").unwrap();
    dvb_engine::reconcile::reconcile(e.clone()).await.unwrap();
    for removed in [work.join("b1"), work.join("o1"), ended] {
        wait.until(&format!("{} to be removed", removed.display()), || {
            !removed.exists()
        })
        .await;
    }
    assert!(work.join("o2/ws/src").exists());
    assert!(live.join("env").exists());
    assert!(other.join("login-result").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_done_or_dropped_loses_its_ended_runs_directories() {
    for to in [TaskState::Done, TaskState::Cancelled] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let mut more = running();
        more.push(EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        });
        let e = engine(dir.path(), &stub, more);
        let run_dir = dir.path().join("work/b1");
        std::fs::create_dir_all(run_dir.join("ws/src")).unwrap();
        e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(vec![moved(TaskState::Running, to.clone())])
        })
        .unwrap();
        wait.until(
            &format!("the {to:?} task's run directory to be removed"),
            || !run_dir.exists(),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ends_after_its_task_is_done_loses_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.extend([
        moved(TaskState::Running, TaskState::Done),
        EventKind::RunStarted(orchestrator("o1")),
    ]);
    let e = engine(dir.path(), &stub, more);
    let work = dir.path().join("work");
    for run in ["b1", "o1"] {
        std::fs::create_dir_all(work.join(run).join("ws/src")).unwrap();
        std::fs::create_dir_all(work.join(run).join("build/debug")).unwrap();
        dispatch::run_ended(&e, &RunId::new(run), EndReason::Completed)
            .await
            .unwrap();
    }
    wait.until("the builder's run directory to be removed", || {
        !work.join("b1").exists()
    })
    .await;
    wait.until("the orchestrator's run directory to be removed", || {
        !work.join("o1").exists()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_task_loses_its_runs_directories_when_its_goal_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let mut more = running();
    more.extend([
        EventKind::RunEnded {
            run: RunId::new("b1"),
            reason: EndReason::Crashed,
            detail: None,
            stderr: None,
        },
        moved(
            TaskState::Running,
            TaskState::Failed {
                reason: "crashed".into(),
            },
        ),
    ]);
    let e = engine(dir.path(), &stub, more);
    let run_dir = dir.path().join("work/b1");
    std::fs::create_dir_all(run_dir.join("ws/src")).unwrap();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDropped {
            goal: dvb_core::GoalId::new("g"),
        }])
    })
    .unwrap();
    assert_eq!(state(&e), TaskState::Cancelled);
    wait.until("the dropped goal's run directory to be removed", || {
        !run_dir.exists()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_held_for_disk_space_and_starts_when_it_frees() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    let gb = 1024 * 1024 * 1024;
    *stub.readings.disk.lock().unwrap() = Some(10 * gb);
    dispatch::start_task(&e, builder("q1", "x"), Actor::Engine)
        .await
        .unwrap();
    wait.until("the task to be told", || count(&e, "run.output") == 1)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    assert_eq!(count(&e, "run.output"), 1);
    let told = support::events::all(&e).into_iter().find_map(|ev| {
        if let EventKind::RunOutput {
            resource: Some(dvb_core::event::ResourceWait::Disk),
            item: dvb_core::event::OutputItem::Note(n),
            ..
        } = ev.kind
        {
            Some(n)
        } else {
            None
        }
    });
    assert_eq!(told.as_deref(), Some("10 GB free, needs 25 GB"));
    // Space frees as a run ends.
    *stub.readings.disk.lock().unwrap() = Some(30 * gb);
    e.disk_freed.notify_waiters();
    wait.until("the held run to start", || {
        stub.starts.load(Ordering::SeqCst) == 1 && state(&e) == TaskState::Running
    })
    .await;
}

const MEMORY_MIN: u64 = 10 * 1024 * 1024 * 1024;

#[tokio::test(start_paused = true)]
async fn freeing_only_memory_refreshes_the_held_tasks_status_to_waiting_for_disk() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    *stub.readings.disk.lock().unwrap() = Some(0);
    *stub.readings.memory.lock().unwrap() = Some(0);
    dispatch::start_task(&e, builder("both-resources", "x"), Actor::Engine)
        .await
        .unwrap();
    wait.until("both resource notices", || count(&e, "run.output") == 2)
        .await;
    assert_memory_status(&e, true);

    // Only memory recovers; the real thirty-second poll must refresh status
    // even though this run's disk notice was already delivered.
    let before_recovery = e.store.last_id().unwrap();
    let feed = dvb_engine::feed::after(&e, before_recovery).unwrap();
    tokio::pin!(feed);
    assert_eq!(feed.next().await, Some(dvb_engine::feed::FeedItem::Live));
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    let (status, body) = support::http::request(&e, Method::GET, "/tasks/t", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: TaskView = serde_json::from_slice(&body).unwrap();
    assert_eq!(view.reason.as_deref(), Some("Waiting for disk"));
    let changed = EventKind::TaskReasonChanged {
        resource: Some(dvb_core::event::ResourceWait::Disk),
        task: TaskId::new(TASK),
        run: RunId::new("both-resources"),
        reason: Some("Waiting for disk".into()),
    };
    assert!(
        e.store
            .after(before_recovery)
            .unwrap()
            .iter()
            .any(|ev| ev.kind == changed),
        "changed reason must invalidate live task snapshots through a task event"
    );
    assert!(matches!(
        feed.next().await,
        Some(dvb_engine::feed::FeedItem::Event(ev)) if ev.kind == changed
    ));
    assert!(dvb_cli::task_line(&view).ends_with(" - Preparing: Waiting for disk"));
    assert_eq!(view.state, TaskState::Preparing);
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    let cursor = e.store.last_id().unwrap();
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(e.store.last_id().unwrap(), cursor);
    assert_eq!(memory_notices(&e), 1);
    let events = support::events::all(&e);
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(
                &ev.kind,
                EventKind::RunOutput {
                    resource: Some(dvb_core::event::ResourceWait::Disk),
                    ..
                }
            ))
            .count(),
        1
    );
    let rebuilt = dvb_engine::proj::Projections::rebuild(&events);
    assert_eq!(rebuilt.tasks[&TaskId::new(TASK)].view.reason, view.reason);
}

#[tokio::test(start_paused = true)]
async fn recovered_memory_clears_resource_status_while_admission_still_holds_the_task() {
    for vendor_hold in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let e = engine(dir.path(), &stub, vec![]);
        *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
        *stub.readings.memory.lock().unwrap() = Some(0);
        if vendor_hold {
            limit(&e, 600_000);
        } else {
            e.update.close_admission();
        }
        dispatch::start_task(&e, builder("memory-vendor", "x"), Actor::Engine)
            .await
            .unwrap();
        wait.until("memory notice", || memory_notices(&e) == 1)
            .await;
        assert_memory_status(&e, true);

        let before_recovery = e.store.last_id().unwrap();
        let feed = dvb_engine::feed::after(&e, before_recovery).unwrap();
        tokio::pin!(feed);
        assert_eq!(feed.next().await, Some(dvb_engine::feed::FeedItem::Live));
        *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }
        let (status, body) = support::http::request(&e, Method::GET, "/tasks/t", None).await;
        assert_eq!(status, StatusCode::OK);
        let view: TaskView = serde_json::from_slice(&body).unwrap();
        assert_eq!(view.reason, None);
        let cleared = EventKind::TaskReasonChanged {
            resource: Some(dvb_core::event::ResourceWait::Available),
            task: TaskId::new(TASK),
            run: RunId::new("memory-vendor"),
            reason: None,
        };
        assert!(
            e.store
                .after(before_recovery)
                .unwrap()
                .iter()
                .any(|ev| ev.kind == cleared),
            "cleared reason must invalidate live task snapshots through a task event"
        );
        assert!(matches!(
            feed.next().await,
            Some(dvb_engine::feed::FeedItem::Event(ev)) if ev.kind == cleared
        ));
        assert!(!dvb_cli::task_line(&view).contains("Waiting for memory"));
        assert_eq!(view.state, TaskState::Preparing);
        assert_eq!(e.vendors.is_limited(Vendor::Fake), vendor_hold);
        assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
        assert_memory_status(&e, false); // Also checks replay.
        assert_eq!(memory_notices(&e), 1);
        let cursor = e.store.last_id().unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(e.store.last_id().unwrap(), cursor);
    }
}

fn memory_notices(e: &AppState) -> usize {
    support::events::all(e)
        .iter()
        .filter(|ev| {
            matches!(
                &ev.kind,
                EventKind::RunOutput {
                    resource: Some(dvb_core::event::ResourceWait::Memory),
                    ..
                }
            )
        })
        .count()
}

fn assert_memory_status(e: &AppState, held: bool) {
    let view = e.proj().tasks[&TaskId::new(TASK)].view.clone();
    assert_eq!(view.reason.as_deref() == Some("Waiting for memory"), held);
    assert_eq!(
        dvb_cli::task_line(&view).contains("Waiting for memory"),
        held
    );
    let events = support::events::all(e);
    let rebuilt = dvb_engine::proj::Projections::rebuild(&events);
    assert_eq!(rebuilt.tasks[&TaskId::new(TASK)].view.reason, view.reason);
}

#[tokio::test(start_paused = true)]
async fn load_holds_task_launches_and_refreshes_resource_status() {
    for review in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let stub = Arc::new(Stub::default());
        let e = engine(
            dir.path(),
            &stub,
            if review {
                vec![
                    EventKind::BranchPublished {
                        task: TaskId::new(TASK),
                        round: 1,
                        branch: "dvb/t-1".into(),
                        head: "h1".into(),
                    },
                    moved(TaskState::Planned, TaskState::InReview),
                ]
            } else {
                vec![]
            },
        );
        *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
        let mut active = builder("active", "x");
        active.task = None;
        e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![EventKind::RunStarted(active.clone())];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        dvb_engine::launch::launch(e.clone(), active.run.clone(), String::new(), vec![], None)
            .await
            .unwrap();
        *stub.readings.load.lock().unwrap() = Some((4.01, 4));
        if review {
            dispatch::step_now(&e, &TaskId::new(TASK)).await.unwrap();
        } else {
            dispatch::start_task(&e, builder("load-builder", "x"), Actor::Engine)
                .await
                .unwrap();
        }
        wait.until("load hold", || {
            e.proj().tasks[&TaskId::new(TASK)].view.reason.as_deref()
                == Some("Waiting for host load")
        })
        .await;
        let (_, body) = support::http::request(&e, Method::GET, "/tasks/t", None).await;
        let view: TaskView = serde_json::from_slice(&body).unwrap();
        assert!(dvb_cli::task_line(&view).contains("Waiting for host load"));
        assert_eq!(
            view.state,
            if review {
                TaskState::InReview
            } else {
                TaskState::Preparing
            }
        );
        let events = support::events::all(&e);
        assert_eq!(
            dvb_engine::proj::Projections::rebuild(&events).tasks[&TaskId::new(TASK)]
                .view
                .reason,
            view.reason
        );
        let cursor = e.store.last_id().unwrap();
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(e.store.last_id().unwrap(), cursor);
        assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
        assert_eq!(stub.stops.load(Ordering::SeqCst), 0);
        assert_eq!(ended(&e, "active"), None);
        assert_eq!(stub.states.lock().unwrap()[&active.run], UnitState::Active);

        *stub.readings.memory.lock().unwrap() = Some(0);
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        // Load recovers first, leaving the existing memory hold and notice.
        *stub.readings.load.lock().unwrap() = Some((3.99, 4));
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_memory_status(&e, true);
        *stub.readings.load.lock().unwrap() = Some((4.01, 4));
        *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
        let cursor = e.store.last_id().unwrap();
        let feed = dvb_engine::feed::after(&e, cursor).unwrap();
        tokio::pin!(feed);
        assert_eq!(feed.next().await, Some(dvb_engine::feed::FeedItem::Live));
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert!(
            matches!(feed.next().await, Some(dvb_engine::feed::FeedItem::Event(ev))
            if matches!(ev.kind, EventKind::TaskReasonChanged { reason: Some(ref reason), .. } if reason == "Waiting for host load"))
        );
        // Resources recover while the vendor still holds admission: clear status.
        limit(&e, 600_000);
        *stub.readings.load.lock().unwrap() = Some((4.0, 4));
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            e.proj().tasks[&TaskId::new(TASK)].view.reason.as_deref(),
            review.then_some("code review")
        );
        assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(600)).await;
        wait.until("load-held task launch", || {
            stub.starts.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(ended(&e, "active"), None);
        assert_eq!(stub.stops.load(Ordering::SeqCst), 0);
        assert_eq!(
            support::events::all(&e)
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::RunOutput {
                        resource: Some(dvb_core::event::ResourceWait::HostLoad),
                        ..
                    }
                ))
                .count(),
            1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn load_is_rechecked_at_guarded_task_launch_after_update_admission() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
    e.update.close_admission();
    dispatch::start_task(&e, builder("load-update", "x"), Actor::Engine)
        .await
        .unwrap();
    tokio::task::yield_now().await;
    *stub.readings.load.lock().unwrap() = Some((2.1, 2));
    e.update.reopen_admission();
    wait.until("guarded load hold", || {
        e.proj().tasks[&TaskId::new(TASK)].view.reason.as_deref() == Some("Waiting for host load")
    })
    .await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    *stub.readings.load.lock().unwrap() = Some((1.9, 2));
    e.disk_freed.notify_waiters();
    wait.until("guarded load release", || state(&e) == TaskState::Running)
        .await;
    assert_eq!(e.proj().tasks[&TaskId::new(TASK)].view.reason, None);
}

#[tokio::test(start_paused = true)]
async fn memory_hold_polls_at_thirty_seconds_notices_once_and_releases_at_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    assert_eq!(e.available_memory_bytes(), Some(u64::MAX));
    *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN - 1);
    dispatch::start_task(&e, builder("memory-builder", "x"), Actor::Engine)
        .await
        .unwrap();
    wait.until("memory notice", || memory_notices(&e) == 1)
        .await;
    assert_memory_status(&e, true);
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(memory_notices(&e), 1);
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    // Exactly the minimum is sufficient; no notification, only the real poll.
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait.until("memory-held builder launch", || {
        state(&e) == TaskState::Running
    })
    .await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
    assert_eq!(memory_notices(&e), 1);
    assert_memory_status(&e, false);
}

#[tokio::test(start_paused = true)]
async fn memory_held_reviewer_releases_when_an_actual_run_in_another_project_ends() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(
        dir.path(),
        &stub,
        vec![
            EventKind::BranchPublished {
                task: TaskId::new(TASK),
                round: 1,
                branch: "dvb/t-1".into(),
                head: "h1".into(),
            },
            moved(TaskState::Planned, TaskState::InReview),
        ],
    );
    *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
    // A real unit is launched in a different project before memory falls.
    let other = ProjectId::new("other");
    e.append_checked(Some(&other), Actor::Engine, |_, _| {
        Ok(vec![support::events::project_event(
            &other, "other", "/nowhere",
        )])
    })
    .unwrap();
    let mut active = builder("other-run", "x");
    active.task = None;
    e.append_checked(Some(&other), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(active.clone())])
    })
    .unwrap();
    dvb_engine::launch::launch(e.clone(), active.run.clone(), String::new(), vec![], None)
        .await
        .unwrap();
    assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
    *stub.readings.memory.lock().unwrap() = Some(0);
    dispatch::step_now(&e, &TaskId::new(TASK)).await.unwrap();
    wait.until("review memory notice", || memory_notices(&e) == 1)
        .await;
    assert_memory_status(&e, true);
    assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
    // The launched unit actually ends; its watcher records the end and wakes
    // memory waiters, without a synthetic Notify or waiting for the poll.
    stub.end(&active.run);
    // One unit watch interval, far short of the resource poll.
    tokio::time::advance(Duration::from_millis(200)).await;
    wait.until("review launch after another project's run ends", || {
        stub.starts.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_memory_status(&e, false);
    assert_eq!(
        e.proj().tasks[&TaskId::new(TASK)].view.reason.as_deref(),
        Some("code review")
    );
    assert!(
        e.proj()
            .runs
            .values()
            .any(|r| r.assignment.reviews.as_deref() == Some("h1"))
    );
    assert_eq!(memory_notices(&e), 1);
}

#[tokio::test(start_paused = true)]
async fn memory_is_rechecked_after_update_and_a_held_builder_can_be_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
    e.update.close_admission();
    dispatch::start_task(&e, builder("memory-update", "x"), Actor::Engine)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    *stub.readings.memory.lock().unwrap() = Some(0);
    e.update.reopen_admission();
    wait.until("memory recheck", || memory_notices(&e) == 1)
        .await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    dispatch::stop(&e, &TaskId::new(TASK), Actor::Owner)
        .await
        .unwrap();
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    assert!(matches!(state(&e), TaskState::Stopped { .. }));
    assert_memory_status(&e, false);
}

#[tokio::test(start_paused = true)]
async fn freeing_memory_does_not_launch_a_paused_project_until_unpause() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(dir.path(), &stub, vec![]);
    *stub.readings.disk.lock().unwrap() = Some(u64::MAX);
    *stub.readings.memory.lock().unwrap() = Some(0);
    dispatch::start_task(&e, builder("memory-paused", "x"), Actor::Engine)
        .await
        .unwrap();
    wait.until("paused memory notice", || memory_notices(&e) == 1)
        .await;
    owner::pause(&e, &ProjectId::new("p"), true).unwrap();
    *stub.readings.memory.lock().unwrap() = Some(MEMORY_MIN);
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    owner::pause(&e, &ProjectId::new("p"), false).unwrap();
    wait.until("unpaused memory-held run", || {
        state(&e) == TaskState::Running
    })
    .await;
    assert_memory_status(&e, false);
    assert_eq!(memory_notices(&e), 1);
}

#[test]
fn historical_disk_wait_is_cleared_when_the_reviewer_reaches_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let e = engine(
        dir.path(),
        &Arc::new(Stub::default()),
        vec![
            moved(TaskState::Planned, TaskState::InReview),
            EventKind::RunStarted(reviewer("historical-review", "h1")),
            EventKind::RunOutput {
                resource: None,
                run: RunId::new("historical-review"),
                item: dvb_core::event::OutputItem::Note(
                    "waiting for disk space: 10 GB free, needs 25 GB".into(),
                ),
            },
            EventKind::RunPreflight {
                run: RunId::new("historical-review"),
                checks: vec![],
                cli_version: None,
                boundary: "fake".into(),
            },
        ],
    );
    assert_eq!(
        e.proj().tasks[&TaskId::new(TASK)].view.reason.as_deref(),
        Some("code review")
    );
    let events = support::events::all(&e);
    assert_eq!(
        dvb_engine::proj::Projections::rebuild(&events).tasks[&TaskId::new(TASK)]
            .view
            .reason
            .as_deref(),
        Some("code review")
    );
}

#[tokio::test]
async fn cargo_caches_are_shared_per_project_while_builds_stay_private() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(
        dir.path(),
        &stub,
        vec![
            EventKind::RunStarted(builder("b1", "x")),
            EventKind::RunStarted(builder("b2", "x")),
        ],
    );
    let other = ProjectId::new("other");
    e.append_checked(Some(&other), Actor::Engine, {
        let kinds = vec![
            support::events::project_event(&other, "other", "/nowhere"),
            EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(builder("b3", "x"))
                    .task(None)
                    .build(),
            ),
        ];
        move |_, _| Ok(kinds)
    })
    .unwrap();
    let mut caches = Vec::new();
    for run in ["b1", "b2", "b3"] {
        dvb_engine::launch::launch(e.clone(), RunId::new(run), "[]".into(), vec![], None)
            .await
            .unwrap();
        let run_dir = dir.path().join("work").join(run);
        let env: BTreeMap<_, _> = dvb_engine::units::read_env(&run_dir.join("env"))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            env["CARGO_TARGET_DIR"],
            run_dir.join("build").to_str().unwrap()
        );
        assert_eq!(
            env["CARGO_HOME"],
            Path::new(&env["DVB_BUILD_CACHE"])
                .join("cargo")
                .to_str()
                .unwrap()
        );
        // The engine has no RUSTUP_HOME of its own: the run's unit or the
        // project cache supplies it, so the env file must not override either.
        assert!(!env.contains_key("RUSTUP_HOME"));
        let all = std::fs::canonicalize(dir.path().join("mirrors/.cache")).unwrap();
        assert_eq!(
            mirrors_seen(dir.path(), run)
                .into_iter()
                .filter(|seen| seen.starts_with(&all))
                .collect::<Vec<_>>(),
            [std::fs::canonicalize(&env["DVB_BUILD_CACHE"]).unwrap()]
        );
        assert!(!Path::new(&env["DVB_BUILD_CACHE"]).starts_with(dir.path().join("state")));
        assert!(!env.contains_key("RUSTC_WRAPPER"));
        assert!(!env.contains_key("CARGO_INCREMENTAL"));
        let bin = run_dir.join("tools");
        std::fs::create_dir_all(&bin).unwrap();
        let sccache = bin.join("sccache");
        std::fs::write(&sccache, "#!/bin/sh\nexec \"$@\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sccache, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for path in [std::ffi::OsStr::new("/missing"), bin.as_os_str()] {
            let mut probe = std::process::Command::new("/usr/bin/env");
            probe
                .env_clear()
                .envs(&env)
                .env("HOME", run_dir.join("home"))
                .env("PATH", path);
            dvb_run::prepare_build_cache(
                &mut probe,
                Path::new(&env["DVB_BUILD_CACHE"]),
                &run_dir,
                path,
                None,
            )
            .unwrap();
            let output = probe.output().unwrap();
            assert!(output.status.success());
            let usable: BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            assert_eq!(usable["HOME"], run_dir.join("home").to_str().unwrap());
            assert_eq!(usable["CARGO_TARGET_DIR"], env["CARGO_TARGET_DIR"]);
            assert_eq!(usable["CARGO_HOME"], env["CARGO_HOME"]);
            assert_eq!(
                usable["RUSTUP_HOME"],
                Path::new(&env["DVB_BUILD_CACHE"])
                    .join("rustup")
                    .to_str()
                    .unwrap()
            );
            if path == bin.as_os_str() {
                assert_eq!(usable["RUSTC_WRAPPER"], sccache.to_str().unwrap());
                assert_eq!(
                    usable["SCCACHE_DIR"],
                    Path::new(&env["DVB_BUILD_CACHE"])
                        .join("sccache")
                        .to_str()
                        .unwrap()
                );
                assert_eq!(usable["CARGO_INCREMENTAL"], "0");
                assert_eq!(
                    usable["SCCACHE_SERVER_UDS"],
                    run_dir.join("build/sccache.sock").to_str().unwrap()
                );
            } else {
                assert!(!usable.contains_key("RUSTC_WRAPPER"));
                assert!(!usable.contains_key("CARGO_INCREMENTAL"));
                assert!(!usable.contains_key("SCCACHE_DIR"));
            }
        }
        let cargo = Path::new(&env["CARGO_HOME"]);
        std::fs::write(cargo.join("writable"), run).unwrap();
        std::fs::create_dir_all(cargo.join("registry")).unwrap();
        std::fs::create_dir_all(cargo.join("git")).unwrap();
        for entry in [
            cargo.join("registry/reused"),
            cargo.join("git/reused"),
            Path::new(&env["DVB_BUILD_CACHE"]).join("sccache/reused"),
        ] {
            if run == "b2" {
                assert_eq!(std::fs::read_to_string(&entry).unwrap(), "b1");
            } else {
                assert!(!entry.exists());
                std::fs::write(entry, run).unwrap();
            }
        }
        std::fs::create_dir_all(run_dir.join("home")).unwrap();
        assert!(!run_dir.join("home/private").exists());
        std::fs::write(run_dir.join("home/private"), run).unwrap();
        std::fs::create_dir_all(run_dir.join("build")).unwrap();
        assert!(!run_dir.join("build/private").exists());
        std::fs::write(run_dir.join("build/private"), run).unwrap();
        caches.push(env["CARGO_HOME"].clone());
    }
    assert_eq!(caches[0], caches[1]);
    assert_ne!(caches[0], caches[2]);
}

/// What of `mirrors/` a run's unit shows: `10-common.conf` hides it with a
/// TemporaryFileSystem, and each bind destination under it, its symlinks
/// followed as systemd follows them, mounts one directory back.
fn mirrors_seen(root: &Path, run: &str) -> Vec<std::path::PathBuf> {
    let unit = include_str!("../../../install/dvb-run-.service.d/10-common.conf");
    let paths = |key: &'static str| {
        unit.lines()
            .filter_map(move |line| line.strip_prefix(key)?.strip_prefix('='))
            .flat_map(str::split_whitespace)
            .map(move |value| {
                let mut parts = value.trim_start_matches('-').split(':');
                let source = parts.next().unwrap_or_default();
                let target = if key == "TemporaryFileSystem" {
                    source
                } else {
                    parts.next().unwrap_or(source)
                };
                let local = target
                    .replace("/var/lib/dvb", root.to_str().unwrap())
                    .replace("%i", run);
                std::fs::canonicalize(&local).unwrap_or_else(|_| local.into())
            })
    };
    let mirrors = std::fs::canonicalize(root.join("mirrors")).unwrap();
    assert!(paths("TemporaryFileSystem").any(|hidden| hidden == mirrors));
    paths("BindPaths")
        .chain(paths("BindReadOnlyPaths"))
        .filter(|target| target.starts_with(&mirrors))
        .collect()
}

#[tokio::test]
async fn a_run_sees_only_its_own_projects_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub::default());
    let e = engine(
        dir.path(),
        &stub,
        vec![EventKind::RunStarted(builder("b1", "x"))],
    );
    let other = ProjectId::new("other");
    e.append_checked(Some(&other), Actor::Engine, {
        let kinds = vec![
            support::events::project_event(&other, "other", "/nowhere"),
            EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(builder("b2", "x"))
                    .task(None)
                    .build(),
            ),
        ];
        move |_, _| Ok(kinds)
    })
    .unwrap();
    let mut mirrors = Vec::new();
    for run in ["b1", "b2"] {
        dvb_engine::launch::launch(e.clone(), RunId::new(run), "[]".into(), vec![], None)
            .await
            .unwrap();
        let env: BTreeMap<_, _> =
            dvb_engine::units::read_env(&dir.path().join("work").join(run).join("env"))
                .unwrap()
                .into_iter()
                .collect();
        std::fs::create_dir_all(&env["DVB_MIRROR"]).unwrap();
        mirrors.push(std::fs::canonicalize(&env["DVB_MIRROR"]).unwrap());
        let cache = std::fs::canonicalize(&env["DVB_BUILD_CACHE"]).unwrap();
        assert_eq!(
            mirrors_seen(dir.path(), run),
            [cache, mirrors.last().unwrap().clone()]
        );
    }
    assert_ne!(mirrors[0], mirrors[1]);
}

#[tokio::test]
async fn an_engine_rustup_home_is_passed_on_to_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::dev(dir.path(), "dvb".into());
    cfg.rustup_home = Some("/opt/rust/rustup".into());
    let e = engine_config(
        cfg,
        &Arc::new(Stub::default()),
        vec![EventKind::RunStarted(builder("b1", "x"))],
    );
    dvb_engine::launch::launch(e.clone(), RunId::new("b1"), "[]".into(), vec![], None)
        .await
        .unwrap();
    let run_dir = dir.path().join("work/b1");
    let env: BTreeMap<_, _> = dvb_engine::units::read_env(&run_dir.join("env"))
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(env["RUSTUP_HOME"], "/opt/rust/rustup");
}

#[test]
fn existing_installed_layout_gains_a_writable_persistent_cache_without_opening_state() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    for (name, mode) in [("state", 0o700), ("mirrors", 0o2750), ("work", 0o2770)] {
        let path = dir.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let mut cfg = Config::dev(dir.path(), "dvb".into());
    cfg.dev = false;
    let stub: Arc<dyn Launcher> = Arc::new(Stub::default());
    let e = Engine::open(
        cfg.clone(),
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let cache = dir.path().join("mirrors/.cache");
    assert_eq!(
        std::fs::metadata(&cache).unwrap().permissions().mode() & 0o7777,
        0o2770
    );
    std::fs::write(cache.join("persistent"), "cached").unwrap();
    drop(e);
    let _reopened = Engine::open(
        cfg,
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(stub),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(cache.join("persistent")).unwrap(),
        "cached"
    );
    for (name, mode) in [("state", 0o700), ("mirrors", 0o2750), ("work", 0o2770)] {
        assert_eq!(
            std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode
        );
    }
}
