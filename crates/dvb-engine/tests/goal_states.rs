//! A goal's state follows its tasks, the same in `GET /goals` and `dvb status`,
//! and each goal is listed once.

mod support;
use axum::body::Body;
use axum::http::Request;
use dvb_core::api::{GoalState, GoalView};
use dvb_core::event::PlannedTask;
use dvb_core::roles::ReviewKind;
use dvb_core::{Actor, EventKind, GoalId, ProjectId, RoleId, TaskId, TaskState};
use dvb_engine::api;
use support::events::{goal_event, project_event};
use tower::ServiceExt;

fn plan(goal: &GoalId, tasks: &[&str]) -> EventKind {
    EventKind::PlanProposed {
        goal: goal.clone(),
        tasks: tasks
            .iter()
            .map(|t| PlannedTask {
                after: vec![],
                task: TaskId::new(*t),
                title: (*t).into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "x".into(),
                routine: false,
            })
            .collect(),
    }
}

fn state(task: &str, from: TaskState, to: TaskState) -> EventKind {
    EventKind::TaskStateChanged {
        task: TaskId::new(task),
        from,
        to,
    }
}

#[tokio::test]
async fn goal_state_follows_its_tasks_in_the_api_and_in_status() {
    let (_dir, e) = support::engine::engine();
    let p = ProjectId::new("p");
    let goal = |n: &str| GoalId::new(n);
    let mut kinds = vec![project_event(&p, "p", "/nowhere")];
    // running: one task in review, one planned; failed: one done, one failed;
    // done: all merged (one cancelled task is ignored); dropped.
    for (g, tasks) in [
        ("running", &["r1", "r2"][..]),
        ("failed", &["f1", "f2"]),
        ("done", &["d1", "d2"]),
        ("dropped", &["x1"]),
        ("fresh", &["n1"]),
    ] {
        kinds.push(goal_event(&goal(g), g, None));
        kinds.push(plan(&goal(g), tasks));
        if g != "fresh" {
            kinds.push(EventKind::GoalApproved { goal: goal(g) });
        }
    }
    kinds.push(state("r1", TaskState::Planned, TaskState::InReview));
    kinds.push(state("f1", TaskState::Planned, TaskState::Done));
    kinds.push(state(
        "f2",
        TaskState::Planned,
        TaskState::Failed {
            reason: "boom".into(),
        },
    ));
    kinds.push(state("d1", TaskState::Planned, TaskState::Done));
    kinds.push(state("d2", TaskState::Planned, TaskState::Done));
    kinds.push(EventKind::GoalDropped {
        goal: goal("dropped"),
    });
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(kinds))
        .unwrap();

    let res = api::router(e.clone())
        .oneshot(
            Request::builder()
                .uri("/goals?project=p")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 16)
        .await
        .unwrap();
    let goals: Vec<GoalView> = serde_json::from_slice(&body).unwrap();
    assert_eq!(goals.len(), 5, "each goal once");
    let got = |t: &str| goals.iter().find(|g| g.text == t).unwrap();
    assert_eq!(got("running").state, GoalState::InProgress);
    assert_eq!(got("failed").state, GoalState::Failed);
    assert_eq!(got("done").state, GoalState::Done);
    assert_eq!(got("dropped").state, GoalState::Dropped);
    assert_eq!(got("fresh").state, GoalState::Stated);
    assert_eq!(
        dvb_cli::goal_line(got("running")),
        "Goal running: running - In progress"
    );
    assert_eq!(
        dvb_cli::goal_line(got("failed")),
        "Goal failed: failed - Some tasks failed or were stopped"
    );
    assert_eq!(dvb_cli::goal_line(got("done")), "Goal done: done - Done");
}
