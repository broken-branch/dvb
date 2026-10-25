//! Goal redirection in dev mode, through the real binary: after `dvb redirect` ("change
//! course"), the old plan's unstarted task is cancelled, the question about the
//! old plan has lapsed, the new plan appears as one question, no task of it
//! starts before the yes, and after the yes it runs to the merge.

pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::{GoalView, QuestionView, TaskView};
use dvb_core::event::Answer;
use dvb_core::{Event, EventKind, RefusalCode, TaskState};
use std::process::Command;
use support::{
    Engine, add_project, approve, engine_cmd, engine_up, git, seed_origin, state_goal, until,
};

#[tokio::test(flavor = "multi_thread")]
async fn socket_change_course_cancels_the_old_plan_and_asks_again() {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");
    let seeded = git(&origin, &["rev-parse", "main"]);

    let dir = d.join("e");
    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let p = add_project(&c, &origin).await;
    let goal = state_goal(&c, &p, "Add a note").await;
    let old_q = until("the first plan's question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().next()
    })
    .await;
    let old_tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    assert_eq!(old_tasks.len(), 1);

    // Change course from the command line.
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(&dir)
        .args([
            "redirect",
            goal.id.as_str(),
            "Add",
            "a",
            "different",
            "note",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    // The old plan's unstarted task is cancelled; its question has lapsed.
    let old: TaskView = c.get(&format!("/tasks/{}", old_tasks[0].id)).await.unwrap();
    assert_eq!(old.state, TaskState::Cancelled);
    let yes = old_q.options.iter().find(|o| o.approves_plan).unwrap().id;
    match c
        .post::<_, QuestionView>(
            &format!("/questions/{}/answer", old_q.id),
            &Answer::Choice(yes),
        )
        .await
    {
        Err(CliError::Refused(r)) => assert_eq!(r.code, RefusalCode::QuestionLapsed),
        other => panic!("an answer to the old plan: {other:?}"),
    }

    // The new plan is one question; nothing of it starts before the yes.
    let new_q = until("the new plan's question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().find(|q| q.id != old_q.id)
    })
    .await;
    let open: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    let new_task = tasks
        .iter()
        .find(|t| t.id != old_tasks[0].id)
        .unwrap()
        .id
        .clone();
    let t: TaskView = c.get(&format!("/tasks/{new_task}")).await.unwrap();
    assert_eq!(t.state, TaskState::Planned);

    approve(&c, &new_q).await;
    until("the new task to be done", async || {
        let t: TaskView = c.get(&format!("/tasks/{new_task}")).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    until("the goal to be done", async || {
        let mut done = false;
        c.read_events(0, true, |ev: Event| {
            done |= matches!(ev.kind, EventKind::GoalDone { .. });
            true
        })
        .await
        .ok()?;
        done.then_some(())
    })
    .await;

    let mut refused_starts = 0;
    let mut cancelled = None;
    c.read_events(0, true, |ev: Event| {
        if let EventKind::CommandRefused(r) = &ev.kind
            && r.refusal.current().unwrap().code == RefusalCode::GoalNotApproved
        {
            refused_starts += 1;
        }
        if let EventKind::PlanCancelled { tasks, .. } = &ev.kind {
            cancelled = Some(tasks.clone());
        }
        true
    })
    .await
    .unwrap();
    // The fake orchestrator tries to start each plan before the yes.
    assert_eq!(refused_starts, 2);
    assert_eq!(cancelled, Some(vec![old_tasks[0].id.clone()]));
    assert_eq!(
        git(
            &origin,
            &["rev-list", "--count", &format!("{seeded}..main")]
        ),
        "1"
    );
}

/// Redirect while the first orchestrator run is still planning: its late plan
/// and question are for the old goal and are refused; only the new plan counts.
#[tokio::test(flavor = "multi_thread")]
async fn socket_a_plan_still_being_made_for_the_old_goal_is_refused() {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");

    // The orchestrator thinks for a while before it plans.
    let dir = d.join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    let script = include_str!("../../dvb-engine/src/fake_scripts/orchestrator-goal.json").replacen(
        '[',
        r#"[{"sleep_ms": 1500},"#,
        1,
    );
    std::fs::write(dir.join("fake/orchestrator-goal.json"), script).unwrap();
    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let p = add_project(&c, &origin).await;
    let goal = state_goal(&c, &p, "OLD GOAL").await;
    // The first orchestrator run is planning; change course now.
    until("the first orchestrator run", async || {
        let mut started = false;
        c.read_events(0, true, |ev: Event| {
            started |= matches!(ev.kind, EventKind::RunStarted(_));
            true
        })
        .await
        .ok()?;
        started.then_some(())
    })
    .await;
    let _: GoalView = c
        .post(
            &format!("/goals/{}/redirect", goal.id),
            &dvb_core::api::Redirect {
                text: "NEW GOAL".into(),
            },
        )
        .await
        .unwrap();

    // One plan, for the new goal, and its one question.
    let q = until("the new plan's question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().next()
    })
    .await;
    assert!(q.subject.contains("NEW GOAL"), "{}", q.subject);
    // Both runs have finished: the old one's plan never landed.
    until("both orchestrator runs to end", async || {
        let mut started = 0;
        let mut ended = 0;
        c.read_events(0, true, |ev: Event| {
            started += usize::from(matches!(ev.kind, EventKind::RunStarted(_)));
            ended += usize::from(matches!(ev.kind, EventKind::RunEnded { .. }));
            true
        })
        .await
        .ok()?;
        (started == 2 && ended == 2).then_some(())
    })
    .await;
    let mut plans = Vec::new();
    c.read_events(0, true, |ev: Event| {
        if let EventKind::PlanProposed { tasks, .. } = &ev.kind {
            plans.extend(tasks.iter().map(|t| t.title.clone()));
        }
        true
    })
    .await
    .unwrap();
    assert_eq!(plans, ["NEW GOAL"]);
    let open: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
}
