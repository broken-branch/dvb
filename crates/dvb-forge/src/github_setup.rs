//! The engine exchanges the manifest code, holds the key and verifies installation ownership.
pub use crate::gateway::Client;
use crate::gateway::Wait;
use crate::github::{
    AppConfig, api_error, github_builder, http_status, load_app_key, refused, segment, text,
};
use dvb_core::api::{
    GithubAccountType, GithubConnection, GithubFailure, GithubFailureCode, GithubManifest,
    GithubManifestRequest, GithubSetupProgress, GithubSetupStep,
};
use dvb_core::forge::ForgeError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const REQUIRED_PERMISSIONS: [(&str, &str); 10] = [
    ("contents", "write"),
    ("pull_requests", "write"),
    ("issues", "write"),
    ("workflows", "write"),
    ("checks", "read"),
    ("statuses", "read"),
    ("actions", "write"),
    ("metadata", "read"),
    ("administration", "write"),
    ("organization_projects", "write"),
];

/// GitHub has no projects permission for an App on a personal account, so a
/// user's App asks for everything but the organization's board.
fn required_permissions(account: GithubAccountType) -> Vec<(&'static str, &'static str)> {
    REQUIRED_PERMISSIONS
        .into_iter()
        .filter(|(permission, _)| {
            account == GithubAccountType::Organization || *permission != "organization_projects"
        })
        .collect()
}

/// GitHub's `owner.type` / `account.type` for this account type.
fn github_type(account: GithubAccountType) -> &'static str {
    match account {
        GithubAccountType::Organization => "Organization",
        GithubAccountType::User => "User",
    }
}

