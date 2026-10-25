//! Only the opaque manifest/installation result crosses the desk. The PEM stays in the engine.
use super::{DeskState, header};
use axum::{
    extract::{Query, Request, State},
    http::{Method, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use dvb_cli::client::{Client, REQUEST_TIMEOUT};
use dvb_core::api::{
    GithubCallback, GithubCallbackError, GithubInstallRequest, GithubManifestRequest, GithubPrefill,
};
use std::collections::HashMap;

/// An engine answer that is not a success: its status, and its refusal when the
/// engine refused the request (4xx). Internal errors keep their detail in the engine.
struct Failed(StatusCode, Option<dvb_core::Refusal>);

impl From<StatusCode> for Failed {
    fn from(status: StatusCode) -> Self {
        Self(status, None)
    }
}

async fn engine(
    d: &DeskState,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, Failed> {
    let reply = Client::new(d.engine_sock.clone())
        .raw(method, path, body, REQUEST_TIMEOUT)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = StatusCode::from_u16(reply.status).map_err(|_| StatusCode::BAD_GATEWAY)?;
    if status.is_success() {
        Ok(reply.body.into_bytes())
    } else if status.is_client_error() {
        Err(Failed(status, serde_json::from_str(&reply.body).ok()))
    } else {
        Err(status.into())
    }
}

#[derive(serde::Deserialize)]
struct Setup {
    organization: String,
    owner_name: String,
    owner_email: String,
}

pub async fn start(State(d): State<DeskState>, req: Request) -> Response {
    if !d.authenticated(req.method(), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 1 << 16).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(v) = serde_json::from_slice::<Setup>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let req = GithubManifestRequest {
        organization: v.organization,
        owner_name: v.owner_name,
        owner_email: v.owner_email,
        redirect_url: format!("http://localhost:{}/github/callback", d.port),
        state: super::token(),
    };
    let Ok(body) = serde_json::to_vec(&req) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match engine(&d, Method::POST, "/github/manifest", body).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        // The engine's refusal says what to fix, such as an unknown account name.
        Err(Failed(status, Some(refusal))) => (status, axum::Json(refusal)).into_response(),
        Err(Failed(status, None)) => (
            status,
            "GitHub setup could not start because dvb or GitHub did not answer; retry in a moment.",
        )
            .into_response(),
    }
}

/// Create App suggestions: what the engine stored for an earlier App, then the
/// owner's git identity, which only the desk can read: it runs as the owner.
pub async fn prefill(State(d): State<DeskState>, req: Request) -> Response {
    if !d.authenticated(req.method(), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let stored = match engine(&d, Method::GET, "/github/prefill", Vec::new()).await {
        Ok(bytes) => bytes,
        Err(Failed(status, _)) => {
            return (status, "Could not read the GitHub setup values.").into_response();
        }
    };
    let Ok(mut values) = serde_json::from_slice::<GithubPrefill>(&stored) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(values) = tokio::task::spawn_blocking(move || {
        dvb_cli::github::fill_from_git(&mut values, |key| dvb_cli::github::git_config(None, key));
        values
    })
    .await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(values)).into_response()
}

/// Re-enter installation without giving callback state generation to the browser.
pub async fn install(State(d): State<DeskState>, req: Request) -> Response {
    if !d.authenticated(req.method(), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let body = match serde_json::to_vec(&GithubInstallRequest {
        state: super::token(),
    }) {
        Ok(body) => body,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match engine(&d, Method::POST, "/github/install", body).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], bytes).into_response(),
        Err(Failed(status, _)) => (status, "Could not retry installation. Return to GitHub setup and check that Create App completed.").into_response(),
    }
}

pub async fn callback(
    State(d): State<DeskState>,
    Query(q): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !d.host_ok(&headers) {
        return callback_failure(StatusCode::FORBIDDEN);
    }
    let installation_id = match q.get("installation_id") {
        Some(s) => match s.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => return callback_failure(StatusCode::BAD_REQUEST),
        },
        None => None,
    };
    let callback = GithubCallback {
        state: q.get("state").cloned().unwrap_or_default(),
        code: q.get("code").cloned(),
        installation_id,
        error: q.get("error").map(|_| GithubCallbackError::Denied),
    };
    let Ok(body) = serde_json::to_vec(&callback) else {
        return callback_failure(StatusCode::BAD_REQUEST);
    };
    let phase = if callback.installation_id.is_some() {
        "install"
    } else {
        "create"
    };
    // The session cookie belongs to 127.0.0.1, while GitHub returns to localhost.
    let mut response = match engine(&d, Method::POST, "/github/callback", body).await {
        Ok(_) => Redirect::to(&format!(
            "http://127.0.0.1:{}/#github-complete/{phase}",
            d.port
        ))
        .into_response(),
        Err(Failed(status, _)) => callback_failure(status),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    response
}

fn callback_failure(status: StatusCode) -> Response {
    (status, [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
    ], Html(r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>GitHub setup could not finish</title><main><h1>GitHub setup could not finish</h1><p>The callback could not be verified. The service may be unavailable, or GitHub returned an invalid or expired callback.</p><p>Return to your original dvb window. Your project draft is kept there. Retry GitHub status to see the failure advice, then retry Create App or Install App there.</p><p>If this window did not close automatically, you can close it now.</p></main><script>window.close()</script></html>"#)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::sync::Arc;
    use tower::ServiceExt;
    fn desk() -> DeskState {
        Arc::new(super::super::Desk::new("/unused".into(), 7300))
    }
    #[tokio::test]
    async fn malformed_callback_explains_return_and_retry_without_claiming_success() {
        let response = super::super::router(desk())
            .oneshot(
                Request::get("/github/callback?state=bad&code=abc")
                    .header("host", "localhost:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("could not be verified"));
        assert!(text.contains("original dvb window"));
        assert!(text.contains("Retry"));
        assert!(text.contains("window.close()"));
    }

    #[tokio::test]
    async fn socket_install_retry_generates_state_and_forwards_safe_status() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = axum::Router::new().route(
            "/github/install",
            axum::routing::post(
                |axum::Json(body): axum::Json<GithubInstallRequest>| async move {
                    assert_eq!(body.state.len(), 32);
                    assert!(body.state.chars().all(|c| c.is_ascii_hexdigit()));
                    assert_ne!(body.state, "untrusted");
                    axum::Json(serde_json::json!({"install_app":{"status":"awaiting_callback"}}))
                },
            ),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7400));
        let res = super::super::router(d.clone())
            .oneshot(
                Request::post("/github/install")
                    .header("host", "127.0.0.1:7400")
                    .header("origin", "http://127.0.0.1:7400")
                    .header("cookie", format!("dvb_session={}", d.session))
                    .body(Body::from(r#"{"state":"untrusted"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["install_app"]["status"],
            "awaiting_callback"
        );
        server.abort();
    }

    #[tokio::test]
    async fn socket_declined_callback_reaches_the_engine_and_returns_to_setup() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = axum::Router::new().route(
            "/github/callback",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    assert_eq!(body["error"], "denied");
                    assert!(!body.to_string().contains("secret-error-description"));
                    axum::Json(serde_json::json!({"create_app":{"status":"failed"}}))
                },
            ),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7400));
        let res = super::super::router(d).oneshot(Request::get(format!(
            "/github/callback?state={}&error=access_denied&error_description=secret-error-description", "a".repeat(32)
        )).header("host", "localhost:7400").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers()[header::LOCATION],
            "http://127.0.0.1:7400/#github-complete/create"
        );
        server.abort();
    }

    #[tokio::test]
    async fn socket_prefill_forwards_the_engines_stored_values() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = axum::Router::new().route(
            "/github/prefill",
            axum::routing::get(|| async {
                axum::Json(GithubPrefill {
                    organization: Some("example-org".into()),
                    owner_name: Some("Stored Owner".into()),
                    owner_email: Some("stored@example.invalid".into()),
                })
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7400));
        let get = |cookie: bool| {
            let mut req = Request::get("/github/prefill").header("host", "127.0.0.1:7400");
            if cookie {
                req = req.header("cookie", format!("dvb_session={}", d.session));
            }
            req.body(Body::empty()).unwrap()
        };
        let refused = super::super::router(d.clone())
            .oneshot(get(false))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let res = super::super::router(d.clone())
            .oneshot(get(true))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<GithubPrefill>(&bytes).unwrap(),
            GithubPrefill {
                organization: Some("example-org".into()),
                owner_name: Some("Stored Owner".into()),
                owner_email: Some("stored@example.invalid".into()),
            }
        );
        server.abort();
    }

    #[tokio::test]
    async fn missing_setup_fields_return_bad_request_before_forwarding() {
        let d = desk();
        for missing in ["organization", "owner_name", "owner_email"] {
            let mut body = serde_json::json!({
                "organization":"example",
                "owner_name":"Owner",
                "owner_email":"owner@example.invalid"
            });
            body.as_object_mut().unwrap().remove(missing);
            let res = super::super::router(d.clone())
                .oneshot(
                    Request::post("/github/start")
                        .header("host", "127.0.0.1:7300")
                        .header("origin", "http://127.0.0.1:7300")
                        .header("cookie", format!("dvb_session={}", d.session))
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "missing {missing}");
        }
    }

    #[tokio::test]
    async fn setup_writes_require_cookie_origin_and_host_before_contacting_engine() {
        let d = desk();
        for route in ["/github/start", "/github/install"] {
            for (host, origin, cookie) in [
                ("evil.invalid", "http://127.0.0.1:7300", true),
                ("127.0.0.1:7300", "https://github.com", true),
                ("127.0.0.1:7300", "http://127.0.0.1:7300", false),
            ] {
                let mut req = Request::post(route)
                    .header("host", host)
                    .header("origin", origin);
                if cookie {
                    req = req.header("cookie", format!("dvb_session={}", d.session));
                }
                let res = super::super::router(d.clone())
                    .oneshot(req.body(Body::from("{}")).unwrap())
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::FORBIDDEN);
            }
        }
    }
    #[tokio::test]
    async fn external_callback_never_returns_to_setup_without_the_engine() {
        let d = desk();
        let both = format!(
            "/github/callback?state={}&code=abc&installation_id=7",
            "a".repeat(32)
        );
        for uri in [
            "/github/callback?code=abc",
            "/github/callback?state=bad&installation_id=7",
            &both,
        ] {
            let res = super::super::router(d.clone())
                .oneshot(
                    Request::get(uri)
                        .header("host", "localhost:7300")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(!res.headers().contains_key(header::LOCATION));
        }
    }
    #[tokio::test]
    async fn malformed_installation_ids_are_rejected_before_forwarding() {
        for id in ["invalid", "", "-1", "18446744073709551616"] {
            for code in ["", "&code=abc"] {
                let response = super::super::router(desk())
                    .oneshot(
                        Request::get(format!(
                            "/github/callback?state={}&installation_id={id}{code}",
                            "a".repeat(32)
                        ))
                        .header("host", "localhost:7300")
                        .body(Body::empty())
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            }
        }
    }

    #[tokio::test]
    async fn socket_manifest_bridge_forwards_codes_and_keeps_conversion_data_out_of_browser() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let manifest_calls = calls.clone();
        let callback_calls = calls.clone();
        let app = axum::Router::new()
            .route(
                "/github/manifest",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let calls = manifest_calls.clone();
                    async move {
                        calls.lock().unwrap().push(body);
                        axum::Json(serde_json::json!({
                            "create_url": "https://github.com/organizations/example-org/settings/apps/new",
                            "manifest": "recorded-manifest"
                        }))
                    }
                }),
            )
            .route(
                "/github/callback",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let calls = callback_calls.clone();
                    async move {
                        calls.lock().unwrap().push(body);
                        axum::Json(serde_json::json!({
                            "connected": false, "organization": "example-org",
                            "install_url": "https://github.com/apps/fixture/installations/new", "pem": "discarded-key"
                        }))
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7300));
        let request = Request::post("/github/start")
            .header("host", "127.0.0.1:7300")
            .header("origin", "http://127.0.0.1:7300")
            .header("cookie", format!("dvb_session={}", d.session))
            .body(Body::from(
                "{\"organization\":\"example-org\",\"owner_name\":\"Example \
                Owner\",\"owner_email\":\"owner@example.invalid\",\"state\":\"untrust\
                ed\",\"redirect_url\":\"https://evil.invalid\"}",
            ))
            .unwrap();
        let response = super::super::router(d.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = calls.lock().unwrap()[0].clone();
        assert_eq!(
            sent["redirect_url"],
            "http://localhost:7300/github/callback"
        );
        assert_ne!(sent["state"], "untrusted");
        let response = super::super::router(d)
            .oneshot(
                Request::get(format!(
                    "/github/callback?state={}&code=recorded-code",
                    sent["state"].as_str().unwrap()
                ))
                .header("host", "localhost:7300")
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "http://127.0.0.1:7300/#github-complete/create"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("discarded-key"));
        let call = calls.lock().unwrap()[1].clone();
        assert_eq!(call["code"], "recorded-code");
        assert_eq!(call["state"], sent["state"]);
        server.abort();
    }
    #[tokio::test]
    async fn socket_start_shows_the_engines_refusal_and_hides_internal_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = axum::Router::new().route(
            "/github/manifest",
            axum::routing::post(|axum::Json(body): axum::Json<GithubManifestRequest>| async move {
                if body.organization == "missing-login" {
                    (
                        StatusCode::CONFLICT,
                        axum::Json(serde_json::json!({
                            "code": "INVALID_VALUE",
                            "detail": "GitHub has no personal account or organization named missing-login."
                        })),
                    )
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({
                            "code": "INTERNAL_ERROR",
                            "detail": "api.github.com: connection refused"
                        })),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7300));
        for (login, status, kind, text) in [
            (
                "missing-login",
                StatusCode::CONFLICT,
                "application/json",
                "{\"code\":\"INVALID_VALUE\",\"detail\":\"GitHub has no personal account or organization named missing-login.\"}",
            ),
            (
                "example-org",
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain; charset=utf-8",
                "GitHub setup could not start because dvb or GitHub did not answer; retry in a moment.",
            ),
        ] {
            let response = super::super::router(d.clone())
                .oneshot(
                    Request::post("/github/start")
                        .header("host", "127.0.0.1:7300")
                        .header("origin", "http://127.0.0.1:7300")
                        .header("cookie", format!("dvb_session={}", d.session))
                        .body(Body::from(
                            serde_json::json!({
                                "organization": login,
                                "owner_name": "Example Owner",
                                "owner_email": "owner@example.invalid"
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CONTENT_TYPE], kind);
            let body = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&body), text);
        }
        server.abort();
    }
    #[tokio::test]
    async fn socket_localhost_callback_returns_to_session_origin_only_after_state_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = axum::Router::new().route(
            "/github/callback",
            axum::routing::post(|axum::Json(body): axum::Json<GithubCallback>| async move {
                if body.state == "a".repeat(32) {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let d = Arc::new(super::super::Desk::new(sock, 7400));
        for (state, expected_status) in [
            ("b".repeat(32), StatusCode::FORBIDDEN),
            ("a".repeat(32), StatusCode::SEE_OTHER),
        ] {
            // GitHub's localhost callback has no 127.0.0.1 session cookie.
            let response = super::super::router(d.clone())
                .oneshot(
                    Request::get(format!("/github/callback?state={state}&code=recorded-code"))
                        .header("host", "localhost:7400")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            if expected_status == StatusCode::SEE_OTHER {
                assert_eq!(
                    response.headers()[header::LOCATION],
                    "http://127.0.0.1:7400/#github-complete/create"
                );
            } else {
                assert!(!response.headers().contains_key(header::LOCATION));
            }
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        }
        server.abort();
    }
}
