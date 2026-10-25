//! Owner-only App setup. The desk forwards codes; only the engine holds the PEM.
use super::invalid;
use crate::{AppState, Fail};
use axum::{
    Json,
    extract::{Query, State},
};
use dvb_core::api::{
    GithubCallback, GithubCheckDiscovery, GithubChecksQuery, GithubConnection, GithubFailureCode,
    GithubInstallRequest, GithubManifest, GithubManifestRequest, GithubPrefill, GithubRepositories,
    GithubSetupProgress, GithubSetupStep,
};
use dvb_core::{Refusal, RefusalCode};
use dvb_forge::github_setup as setup;
use dvb_forge::scan::ScanPolicy;

/// The unauthenticated setup client, shared so its rate-limit gate holds across requests.
fn setup_api(e: &AppState) -> Result<(String, setup::Client), Fail> {
    let mut cached = e
        .github_setup_client
        .lock()
        .unwrap_or_else(dvb_core::sync::recover);
    if let Some(api) = &*cached {
        return Ok(api.clone());
    }
    let api =
        setup::client("https://api.github.com", e.github_wait.clone()).map_err(Fail::internal)?;
    Ok(cached
        .insert(("https://api.github.com".to_owned(), api))
        .clone())
}

#[utoipa::path(get, path = "/github", operation_id = "github_connection", tag = "github",
    responses(crate::api::ErrorResponses, (status = 200, description = "GitHub setup status", body = dvb_core::api::GithubConnection)))]
pub async fn connection(State(e): State<AppState>) -> Json<GithubConnection> {
    let mut connection = setup::connection(&e.cfg.paths.state);
    if !connection.connected
        && let Ok(pending) = setup::pending(&e.cfg.paths.state)
        && e.clock.now().0 > pending.expires_at
    {
        let step = if pending.app_id.is_some() {
            &mut connection.install_app
        } else {
            &mut connection.create_app
        };
        *step = GithubSetupStep::Failed {
            failure: setup::setup_failure(GithubFailureCode::CallbackExpired),
        };
        connection.install_url = None;
    }
    Json(connection)
}
/// Suggestions for Create App from what an earlier App stored: the installed or
/// pending App's organization, and the owner identity it recorded. The engine
/// does not run as the owner, so the owner's git config is the desk's to add.
#[utoipa::path(get, path = "/github/prefill", operation_id = "github_prefill", tag = "github",
    responses(crate::api::ErrorResponses, (status = 200, description = "Stored Create App values, each null when unknown", body = GithubPrefill)))]
pub async fn prefill(State(e): State<AppState>) -> Json<GithubPrefill> {
    let publication = e.publication();
    let pending = setup::pending(&e.cfg.paths.state).ok();
    let known = |stored: String, pending: Option<String>| {
        Some(stored)
            .filter(|s| !s.trim().is_empty())
            .or(pending.filter(|s| !s.trim().is_empty()))
    };
    Json(GithubPrefill {
        organization: setup::connection(&e.cfg.paths.state).organization,
        owner_name: known(
            publication.owner_name,
            pending.as_ref().map(|p| p.owner_name.clone()),
        ),
        owner_email: known(publication.owner_email, pending.map(|p| p.owner_email)),
    })
}
#[utoipa::path(post, path = "/github/manifest", operation_id = "github_manifest", tag = "github",
    request_body = dvb_core::api::GithubManifestRequest,
    responses(crate::api::ErrorResponses, (status = 200, description = "Prepared private App manifest", body = dvb_core::api::GithubManifest)))]
