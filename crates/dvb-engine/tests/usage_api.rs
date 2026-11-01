//! Usage reads, usage-check failures, and run output over the event log.

mod support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use dvb_core::api::{RunOutputLine, RunView, Usage};
use dvb_core::event::OutputItem;
use dvb_core::launcher::{LaunchError, UnitState};
use dvb_core::run_verbs::{RateLimitBody, UsageReading};
use dvb_core::{Actor, Effort, EventId, EventKind, ProjectId, RunId, Vendor};
use dvb_engine::{AppState, EngineOptions, api, vendors};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use support::http::call;
use support::launcher::FakeLauncher;
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

#[tokio::test(start_paused = true)]
async fn a_missing_usage_reading_stops_its_unit_without_changing_the_pause() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher::default());
    let e = support::engine::open_with(
        dir.path(),
        "unused",
        EngineOptions {
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    );
    let reset = e.clock.now().plus_millis(3_600_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    let before: Usage = get(&e, "/usage").await;
    let events = support::events::all(&e);
    let (status, error) = call(&e, Method::POST, "/usage/codex/check", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    let after: Usage = get(&e, "/usage").await;
    assert_eq!(after.windows, before.windows);
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
    assert_eq!(support::events::all(&e), events);
    assert!(e.proj().runs.is_empty(), "a reading is not a task run");
    let units = stub.units.lock().unwrap().clone();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].vendor, Vendor::Codex);
    assert_eq!(stub.stops.load(Ordering::SeqCst), 1);
    assert!(!stub.is_active(&units[0].run));
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("no usage reading")
    );
}

