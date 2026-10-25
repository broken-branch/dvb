//! The project brief lives only in the event store: set and read through the
//! API and in the orchestrator and builder prompts.
mod support;

use axum::http::{Method, StatusCode};
use dvb_core::{Actor, EventKind, GoalId, ProjectId, RoleId, RunAssignment, RunId, TaskId};
use dvb_engine::prompt::render;
use dvb_engine::{AppState, prompt};
use serde_json::json;
use support::events::{backend_plan_event, goal_event, project_event};
use support::http::call as send;

const BRIEF: &str = "Zebra-quartz-7731: the owner's private context";

fn engine(options: dvb_engine::EngineOptions) -> (tempfile::TempDir, AppState, ProjectId) {
    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(dir.path(), "dvb", options);
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            project_event(&p, "fieldnotes", "org/repo"),
            goal_event(&GoalId::new("g"), "Ship it", None),
            backend_plan_event(&GoalId::new("g"), &TaskId::new("t")),
        ])
    })
    .unwrap();
    (dir, e, p)
}

fn assignment(run: &str, task: Option<&str>, role: &str) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .task(task.map(TaskId::new))
        .role(RoleId::new(role))
        .reason(String::new())
        .build()
}

async fn rendered(e: &AppState, p: &ProjectId, run: &str, template: &str) -> String {
    let (r, proj) = {
        let g = e.proj();
        (g.runs[&RunId::new(run)].clone(), g.projects[p].clone())
    };
    let mut vars = prompt::context(e, &r, &proj).await.unwrap();
    vars.push(("project_name", proj.view.name.clone()));
    render(template, &vars, false)
}

#[tokio::test]
async fn the_brief_is_set_and_read_through_the_api_and_replaces_the_last() {
    let (_d, e, p) = engine(Default::default());
    let path = format!("/projects/{p}/brief");
    assert_eq!(
        send(&e, Method::GET, &path, None).await.1,
        json!({"text": ""})
    );
    let (status, body) = send(&e, Method::PUT, &path, Some(json!({"text": BRIEF}))).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"text": BRIEF})));
    assert_eq!(
        send(&e, Method::GET, &path, None).await.1,
        json!({"text": BRIEF})
    );
    send(&e, Method::PUT, &path, Some(json!({"text": "second"}))).await;
    assert_eq!(
        send(&e, Method::GET, &path, None).await.1,
        json!({"text": "second"})
    );
    let unknown = send(
        &e,
        Method::PUT,
        "/projects/nope/brief",
        Some(json!({"text": "x"})),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::CONFLICT);
    assert_eq!(unknown.1["code"], "PROJECT_UNKNOWN");
}

#[tokio::test]
async fn the_brief_survives_a_restart() {
    let (d, e, p) = engine(Default::default());
    send(
        &e,
        Method::PUT,
        &format!("/projects/{p}/brief"),
        Some(json!({"text": BRIEF})),
    )
    .await;
    drop(e);
    let e = support::engine::open(d.path());
    assert_eq!(e.proj().projects[&p].brief, BRIEF);
}

#[tokio::test]
async fn both_the_orchestrator_and_every_builder_prompt_contain_the_brief() {
    let (_d, e, p) = engine(Default::default());
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
            EventKind::RunStarted(assignment("o1", None, "orchestrator")),
            EventKind::RunStarted(assignment("b1", Some("t"), "backend")),
            EventKind::RunStarted(assignment("b2", Some("t"), "frontend")),
            EventKind::RunStarted(assignment("b3", Some("t"), "clerk")),
        ])
    })
    .unwrap();
    let orchestrator = include_str!("../../../prompts/orchestrator.md");
    let builder = include_str!("../../../prompts/builder.md");
    let clerk = include_str!("../../../prompts/clerk.md");
    assert!(!rendered(&e, &p, "o1", orchestrator).await.contains(BRIEF));
    send(
        &e,
        Method::PUT,
        &format!("/projects/{p}/brief"),
        Some(json!({"text": BRIEF})),
    )
    .await;
    assert!(rendered(&e, &p, "o1", orchestrator).await.contains(BRIEF));
    for run in ["b1", "b2"] {
        assert!(
            rendered(&e, &p, run, builder).await.contains(BRIEF),
            "{run}"
        );
    }
    assert!(rendered(&e, &p, "b3", clerk).await.contains(BRIEF), "clerk");
    // Changing the brief changes the next prompt.
    send(
        &e,
        Method::PUT,
        &format!("/projects/{p}/brief"),
        Some(json!({"text": ""})),
    )
    .await;
    assert!(!rendered(&e, &p, "b1", builder).await.contains(BRIEF));
}