pub async fn manifest(
    State(e): State<AppState>,
    Json(body): Json<GithubManifestRequest>,
) -> Result<Json<GithubManifest>, Fail> {
    let _effect = e.update.effect().await;
    let _guard = e.github_setup.lock().await;
    if e.cfg.fake {
        return Err(invalid("GitHub setup is unavailable in Fake mode"));
    }
    if e.cfg.paths.state.join("github-app.json").exists() {
        return Err(invalid("GitHub is already connected"));
    }
    // Do not replace a converted App/key with another flow.
    if setup::pending(&e.cfg.paths.state).is_ok_and(|p| p.app_id.is_some()) {
        return Err(invalid("finish installing the pending App"));
    }
    let mut publication = e.publication();
    publication.owner_name.clone_from(&body.owner_name);
    publication.owner_email.clone_from(&body.owner_email);
    publication
        .validate()
        .map_err(|detail| invalid(detail.to_string()))?;
    // Owner-typed config, not dvb's writing: the owner's name may be in it.
    if let Some(hit) = ScanPolicy::default().scan("organization", &body.organization) {
        return Err(crate::publish::project::scan_refusal(hit));
    }
    let (_, api) = setup_api(&e)?;
    let account_type = setup::account_type(&api, &body.organization)
        .await
        .map_err(|error| Fail::context("cannot read the GitHub account", error))?
        .ok_or_else(|| {
            invalid(format!(
                "GitHub has no personal account or organization named {}.",
                body.organization
            ))
        })?;
    let manifest = setup::manifest(&body, account_type).map_err(Fail::internal)?;
    if let Some(hit) = ScanPolicy::default().scan("App manifest", &manifest.manifest) {
        return Err(crate::publish::project::scan_refusal(hit));
    }
    setup::save_pending(
        &e.cfg.paths.state,
        &setup::PendingApp {
            organization: body.organization,
            state: body.state,
            expires_at: e.clock.now().0 + 3_600_000,
            owner_name: body.owner_name,
            owner_email: body.owner_email,
            app_id: None,
            slug: None,
            pem: None,
            account_type,
        },
    )
    .map_err(Fail::internal)?;
    setup::save_progress(
        &e.cfg.paths.state,
        &GithubSetupProgress {
            create_app: GithubSetupStep::AwaitingCallback,
            install_app: GithubSetupStep::NotStarted,
        },
    )
    .map_err(Fail::internal)?;
    Ok(Json(manifest))
}
#[utoipa::path(post, path = "/github/callback", operation_id = "github_callback", tag = "github",
    request_body = dvb_core::api::GithubCallback,
    responses(crate::api::ErrorResponses, (status = 200, description = "App conversion or verified installation", body = dvb_core::api::GithubConnection)))]