#[tokio::test(start_paused = true)]
async fn malformed_and_failed_usage_readings_leave_the_pause_and_log_unchanged() {
    for contents in [
        "{".to_owned(),
        r#"{"read":{"used_pct":"invalid"}}"#.to_owned(),
        serde_json::to_string(&UsageReading::Failed("account unavailable".into())).unwrap(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let stub = Arc::new(FakeLauncher {
            start_state: Some(UnitState::Inactive),
            forbid_stop: Some("the reading worker has already exited"),
            start_hook: Some(Arc::new(move |unit| {
                std::fs::write(work.join(unit.run.as_str()).join("reading.json"), &contents)
                    .map_err(|err| LaunchError(err.to_string()))
            })),
            ..Default::default()
        });
        let e = support::engine::open_with(
            dir.path(),
            "unused",
            EngineOptions {
                launcher: Some(stub.clone()),
                ..Default::default()
            },
        );
        let reset = e.clock.now().plus_millis(3_600_000);
        vendors::record(
            &e,
            Vendor::Codex,
            &RateLimitBody {
                used_pct: 100.0,
                window: "5h".into(),
                resets_at: Some(reset),
            },
        )
        .unwrap();
        let before: Usage = get(&e, "/usage").await;
        let events = support::events::all(&e);
        let (status, error) = call(&e, Method::POST, "/usage/codex/check", None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
        let after: Usage = get(&e, "/usage").await;
        assert_eq!(after.windows, before.windows);
        assert!(e.vendors.is_limited(Vendor::Codex));
        assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
        assert_eq!(support::events::all(&e), events);
        assert!(e.proj().runs.is_empty(), "a reading is not a task run");
        assert_eq!(stub.starts.load(Ordering::SeqCst), 1);
        assert_eq!(stub.stops.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn a_usage_reading_start_failure_preserves_the_pause() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher {
        start_hook: Some(Arc::new(|_| Err(LaunchError("start job rejected".into())))),
        forbid_stop: Some("no unit was accepted"),
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "unused",
        EngineOptions {
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    );
    let reset = e.clock.now().plus_millis(3_600_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    let before: Usage = get(&e, "/usage").await;
    let events = support::events::all(&e);
    let (status, error) = call(&e, Method::POST, "/usage/codex/check", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    let after: Usage = get(&e, "/usage").await;
    assert_eq!(after.windows, before.windows);
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
    assert_eq!(support::events::all(&e), events);
    assert!(e.proj().runs.is_empty(), "a reading is not a task run");
    assert!(stub.units.lock().unwrap().is_empty());
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("starting the usage run")
    );
}

#[tokio::test(start_paused = true)]
async fn a_usage_reading_stop_failure_is_reported_without_changing_the_pause() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher {
        fail_stops: AtomicBool::new(true),
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "unused",
        EngineOptions {
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    );
    let reset = e.clock.now().plus_millis(3_600_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    let before: Usage = get(&e, "/usage").await;
    let events = support::events::all(&e);
    let (status, error) = call(&e, Method::POST, "/usage/codex/check", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    let after: Usage = get(&e, "/usage").await;
    assert_eq!(after.windows, before.windows);
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
    assert_eq!(support::events::all(&e), events);
    assert!(e.proj().runs.is_empty(), "a reading is not a task run");
    let units = stub.units.lock().unwrap().clone();
    assert_eq!(units.len(), 1);
    assert!(stub.is_active(&units[0].run));
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("stopping the usage reading")
    );
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("the stop job failed")
    );
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_usage_check_stops_its_reading_unit() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher::default());
    let e = support::engine::open_with(
        dir.path(),
        "unused",
        EngineOptions {
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    );
    let reset = e.clock.now().plus_millis(3_600_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    let before: Usage = get(&e, "/usage").await;
    let events = support::events::all(&e);
    let request = tokio::spawn({
        let e = e.clone();
        async move { call(&e, Method::POST, "/usage/codex/check", None).await }
    });
    stub.ready.acquire().await.unwrap().forget();
    let units = stub.units.lock().unwrap().clone();
    assert_eq!(units.len(), 1);
    assert!(stub.is_active(&units[0].run));
    assert!(!request.is_finished());
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    // Allow asynchronous cleanup and the original reading timeout to run.
    tokio::time::advance(Duration::from_secs(46)).await;
    tokio::task::yield_now().await;
    let after: Usage = get(&e, "/usage").await;
    assert_eq!(after.windows, before.windows);
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
    assert_eq!(support::events::all(&e), events);
    assert!(e.proj().runs.is_empty(), "a reading is not a task run");
    assert!(
        !stub.is_active(&units[0].run),
        "cancelled check leaked its reading unit"
    );
    assert_eq!(stub.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_usage_check_stops_a_unit_whose_start_is_still_pending() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(FakeLauncher {
        block_start: AtomicBool::new(true),
        ..Default::default()
    });
    let e = support::engine::open_with(
        dir.path(),
        "unused",
        EngineOptions {
            launcher: Some(stub.clone()),
            ..Default::default()
        },
    );
    let reset = e.clock.now().plus_millis(3_600_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    let before: Usage = get(&e, "/usage").await;
    let events = support::events::all(&e);
    let request = tokio::spawn({
        let e = e.clone();
        async move { call(&e, Method::POST, "/usage/codex/check", None).await }
    });
    stub.starting.acquire().await.unwrap().forget();
    let units = stub.units.lock().unwrap().clone();
    assert_eq!(units.len(), 1);
    assert!(stub.is_active(&units[0].run));
    assert!(!request.is_finished());
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    // Allow asynchronous cleanup and the original reading timeout to run.
    tokio::time::advance(Duration::from_secs(46)).await;
    tokio::task::yield_now().await;
    let after: Usage = get(&e, "/usage").await;
    assert_eq!(after.windows, before.windows);
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(e.vendors.resets_at(Vendor::Codex), Some(reset));
    assert_eq!(support::events::all(&e), events);
    assert!(e.proj().runs.is_empty(), "a reading is not a task run");
    assert!(
        !stub.is_active(&units[0].run),
        "cancelled check leaked its reading unit"
    );
    assert_eq!(stub.stops.load(Ordering::SeqCst), 1);
}