/// Where the owner changes which repositories the installation reaches.
pub fn installation_settings_url(
    account: GithubAccountType,
    login: &str,
    installation: u64,
) -> String {
    match account {
        GithubAccountType::Organization => {
            format!(
                "https://github.com/organizations/{login}/settings/installations/{installation}"
            )
        }
        GithubAccountType::User => {
            format!("https://github.com/settings/installations/{installation}")
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PendingApp {
    pub organization: String,
    pub state: String,
    pub expires_at: i64,
    pub owner_name: String,
    pub owner_email: String,
    pub app_id: Option<u64>,
    pub slug: Option<String>,
    /// Durable conversion recovery; removed with the pending file after installation.
    #[serde(default)]
    pub pem: Option<String>,
    #[serde(default)]
    pub account_type: GithubAccountType,
}

pub fn client(api: &str, wait: Arc<Wait>) -> Result<Client, ForgeError> {
    let api = github_builder()
        .base_uri(api)
        .map_err(api_error)?
        .build()
        .map_err(api_error)?;
    Ok(Client { api, wait })
}

/// Whether `login` is a personal account or an organization, from GitHub's public
/// read of the account: no token needed during setup. None when
/// GitHub has no user or organization of that name.
pub async fn account_type(
    api: &Client,
    login: &str,
) -> Result<Option<GithubAccountType>, ForgeError> {
    if !valid_login(login) {
        return Ok(None);
    }
    let v: Value = match api.get(format!("/users/{}", segment(login))).await {
        Ok(v) => v,
        Err(error) if http_status(&error) == Some(404) => return Ok(None),
        Err(error) => return Err(api_error(error)),
    };
    Ok(match v["type"].as_str() {
        Some("Organization") => Some(GithubAccountType::Organization),
        Some("User") => Some(GithubAccountType::User),
        Some(_) | None => None,
    })
}

fn valid_login(login: &str) -> bool {
    !login.is_empty() && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn manifest(
    req: &GithubManifestRequest,
    account: GithubAccountType,
) -> Result<GithubManifest, ForgeError> {
    let org = &req.organization;
    if req.state.len() != 32
        || !req.state.chars().all(|c| c.is_ascii_hexdigit())
        || req.owner_name.trim().is_empty()
        || req.owner_email.trim().is_empty()
    {
        return Err(refused(
            "organization, callback state and local owner identity are required",
        ));
    }
    let port = req
        .redirect_url
        .strip_prefix("http://localhost:")
        .and_then(|s| s.strip_suffix("/github/callback"))
        .and_then(|s| s.parse::<u16>().ok());
    if port.is_none_or(|p| p == 0) {
        return Err(refused("GitHub callback must use the loopback desk"));
    }
    // GitHub echoes the create URL's state; callback URLs must have no query.
    let callback = &req.redirect_url;
    let app_url = "https://github.com/broken-branch/dvb";
    Ok(GithubManifest {
        create_url: match account {
            GithubAccountType::Organization => format!(
                "https://github.com/organizations/{org}/settings/apps/new?state={}",
                req.state
            ),
            GithubAccountType::User => format!(
                "https://github.com/settings/apps/new?state={}",
                req.state
            ),
        },
        manifest: serde_json::to_string(&json!({
            "name": format!(
                "dvb-{}-{}",
                org.chars().take(16).collect::<String>(),
                &req.state[..8]
            ),
            "url": app_url,
            "public": false,
            "redirect_url": callback,
            "setup_url": callback,
            "setup_on_update": false,
            "hook_attributes": { "url": app_url, "active": false },
            "default_events": [],
            "default_permissions": required_permissions(account).into_iter().collect::<BTreeMap<_, _>>()
        }))
        .map_err(|_| refused("cannot encode GitHub manifest"))?,
    })
}

/// Atomic, engine-owned files; neither credentials nor identity enter events.
pub fn write_private(state: &Path, name: &str, bytes: &[u8]) -> Result<(), ForgeError> {
    dvb_core::io::write_atomic(&state.join(name), bytes, 0o600, true)
        .map_err(|e| refused(format!("cannot write GitHub configuration: {e}")))?;
    std::fs::File::open(state)
        .and_then(|f| f.sync_all())
        .map_err(|e| refused(format!("cannot sync GitHub configuration directory: {e}")))
}

pub fn save_pending(state: &Path, pending: &PendingApp) -> Result<(), ForgeError> {
    write_private(
        state,
        "github-app.pending.json",
        &serde_json::to_vec(pending).map_err(|_| refused("cannot encode pending App"))?,
    )
}

pub fn pending(state: &Path) -> Result<PendingApp, ForgeError> {
    serde_json::from_slice(
        &std::fs::read(state.join("github-app.pending.json"))
            .map_err(|_| refused("no pending GitHub setup"))?,
    )
    .map_err(|_| refused("invalid pending GitHub setup"))
}

pub async fn exchange(
    api: &Client,
    state: &Path,
    pending: &mut PendingApp,
    code: &str,
) -> Result<(), ForgeError> {
    if pending.app_id.is_some() {
        if let Some(pem) = &pending.pem {
            write_private(state, "github-app.pem", pem.as_bytes())?;
        }
        return Ok(());
    }
    if code.is_empty()
        || code.len() > 200
        || !code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(refused("invalid manifest code"));
    }
    let v: Value = api
        .post(format!("/app-manifests/{code}/conversions"), None::<&()>)
        .await
        .map_err(api_error)?;
    let id = v["id"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| refused("App conversion has no ID"))?;
    let slug = text(&v, "slug")?;
    if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(refused("invalid App slug"));
    }
    // The conversion itself must belong to the selected account and remain private.
    if v["owner"]["login"]
        .as_str()
        .is_none_or(|s| !s.eq_ignore_ascii_case(&pending.organization))
        || v["owner"]["type"] != github_type(pending.account_type)
        || v["public"] == true
    {
        return Err(refused(
            "converted App does not match the private setup for this account",
        ));
    }
    let pem = text(&v, "pem")?;
    jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|_| refused("conversion returned an invalid RSA key"))?;
    let pem = pem.to_owned();
    pending.pem = Some(pem.clone());
    pending.app_id = Some(id);
    pending.slug = Some(slug.into());
    save_pending(state, pending)?;
    write_private(state, "github-app.pem", pem.as_bytes())
}

pub async fn verify_installation(
    api: &Client,
    api_url: &str,
    state: &Path,
    pending: &PendingApp,
    installation_id: u64,
) -> Result<AppConfig, ForgeError> {
    let app_id = pending
        .app_id
        .ok_or_else(|| refused("create the App before installation"))?;
    if let Some(pem) = &pending.pem {
        write_private(state, "github-app.pem", pem.as_bytes())?;
    }
    let key = load_app_key(state)?;
    let app = github_builder()
        .base_uri(api_url)
        .map_err(api_error)?
        .app(octocrab::models::AppId(app_id), key)
        .build()
        .map_err(api_error)?;
    let app = Client {
        api: app,
        wait: api.wait.clone(),
    };
    let v: Value = app
        .get(format!("/app/installations/{installation_id}"))
        .await
        .map_err(api_error)?;
    if v["account"]["type"] != github_type(pending.account_type)
        || v["account"]["login"]
            .as_str()
            .is_none_or(|s| !s.eq_ignore_ascii_case(&pending.organization))
        || !v["suspended_at"].is_null()
    {
        return Err(refused(
            "installation does not belong to this App and account",
        ));
    }
    for (permission, level) in required_permissions(pending.account_type) {
        let actual = v["permissions"][permission].as_str();
        if actual != Some(level) && !(level == "read" && actual == Some("write")) {
            return Err(refused(format!(
                "installation is missing required App permission {permission}:{level}"
            )));
        }
    }
    Ok(AppConfig {
        app_id,
        installation_id,
        organization: pending.organization.clone(),
        account_type: pending.account_type,
    })
}

pub fn setup_failure(code: GithubFailureCode) -> GithubFailure {
    let (message, retryable) = match code {
        GithubFailureCode::NotConnected => (
            "GitHub App is not configured. Run dvb github connect.",
            false,
        ),
        GithubFailureCode::RepositoryInaccessible => (
            "Check the App's repository access settings and the account it is installed on.",
            false,
        ),
        GithubFailureCode::DiscoveryFailed => (
            "Could not read repository access or checks. Retry and check App permissions or GitHub availability.",
            true,
        ),
        GithubFailureCode::AuthorizationDenied => (
            "GitHub authorization was declined. Retry the setup step and approve access.",
            true,
        ),
        GithubFailureCode::CallbackExpired => (
            "The setup link expired. Restart Create App, or retry Install App if the App was created.",
            true,
        ),
        GithubFailureCode::CreateAppFailed => (
            "Could not create the App. Retry Create App; check that you can administer the account and GitHub availability.",
            true,
        ),
        GithubFailureCode::InstallAppFailed => (
            "Could not verify the installation. Retry Install App for the selected account with the requested permissions.",
            true,
        ),
    };
    GithubFailure {
        code,
        message: message.into(),
        retryable,
    }
}

pub fn progress(state: &Path) -> Result<GithubSetupProgress, ForgeError> {
    match std::fs::read(state.join("github-setup-status.json")) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| refused("invalid GitHub setup status"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(GithubSetupProgress::default())
        }
        Err(_) => Err(refused("cannot read GitHub setup status")),
    }
}
pub fn save_progress(state: &Path, progress: &GithubSetupProgress) -> Result<(), ForgeError> {
    write_private(
        state,
        "github-setup-status.json",
        &serde_json::to_vec(progress).map_err(|_| refused("cannot encode GitHub setup status"))?,
    )
}