pub async fn callback(
    State(e): State<AppState>,
    Json(body): Json<GithubCallback>,
) -> Result<Json<GithubConnection>, Fail> {
    let _effect = e.update.effect().await;
    let _guard = e.github_setup.lock().await;
    if e.cfg.fake {
        return Err(invalid("GitHub setup is unavailable in Fake mode"));
    }
    let mut pending = setup::pending(&e.cfg.paths.state).map_err(Fail::internal)?;
    let now = e.clock.now();
    let refusal = if body.state != pending.state {
        Some(RefusalCode::RoleNotAllowed)
    } else if now.0 > pending.expires_at {
        Some(RefusalCode::ValueChanged)
    } else {
        None
    };
    if let Some(code) = refusal {
        if code == RefusalCode::ValueChanged {
            let mut progress = setup::progress(&e.cfg.paths.state).map_err(Fail::internal)?;
            let step = if pending.app_id.is_some() {
                &mut progress.install_app
            } else {
                &mut progress.create_app
            };
            *step = GithubSetupStep::Failed {
                failure: setup::setup_failure(GithubFailureCode::CallbackExpired),
            };
            setup::save_progress(&e.cfg.paths.state, &progress).map_err(Fail::internal)?;
        }
        return Err(Refusal::new(code, "GitHub setup callback expired or changed").into());
    }
    if usize::from(body.code.is_some())
        + usize::from(body.installation_id.is_some())
        + usize::from(body.error.is_some())
        != 1
    {
        return Err(invalid("expected one GitHub callback result"));
    }
    let installing =
        body.installation_id.is_some() || (body.error.is_some() && pending.app_id.is_some());
    let result: Result<(), Fail> = async {
        if body.error.is_some() {
            return Err(invalid("GitHub authorization declined"));
        }
        let (api_url, api) = setup_api(&e)?;
        if let Some(code) = body.code {
            setup::exchange(&api, &e.cfg.paths.state, &mut pending, &code)
                .await
                .map_err(Fail::internal)?;
            pending.expires_at = now.0 + 3_600_000;
            setup::save_pending(&e.cfg.paths.state, &pending).map_err(Fail::internal)?;
        }
        if let Some(id) = body.installation_id {
            let cfg = setup::verify_installation(&api, &api_url, &e.cfg.paths.state, &pending, id)
                .await
                .map_err(Fail::internal)?;
            let mut publication = e.publication();
            publication.owner_name.clone_from(&pending.owner_name);
            publication.owner_email.clone_from(&pending.owner_email);
            publication.validate().map_err(Fail::internal)?;
            publication
                .save(&e.cfg.paths.state)
                .map_err(|error| Fail::context("cannot save publication settings", error))?;
            setup::write_private(
                &e.cfg.paths.state,
                "github-app.json",
                &serde_json::to_vec(&cfg)
                    .map_err(|error| Fail::context("cannot encode App settings", error))?,
            )
            .map_err(Fail::internal)?;
            let forge = dvb_forge::GitHub::from_state(&e.cfg.paths.state, e.github_wait.clone())
                .map_err(Fail::internal)?;
            *e.publication.lock().unwrap_or_else(dvb_core::sync::recover) = publication;
            *e.forge.lock().unwrap_or_else(dvb_core::sync::recover) =
                Some(std::sync::Arc::new(forge));
            std::fs::remove_file(e.cfg.paths.state.join("github-app.pending.json"))
                .map_err(|error| Fail::io("cannot retire pending App setup", error))?;
        }
        Ok(())
    }
    .await;
    let mut progress = setup::progress(&e.cfg.paths.state).map_err(Fail::internal)?;
    match result {
        Ok(()) => {
            progress.create_app = GithubSetupStep::Succeeded;
            progress.install_app = if installing {
                GithubSetupStep::Succeeded
            } else {
                GithubSetupStep::AwaitingCallback
            };
        }
        Err(error) => {
            // Fail's diagnostics may contain backend context; the owner gets fixed advice.
            eprintln!("GitHub setup callback failed: {error:?}");
            let code = if body.error.is_some() {
                GithubFailureCode::AuthorizationDenied
            } else if installing {
                GithubFailureCode::InstallAppFailed
            } else {
                GithubFailureCode::CreateAppFailed
            };
            let step = if installing {
                &mut progress.install_app
            } else {
                &mut progress.create_app
            };
            *step = GithubSetupStep::Failed {
                failure: setup::setup_failure(code),
            };
        }
    }
    setup::save_progress(&e.cfg.paths.state, &progress).map_err(Fail::internal)?;
    Ok(Json(setup::connection(&e.cfg.paths.state)))
}

/// The desk generates fresh callback state; the engine retains the converted App/key.
#[utoipa::path(post, path = "/github/install", operation_id = "github_install", tag = "github",
    request_body = GithubInstallRequest,
    responses(crate::api::ErrorResponses, (status = 200, description = "Retry installation with a fresh link", body = GithubConnection)))]
pub async fn install(
    State(e): State<AppState>,
    Json(body): Json<GithubInstallRequest>,
) -> Result<Json<GithubConnection>, Fail> {
    let _effect = e.update.effect().await;
    let _guard = e.github_setup.lock().await;
    if e.cfg.fake || setup::connection(&e.cfg.paths.state).connected {
        return Err(invalid("GitHub installation retry is unavailable"));
    }
    if body.state.len() != 32 || !body.state.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid("invalid callback state"));
    }
    let mut pending = setup::pending(&e.cfg.paths.state).map_err(Fail::internal)?;
    if pending.app_id.is_none() {
        return Err(invalid("create the App before installation"));
    }
    pending.state = body.state;
    pending.expires_at = e.clock.now().0 + 3_600_000;
    setup::save_pending(&e.cfg.paths.state, &pending).map_err(Fail::internal)?;
    setup::save_progress(
        &e.cfg.paths.state,
        &GithubSetupProgress {
            create_app: GithubSetupStep::Succeeded,
            install_app: GithubSetupStep::AwaitingCallback,
        },
    )
    .map_err(Fail::internal)?;
    Ok(Json(setup::connection(&e.cfg.paths.state)))
}

#[utoipa::path(get, path = "/github/repositories", operation_id = "github_repositories", tag = "github",
    responses(crate::api::ErrorResponses, (status = 200, description = "Repositories reachable by the installed App", body = GithubRepositories)))]
