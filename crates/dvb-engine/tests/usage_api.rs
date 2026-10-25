//! `GET /usage`, `GET /runs/{id}` and `GET /runs/{id}/output` over the event log.

mod support;

use axum::body::Body;
use axum::http::Request;
use dvb_core::api::{RunOutputLine, RunView, Usage};
use dvb_core::event::OutputItem;
use dvb_core::{Actor, Effort, EventId, EventKind, ProjectId, RunId, Vendor};
use dvb_engine::{AppState, api};
use serde::de::DeserializeOwned;
use tower::ServiceExt;

fn engine_with_a_run() -> (tempfile::TempDir, AppState) {
    let (dir, e) = support::engine::engine();
    let p = ProjectId::new("p");
    let run = RunId::new("r1");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(run.clone())
                    .model("claude-sonnet-5-5".into())
                    .vendor(Vendor::Claude)
                    .effort(Effort::High)
                    .build(),
            ),
            EventKind::RunOutput {
                resource: None,
                run: run.clone(),
                item: OutputItem::Text("hello".into()),
            },
            EventKind::RunUsage {
                run: run.clone(),
                uncached_in: 1_000_000,
                cached_in: 0,
                out: 0,
                est_cost: 3.0,
            },
            EventKind::RunUsage {
                run,
                uncached_in: 500_000,
                cached_in: 0,
                out: 0,
                est_cost: 1.5,
            },
        ])
    })
    .unwrap();
    (dir, e)
}

async fn get<T: DeserializeOwned>(e: &AppState, path: &str) -> T {
    let res = api::router(e.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(res.status().is_success(), "{path}: {}", res.status());
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn usage_sums_a_runs_events_and_filters_by_project() {
    let (_dir, e) = engine_with_a_run();
    let u: Usage = get(&e, "/usage").await;
    assert_eq!(u.rows.len(), 1);
    assert_eq!(u.rows[0].uncached_in, 1_500_000);
    assert!((u.rows[0].est_cost - 4.5).abs() < 1e-9);
    let none: Usage = get(&e, "/usage?project=other").await;
    assert!(none.rows.is_empty());
}

#[tokio::test]
async fn usage_output_and_notes_reads_never_decode_other_runs_lines() {
    let (_dir, e) = engine_with_a_run();
    // Decoding every line runs ever wrote held most of the engine's memory.
    rusqlite::Connection::open(e.cfg.paths.state.join("dvb.sqlite"))
        .unwrap()
        .execute(
            r#"INSERT INTO events (at, actor, project, kind, payload)
               VALUES ('1', '"engine"', 'p', 'run.output', '{"item":{"text":1},"run":"r2"}')"#,
            [],
        )
        .unwrap();
    assert!(e.store.after(EventId(0)).is_err());
    let u: Usage = get(&e, "/usage").await;
    assert_eq!(u.rows[0].uncached_in, 1_500_000);
    let lines: Vec<RunOutputLine> = get(&e, "/runs/r1/output").await;
    assert_eq!(lines.len(), 1);
    e.sync_notes(&ProjectId::new("p")).unwrap();
}

#[tokio::test]
async fn a_run_and_its_output_are_served() {
    let (_dir, e) = engine_with_a_run();
    let r: RunView = get(&e, "/runs/r1").await;
    assert_eq!(r.model, "claude-sonnet-5-5");
    assert_eq!(r.ended, None);
    let lines: Vec<RunOutputLine> = get(&e, "/runs/r1/output").await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].item, OutputItem::Text("hello".into()));
    let after: Vec<RunOutputLine> =
        get(&e, &format!("/runs/r1/output?after={}", lines[0].id)).await;
    assert!(after.is_empty());
}

#[tokio::test]
async fn the_usage_view_carries_the_vendor_windows() {
    let (_dir, e) = engine_with_a_run();
    let body = dvb_core::run_verbs::RateLimitBody {
        used_pct: 67.0,
        window: "7d".into(),
        resets_at: None,
    };
    dvb_engine::vendors::record(&e, Vendor::Codex, &body).unwrap();
    let u: Usage = get(&e, "/usage").await;
    assert_eq!(u.windows.len(), 1);
    assert_eq!(u.windows[0].vendor, Vendor::Codex);
    assert!(!u.windows[0].limited);
}