pub fn connection(state: &Path) -> GithubConnection {
    let progress = progress(state).unwrap_or_else(|_| GithubSetupProgress {
        create_app: GithubSetupStep::Failed {
            failure: setup_failure(GithubFailureCode::CreateAppFailed),
        },
        install_app: GithubSetupStep::Failed {
            failure: setup_failure(GithubFailureCode::InstallAppFailed),
        },
    });
    if let Ok(bytes) = std::fs::read(state.join("github-app.json"))
        && let Ok(cfg) = serde_json::from_slice::<AppConfig>(&bytes)
    {
        return GithubConnection {
            repository_access_url: Some(installation_settings_url(
                cfg.account_type,
                &cfg.organization,
                cfg.installation_id,
            )),
            organization: Some(cfg.organization),
            account_type: Some(cfg.account_type),
            install_url: None,
            connected: true,
            create_app: GithubSetupStep::Succeeded,
            install_app: GithubSetupStep::Succeeded,
        };
    }
    match pending(state) {
        Ok(p) => GithubConnection {
            organization: Some(p.organization),
            account_type: Some(p.account_type),
            install_url: p.slug.map(|slug| {
                format!(
                    "https://github.com/apps/{slug}/installations/new?state={}",
                    p.state
                )
            }),
            connected: false,
            repository_access_url: None,
            create_app: if p.app_id.is_some() {
                GithubSetupStep::Succeeded
            } else if matches!(progress.create_app, GithubSetupStep::NotStarted) {
                GithubSetupStep::AwaitingCallback
            } else {
                progress.create_app
            },
            install_app: if p.app_id.is_some()
                && matches!(progress.install_app, GithubSetupStep::NotStarted)
            {
                GithubSetupStep::AwaitingCallback
            } else {
                progress.install_app
            },
        },
        Err(_) => GithubConnection {
            organization: None,
            account_type: None,
            install_url: None,
            connected: false,
            repository_access_url: None,
            create_app: progress.create_app,
            install_app: progress.install_app,
        },
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn setup_conversion_uses_the_shared_rate_limit_gate() {
        use bytes::Bytes;
        use http_body_util::Full;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let service = tower::service_fn(move |_: http::Request<octocrab::OctoBody>| {
            requests.fetch_add(1, Ordering::SeqCst);
            async {
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(429)
                        .header("retry-after", "7")
                        .body(Full::new(Bytes::from_static(
                            br#"{"message":"private-body"}"#,
                        )))
                        .unwrap(),
                )
            }
        });
        let api = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let clock = Arc::new(dvb_core::FakeClock::at(dvb_core::Timestamp(10_000)));
        let api = Client {
            api,
            wait: Arc::new(Wait {
                clock: clock.clone(),
                deadline: std::sync::Mutex::new(None),
            }),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut pending = PendingApp {
            organization: "org".into(),
            state: "a".repeat(32),
            expires_at: 0,
            owner_name: "Owner".into(),
            owner_email: "owner@example.invalid".into(),
            app_id: None,
            slug: None,
            pem: None,
            account_type: GithubAccountType::Organization,
        };
        let limited = exchange(&api, dir.path(), &mut pending, "code")
            .await
            .unwrap_err();
        assert_eq!(limited.detail, "GitHub HTTP 429 (rate limit)");
        assert_eq!(
            limited.rate_limit.unwrap().reset,
            Some(dvb_core::Timestamp(17_000))
        );
        let installation = Client {
            api: api.api.user_access_token("offline-installation").unwrap(),
            wait: api.wait.clone(),
        };
        clock.set(dvb_core::Timestamp(17_999));
        assert!(installation.get("/app/installations/7").await.is_err());
        let blocked = exchange(&api, dir.path(), &mut pending, "code")
            .await
            .unwrap_err();
        assert_eq!(blocked.detail, "GitHub rate limit");
        let forge = crate::GitHub::with_app_client(
            api.api.clone(),
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            api.wait.clone(),
        )
        .unwrap();
        let blocked = forge
            .client(&dvb_core::forge::Repo("org/repo".into()))
            .await
            .err()
            .unwrap();
        assert_eq!(blocked.detail, "GitHub rate limit");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        clock.set(dvb_core::Timestamp(18_000));
        assert!(
            exchange(&api, dir.path(), &mut pending, "code")
                .await
                .is_err()
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn request() -> GithubManifestRequest {
        GithubManifestRequest {
            organization: "example-org".into(),
            redirect_url: "http://localhost:7300/github/callback".into(),
            state: "a".repeat(32),
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
        }
    }
    #[test]
    fn private_manifest_has_required_fields_and_keeps_callback_state_and_identity_separate() {
        for port in [7300, 7400] {
            let mut req = request();
            req.redirect_url = format!("http://localhost:{port}/github/callback");
            let prepared = manifest(&req, GithubAccountType::Organization).unwrap();
            let v: Value = serde_json::from_str(&prepared.manifest).unwrap();
            // GitHub requires name, url and hook_attributes.url even when delivery is disabled.
            for field in ["name", "url"] {
                assert!(v[field].as_str().is_some_and(|s| !s.is_empty()));
            }
            assert!(v["hook_attributes"].is_object());
            assert_eq!(v["hook_attributes"]["url"], v["url"]);
            assert_eq!(
                v["hook_attributes"]["url"],
                "https://github.com/broken-branch/dvb"
            );
            assert_eq!(v["hook_attributes"]["active"], false);
            assert_eq!(v["default_events"], json!([]));
            assert_eq!(v["public"], false);
            assert_eq!(
                v["default_permissions"],
                json!({
                    "contents":"write", "pull_requests":"write", "issues":"write",
                    "workflows":"write", "checks":"read", "statuses":"read", "actions":"write",
                    "metadata":"read", "administration":"write", "organization_projects":"write"
                })
            );
            assert_eq!(v["redirect_url"], req.redirect_url);
            assert_eq!(v["setup_url"], req.redirect_url);
            assert_eq!(
                prepared.create_url,
                format!(
                    "https://github.com/organizations/example-org/settings/apps/new?state={}",
                    req.state
                )
            );
            assert!(!prepared.manifest.contains(&req.owner_name));
            assert!(!prepared.manifest.contains(&req.owner_email));
        }
    }
    #[test]
    fn a_personal_accounts_app_is_created_under_the_user_without_the_board_permission() {
        let mut req = request();
        req.organization = "example-user".into();
        let prepared = manifest(&req, GithubAccountType::User).unwrap();
        assert_eq!(
            prepared.create_url,
            format!("https://github.com/settings/apps/new?state={}", req.state)
        );
        let v: Value = serde_json::from_str(&prepared.manifest).unwrap();
        assert_eq!(
            v["default_permissions"],
            json!({
                "contents":"write", "pull_requests":"write", "issues":"write",
                "workflows":"write", "checks":"read", "statuses":"read", "actions":"write",
                "metadata":"read", "administration":"write"
            })
        );
        assert_eq!(v["public"], false);
        assert!(v["name"].as_str().unwrap().starts_with("dvb-example-user-"));
        assert_eq!(
            installation_settings_url(GithubAccountType::User, "example-user", 7),
            "https://github.com/settings/installations/7"
        );
        assert_eq!(
            installation_settings_url(GithubAccountType::Organization, "example-org", 7),
            "https://github.com/organizations/example-org/settings/installations/7"
        );
    }
    #[test]
    fn configuration_written_before_personal_accounts_reads_as_an_organization() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"app_id":7,"installation_id":8,"organization":"example-org"}"#,
        )
        .unwrap();
        assert_eq!(cfg.account_type, GithubAccountType::Organization);
        let pending: PendingApp = serde_json::from_value(json!({
            "organization":"example-org", "state":"a".repeat(32), "expires_at":0,
            "owner_name":"Owner", "owner_email":"owner@example.invalid",
            "app_id":null, "slug":null
        }))
        .unwrap();
        assert_eq!(pending.account_type, GithubAccountType::Organization);
    }
    #[test]
    fn manifest_refuses_invalid_callbacks_and_organizations() {
        for url in [
            "https://evil.invalid/github/callback",
            "http://127.0.0.1:7300/github/callback",
            "http://localhost:0/github/callback",
            "http://localhost:65536/github/callback",
            "http://localhost:7300/other",
            "http://localhost:7300/github/callback?state=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "http://localhost:7300/github/callback#setup",
        ] {
            let mut bad = request();
            bad.redirect_url = url.into();
            assert!(manifest(&bad, GithubAccountType::Organization).is_err());
        }
    }
    #[test]
    fn failed_configuration_writes_refuse_and_clean_up() {
        let state = tempfile::tempdir().unwrap();
        let missing = state.path().join("missing");
        let error = write_private(&missing, "file", b"bytes").unwrap_err();
        assert!(
            error
                .detail
                .starts_with("cannot write GitHub configuration: No such file or directory"),
            "{}",
            error.detail
        );
        assert!(!error.retryable);
        for name in ["missing/file", "directory"] {
            std::fs::create_dir_all(state.path().join("directory")).unwrap();
            let error = write_private(state.path(), name, b"bytes").unwrap_err();
            assert!(
                error
                    .detail
                    .starts_with("cannot write GitHub configuration: "),
                "{}",
                error.detail
            );
            assert!(!error.retryable);
            assert_eq!(std::fs::read_dir(state.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn configuration_writes_are_private_atomic_and_replace_symlinks() {
        let state = tempfile::tempdir().unwrap();
        let unrelated = state.path().join("unrelated");
        std::fs::write(&unrelated, "keep").unwrap();
        std::os::unix::fs::symlink(&unrelated, state.path().join("github-app.pem")).unwrap();
        write_private(state.path(), "github-app.pem", b"fixture bytes").unwrap();
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "keep");
        assert_eq!(
            std::fs::metadata(state.path().join("github-app.pem"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        write_private(state.path(), "github-app.pem", b"replacement").unwrap();
        assert_eq!(
            std::fs::read_to_string(state.path().join("github-app.pem")).unwrap(),
            "replacement"
        );
    }
}
