//! Owner-router transport; callers assert the status and response shape.
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode};
use dvb_engine::{AppState, api};
use serde_json::Value;
use tower::ServiceExt;

pub async fn request(
    e: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let req = Request::builder().method(method).uri(path);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = api::router(e.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, bytes)
}

pub async fn call(
    e: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, bytes) = request(e, method, path, body).await;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("nonempty response must be valid JSON")
    };
    (status, body)
}

/// Decode a router response while retaining the engine error shape used by assertions.
fn json_response<T: serde::de::DeserializeOwned>(
    status: StatusCode,
    bytes: &[u8],
) -> Result<T, dvb_engine::Fail> {
    if status.is_success() {
        Ok(serde_json::from_slice(bytes).unwrap())
    } else {
        let refusal: dvb_core::Refusal = serde_json::from_slice(bytes).unwrap();
        if refusal.code == dvb_core::RefusalCode::NotFound {
            Err(dvb_engine::Fail::NotFound(refusal.detail))
        } else {
            Err(dvb_engine::Fail::Refused(refusal))
        }
    }
}

pub async fn owner<T: serde::de::DeserializeOwned>(
    e: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<axum::Json<T>, dvb_engine::Fail> {
    let (status, bytes) = request(e, method, path, body).await;
    json_response(status, &bytes).map(axum::Json)
}

pub async fn run<T: serde::de::DeserializeOwned, B: serde::Serialize>(
    verb: &str,
    axum::extract::State(e): axum::extract::State<AppState>,
    caller: dvb_engine::serve::Caller,
    axum::Json(body): axum::Json<B>,
) -> Result<axum::Json<T>, dvb_engine::Fail> {
    let pid = super::launcher::register_peer(caller.run.assignment.run);
    let response = dvb_engine::run_api::router(e)
        .oneshot(
            Request::post(format!("/run/{verb}"))
                .extension(axum::extract::ConnectInfo(dvb_engine::serve::Peer {
                    uid: None,
                    pid: Some(pid),
                }))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    super::launcher::unregister_peer(pid);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    json_response(status, &bytes).map(axum::Json)
}
