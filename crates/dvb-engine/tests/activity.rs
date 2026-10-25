//! Seen receipts and durable compatibility of the removed GitHub activity events.

mod support;

use axum::http::{Method, StatusCode};
use dvb_core::{Actor, EventKind, ProjectId};
use dvb_engine::AppState;
use serde_json::json;
use support::events::project_event;
use support::http::call;

fn engine() -> (tempfile::TempDir, AppState) {
    let (dir, e) = support::engine::engine();
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![project_event(&p, "p", "org/p")])
    })
    .unwrap();
    (dir, e)
}

#[tokio::test]
async fn seen_appends_project_seen_and_refuses_an_unknown_project() {
    let (_dir, e) = engine();
    let before = e.store.last_id().unwrap();
    let (s, _) = call(&e, Method::POST, "/projects/p/seen", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let events = e.store.after(before).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::ProjectSeen {});

    let before = e.store.last_id().unwrap();
    let (s, r) = call(&e, Method::POST, "/projects/nope/seen", None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(r["code"], "PROJECT_UNKNOWN");
    assert!(
        e.store
            .after(before)
            .unwrap()
            .iter()
            .all(|ev| !matches!(ev.kind, EventKind::ProjectSeen {}))
    );
}

#[tokio::test]
async fn literal_historical_activity_events_replay_without_rewriting_storage() {
    const SETTINGS: &str = r#"{"key":"github_activity","old":true,"new":false}"#;
    const REFUSAL: &str = r#"{"command":"github_activity","code":"GITHUB_ACTIVITY_OFF","detail":"the github_activity setting is off"}"#;
    let (dir, e) = engine();
    drop(e);
    let path = dir.path().join("state/dvb.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    for (kind, payload) in [("settings.changed", SETTINGS), ("command.refused", REFUSAL)] {
        db.execute(r#"INSERT INTO events (at, actor, project, kind, payload) VALUES ('1', '"owner"', NULL, ?1, ?2)"#, [kind, payload]).unwrap();
    }
    let before: Vec<(i64, String, String)> = db
        .prepare("SELECT id, kind, payload FROM events ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let e = support::engine::open(dir.path());
    let stored = support::events::all(&e);
    for (kind, payload) in [("settings.changed", SETTINGS), ("command.refused", REFUSAL)] {
        let event = stored.iter().find(|e| e.kind.kind() == kind).unwrap();
        let wire = serde_json::to_value(&event.kind).unwrap();
        let expected: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(wire, json!({"kind":kind,"payload":expected}));
    }
    let (_, settings) = call(&e, Method::GET, "/settings", None).await;
    assert!(settings["values"].get("github_activity").is_none());
    for (id, kind, payload) in before {
        let row: (String, String) = db
            .query_row(
                "SELECT kind, payload FROM events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (kind, payload));
    }
}
