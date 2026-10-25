//! Owner routes, input failures and versioned Refusal responses.

mod support;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use dvb_engine::{Config, Engine, api, run_api};
use tower::ServiceExt;

use support::engine::engine;

async fn call(router: axum::Router, method: Method, path: &str) -> (StatusCode, Option<String>) {
    let res = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let version = res
        .headers()
        .get("dvb-version")
        .map(|v| v.to_str().unwrap().to_owned());
    let status = res.status();
    (status, version)
}

/// `/projects/{id}/pause` -> `/projects/x/pause`.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|seg| if seg.starts_with('{') { "x" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_openapi_route_is_served() {
    let doc = serde_json::to_value(api::openapi()).unwrap();
    let paths = doc["paths"].as_object().unwrap();
    for (path, item) in paths {
        for method in item.as_object().unwrap().keys() {
            let m = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let (_dir, e) = engine();
            let (status, version) = call(api::router(e), m, &concrete(path)).await;
            assert!(
                status != StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path}: {status}"
            );
            assert_eq!(
                version.as_deref(),
                Some(dvb_core::VERSION),
                "{method} {path}"
            );
        }
    }
}

#[tokio::test]
async fn a_note_from_outside_a_run_is_refused() {
    let (_dir, e) = engine();
    let res = run_api::router(e)
        .oneshot(Request::post("/run/note").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        res.status().as_u16(),
        dvb_core::RefusalCode::NotARun.http_status()
    );
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
        .await
        .unwrap();
    let refusal: dvb_core::Refusal = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(refusal.code, dvb_core::RefusalCode::NotARun);
}

async fn owner_request(method: Method, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let (_dir, e) = engine();
    let res = api::router(e)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.headers()["dvb-version"], dvb_core::VERSION);
    assert_eq!(res.headers()["content-type"], "application/json");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn malformed_and_invalid_owner_inputs_use_invalid_value() {
    for (method, path, body) in [
        (Method::POST, "/projects", "{"),
        (Method::PUT, "/only", r#"{"models":[],"hours":0}"#),
        (
            Method::POST,
            "/github/manifest",
            r#"{"owner":"","name":""}"#,
        ),
        (Method::GET, "/events/recent?limit=bad", ""),
        (Method::GET, "/settings/history?key=unknown", ""),
        (Method::GET, "/runs/missing/output?after=bad", ""),
        (Method::POST, "/logins/unknown", ""),
    ] {
        let (status, refusal) = owner_request(method, path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {refusal}");
        let refusal: dvb_core::Refusal = serde_json::from_value(refusal).unwrap();
        assert_eq!(refusal.code, dvb_core::RefusalCode::InvalidValue, "{path}");
    }
}

#[tokio::test]
async fn optional_bodies_reject_supplied_malformed_values_consistently() {
    for path in ["/tasks/missing/retry", "/update", "/logins/codex"] {
        for body in ["{", " ", "[]", r#"{"text":2,"code":2,"to":2}"#] {
            let (status, refusal) = owner_request(Method::POST, path, body).await;
            assert_eq!(status, StatusCode::CONFLICT, "{path} {body}: {refusal}");
            assert_eq!(refusal["code"], "INVALID_VALUE", "{path} {body}: {refusal}");
        }
    }
}

#[tokio::test]
async fn absent_and_null_optional_bodies_keep_default_behavior() {
    // Missing resources and a disabled dev update source avoid external effects.
    for (path, expected) in [
        ("/tasks/missing/retry", "NOT_FOUND"),
        ("/update", "UPDATE_REFUSED"),
    ] {
        for body in ["", "null", "{}"] {
            let (_, refusal) = owner_request(Method::POST, path, body).await;
            assert_eq!(refusal["code"], expected, "{path} {body}: {refusal}");
        }
    }
}

#[tokio::test]
async fn owner_missing_resources_and_methods_use_refusal_json() {
    for (method, path, expected) in [
        (Method::GET, "/tasks/missing", StatusCode::NOT_FOUND),
        (Method::GET, "/nope", StatusCode::NOT_FOUND),
        (Method::DELETE, "/settings", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let (status, refusal) = owner_request(method, path, "").await;
        assert_eq!(status, expected);
        serde_json::from_value::<dvb_core::Refusal>(refusal).unwrap();
    }
}

#[tokio::test]
async fn optional_login_bodies_start_and_finish_without_external_commands() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), "unused".into());
    let work = cfg.paths.work.clone();
    let e = Engine::open(
        cfg,
        dvb_engine::EngineOptions {
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher {
                start_state: Some(dvb_core::launcher::UnitState::Inactive),
                stop_changes_state: false,
                start_hook: Some(std::sync::Arc::new({
                    let work = work.clone();
                    move |unit| {
                        std::fs::write(work.join(unit.run.as_str()).join("login.json"),
r#"{"url":"https://example.invalid/login","prompt":"Use the vendor page"}"#)
.map_err(|error| dvb_core::launcher::LaunchError(error.to_string()))
                    }
                })),
                ..Default::default()
            })),
            ..Default::default()
        },
    )
    .unwrap();
    for body in ["", "null"] {
        let response = api::router(e.clone())
            .oneshot(
                Request::post("/logins/codex")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let started: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(started["status"], "pending");
        assert_eq!(started["url"], "https://example.invalid/login");
        let run = started["run"].as_str().unwrap();
        std::fs::write(work.join(run).join("login-result"), "ok").unwrap();
        let response = api::router(e.clone())
            .oneshot(
                Request::post("/logins/codex")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"run":run,"code":"unused"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["status"],
            "logged_in"
        );
    }
}
