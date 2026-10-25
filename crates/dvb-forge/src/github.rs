//! Private GitHub App gateway. Cached tokens are scoped to one repo with the
//! App's permissions, and never included in a URL, event, config or error.
use crate::gateway::{Client, Error as GatewayError, Wait};
use crate::scan::ScanPolicy;
use dvb_core::Timestamp;
use dvb_core::api::GithubAccountType;
use dvb_core::event::ChecksStatus;
use dvb_core::forge::{
    CheckObservation, ExpectedChecks, Forge, ForgeError, MergeError, Pr, PrObservation, PrState,
    Repo,
};
use dvb_core::git::{Git, GitError};
use octocrab::Octocrab;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

pub struct GitHub {
    app: Client,
    wait: Arc<Wait>,
    pub(super) mirror_ids: Mutex<MirrorIds>,
    pub(super) state_dir: Option<std::path::PathBuf>,
    installation: u64,
    pub(super) org: String,
    pub(super) account: GithubAccountType,
    sessions: tokio::sync::Mutex<BTreeMap<Option<Repo>, Session>>,
    /// Creation receipts, restored by the engine from its durable event log.
    created: Mutex<BTreeMap<Repo, u64>>,
}

#[derive(Clone)]
struct Session {
    token: String,
    client: Client,
    expires_at: Timestamp,
}

#[derive(Default, serde::Serialize, Deserialize)]
#[serde(default)]
pub(super) struct MirrorIds {
    pub boards: BTreeMap<String, String>,
    pub cards: BTreeMap<String, String>,
    pub milestones: BTreeMap<String, u64>,
    /// Repository -> board ID whose optional repository link was forbidden.
    pub forbidden_board_links: BTreeMap<String, String>,
    #[serde(skip)]
    pub fields: BTreeMap<String, BTreeMap<String, Value>>,
}

#[derive(serde::Serialize, Deserialize)]
pub struct AppConfig {
    pub app_id: u64,
    pub installation_id: u64,
    pub organization: String,
    #[serde(default)]
    pub account_type: GithubAccountType,
}

/// Startup and installation read the same engine-owned RSA key, made mode 0600 if it is not.
pub(crate) fn load_app_key(state: &Path) -> Result<jsonwebtoken::EncodingKey, ForgeError> {
    let path = state.join("github-app.pem");
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| refused("cannot read GitHub App key metadata"))?;
    if !meta.is_file() || meta.uid() != dvb_core::io::own_uid() {
        return Err(refused("GitHub App key must be an engine-owned file"));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| refused("cannot make the GitHub App key mode 0600"))?;
        eprintln!(
            "dvb forge: GitHub App key had mode {:o}; made it 0600",
            meta.mode() & 0o777
        );
    }
    jsonwebtoken::EncodingKey::from_rsa_pem(
        &std::fs::read(path).map_err(|_| refused("cannot read GitHub App key"))?,
    )
    .map_err(|_| refused("invalid GitHub App RSA key"))
}

pub(super) fn refused(detail: impl Into<String>) -> ForgeError {
    ForgeError {
        rate_limit: None,
        retryable: false,
        detail: detail.into(),
    }
}
pub(super) fn http_status(error: &GatewayError) -> Option<u16> {
    if let Some(octocrab::Error::GitHub { source, .. }) = error.api() {
        Some(source.status_code.as_u16())
    } else {
        None
    }
}
pub(super) fn api_error(error: impl Into<GatewayError>) -> ForgeError {
    let error = error.into();
    if let GatewayError::Wait(limit, status) = error {
        return ForgeError {
            retryable: true,
            detail: status.map_or_else(
                || "GitHub rate limit".into(),
                |status| format!("GitHub HTTP {status} (rate limit)"),
            ),
            rate_limit: Some(limit),
        };
    }
    let status = http_status(&error);
    ForgeError {
        rate_limit: None,
        retryable: status.is_none_or(|s| s == 429 || s == 403 || s >= 500),
        // Response bodies can echo tokens or unscanned submitted data. Never log them;
        // only the plain-word validation codes GitHub returns (`resource`/`field`/`code`).
        detail: status.map_or_else(
            || "GitHub transport or response error".into(),
            |s| {
                let errors = if let Some(octocrab::Error::GitHub { source, .. }) = error.api() {
                    source.errors.as_deref()
                } else {
                    None
                };
                let reason = if let Some(octocrab::Error::GitHub { source, .. }) = error.api() {
                    limit_reason(s, &source.message)
                } else {
                    LimitReason::Other
                };
                format!(
                    "GitHub HTTP {s}{reason}{}",
                    validation_codes(errors.unwrap_or_default())
                )
            },
        ),
    }
}
/// Name why a 403/429 was refused, from fixed phrases only. The message itself is never kept,
/// since bodies can echo tokens or submitted data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LimitReason {
    Other,
    RateLimit,
    PermissionMissing,
}
impl std::fmt::Display for LimitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Other => "",
            Self::RateLimit => " (rate limit)",
            Self::PermissionMissing => " (App permission missing)",
        })
    }
}
pub(super) fn limit_reason(status: u16, message: &str) -> LimitReason {
    let message = message.to_ascii_lowercase();
    if !matches!(status, 403 | 429) {
        LimitReason::Other
    } else if message.contains("rate limit") || message.contains("abuse detection") {
        LimitReason::RateLimit
    } else if message.contains("not accessible by integration") {
        LimitReason::PermissionMissing
    } else {
        LimitReason::Other
    }
}
/// Whether GitHub refused a feature for the account's plan ("Upgrade to GitHub Pro or make
/// this repository public to enable this feature."), from fixed phrases only.
pub(super) fn plan_refused(status: u16, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    status == 403
        && (message.contains("upgrade to github")
            || message.contains("make this repository public"))
}
/// Whether a failed rerun may succeed later: transport trouble, 429, 5xx or a rate-limited 403.
fn rerun_is_transient(error: &GatewayError) -> bool {
    let Some(octocrab::Error::GitHub { source, .. }) = error.api() else {
        return true;
    };
    let s = source.status_code.as_u16();
    s == 429 || s >= 500 || (s == 403 && limit_reason(s, &source.message) == LimitReason::RateLimit)
}
fn validation_codes(errors: &[Value]) -> String {
    // https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api#validation-failed
    let codes: Vec<String> = errors
        .iter()
        .filter(|e| e.is_object())
        .map(|e| {
            format!(
                "{}.{} {}",
                error_word(&e["resource"]),
                error_word(&e["field"]),
                error_word(&e["code"])
            )
        })
        .collect();
    if codes.is_empty() {
        String::new()
    } else {
        format!(" ({})", codes.join(", "))
    }
}
/// A code, type or name from a GitHub error, kept only when it is one plain word.
/// Digits may follow the first letter, as in `createProjectV2`.
pub(super) fn error_word(value: &Value) -> &str {
    value
        .as_str()
        .filter(|v| {
            (1..=40).contains(&v.len())
                && v.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .unwrap_or("?")
}
const SEGMENT: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
pub(super) fn segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

pub(super) fn text<'a>(v: &'a Value, field: &str) -> Result<&'a str, ForgeError> {
    v.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| refused(format!("GitHub response missing {field}")))
}

// Both App token minting and installation requests use this bounded transport.
// Transport errors are retryable through api_error and the mirror's backoff.
pub(crate) fn github_builder() -> octocrab::OctocrabBuilder<
    octocrab::NoSvc,
    octocrab::DefaultOctocrabBuilderConfig,
    octocrab::NoAuth,
    octocrab::NotLayerReady,
> {
    Octocrab::builder()
        .set_connect_timeout(Some(std::time::Duration::from_secs(10)))
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .set_write_timeout(Some(std::time::Duration::from_secs(30)))
        .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
}

