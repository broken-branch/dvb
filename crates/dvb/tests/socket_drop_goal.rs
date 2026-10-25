//! `dvb goal drop` in dev mode, through the real binary: the goal's planned task
//! is cancelled and its open question leaves Needs you. After an engine restart
//! the goal is still dropped, nothing is open or running, and the orchestrator
//! is not woken for it.

pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{GoalState, GoalView, QuestionView, TaskView};
use dvb_core::{Event, EventKind, TaskState};
use std::process::Command;
use std::time::Duration;
use support::{Engine, add_project, engine_cmd, engine_up, seed_origin, state_goal, until};

/// Runs started and runs ended so far.
async fn runs(c: &Client) -> (usize, usize) {
    let (mut started, mut ended) = (0, 0);
    c.read_events(0, true, |ev: Event| {
        started += usize::from(matches!(ev.kind, EventKind::RunStarted(_)));
        ended += usize::from(matches!(ev.kind, EventKind::RunEnded { .. }));
        true
    })
    .await
    .unwrap();
    (started, ended)
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_dropped_goal_stays_dropped_after_a_restart() {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");
    let dir = d.join("e");

    let engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let p = add_project(&c, &origin).await;
    let goal = state_goal(&c, &p, "Add a note").await;
    until("the plan's question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().next()
    })
    .await;
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::Planned);
    until("the orchestrator run to end", async || {
        let (started, ended) = runs(&c).await;
        (started > 0 && started == ended).then_some(())
    })
    .await;
    let before = runs(&c).await;

    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(&dir)
        .args(["goal", "drop", goal.id.as_str()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    // Restart the engine.
    drop(engine);
    let _engine = Engine::spawn(engine_cmd(&dir));
    engine_up(&c).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let goals: Vec<GoalView> = c.get(&format!("/goals?project={}", p.id)).await.unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, GoalState::Dropped);
    let open: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert!(open.is_empty(), "{open:?}");
    let task: TaskView = c.get(&format!("/tasks/{}", tasks[0].id)).await.unwrap();
    assert_eq!(task.state, TaskState::Cancelled);
    // Nothing running, and the orchestrator was not woken for the goal.
    assert_eq!(runs(&c).await, before);
}