pub async fn repositories(State(e): State<AppState>) -> Result<Json<GithubRepositories>, Fail> {
    let forge = e
        .forge()
        .ok_or_else(|| invalid("GitHub App is not configured. Run dvb github connect."))?;
    Ok(Json(
        forge
            .accessible_repositories()
            .await
            .map_err(Fail::internal)?,
    ))
}

#[utoipa::path(get, path = "/github/checks", operation_id = "github_checks", tag = "github", params(GithubChecksQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Default branch check discovery; only discovered with empty checks confirms absence", body = GithubCheckDiscovery)))]
pub async fn checks(
    State(e): State<AppState>,
    Query(query): Query<GithubChecksQuery>,
) -> Json<GithubCheckDiscovery> {
    let Some(forge) = e.forge() else {
        return Json(GithubCheckDiscovery::Failed {
            failure: setup::setup_failure(GithubFailureCode::NotConnected),
        });
    };
    Json(
        forge
            .discover_checks(&dvb_core::forge::Repo(query.repo))
            .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use dvb_core::api::GithubAccountType;
    fn engine(dir: &std::path::Path) -> AppState {
        let mut cfg = Config::dev(dir, "dvb".into());
        cfg.fake = false;
        Engine::open(cfg, Default::default()).unwrap()
    }
    fn request() -> GithubManifestRequest {
        GithubManifestRequest {
            organization: "example-org".into(),
            redirect_url: "http://localhost:7300/github/callback".into(),
            state: "a".repeat(32),
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
        }
    }
    async fn recorded_github(e: &AppState) -> tokio::task::JoinHandle<()> {
        use axum::{
            extract::Path,
            http::StatusCode,
            response::IntoResponse,
            routing::{get, post},
        };
        let out = std::process::Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
            ])
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success());
        let pem = String::from_utf8(out.stdout).unwrap();
        let app = axum::Router::new()
            .route(
                "/app-manifests/{code}/conversions",
                post(move |Path(code): Path<String>| {
                    let pem = pem.clone();
                    async move {
                        if code == "failed" {
                            return (StatusCode::INTERNAL_SERVER_ERROR, "secret-echo")
                                .into_response();
                        }
                        let owner = if code == "personal" {
                            serde_json::json!({"login":"example-user","type":"User"})
                        } else {
                            serde_json::json!({"login":"example-org","type":"Organization"})
                        };
                        Json(serde_json::json!({"id":42,"slug":"fixture-app","pem":pem,
                        "owner":owner,"public":false}))
                        .into_response()
                    }
                }),
            )
            .route(
                "/app/installations/{id}",
                get(|Path(id): Path<u64>| async move {
                    // Installation 9 is on the personal account, 7 on the organization.
                    let (account, kind) = match id {
                        7 => (
                            serde_json::json!({"login":"example-org","type":"Organization"}),
                            GithubAccountType::Organization,
                        ),
                        9 => (
                            serde_json::json!({"login":"example-user","type":"User"}),
                            GithubAccountType::User,
                        ),
                        _ => (
                            serde_json::json!({"login":"other-org","type":"Organization"}),
                            GithubAccountType::Organization,
                        ),
                    };
                    let permissions: serde_json::Value =
                        serde_json::from_str(&setup::manifest(&request(), kind).unwrap().manifest)
                            .unwrap();
                    Json(serde_json::json!({"id":id,"app_id":42,"account":account,
                    "suspended_at":null,"permissions":permissions["default_permissions"]}))
                }),
            )
            .route(
                "/users/{login}",
                get(|Path(login): Path<String>| async move {
                    match login.as_str() {
                        "example-org" => {
                            Json(serde_json::json!({"login":login,"type":"Organization"}))
                                .into_response()
                        }
                        "example-user" => {
                            Json(serde_json::json!({"login":login,"type":"User"})).into_response()
                        }
                        "unreachable" => StatusCode::BAD_GATEWAY.into_response(),
                        _ => (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({"message":"Not Found"})),
                        )
                            .into_response(),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        *e.github_setup_client.lock().unwrap() = Some((
            url.clone(),
            setup::client(&url, e.github_wait.clone()).unwrap(),
        ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })
    }

    #[tokio::test]
    async fn socket_callback_success_failure_retry_and_restart_are_visible_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let server = recorded_github(&e).await;
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let creation = |code: &str| GithubCallback {
            state: "a".repeat(32),
            code: Some(code.into()),
            installation_id: None,
            error: None,
        };
        let failed = callback(State(e.clone()), Json(creation("failed")))
            .await
            .unwrap()
            .0;
        assert!(matches!(failed.create_app, GithubSetupStep::Failed { .. }));
        assert!(
            !serde_json::to_string(&failed)
                .unwrap()
                .contains("secret-echo")
        );
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let created = callback(State(e.clone()), Json(creation("success")))
            .await
            .unwrap()
            .0;
        assert!(matches!(created.create_app, GithubSetupStep::Succeeded));
        assert!(matches!(
            created.install_app,
            GithubSetupStep::AwaitingCallback
        ));
        assert!(!created.connected);
        let old_pending = setup::pending(&e.cfg.paths.state).unwrap();
        let install_body = |state: String, id| GithubCallback {
            state,
            code: None,
            installation_id: Some(id),
            error: None,
        };
        let refused = callback(
            State(e.clone()),
            Json(install_body(old_pending.state.clone(), 8)),
        )
        .await
        .unwrap()
        .0;
        assert!(matches!(
            refused.install_app,
            GithubSetupStep::Failed { .. }
        ));
        assert!(!refused.connected);
        let restarted = engine(dir.path());
        assert!(matches!(
            connection(State(restarted)).await.0.install_app,
            GithubSetupStep::Failed { .. }
        ));
        let retry = install(
            State(e.clone()),
            Json(GithubInstallRequest {
                state: "b".repeat(32),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(matches!(
            retry.install_app,
            GithubSetupStep::AwaitingCallback
        ));
        assert_eq!(
            setup::pending(&e.cfg.paths.state).unwrap().app_id,
            old_pending.app_id
        );
        assert!(
            callback(State(e.clone()), Json(install_body(old_pending.state, 7)))
                .await
                .is_err()
        );
        let installed = callback(State(e.clone()), Json(install_body("b".repeat(32), 7)))
            .await
            .unwrap()
            .0;
        assert!(installed.connected);
        assert!(matches!(installed.create_app, GithubSetupStep::Succeeded));
        assert!(matches!(installed.install_app, GithubSetupStep::Succeeded));
        assert_eq!(
            installed.repository_access_url.as_deref(),
            Some("https://github.com/organizations/example-org/settings/installations/7")
        );
        let json = serde_json::to_string(&installed).unwrap();
        assert!(!json.contains("PRIVATE KEY"));
        assert!(!json.contains("owner@example.invalid"));
        assert!(e.forge().is_some());
        // The installed App's identity and organization survive a restart.
        let stored = prefill(State(engine(dir.path()))).await.0;
        assert_eq!(
            stored,
            GithubPrefill {
                organization: Some("example-org".into()),
                owner_name: Some("Example Owner".into()),
                owner_email: Some("owner@example.invalid".into()),
            }
        );
        assert!(
            !serde_json::to_string(&stored)
                .unwrap()
                .contains("PRIVATE KEY")
        );
        server.abort();
    }

    #[tokio::test]
    async fn prefill_is_empty_on_a_fresh_install_and_offers_a_pending_apps_values() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        assert_eq!(prefill(State(e.clone())).await.0, GithubPrefill::default());
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let pending = prefill(State(e)).await.0;
        assert_eq!(pending.organization.as_deref(), Some("example-org"));
        assert_eq!(pending.owner_name.as_deref(), Some("Example Owner"));
        assert_eq!(
            pending.owner_email.as_deref(),
            Some("owner@example.invalid")
        );
        // The callback state stays in the engine.
        assert!(
            !serde_json::to_string(&pending)
                .unwrap()
                .contains(&"a".repeat(32))
        );
    }

    #[tokio::test]
    async fn installation_expiry_applies_after_conversion_and_retry_refreshes_it() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let mut p = setup::pending(&e.cfg.paths.state).unwrap();
        p.app_id = Some(42);
        p.slug = Some("fixture".into());
        p.expires_at = 0;
        setup::save_pending(&e.cfg.paths.state, &p).unwrap();
        let expired = connection(State(e.clone())).await.0;
        assert!(matches!(
            expired.install_app,
            GithubSetupStep::Failed { .. }
        ));
        assert!(expired.install_url.is_none());
        assert!(
            callback(
                State(e.clone()),
                Json(GithubCallback {
                    state: p.state,
                    code: None,
                    installation_id: Some(7),
                    error: None
                })
            )
            .await
            .is_err()
        );
        let retried = install(
            State(e.clone()),
            Json(GithubInstallRequest {
                state: "b".repeat(32),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(matches!(
            retried.install_app,
            GithubSetupStep::AwaitingCallback
        ));
        assert!(retried.install_url.unwrap().ends_with(&"b".repeat(32)));
        assert!(setup::pending(&e.cfg.paths.state).unwrap().expires_at > 0);
    }

    #[tokio::test]
    async fn discovery_and_repository_routes_report_missing_connection_and_forge_results() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        assert!(matches!(
            checks(
                State(e.clone()),
                Query(GithubChecksQuery {
                    repo: "example-org/repo".into()
                })
            )
            .await
            .0,
            GithubCheckDiscovery::Failed {
                failure: dvb_core::api::GithubFailure {
                    code: GithubFailureCode::NotConnected,
                    ..
                }
            }
        ));
        assert!(repositories(State(e.clone())).await.is_err());
        let forge = std::sync::Arc::new(dvb_forge::FakeForge::default());
        *e.forge.lock().unwrap() = Some(forge.clone());
        let repo = dvb_core::forge::Repo("fake/repo".into());
        forge.set_checks(
            &repo,
            "main",
            vec![dvb_core::forge::CheckObservation {
                name: "pending".into(),
                status: dvb_core::event::ChecksStatus::Pending,
                run: Some(1),
                conclusion: None,
                url: None,
            }],
        );
        let found = checks(
            State(e.clone()),
            Query(GithubChecksQuery {
                repo: repo.0.clone(),
            }),
        )
        .await
        .0;
        assert!(
            matches!(found, GithubCheckDiscovery::Discovered { checks, .. } if checks.len() == 1)
        );
        assert_eq!(
            repositories(State(e)).await.unwrap().0.repositories[0].repo,
            repo.0
        );
    }

    #[tokio::test]
    async fn setup_status_reports_both_steps_and_only_valid_callbacks_can_change_it() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let status = serde_json::to_value(connection(State(e.clone())).await.0).unwrap();
        assert_eq!(status["create_app"]["status"], "awaiting_callback");
        assert_eq!(status["install_app"]["status"], "not_started");
        let denied = serde_json::from_value(serde_json::json!({
            "state":"a".repeat(32), "error":"denied"
        }))
        .unwrap();
        let response = callback(State(e.clone()), Json(denied)).await.unwrap().0;
        let status = serde_json::to_value(response).unwrap();
        assert_eq!(status["create_app"]["status"], "failed");
        assert_eq!(
            status["create_app"]["failure"]["code"],
            "authorization_denied"
        );
        assert!(!status.to_string().contains("PRIVATE KEY"));
    }
    #[tokio::test]
    async fn invalid_owner_identity_is_refused_before_saving_the_manifest() {
        for (name, email) in [
            ("dvb", "owner@example.invalid"),
            ("Example Owner", "dvb@dvb.invalid"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let e = engine(dir.path());
            let mut body = request();
            body.owner_name = name.into();
            body.owner_email = email.into();
            let error = manifest(State(e.clone()), Json(body)).await.unwrap_err();
            assert!(matches!(
                error,
                Fail::Refused(Refusal {
                    code: RefusalCode::InvalidValue,
                    ..
                })
            ));
            assert!(!e.cfg.paths.state.join("github-app.pending.json").exists());
        }
    }
    #[tokio::test]
    async fn an_account_named_like_its_owner_is_connected_and_a_secret_in_it_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        let mut body = request();
        body.owner_name = "example-user".into();
        body.organization = format!("ghp_{}", "a".repeat(36));
        let error = manifest(State(e.clone()), Json(body.clone()))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fail::Refused(Refusal {
                code: RefusalCode::ScanHit { .. },
                ..
            })
        ));
        // The organization is owner-typed config: their own name may be in it.
        body.organization = "example-user".into();
        let _ = manifest(State(e.clone()), Json(body)).await.unwrap();
        assert!(e.cfg.paths.state.join("github-app.pending.json").exists());
    }
    #[tokio::test]
    async fn wrong_state_and_expired_code_are_refused_before_any_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let error = callback(
            State(e.clone()),
            Json(GithubCallback {
                state: "b".repeat(32),
                code: Some("unused".into()),
                installation_id: None,
                error: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Fail::Refused(Refusal {
                code: RefusalCode::RoleNotAllowed,
                ..
            })
        ));
        let mut p = setup::pending(&e.cfg.paths.state).unwrap();
        p.expires_at = 0;
        setup::save_pending(&e.cfg.paths.state, &p).unwrap();
        let expired = callback(
            State(e.clone()),
            Json(GithubCallback {
                state: p.state.clone(),
                code: Some("unused".into()),
                installation_id: None,
                error: None,
            }),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            expired,
            Fail::Refused(Refusal {
                code: RefusalCode::ValueChanged,
                ..
            })
        ));
        assert!(!e.cfg.paths.state.join("github-app.pem").exists());
        assert!(e.forge().is_none());
        drop(e);
        let restarted = engine(dir.path());
        assert_eq!(
            setup::pending(&restarted.cfg.paths.state).unwrap().state,
            p.state
        );
    }
    #[tokio::test]
    async fn fake_mode_never_exchanges_credentials_and_converted_apps_are_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        assert!(manifest(State(fake), Json(request())).await.is_err());
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        let _ = manifest(State(e.clone()), Json(request())).await.unwrap();
        let mut p = setup::pending(&e.cfg.paths.state).unwrap();
        p.app_id = Some(42);
        p.slug = Some("fixture-app".into());
        setup::save_pending(&e.cfg.paths.state, &p).unwrap();
        assert!(manifest(State(e.clone()), Json(request())).await.is_err());
        assert_eq!(setup::pending(&e.cfg.paths.state).unwrap().app_id, Some(42));
    }

    #[tokio::test]
    async fn a_personal_account_is_set_up_under_the_user_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let server = recorded_github(&e).await;
        let mut body = request();
        body.organization = "example-user".into();
        let prepared = manifest(State(e.clone()), Json(body)).await.unwrap().0;
        assert_eq!(
            prepared.create_url,
            format!(
                "https://github.com/settings/apps/new?state={}",
                "a".repeat(32)
            )
        );
        let pending = connection(State(e.clone())).await.0;
        assert_eq!(pending.account_type, Some(GithubAccountType::User));
        let created = callback(
            State(e.clone()),
            Json(GithubCallback {
                state: "a".repeat(32),
                code: Some("personal".into()),
                installation_id: None,
                error: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(matches!(created.create_app, GithubSetupStep::Succeeded));
        let installed = callback(
            State(e.clone()),
            Json(GithubCallback {
                state: "a".repeat(32),
                code: None,
                installation_id: Some(9),
                error: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(installed.connected, "{installed:?}");
        assert_eq!(installed.organization.as_deref(), Some("example-user"));
        assert_eq!(installed.account_type, Some(GithubAccountType::User));
        assert_eq!(
            installed.repository_access_url.as_deref(),
            Some("https://github.com/settings/installations/9")
        );
        assert!(e.forge().is_some());
        // The account type survives a restart.
        assert_eq!(
            connection(State(engine(dir.path()))).await.0.account_type,
            Some(GithubAccountType::User)
        );
        server.abort();
    }
    #[tokio::test]
    async fn an_unknown_account_is_refused_in_one_sentence_before_any_app_exists() {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(dir.path());
        let _server = recorded_github(&e).await;
        for login in ["nobody-here", "personal/name"] {
            let mut body = request();
            body.organization = login.into();
            let Err(Fail::Refused(refusal)) = manifest(State(e.clone()), Json(body)).await else {
                panic!("{login} must be refused");
            };
            assert_eq!(refusal.code, RefusalCode::InvalidValue);
            assert_eq!(
                refusal.detail,
                format!("GitHub has no personal account or organization named {login}.")
            );
        }
        let mut body = request();
        body.organization = "unreachable".into();
        let Err(Fail::Internal(error)) = manifest(State(e.clone()), Json(body)).await else {
            panic!("an unreadable account is an error");
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot read the GitHub account: GitHub HTTP 502")
        );
        assert!(!e.cfg.paths.state.join("github-app.pending.json").exists());
        assert_eq!(connection(State(e)).await.0.account_type, None);
    }
}