impl GitHub {
    pub fn from_state(state: &Path, wait: Arc<Wait>) -> Result<Self, ForgeError> {
        let cfg: AppConfig = serde_json::from_slice(
            &std::fs::read(state.join("github-app.json"))
                .map_err(|_| refused("cannot read GitHub App configuration"))?,
        )
        .map_err(|_| refused("invalid GitHub App configuration"))?;
        let key = load_app_key(state)?;
        let app = github_builder()
            .app(octocrab::models::AppId(cfg.app_id), key)
            .build()
            .map_err(api_error)?;
        let mut forge = Self::with_app_client(
            app,
            cfg.installation_id,
            cfg.organization,
            cfg.account_type,
            wait,
        )?;
        let ids = match std::fs::read(state.join("github-mirror.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| refused("invalid GitHub mirror receipts"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MirrorIds::default(),
            Err(_) => return Err(refused("cannot read GitHub mirror receipts")),
        };
        forge.mirror_ids = Mutex::new(ids);
        forge.state_dir = Some(state.to_owned());
        Ok(forge)
    }

    /// Inject a bounded App-authenticated client; installation clients reuse its transport.
    pub fn with_app_client(
        app: Octocrab,
        installation: u64,
        org: String,
        account: GithubAccountType,
        wait: Arc<Wait>,
    ) -> Result<Self, ForgeError> {
        if !valid_name(&org) {
            return Err(refused("invalid organization"));
        }
        Ok(Self {
            app: Client {
                api: app,
                wait: wait.clone(),
            },
            wait,
            mirror_ids: Mutex::new(MirrorIds::default()),
            state_dir: None,
            installation,
            org,
            account,
            sessions: tokio::sync::Mutex::new(BTreeMap::new()),
            created: Mutex::new(BTreeMap::new()),
        })
    }

    pub(super) fn repo_name<'a>(&self, repo: &'a Repo) -> Result<&'a str, ForgeError> {
        let Some((owner, name)) = repo.0.split_once('/') else {
            return Err(refused("expected organization/repository"));
        };
        if !owner.eq_ignore_ascii_case(&self.org) || !valid_name(name) {
            return Err(refused("repository outside the installation organization"));
        }
        Ok(name)
    }
    pub(super) fn route(&self, repo: &Repo, suffix: &str) -> Result<String, ForgeError> {
        Ok(format!(
            "/repos/{}/{}{suffix}",
            segment(&self.org),
            segment(self.repo_name(repo)?)
        ))
    }

    /// One token per repository (None: installation-wide) carrying all the App's permissions.
    async fn session(&self, repo: Option<&Repo>) -> Result<Session, ForgeError> {
        let body = match repo {
            Some(repo) => json!({"repositories": [self.repo_name(repo)?]}),
            None => json!({}),
        };
        let key = repo.cloned();
        let mut sessions = self.sessions.lock().await;
        self.wait.check().map_err(api_error)?;
        if let Some(session) = sessions.get(&key)
            && self.wait.clock.now() < session.expires_at.plus_millis(-60_000)
        {
            return Ok(session.clone());
        }
        let response = self
            .app
            .post(
                format!("/app/installations/{}/access_tokens", self.installation),
                Some(&body),
            )
            .await
            .map_err(api_error)?;
        let token = text(&response, "token")?.to_owned();
        let expires_at = stamp_of(text(&response, "expires_at")?)?;
        let session = Session {
            client: self.client_with_token(token.clone())?,
            token,
            expires_at,
        };
        sessions.insert(key, session.clone());
        drop(sessions);
        Ok(session)
    }
    async fn token(&self, repo: Option<&Repo>) -> Result<String, ForgeError> {
        Ok(self.session(repo).await?.token)
    }
    pub(super) async fn client(&self, repo: &Repo) -> Result<Client, ForgeError> {
        Ok(self.session(Some(repo)).await?.client)
    }
    fn client_with_token(&self, token: String) -> Result<Client, ForgeError> {
        Ok(Client {
            api: self.app.api.user_access_token(token).map_err(api_error)?,
            wait: self.wait.clone(),
        })
    }
    pub(super) async fn metadata(&self, repo: &Repo) -> Result<Value, ForgeError> {
        let api = self.client(repo).await?;
        self.metadata_with(repo, &api).await
    }
    async fn metadata_with(&self, repo: &Repo, api: &Client) -> Result<Value, ForgeError> {
        api.get(self.route(repo, "")?).await.map_err(api_error)
    }
    async fn git(
        &self,
        repo: &Repo,
        mirror: &Path,
        args: Vec<String>,
    ) -> Result<String, ForgeError> {
        let token = self.token(Some(repo)).await?;
        self.wait.check().map_err(api_error)?;
        credential_git(mirror, &args, &token).await
    }
    fn git_url(&self, repo: &Repo) -> Result<String, ForgeError> {
        self.repo_name(repo)?;
        Ok(format!("https://github.com/{}.git", repo.0))
    }
    pub(super) async fn list(
        &self,
        api: &Client,
        route: &str,
        field: Option<&str>,
    ) -> Result<Vec<Value>, ForgeError> {
        let mut result = Vec::new();
        for page in 1..=1000 {
            let sep = if route.contains('?') { '&' } else { '?' };
            let v: Value = api
                .get(format!("{route}{sep}per_page=100&page={page}"))
                .await
                .map_err(api_error)?;
            let rows = match field {
                Some(f) => v.get(f),
                None => Some(&v),
            }
            .and_then(Value::as_array)
            .ok_or_else(|| refused("GitHub list response is unreadable"))?;
            result.extend(rows.iter().cloned());
            if rows.len() < 100 {
                return Ok(result);
            }
        }
        Err(refused("GitHub pagination limit exceeded"))
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

// Local commands keep the gateway's inherited-config and credential isolation.
fn local_git(dir: &Path) -> Git {
    let mut git = Git::new(dir);
    git.command()
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "credential.useHttpPath=true",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .env("DVB_GIT_TOKEN", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    git
}

fn local_git_error(error: &GitError) -> ForgeError {
    git_error(error, None)
}

fn git_error(error: &GitError, token: Option<&str>) -> ForgeError {
    let GitError::Failed { stderr, .. } = error else {
        return refused("cannot start local git operation");
    };
    let stderr = String::from_utf8_lossy(stderr);
    let lower = stderr.to_ascii_lowercase();
    let local = token.is_none()
        || [
            "not a git repository",
            "cannot change to",
            "bad object",
            "unknown revision",
            "src refspec",
            "needed a single revision",
        ]
        .iter()
        .any(|reason| lower.contains(reason));
    let rejected = [
        "[rejected]",
        "[remote rejected]",
        "non-fast-forward",
        "gh006",
        "gh013",
        "rule violations",
        "protected branch",
        "stale info",
        "cannot lock ref",
        "couldn't find remote ref",
        "remote ref does not exist",
        "does not match any",
        "needed a single revision",
        "authentication failed",
        "permission denied",
    ]
    .iter()
    .any(|reason| lower.contains(reason));
    let prefix = if local {
        "local git operation failed"
    } else {
        "authenticated git operation failed"
    };
    // Keep rejected/fatal lines, never command arguments, helpers or URLs.
    let lines = stderr
        .lines()
        .filter(|line| {
            let line = line.trim();
            line.starts_with("remote:")
                || line.starts_with("fatal:")
                || line.starts_with("error:")
                || line.contains("[rejected]")
                || line.contains("[remote rejected]")
        })
        .take(8)
        .map(|line| {
            let line = match token.filter(|token| !token.is_empty()) {
                Some(token) => line.replace(token, "[redacted]"),
                None => line.to_owned(),
            };
            line.split_whitespace()
                .map(|word| {
                    if word.contains("://") {
                        "[remote]"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .filter(|c| !c.is_control())
                .take(500)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("; ");
    ForgeError {
        rate_limit: None,
        retryable: !local && !rejected,
        detail: if lines.is_empty() {
            prefix.into()
        } else {
            format!("{prefix}: {lines}")
        },
    }
}

/// The helper reads the token from this command's environment. The token is never
/// an argument or persisted config. Local Git owns config, prompt and trace isolation.
async fn credential_git(dir: &Path, args: &[String], token: &str) -> Result<String, ForgeError> {
    let helper = "!f() { if test \"$1\" = get; then printf '%s\\n' username=x-access-token \
                \"password=$DVB_GIT_TOKEN\"; fi; }; f";
    let mut git = local_git(dir);
    git.command()
        .args([
            "-c",
            &format!("credential.helper={helper}"),
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
        ])
        .env("DVB_GIT_TOKEN", token);
    git.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), &[])
        .await
        .map_err(|error| git_error(&error, Some(token)))
}

#[async_trait::async_trait]
impl Forge for GitHub {
    async fn accessible_repositories(
        &self,
    ) -> Result<dvb_core::api::GithubRepositories, ForgeError> {
        use dvb_core::api::{GithubRepositories, GithubRepository};
        let api = self.session(None).await?.client;
        let rows = self
            .list(&api, "/installation/repositories", Some("repositories"))
            .await?;
        let mut repositories = Vec::new();
        for row in rows {
            let repo = text(&row, "full_name")?.to_owned();
            self.repo_name(&Repo(repo.clone()))?;
            repositories.push(GithubRepository {
                repo,
                private: row["private"]
                    .as_bool()
                    .ok_or_else(|| refused("repository visibility missing"))?,
                default_branch: text(&row, "default_branch")?.into(),
            });
        }
        repositories.sort_by(|a, b| a.repo.cmp(&b.repo));
        Ok(GithubRepositories {
            organization: self.org.clone(),
            repository_access_url: crate::github_setup::installation_settings_url(
                self.account,
                &self.org,
                self.installation,
            ),
            repositories,
        })
    }
    async fn discover_checks(&self, repo: &Repo) -> dvb_core::api::GithubCheckDiscovery {
        use dvb_core::api::{GithubCheckDiscovery as Discovery, GithubFailureCode};
        let inaccessible = || Discovery::Inaccessible {
            failure: crate::github_setup::setup_failure(GithubFailureCode::RepositoryInaccessible),
        };
        let failed = |retryable| {
            let mut failure =
                crate::github_setup::setup_failure(GithubFailureCode::DiscoveryFailed);
            failure.retryable = retryable;
            Discovery::Failed { failure }
        };
        let api = match self.session(None).await {
            Ok(session) => session.client,
            Err(error) => return failed(error.retryable),
        };
        let route = match self.route(repo, "") {
            Ok(route) => route,
            Err(_) => return inaccessible(),
        };
        let metadata: Value = match api.get(&route).await {
            Ok(value) => value,
            Err(error) => {
                return if matches!(http_status(&error), Some(401 | 403 | 404))
                    && !matches!(error, GatewayError::Wait(..))
                {
                    inaccessible()
                } else {
                    failed(api_error(error).retryable)
                };
            }
        };
        let Some(branch) = metadata["default_branch"]
            .as_str()
            .filter(|s| !s.is_empty())
        else {
            return failed(false);
        };
        let commit: Value = match api
            .get(format!("{route}/commits/{}", segment(branch)))
            .await
        {
            Ok(value) => value,
            Err(error) => return failed(api_error(error).retryable),
        };
        let Some(head) = commit["sha"].as_str().filter(|s| !s.is_empty()) else {
            return failed(false);
        };
        let mut runs = match self
            .list(
                &api,
                &format!("{route}/commits/{}/check-runs?filter=latest", segment(head)),
                Some("check_runs"),
            )
            .await
        {
            Ok(runs) => runs,
            Err(error) => return failed(error.retryable),
        };
        // GitHub can return several suites/apps with the same required-check name.
        runs.sort_by_key(|run| std::cmp::Reverse(run["id"].as_u64().unwrap_or(0)));
        let mut checks = BTreeMap::new();
        for run in runs {
            let Some(name) = run["name"].as_str().filter(|s| !s.trim().is_empty()) else {
                return failed(false);
            };
            if run["head_sha"] != head || run["id"].as_u64().is_none() {
                return failed(false);
            }
            let status = check_status(&run);
            checks
                .entry(name.to_owned())
                .or_insert_with(|| CheckObservation {
                    name: name.into(),
                    status,
                    run: run["id"].as_u64(),
                    conclusion: run["conclusion"].as_str().map(str::to_owned),
                    url: run["html_url"]
                        .as_str()
                        .filter(|url| url.starts_with("https://github.com/"))
                        .map(str::to_owned),
                });
        }
        Discovery::Discovered {
            default_branch: branch.into(),
            head: head.into(),
            checks: checks.into_values().collect(),
        }
    }

    async fn mirror_project(&self, repo: &Repo) -> Result<(), ForgeError> {
        self.prepare_board(repo).await
    }
    async fn mirror_goal(
        &self,
        repo: &Repo,
        goal: &dvb_core::forge::GoalMirror,
    ) -> Result<u64, ForgeError> {
        self.sync_goal(repo, goal).await
    }
    async fn mirror_task(
        &self,
        repo: &Repo,
        task: &dvb_core::forge::TaskMirror,
    ) -> Result<(), ForgeError> {
        self.sync_task(repo, task).await
    }

    fn can_create_repo(&self) -> bool {
        // GitHub serves `POST /user/repos` only to user tokens, never to an
        // installation, so an App creates repositories only in an organization.
        self.account == GithubAccountType::Organization
    }

    async fn create_repo(&self, org: &str, name: &str) -> Result<Repo, ForgeError> {
        if !self.can_create_repo() {
            return Err(refused(
                "an App cannot create a personal account's repository",
            ));
        }
        if org != self.org || !valid_name(name) {
            return Err(refused("invalid repository or organization"));
        }
        let api = self.session(None).await?.client;
        let v: Value = api
            .post(
                format!("/orgs/{}/repos", segment(org)),
                Some(&json!({
                    "name":name,
                    "private":true,
                    "auto_init":false,
                    "allow_squash_merge":true,
                    "allow_merge_commit":false,
                    "allow_rebase_merge":false
                })),
            )
            .await
            .map_err(api_error)?;
        if v["private"] != true || text(&v, "full_name")? != format!("{org}/{name}") {
            return Err(refused(
                "GitHub did not create the expected private repository",
            ));
        }
        let repo = Repo(format!("{org}/{name}"));
        self.created
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .insert(
                repo.clone(),
                v["id"]
                    .as_u64()
                    .ok_or_else(|| refused("repository id missing"))?,
            );
        Ok(repo)
    }
    fn created_repo_id(&self, repo: &Repo) -> Option<u64> {
        self.created
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .get(repo)
            .copied()
    }
    async fn resume_setup(&self, repo: &Repo, repository_id: u64) -> Result<(), ForgeError> {
        let metadata = self.metadata(repo).await?;
        if metadata["id"].as_u64() != Some(repository_id) {
            return Err(refused("repository differs from the recorded creation"));
        }
        self.created
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .insert(repo.clone(), repository_id);
        Ok(())
    }
    async fn clone_repo(&self, repo: &Repo, mirror: &Path) -> Result<String, ForgeError> {
        let api = self.client(repo).await?;
        let metadata = self.metadata_with(repo, &api).await?;
        let branch = text(&metadata, "default_branch")?.to_owned();
        let parent = mirror
            .parent()
            .ok_or_else(|| refused("mirror has no parent"))?;
        self.git(
            repo,
            parent,
            vec![
                "clone".into(),
                "--quiet".into(),
                "--bare".into(),
                self.git_url(repo)?,
                mirror.to_string_lossy().into(),
            ],
        )
        .await?;
        local_git(mirror)
            .run(
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
                &[],
            )
            .await
            .map_err(|error| local_git_error(&error))?;
        Ok(branch)
    }
    async fn commit_author(
        &self,
        repo: &Repo,
        sha: &str,
    ) -> Result<dvb_core::api::CommitIdentity, ForgeError> {
        let api = self.client(repo).await?;
        let sha = utf8_percent_encode(sha, NON_ALPHANUMERIC);
        let commit = api
            .get(self.route(repo, &format!("/git/commits/{sha}"))?)
            .await
            .map_err(api_error)?;
        Ok(dvb_core::api::CommitIdentity {
            name: text(&commit["author"], "name")?.into(),
            email: text(&commit["author"], "email")?.into(),
        })
    }
    async fn fetch(&self, repo: &Repo, mirror: &Path, branch: &str) -> Result<String, ForgeError> {
        self.git(
            repo,
            mirror,
            vec![
                "fetch".into(),
                "--quiet".into(),
                "--no-tags".into(),
                self.git_url(repo)?,
                "+refs/heads/*:refs/heads/*".into(),
                "+refs/tags/*:refs/tags/*".into(),
            ],
        )
        .await?;
        local_git(mirror)
            .run(
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
                &[],
            )
            .await
            .map_err(|error| local_git_error(&error))
    }
    async fn push(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        if !branch.starts_with("dvb/") {
            return Err(refused("gateway only pushes dvb task branches"));
        }
        self.git(
            repo,
            mirror,
            vec![
                "push".into(),
                "--quiet".into(),
                self.git_url(repo)?,
                format!("{head}:refs/heads/{branch}"),
            ],
        )
        .await?;
        Ok(())
    }
    async fn push_setup(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError> {
        let id = self
            .created_repo_id(repo)
            .ok_or_else(|| refused("setup push requires a recorded repository creation"))?;
        let api = self.client(repo).await?;
        let metadata = self.metadata_with(repo, &api).await?;
        if metadata["id"].as_u64() != Some(id) {
            return Err(refused("repository differs from the recorded creation"));
        }
        if branch != text(&metadata, "default_branch")? {
            return Err(refused("setup branch must be the default branch"));
        }
        // Empty-ref lease refuses even if another writer initialized the repo meanwhile.
        self.git(
            repo,
            mirror,
            vec![
                "push".into(),
                "--quiet".into(),
                format!("--force-with-lease=refs/heads/{branch}:"),
                self.git_url(repo)?,
                format!("{head}:refs/heads/{branch}"),
            ],
        )
        .await?;
        self.created
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .remove(repo);
        Ok(())
    }
    async fn open_pr(
        &self,
        repo: &Repo,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<Pr, ForgeError> {
        let api = self.client(repo).await?;
        let metadata = self.metadata_with(repo, &api).await?;
        let base = text(&metadata, "default_branch")?;
        let v: Value = api
            .post(
                self.route(repo, "/pulls")?,
                Some(&json!({"head":branch,"base":base,"title":title,"body":body})),
            )
            .await
            .map_err(api_error)?;
        Ok(Pr {
            number: v["number"]
                .as_u64()
                .ok_or_else(|| refused("PR number missing"))?,
            url: text(&v, "html_url")?.into(),
        })
    }
    async fn edit_pr_body(&self, repo: &Repo, pr: u64, body: &str) -> Result<(), ForgeError> {
        let api = self.client(repo).await?;
        let _: Value = api
            .patch(
                self.route(repo, &format!("/pulls/{pr}"))?,
                Some(&json!({"body":body})),
            )
            .await
            .map_err(api_error)?;
        Ok(())
    }
    async fn checks(&self, repo: &Repo, head: &str) -> Result<Vec<CheckObservation>, ForgeError> {
        let api = self.client(repo).await?;
        let route = self.route(
            repo,
            &format!("/commits/{}/check-runs?filter=latest", segment(head)),
        )?;
        let mut runs = self.list(&api, &route, Some("check_runs")).await?;
        // Commit-level latest checks can belong to manual or scheduled workflows.
        // Read PR and push suites directly so those workflows cannot hide them.
        runs.retain(|run| run["app"]["slug"] != "github-actions");
        let workflows = self
            .list(
                &api,
                &self.route(repo, &format!("/actions/runs?head_sha={}", segment(head)))?,
                Some("workflow_runs"),
            )
            .await?;
        // Runs are newest first; an older run must not revive a replaced failure.
        // Per workflow the newest PR run counts, or the newest push run when the
        // workflow has no PR run on this head. Other events never count.
        let mut chosen: Vec<(u64, bool, Value)> = Vec::new();
        for workflow in workflows {
            let pull_request = match workflow["event"].as_str() {
                Some("pull_request") => true,
                Some("push") => false,
                _ => continue,
            };
            if workflow["head_sha"] != head {
                continue;
            }
            let id = workflow["workflow_id"]
                .as_u64()
                .ok_or_else(|| refused("workflow id missing"))?;
            match chosen.iter_mut().find(|(seen, _, _)| *seen == id) {
                None => chosen.push((id, pull_request, workflow)),
                Some(entry) if pull_request && !entry.1 => *entry = (id, true, workflow),
                Some(_) => {}
            }
        }
        for (_, _, workflow) in chosen {
            let suite = workflow["check_suite_id"]
                .as_u64()
                .ok_or_else(|| refused("workflow check suite id missing"))?;
            runs.extend(
                self.list(
                    &api,
                    &self.route(
                        repo,
                        &format!("/check-suites/{suite}/check-runs?filter=latest"),
                    )?,
                    Some("check_runs"),
                )
                .await?,
            );
        }
        let statuses = self
            .list(
                &api,
                &self.route(repo, &format!("/commits/{}/statuses", segment(head)))?,
                None,
            )
            .await?;
        let mut result = Vec::new();
        for run in runs {
            let status = if run["head_sha"] != head {
                ChecksStatus::Unreadable
            } else {
                check_status(&run)
            };
            result.push(CheckObservation {
                conclusion: run["conclusion"].as_str().map(str::to_owned),
                url: run["html_url"].as_str().map(str::to_owned),
                name: text(&run, "name")?.into(),
                status,
                run: (status == ChecksStatus::Failed)
                    .then(|| run["id"].as_u64())
                    .flatten(),
            });
        }
        // Statuses are reverse chronological. Only the newest per context counts.
        let mut seen = BTreeSet::new();
        for status in statuses {
            let name = text(&status, "context")?.to_owned();
            if !seen.insert(name.clone()) {
                continue;
            }
            let value = match status["state"].as_str() {
                Some("success") => ChecksStatus::Success,
                Some("pending") => ChecksStatus::Pending,
                Some("failure" | "error") => ChecksStatus::Failed,
                _ => ChecksStatus::Unreadable,
            };
            result.push(CheckObservation {
                conclusion: status["state"].as_str().map(str::to_owned),
                url: status["target_url"].as_str().map(str::to_owned),
                name,
                status: value,
                run: None,
            });
        }
        Ok(result)
    }
    async fn main_checks(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Vec<CheckObservation>, ForgeError> {
        let api = self.client(repo).await?;
        let commit: Value = api
            .get(self.route(repo, &format!("/commits/{}", segment(branch)))?)
            .await
            .map_err(api_error)?;
        let head = text(&commit, "sha")?;
        let runs = self
            .list(
                &api,
                &self.route(
                    repo,
                    &format!("/actions/runs?head_sha={}&status=completed", segment(head)),
                )?,
                Some("workflow_runs"),
            )
            .await?;
        let mut workflows = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut checks = Vec::new();
        for run in runs {
            // Manual and scheduled runs, and runs on older commits, are not
            // main's CI: without a run of main's own, its state is unknown.
            if run["head_sha"] != head
                || !matches!(run["event"].as_str(), Some("push" | "pull_request"))
            {
                continue;
            }
            let workflow = run["workflow_id"]
                .as_u64()
                .ok_or_else(|| refused("main CI workflow id missing"))?;
            if !workflows.insert(workflow) {
                continue;
            }
            let id = run["id"]
                .as_u64()
                .ok_or_else(|| refused("main CI run id missing"))?;
            let url = text(&run, "html_url")?;
            let mut jobs = self
                .list(
                    &api,
                    &self.route(repo, &format!("/actions/runs/{id}/jobs?filter=all"))?,
                    Some("jobs"),
                )
                .await?;
            // Partial reruns omit jobs that passed; retain each job's newest attempt.
            jobs.sort_by_key(|job| std::cmp::Reverse(job["run_attempt"].as_u64().unwrap_or(1)));
            for job in jobs {
                let name = text(&job, "name")?.to_owned();
                if !names.insert(name.clone()) {
                    continue;
                }
                let status = if job["status"] != "completed" {
                    ChecksStatus::Unreadable
                } else {
                    check_status(&job)
                };
                checks.push(CheckObservation {
                    name,
                    status,
                    run: None,
                    conclusion: job["conclusion"].as_str().map(str::to_owned),
                    url: Some(url.into()),
                });
            }
        }
        Ok(checks)
    }

    async fn rerun_checks(
        &self,
        repo: &Repo,
        head: &str,
        runs: &[u64],
        recovery: bool,
    ) -> Result<(), ForgeError> {
        let api = self.client(repo).await?;
        // Throttling and outages are worth asking again; other refusals
        // (403, 404, 409, 422) will not change.
        let final_unless_transient = |e: GatewayError, what: &str| {
            if rerun_is_transient(&e) {
                api_error(e)
            } else {
                refused(format!("GitHub refused {what}"))
            }
        };
        // The jobs' workflow runs, each rerun once as a whole: GitHub reruns
        // every failed and cancelled job of a run together, and counts the
        // run's attempts, so nothing is kept here.
        let mut workflows = std::collections::BTreeMap::new();
        let mut repeated = Vec::new();
        for id in runs {
            // An Actions job's id is its check run's id; only Actions jobs can
            // be rerun with the App's token (another app's check has no job).
            let job: Value = api
                .get(self.route(repo, &format!("/actions/jobs/{id}"))?)
                .await
                .map_err(|e| final_unless_transient(e, "reading the failed check's job"))?;
            let name = job["name"].as_str().unwrap_or("a check");
            if job["head_sha"] != head {
                return Err(refused(format!("{name} is not a job on the head")));
            }
            // A job of a later attempt is itself a rerun, ended so again.
            let attempt = job["run_attempt"].as_u64().unwrap_or(1);
            if attempt > 1 && !recovery {
                repeated.push(name.to_owned());
                continue;
            }
            let run_id = job["run_id"]
                .as_u64()
                .ok_or_else(|| refused("the failed check's job has no workflow run"))?;
            workflows.insert(run_id, attempt);
        }
        let mut pending = false;
        for (run_id, attempt) in workflows {
            let run: Value = api
                .get(self.route(repo, &format!("/actions/runs/{run_id}"))?)
                .await
                .map_err(|e| {
                    final_unless_transient(e, "reading the failed check's workflow run")
                })?;
            if run["status"] != "completed" {
                pending = true;
                continue;
            }
            // A later attempt needs no request; a known second failure is final
            // once the workflow has completed.
            if run["run_attempt"].as_u64().unwrap_or(1) > attempt {
                continue;
            }
            // `_post` hands back unsuccessful statuses as `Ok`; map them to errors first.
            let response = match api
                .post_raw(self.route(repo, &format!("/actions/runs/{run_id}/rerun-failed-jobs"))?)
                .await
            {
                Ok(r) => octocrab::map_github_error(r)
                    .await
                    .map_err(GatewayError::Api),
                Err(e) => Err(e),
            };
            response.map_err(|e| final_unless_transient(e, "the check rerun"))?;
            pending = true;
        }
        if !pending && !repeated.is_empty() {
            return Err(refused(format!(
                "{} failed again after its rerun",
                repeated.join(", ")
            )));
        }
        Ok(())
    }
    async fn cancel_checks(&self, repo: &Repo, head: &str) -> Result<(), ForgeError> {
        let api = self.client(repo).await?;
        let runs = self
            .list(
                &api,
                &self.route(repo, &format!("/actions/runs?head_sha={}", segment(head)))?,
                Some("workflow_runs"),
            )
            .await?;
        for run in runs.iter().filter(|run| run["status"] != "completed") {
            let id = run["id"]
                .as_u64()
                .ok_or_else(|| refused("workflow run id missing"))?;
            let response = api
                .post_raw(self.route(repo, &format!("/actions/runs/{id}/cancel"))?)
                .await
                .map_err(api_error)?;
            octocrab::map_github_error(response)
                .await
                .map_err(api_error)?;
        }
        Ok(())
    }
    async fn check_excerpt(&self, repo: &Repo, run: u64) -> Result<String, ForgeError> {
        use http_body_util::BodyExt;
        let api = self.client(repo).await?;
        // An Actions job's id is its check run's id. GitHub redirects to the
        // log's storage; the client drops credentials leaving its origin.
        let response = api
            .get_raw(self.route(repo, &format!("/actions/jobs/{run}/logs"))?)
            .await
            .map_err(api_error)?;
        let response = octocrab::map_github_error(response)
            .await
            .map_err(api_error)?;
        let log = http_body_util::Limited::new(response.into_body(), LOG_BYTES)
            .collect()
            .await
            .map_err(|_| refused("the check's log is unreadable"))?
            .to_bytes();
        Ok(log_excerpt(&String::from_utf8_lossy(&log)))
    }
    async fn merge(
        &self,
        repo: &Repo,
        pr: u64,
        sha: &str,
        title: &str,
    ) -> Result<String, MergeError> {
        let title = format!("{title} (#{pr})");
        let api = self.client(repo).await.map_err(MergeError::Forge)?;
        let route = self
            .route(repo, &format!("/pulls/{pr}/merge"))
            .map_err(MergeError::Forge)?;
        let response: Result<Value, _> = api
            .put(
                route,
                Some(&json!({
                    "sha":sha,
                    "merge_method":"squash",
                    "commit_title":title,
                    "commit_message":format!("Reviewed publication {sha}.")
                })),
            )
            .await;
        match response {
            Ok(v) if v["merged"] == true => text(&v, "sha")
                .map(str::to_owned)
                .map_err(MergeError::Forge),
            Ok(_) => Err(MergeError::Refused("GitHub refused the merge".into())),
            Err(e) if http_status(&e) == Some(409) => Err(MergeError::HeadMoved),
            Err(e) if matches!(http_status(&e), Some(405 | 422)) => {
                Err(MergeError::Refused("GitHub refused the merge".into()))
            }
            Err(e) => Err(MergeError::Forge(api_error(e))),
        }
    }
    async fn close_issue(&self, repo: &Repo, number: u64) -> Result<(), ForgeError> {
        let api = self.client(repo).await?;
        let _: Value = api
            .patch(
                self.route(repo, &format!("/issues/{number}"))?,
                Some(&json!({"state":"closed"})),
            )
            .await
            .map_err(api_error)?;
        Ok(())
    }
    async fn issue(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Option<dvb_core::forge::Issue>, dvb_core::forge::IssueError> {
        self.read_issue(repo, number).await
    }
    async fn open_issues(
        &self,
        repo: &Repo,
    ) -> Result<Vec<dvb_core::forge::IssueSummary>, ForgeError> {
        self.list_open_issues(repo).await
    }
    async fn visibility(&self, repo: &Repo) -> Result<dvb_core::forge::Visibility, ForgeError> {
        self.read_visibility(repo).await
    }
    async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<(), ForgeError> {
        if !branch.starts_with("dvb/") {
            return Err(refused("only dvb branches may be deleted"));
        }
        let api = self.client(repo).await?;
        // A missing repository fails here; the ref delete reports it as a gone branch.
        self.metadata_with(repo, &api).await?;
        let response = api
            .delete_raw(self.route(repo, &format!("/git/refs/heads/{}", segment(branch)))?)
            .await
            .map_err(api_error)?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) || status == 404 {
            return Ok(());
        }
        if status == 422 {
            // GitHub returns 422 for a previously deleted ref. Confirm absence,
            // rather than matching the human-readable response message.
            let read = api
                .get_raw(self.route(repo, &format!("/git/ref/heads/{}", segment(branch)))?)
                .await
                .map_err(api_error)?;
            if read.status().as_u16() == 404 {
                return Ok(());
            }
        }
        Err(ForgeError {
            rate_limit: None,
            retryable: status == 429 || status >= 500,
            detail: format!("GitHub HTTP {status}"),
        })
    }
    async fn ruleset(&self, repo: &Repo) -> Result<ExpectedChecks, ForgeError> {
        let api = self.client(repo).await?;
        let metadata = self.metadata_with(repo, &api).await?;
        let branch = text(&metadata, "default_branch")?;
        // Effective rules include inherited organization rules and branch conditions.
        let rules = self
            .list(
                &api,
                &self.route(repo, &format!("/rules/branches/{}", segment(branch)))?,
                None,
            )
            .await?;
        let mut result = ExpectedChecks::default();
        for rule in rules {
            match rule["type"].as_str() {
                Some("pull_request") => result.pr_required = true,
                Some("non_fast_forward") => result.no_force_push = true,
                Some("required_status_checks") => {
                    let checks = rule["parameters"]["required_status_checks"]
                        .as_array()
                        .ok_or_else(|| refused("required checks are unreadable"))?;
                    for check in checks {
                        result.required_checks.push(text(check, "context")?.into());
                    }
                }
                _ => {}
            }
        }
        result.required_checks.sort();
        result.required_checks.dedup();
        Ok(result)
    }
    async fn rulesets_allowed(&self, repo: &Repo) -> Result<bool, ForgeError> {
        let api = self.client(repo).await?;
        match api
            .get(self.route(repo, "/rulesets?includes_parents=false&per_page=1")?)
            .await
        {
            Ok(_) => Ok(true),
            Err(error) => match error.api() {
                Some(octocrab::Error::GitHub { source, .. })
                    if plan_refused(source.status_code.as_u16(), &source.message) =>
                {
                    Ok(false)
                }
                Some(_) | None => Err(api_error(error)),
            },
        }
    }
    async fn install_ruleset(
        &self,
        repo: &Repo,
        branch: &str,
        checks: &[String],
    ) -> Result<(), ForgeError> {
        let api = self.client(repo).await?;
        let mut rules = vec![
            json!({
                "type":"pull_request",
                "parameters":{
                    "required_approving_review_count":0,
                    "dismiss_stale_reviews_on_push":true,
                    "require_code_owner_review":false,
                    "require_last_push_approval":false,
                    "required_review_thread_resolution":false
                }
            }),
            json!({"type":"non_fast_forward"}),
            json!({"type":"deletion"}),
        ];
        if !checks.is_empty() {
            rules.push(json!({
                "type":"required_status_checks",
                "parameters":{
                    "strict_required_status_checks_policy":true,
                    "required_status_checks":checks.iter().map(|c| json!({
                        "context":c
                    })).collect::<Vec<_>>()
                }
            }));
        }
        let body = json!({
            "name":"dvb default branch",
            "target":"branch",
            "enforcement":"active",
            "bypass_actors":[
            ],
            "conditions":{
                "ref_name":{
                    "include":[
                        format!("refs/heads/{branch}")
                    ],
                    "exclude":[
                    ]
                }
            },
            "rules":rules
        });
        // Reuse our named ruleset on retry; do not overwrite unrelated protections.
        let existing = self
            .list(
                &api,
                &self.route(repo, "/rulesets?includes_parents=false")?,
                None,
            )
            .await?;
        if let Some(rule) = existing.iter().find(|v| v["name"] == "dvb default branch") {
            let id = rule["id"]
                .as_u64()
                .ok_or_else(|| refused("ruleset id missing"))?;
            let _: Value = api
                .put(self.route(repo, &format!("/rulesets/{id}"))?, Some(&body))
                .await
                .map_err(api_error)?;
        } else {
            let _: Value = api
                .post(self.route(repo, "/rulesets")?, Some(&body))
                .await
                .map_err(api_error)?;
        }
        Ok(())
    }
    async fn pr(&self, repo: &Repo, number: u64) -> Result<PrObservation, ForgeError> {
        let api = self.client(repo).await?;
        let v: Value = api
            .get(self.route(repo, &format!("/pulls/{number}"))?)
            .await
            .map_err(api_error)?;
        observation(&v)
    }
    async fn find_pr(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Option<PrObservation>, ForgeError> {
        let api = self.client(repo).await?;
        let route = self.route(
            repo,
            &format!(
                "/pulls?state=all&sort=created&direction=desc&head={}",
                segment(&format!("{}:{branch}", self.org))
            ),
        )?;
        let rows = self.list(&api, &route, None).await?;
        rows.iter()
            .find(|v| v["head"]["ref"] == branch)
            .map(observation)
            .transpose()
    }
}
/// A check run's or Actions job's status and conclusion.
fn check_status(run: &Value) -> ChecksStatus {
    match run["status"].as_str() {
        Some("queued" | "in_progress" | "waiting" | "requested" | "pending") => {
            ChecksStatus::Pending
        }
        Some("completed") => match run["conclusion"].as_str() {
            Some("success") => ChecksStatus::Success,
            Some("skipped") => ChecksStatus::Missing,
            Some(
                "failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure"
                | "stale" | "neutral",
            ) => ChecksStatus::Failed,
            Some(_) | None => ChecksStatus::Unreadable,
        },
        Some(_) | None => ChecksStatus::Unreadable,
    }
}

pub(crate) fn stamp_of(value: &str) -> Result<Timestamp, ForgeError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| Timestamp(date.timestamp_millis()))
        .map_err(|_| refused("GitHub time is unreadable"))
}

fn observation(v: &Value) -> Result<PrObservation, ForgeError> {
    let state = if v["merged"] == true || !v["merged_at"].is_null() {
        PrState::Merged
    } else {
        match v["state"].as_str() {
            Some("open") => PrState::Open,
            Some("closed") => PrState::Closed,
            _ => return Err(refused("unreadable PR state")),
        }
    };
    Ok(PrObservation {
        number: v["number"]
            .as_u64()
            .ok_or_else(|| refused("PR number missing"))?,
        url: text(v, "html_url")?.into(),
        state,
        branch: text(&v["head"], "ref")?.into(),
        head: text(&v["head"], "sha")?.into(),
        base: text(&v["base"], "ref")?.into(),
        merge_commit: v["merge_commit_sha"].as_str().map(str::to_owned),
        conflicting: if v["mergeable_state"] == "dirty" {
            Some(true)
        } else if v["mergeable_state"] == "unknown" || v["mergeable"].is_null() {
            None
        } else {
            Some(false)
        },
    })
}

/// A job log read for its excerpt; a larger one is unreadable.
const LOG_BYTES: usize = 64 * 1024 * 1024;
const EXCERPT_LINES: usize = 8;
const EXCERPT_LINE_CHARS: usize = 200;

/// The last lines up to a job log's first error marker (or its end), joined
/// on one line: timestamps, group markers and terminal colors dropped, each
/// line bounded, and any line a secret or path rule matches left out. The
/// owner's name and email are no rule here: dvb does not write the log.
fn log_excerpt(log: &str) -> String {
    let lines: Vec<String> = log
        .lines()
        .map(|line| {
            let line = line.trim_start_matches('\u{feff}');
            let line = match line.split_once(' ') {
                Some((stamp, rest))
                    if stamp.ends_with('Z')
                        && stamp.contains('T')
                        && stamp.starts_with(|c: char| c.is_ascii_digit()) =>
                {
                    rest
                }
                Some(_) | None => line,
            };
            dvb_core::text::plain(line)
        })
        .filter(|line| {
            !line.trim().is_empty()
                && !line.starts_with("##[group]")
                && !line.starts_with("##[endgroup]")
        })
        .collect();
    let end = lines
        .iter()
        .position(|line| line.starts_with("##[error]"))
        .map_or(lines.len(), |i| i + 1);
    lines[end.saturating_sub(EXCERPT_LINES)..end]
        .iter()
        .filter(|line| ScanPolicy::default().scan("check log", line).is_none())
        .map(|line| {
            let line = line.strip_prefix("##[error]").unwrap_or(line).trim();
            if line.chars().count() > EXCERPT_LINE_CHARS {
                format!(
                    "{}…",
                    line.chars().take(EXCERPT_LINE_CHARS).collect::<String>()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn publication_author_is_read_by_sha_with_the_repository_token_and_keeps_read_errors() {
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        let sha = "a".repeat(40);
        let commit_route = format!("/repos/org/repo/git/commits/{sha}");
        let response = Arc::new(Mutex::new((
            200,
            json!({
                "author": {"name":"publisher", "email":"publisher@example.invalid"},
                "committer": {"name":"App", "email":"app@example.invalid"},
            }),
        )));
        let service = tower::service_fn({
            let response = response.clone();
            move |request: http::Request<octocrab::OctoBody>| {
                let commit_route = commit_route.clone();
                let response = response.clone();
                async move {
                    let (status, body) = if request.uri().path().ends_with("/access_tokens") {
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        let body: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(body, json!({"repositories":["repo"]}));
                        (
                            201,
                            json!({"token":"offline-token","expires_at":"2030-01-01T00:00:00Z"}),
                        )
                    } else {
                        assert_eq!(request.method(), http::Method::GET);
                        assert_eq!(request.uri().path(), commit_route);
                        response.lock().unwrap().clone()
                    };
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(status)
                            .body(Full::new(Bytes::from(body.to_string())))
                            .unwrap(),
                    )
                }
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let wait = Arc::new(Wait {
            clock: Arc::new(dvb_core::FakeClock::at(Timestamp(0))),
            deadline: Mutex::new(None),
        });
        let forge =
            GitHub::with_app_client(app, 7, "org".into(), GithubAccountType::Organization, wait)
                .unwrap();
        let repo = Repo("org/repo".into());
        assert_eq!(
            forge.commit_author(&repo, &sha).await.unwrap(),
            dvb_core::api::CommitIdentity {
                name: "publisher".into(),
                email: "publisher@example.invalid".into(),
            }
        );
        *response.lock().unwrap() = (200, json!({"author":{"name":"publisher"}}));
        assert!(forge.commit_author(&repo, &sha).await.is_err());
        *response.lock().unwrap() = (404, json!({"message":"Not Found"}));
        assert!(forge.commit_author(&repo, &sha).await.is_err());
    }
    #[test]
    fn a_log_excerpt_ends_at_the_first_error_bounded_and_scanned() {
        let token = format!("ghp_{}", "a".repeat(36));
        let mut log =
            String::from("\u{feff}2026-10-18T04:20:40.1Z ##[group]Run scripts/check.sh\n");
        for i in 0..20 {
            log.push_str(&format!("2026-10-18T04:20:41.1Z line {i}\n"));
        }
        log.push_str(&format!("2026-10-18T04:20:42.1Z leaked {token}\n"));
        log.push_str("2026-10-18T04:20:42.2Z Author: Owner Person <owner@example.com>\n");
        log.push_str("2026-10-18T04:20:43.1Z \u{1b}[1m\u{1b}[91merror\u{1b}[0m: this `if` can be collapsed\n");
        log.push_str(&format!("2026-10-18T04:20:44.1Z {}\n", "x".repeat(300)));
        log.push_str("2026-10-18T04:20:45.1Z ##[endgroup]\n");
        log.push_str("2026-10-18T04:20:46.1Z ##[error]Process completed with exit code 101.\n");
        log.push_str("2026-10-18T04:20:47.1Z Post job cleanup.\n");
        let excerpt = log_excerpt(&log);
        // The token's line goes; a line naming a person stays.
        assert_eq!(
            excerpt,
            format!(
                "line 17 / line 18 / line 19 / Author: Owner Person <owner@example.com> / \
                 error: this `if` can be collapsed / {}… / Process completed with exit code 101.",
                "x".repeat(200)
            )
        );
        // With no error marker, the log's last lines.
        assert_eq!(log_excerpt("a\n\nb\n"), "a / b");
    }

    #[tokio::test]
    async fn cached_tokens_expire_and_isolate_repositories() {
        use bytes::Bytes;
        use http_body_util::Full;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let service = tower::service_fn(move |request: http::Request<octocrab::OctoBody>| {
            assert!(request.uri().path().ends_with("/access_tokens"));
            count.fetch_add(1, Ordering::SeqCst);
            async {
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(201)
                        .body(Full::new(Bytes::from_static(
                            br#"{"token":"offline-token","expires_at":"1970-01-01T00:10:00Z"}"#,
                        )))
                        .unwrap(),
                )
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let clock = Arc::new(dvb_core::FakeClock::at(Timestamp(0)));
        let wait = Arc::new(Wait {
            clock: clock.clone(),
            deadline: Mutex::new(None),
        });
        let forge = GitHub::with_app_client(
            app,
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            wait,
        )
        .unwrap();
        let repo = Repo("org/one".into());
        assert_eq!(forge.token(Some(&repo)).await.unwrap(), "offline-token");
        forge.client(&repo).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        forge.client(&Repo("org/two".into())).await.unwrap();
        forge.token(None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let fresh = Repo("org/concurrent".into());
        let (one, two) = tokio::join!(forge.client(&fresh), forge.client(&fresh));
        one.unwrap();
        two.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        clock.set(Timestamp(539_999));
        forge.client(&repo).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        clock.set(Timestamp(540_000));
        forge.client(&repo).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_free_plan_is_read_from_the_refused_rulesets_and_other_refusals_stay_errors() {
        use bytes::Bytes;
        use http_body_util::Full;
        let service = tower::service_fn(move |request: http::Request<octocrab::OctoBody>| {
            let route = request.uri().path().to_owned();
            async move {
                let (status, body) = if route.ends_with("/access_tokens") {
                    (
                        201,
                        json!({"token":"offline-token","expires_at":"2030-01-01T00:00:00Z"}),
                    )
                } else if route == "/repos/org/free/rulesets" {
                    (
                        403,
                        json!({"message":"Upgrade to GitHub Pro or make this repository public to enable this feature."}),
                    )
                } else if route == "/repos/org/hidden/rulesets" {
                    (
                        403,
                        json!({"message":"Resource not accessible by integration"}),
                    )
                } else {
                    (200, json!([]))
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .body(Full::new(Bytes::from(body.to_string())))
                        .unwrap(),
                )
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let wait = Arc::new(Wait {
            clock: Arc::new(dvb_core::FakeClock::at(Timestamp(0))),
            deadline: Mutex::new(None),
        });
        let forge = GitHub::with_app_client(
            app,
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            wait,
        )
        .unwrap();
        assert!(
            forge
                .rulesets_allowed(&Repo("org/paid".into()))
                .await
                .unwrap()
        );
        assert!(
            !forge
                .rulesets_allowed(&Repo("org/free".into()))
                .await
                .unwrap()
        );
        let error = forge
            .rulesets_allowed(&Repo("org/hidden".into()))
            .await
            .unwrap_err();
        assert_eq!(error.detail, "GitHub HTTP 403 (App permission missing)");
    }

    #[tokio::test]
    async fn a_setup_resumes_only_for_the_repository_recorded_at_creation() {
        use bytes::Bytes;
        use http_body_util::Full;
        let service = tower::service_fn({
            move |request: http::Request<octocrab::OctoBody>| {
                let route = request.uri().path().to_owned();
                let (status, body) = if route.ends_with("/access_tokens") {
                    (
                        201,
                        json!({"token":"offline-token","expires_at":"2030-01-01T00:00:00Z"}),
                    )
                } else if route == "/repos/org/new" {
                    (200, json!({"id":42,"private":false}))
                } else {
                    (404, json!({"message":"Not Found"}))
                };
                let body = Bytes::from(body.to_string());
                async move {
                    Ok::<_, std::convert::Infallible>(
                        http::Response::builder()
                            .status(status)
                            .body(Full::new(body))
                            .unwrap(),
                    )
                }
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let wait = Arc::new(Wait {
            clock: Arc::new(dvb_core::FakeClock::at(Timestamp(0))),
            deadline: Mutex::new(None),
        });
        let forge = GitHub::with_app_client(
            app,
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            wait,
        )
        .unwrap();
        let repo = Repo("org/new".into());
        assert!(forge.resume_setup(&repo, 41).await.is_err());
        assert_eq!(forge.created_repo_id(&repo), None);
        forge.resume_setup(&repo, 42).await.unwrap();
        assert_eq!(forge.created_repo_id(&repo), Some(42));
    }

    #[tokio::test]
    async fn a_public_repository_is_served_and_each_operation_reads_its_metadata() {
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let service = tower::service_fn(move |request: http::Request<octocrab::OctoBody>| {
            let recorded = recorded.clone();
            async move {
                let route = request.uri().path().to_owned();
                let authorization = request
                    .headers()
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                let method = request.method().clone();
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                recorded
                    .lock()
                    .unwrap()
                    .push((method, route.clone(), body, authorization));
                let response = if route.ends_with("/access_tokens") {
                    json!({"token":"offline-session","expires_at":"2030-01-01T00:00:00Z"})
                } else if route == "/repos/org/repo" {
                    let reads = recorded
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(_, route, _, _)| route == "/repos/org/repo")
                        .count();
                    json!({
                        "private": false,
                        "visibility": "public",
                        "default_branch": if reads == 7 {
                            "release"
                        } else {
                            "main"
                        }
                    })
                } else if route.ends_with("/pulls") {
                    json!({"number":1,"html_url":"https://github.invalid/pr/1"})
                } else {
                    json!([])
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(200)
                        .body(Full::new(Bytes::from(response.to_string())))
                        .unwrap(),
                )
            }
        });
        let app = octocrab::OctocrabBuilder::new_empty()
            .with_service(service)
            .with_auth(octocrab::AuthState::None)
            .build()
            .unwrap();
        let app = app.user_access_token("offline-app").unwrap();
        let wait = Arc::new(Wait {
            clock: Arc::new(dvb_core::FakeClock::at(Timestamp(0))),
            deadline: Mutex::new(None),
        });
        let forge = GitHub::with_app_client(
            app,
            7,
            "org".into(),
            dvb_core::api::GithubAccountType::Organization,
            wait,
        )
        .unwrap();
        let repo = Repo("org/repo".into());
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        for _ in 0..2 {
            let error = forge
                .push(&repo, &missing, "dvb/task", "head")
                .await
                .unwrap_err();
            assert!(error.detail.starts_with("local git operation failed"));
            assert!(error.detail.contains("cannot change to"));
            assert!(!error.retryable);
            forge
                .open_pr(&repo, "dvb/task", "Change", "Reviewed")
                .await
                .unwrap();
            forge.delete_branch(&repo, "dvb/task").await.unwrap();
            forge.ruleset(&repo).await.unwrap();
        }
        forge
            .open_pr(&repo, "dvb/task", "Change", "Reviewed")
            .await
            .unwrap();
        let requests = requests.lock().unwrap();
        let scopes: Vec<_> = requests
            .iter()
            .filter(|(_, route, _, _)| route.ends_with("/access_tokens"))
            .map(|(_, _, body, _)| body.clone())
            .collect();
        assert_eq!(scopes, vec![json!({"repositories":["repo"]})]);
        assert_eq!(
            requests
                .iter()
                .filter(|(_, route, _, _)| route == "/repos/org/repo")
                .count(),
            7
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(_, route, _, _)| route.ends_with("/pulls"))
                .count(),
            3
        );
        assert_eq!(
            requests
                .iter()
                .rev()
                .find(|(_, route, _, _)| route.ends_with("/pulls"))
                .unwrap()
                .2["base"],
            "release"
        );
        assert!(
            requests
                .iter()
                .filter(|(_, route, _, _)| !route.ends_with("/access_tokens"))
                .all(|(_, _, _, auth)| auth == "Bearer offline-session")
        );
        drop(requests);
    }

    #[tokio::test]
    async fn authenticated_git_sets_stall_limits_without_persisting_config() {
        let dir = tempfile::tempdir().unwrap();
        for (key, expected) in [("http.lowSpeedLimit", "1000"), ("http.lowSpeedTime", "60")] {
            let args = ["config".into(), "--get".into(), key.into()];
            assert_eq!(
                credential_git(dir.path(), &args, "offline-fixture")
                    .await
                    .unwrap(),
                expected
            );
        }
        assert!(!dir.path().join(".gitconfig").exists());
    }

    #[tokio::test]
    async fn local_gateway_git_keeps_isolation_and_local_repository_operations() {
        let dir = tempfile::tempdir().unwrap();
        local_git(dir.path())
            .run(&["init", "--quiet", "--bare"], &[])
            .await
            .unwrap();
        local_git(dir.path())
            .run(&["symbolic-ref", "HEAD", "refs/heads/trunk"], &[])
            .await
            .unwrap();
        assert_eq!(
            local_git(dir.path())
                .run(&["symbolic-ref", "HEAD"], &[])
                .await
                .unwrap(),
            "refs/heads/trunk"
        );
        assert_eq!(
            local_git(dir.path())
                .run(&["config", "--get", "credential.helper"], &[])
                .await
                .unwrap(),
            ""
        );
        assert_eq!(
            local_git(dir.path())
                .run(&["config", "--get", "core.hooksPath"], &[])
                .await
                .unwrap(),
            "/dev/null"
        );
        let mut git = local_git(dir.path());
        let overrides: std::collections::BTreeMap<_, _> =
            git.command().as_std().get_envs().collect();
        for (key, value) in [
            ("DVB_GIT_TOKEN", ""),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ASKPASS", "/bin/false"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ] {
            assert_eq!(
                overrides.get(std::ffi::OsStr::new(key)),
                Some(&Some(std::ffi::OsStr::new(value)))
            );
        }
        let error = local_git(dir.path())
            .run(&["rev-parse", "--verify", "refs/heads/missing"], &[])
            .await
            .unwrap_err();
        let error = local_git_error(&error);
        assert!(error.detail.starts_with("local git operation failed"));
        assert!(!error.retryable);
        assert!(error.rate_limit.is_none());
    }

    #[tokio::test]
    async fn authenticated_git_retains_rejections_and_redacts_credentials() {
        let dir = tempfile::tempdir().unwrap();
        for reason in [
            "non-fast-forward",
            "GH013: Repository rule violations",
            "stale info",
            "couldn't find remote ref missing",
        ] {
            let command = format!(
                "alias.fixture=!echo \"remote: {reason} offline-secret \
                https://x:offline-secret@github.com/org/repo\" >&2; exit 1"
            );
            let error = credential_git(
                dir.path(),
                &["-c".into(), command, "fixture".into()],
                "offline-secret",
            )
            .await
            .unwrap_err();
            assert!(!error.retryable, "{error}");
            assert!(error.detail.contains(reason), "{error}");
            assert!(!error.detail.contains("offline-secret"), "{error}");
            assert!(!error.detail.contains("https://"), "{error}");
        }
        let error = credential_git(
            dir.path(),
            &[
                "-c".into(),
                "alias.fixture=!echo 'fatal: unable to access https://github.com/org/repo: Connection \
                 timed out' >&2; exit 1".into(),
                "fixture".into(),
            ],
            "offline-secret",
        ).await.unwrap_err();
        assert!(error.retryable);
        assert!(
            error
                .detail
                .starts_with("authenticated git operation failed")
        );
    }

    #[tokio::test]
    async fn authenticated_git_disables_hooks_prompts_and_global_config() {
        let dir = tempfile::tempdir().unwrap();
        for (key, value) in [
            ("core.hooksPath", "/dev/null"),
            ("credential.useHttpPath", "true"),
        ] {
            assert_eq!(
                credential_git(
                    dir.path(),
                    &["config".into(), "--get".into(), key.into()],
                    "offline-secret"
                )
                .await
                .unwrap(),
                value
            );
        }
        let output = credential_git(
            dir.path(),
            &[
                "-c".into(),
                "alias.fixture=!printf '%s:%s:%s:%s' \"$GIT_TERMINAL_PROMPT\" \"$GIT_ASKPASS\" \
                 \"$GIT_CONFIG_NOSYSTEM\" \"$GIT_CONFIG_GLOBAL\""
                    .into(),
                "fixture".into(),
            ],
            "offline-secret",
        )
        .await
        .unwrap();
        assert_eq!(output, "0:/bin/false:1:/dev/null");
    }

    #[test]
    fn token_expiration_reads_actual_rfc3339_time() {
        let t = stamp_of("2026-10-02T14:26:28Z").unwrap();
        assert_eq!(t, Timestamp(1_790_951_188_000));
        assert!(stamp_of("yesterday").is_err());
    }

    use super::*;
    #[test]
    fn pr_mergeability_distinguishes_unknown_conflicts_and_clean() {
        for (state, mergeable, expected) in [
            ("unknown", Value::Null, None),
            ("unknown", Value::Bool(true), None),
            ("clean", Value::Null, None),
            ("dirty", Value::Null, Some(true)),
            ("dirty", Value::Bool(false), Some(true)),
            ("clean", Value::Bool(true), Some(false)),
            ("blocked", Value::Bool(true), Some(false)),
            ("behind", Value::Bool(true), Some(false)),
            ("unstable", Value::Bool(true), Some(false)),
        ] {
            let seen = observation(&json!({
                "number": 1, "html_url": "https://github.invalid/pr/1", "state": "open",
                "head": {"ref": "task", "sha": "head"}, "base": {"ref": "main"},
                "mergeable_state": state, "mergeable": mergeable,
            }))
            .unwrap();
            assert_eq!(seen.conflicting, expected, "{state}: {mergeable}");
        }
    }

    #[test]
    fn path_segments_keep_unreserved_characters_and_encode_ref_slashes() {
        assert_eq!(segment("example-org"), "example-org");
        assert_eq!(segment("dvb/t-1"), "dvb%2Ft-1");
        assert!(!valid_name("../outside"));
        assert!(!valid_name(""));
    }
    #[test]
    fn an_app_key_must_be_an_engine_owned_regular_file_and_is_made_private() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("github-app.json"),
            r#"{"app_id":7,"installation_id":8,"organization":"example-org"}"#,
        )
        .unwrap();
        let key = tmp.path().join("github-app.pem");
        std::fs::create_dir(&key).unwrap();
        assert_eq!(
            GitHub::from_state(tmp.path(), Arc::new(Wait::default()))
                .err()
                .unwrap()
                .detail,
            "GitHub App key must be an engine-owned file"
        );
        std::fs::remove_dir(&key).unwrap();
        std::fs::write(&key, "invalid test key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            GitHub::from_state(tmp.path(), Arc::new(Wait::default()))
                .err()
                .unwrap()
                .detail,
            "invalid GitHub App RSA key"
        );
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
