//! Bounded event reads use the Fake dev engine, without provider or forge calls.
mod support;
use axum::http::{Method, StatusCode};
use dvb_core::api::EventPage;
use dvb_core::event::OutputItem;
use dvb_core::{Actor, EventKind, GoalId, ProjectId, RunId, TaskId, TaskState};
use dvb_engine::AppState;
use support::http::call;

async fn page(e: &AppState, query: &str) -> EventPage {
    let (status, value) = call(e, Method::GET, &format!("/events/recent?{query}"), None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn bounded_pages_keep_a_cursor_and_do_not_repeat_concurrent_appends() {
    let (_dir, e) = support::engine::approved();
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok((0..1000)
            .map(|i| EventKind::RunOutput {
                resource: None,
                run: RunId::new("r"),
                item: OutputItem::Text(format!("line {i}")),
            })
            .collect())
    })
    .unwrap();
    let first = page(&e, "project=p&limit=7&summary=true").await;
    assert_eq!(first.events.len(), 7);
    assert_eq!(first.cursor, e.store.last_id().unwrap());
    assert_eq!(first.activity[0].fresh, 0);
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    let older = page(
        &e,
        &format!("project=p&limit=7&before={}", first.before.unwrap()),
    )
    .await;
    assert_eq!(older.events.len(), 7);
    assert!(
        older
            .events
            .iter()
            .all(|a| first.events.iter().all(|b| a.id < b.id))
    );
    assert_eq!(page(&e, "limit=999999").await.events.len(), 200);
    assert!(page(&e, "project=other").await.events.is_empty());
    let kinds = page(&e, "kinds=goal.stated,plan.proposed").await;
    assert_eq!(kinds.events.len(), 2);
    assert!(kinds.before.is_none());
}

#[tokio::test]
async fn summaries_count_failed_goals_once_and_ignore_output_beyond_the_page() {
    let (_dir, e) = support::engine::approved();
    let p = ProjectId::new("p");
    let t = TaskId::new("t");
    let fail = || {
        vec![
            EventKind::TaskStateChanged {
                task: t.clone(),
                from: TaskState::Planned,
                to: TaskState::Failed {
                    reason: "failure".into(),
                },
            },
            EventKind::TaskFailed {
                task: t.clone(),
                reason: Some("failure".into()),
            },
        ]
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(fail()))
        .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Failed {
                reason: "failure".into(),
            },
            to: TaskState::Failed {
                reason: "still failed".into(),
            },
        }])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok((0..100)
            .map(|_| EventKind::RunOutput {
                resource: None,
                run: RunId::new("r"),
                item: OutputItem::Text("output".into()),
            })
            .collect())
    })
    .unwrap();
    let a = page(&e, "limit=1&summary=true").await;
    assert_eq!(a.activity[0].fresh, 2); // task failure + goal failure
    assert_eq!(a.activity[0].latest, a.cursor);
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectSeen {}])
    })
    .unwrap();
    assert_eq!(page(&e, "summary=true").await.activity[0].fresh, 0);
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Failed {
                reason: "failure".into(),
            },
            to: TaskState::Running,
        }])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(fail()))
        .unwrap();
    assert_eq!(page(&e, "summary=true").await.activity[0].fresh, 2);
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalDropped {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    assert_eq!(page(&e, "summary=true").await.activity[0].fresh, 2);
}

#[tokio::test]
async fn task_context_includes_run_decisions_and_latest_evidence_and_goal_order() {
    let (_dir, e) = support::engine::approved();
    let p = ProjectId::new("p");
    let kind = |value| serde_json::from_value::<EventKind>(value).unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(vec![
        kind(serde_json::json!({"kind":"run.started","payload":{"run":"r","task":"t","role":"frontend","hire":"h","model":"fake-1","vendor":"fake","effort":"medium","reason":"Build"}})),
        kind(serde_json::json!({"kind":"checks.observed","payload":{"task":"t","head":"one","status":"failed","names":["old check"]}})),
        kind(serde_json::json!({"kind":"checks.observed","payload":{"task":"t","head":"two","status":"success","names":["current check"]}})),
        EventKind::RunOutput { resource: None,
run: RunId::new("r"), item: OutputItem::Text("task output".into()) },
        EventKind::RunOutput { resource: None,
run: RunId::new("foreign"), item: OutputItem::Text("foreign output".into()) },
    ])).unwrap();
    e.append_checked(Some(&p), Actor::Run(RunId::new("r")), |_, _| Ok(vec![kind(serde_json::json!({"kind":"decision.recorded","payload":{"by":"frontend","title":"Task decision","choice":"One","reason":"Simple"}}))])).unwrap();
    let context = page(&e, "task=t").await;
    assert_eq!(context.events.len(), 6);
    assert!(
        context
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::DecisionRecorded { .. }))
    );
    assert!(
        !context
            .events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::RunOutput { run, .. } if run.0 == "foreign"))
    );
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(vec![kind(serde_json::json!({"kind":"run.started","payload":{"run":"review","task":"t","role":"design-reviewer","hire":"h","model":"fake-1","vendor":"fake","effort":"medium","reason":"Review","reviews":"two"}}))])).unwrap();
    assert_eq!(
        page(&e, "kinds=run.started&latest=true").await.events.len(),
        2
    );
    let latest = page(&e, "kinds=checks.observed&latest=true").await;
    assert_eq!(latest.events.len(), 1);
    assert!(
        matches!(&latest.events[0].kind, EventKind::ChecksObserved { head, .. } if head == "two")
    );
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            support::events::goal_event(&GoalId::new("z"), "Earlier", None),
            support::events::goal_event(&GoalId::new("a"), "Later", None),
        ])
    })
    .unwrap();
    let summary = page(&e, "summary=true").await;
    assert_eq!(
        summary.activity[0].goals,
        [GoalId::new("g"), GoalId::new("z"), GoalId::new("a")]
    );
}

#[tokio::test]
async fn role_activity_includes_old_runs_tasks_and_decisions_outside_global_page() {
    let (_dir, e) = support::engine::approved();
    let p = ProjectId::new("p");
    let kind = |v| serde_json::from_value::<EventKind>(v).unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(vec![
        kind(serde_json::json!({"kind":"run.started","payload":{"run":"old","task":"t","role":"backend","hire":"h","model":"fake-1","vendor":"fake","effort":"medium","reason":"Build"}})),
        EventKind::RunOutput { resource: None,
run: RunId::new("old"), item: OutputItem::Text("older activity".into()) },
        EventKind::TaskDone { task: TaskId::new("t"), reason: None },
    ])).unwrap();
    e.append_checked(Some(&p), Actor::Run(RunId::new("old")), |_, _| Ok(vec![kind(serde_json::json!({"kind":"decision.recorded","payload":{"by":"backend","title":"Old decision","choice":"One","reason":"Simple"}}))])).unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok((0..300)
            .map(|_| EventKind::RunOutput {
                resource: None,
                run: RunId::new("other"),
                item: OutputItem::Text("other role".into()),
            })
            .collect())
    })
    .unwrap();
    assert!(
        page(&e, "limit=12")
            .await
            .events
            .iter()
            .all(|e| matches!(e.kind, EventKind::RunOutput { .. }))
    );
    let scoped = page(&e, "project=p&role=backend&limit=12").await;
    assert!(
        scoped
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::RunStarted { .. }))
    );
    assert!(
        scoped
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::TaskDone { .. }))
    );
    assert!(
        scoped
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::DecisionRecorded { .. }))
    );
    assert!(
        !scoped
            .events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::RunOutput { run, .. } if run.0 == "other"))
    );
}
