//! `dvb github connect`: GitHub setup without the app. It drives the engine's
//! setup routes the app's wizard uses; GitHub returns to the desk's callback.

use crate::Target;
use crate::client::{AbortOnDrop, CliError, Client, message};
use crate::doctor;
use dvb_core::api::{
    GithubConnection, GithubInstallRequest, GithubManifest, GithubManifestRequest, GithubPrefill,
    GithubRepositories, GithubSetupStep,
};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The wizard reads the status as often.
pub(crate) const POLL: Duration = Duration::from_secs(3);
pub(crate) const LIMIT: Duration = Duration::from_secs(15 * 60);

pub(crate) struct Connect {
    pub owner: Option<String>,
    pub repos: Vec<String>,
    pub json: bool,
}

/// Prints the links the owner must open, waits for GitHub, then prints the
/// result. Exit codes are `dvb doctor`'s: 0 when connected with the wanted
/// repositories, 1 when anything is still missing.
pub(crate) async fn connect(
    target: &Target,
    client: &Client,
    args: Connect,
    poll: Duration,
    limit: Duration,
    mut print: impl FnMut(&str) -> Result<(), CliError> + Send,
) -> Result<i32, CliError> {
    let status: GithubConnection = client.get("/github").await?;
    if status.connected
        && let (Some(connected), Some(wanted)) = (&status.organization, &args.owner)
        && !connected.eq_ignore_ascii_case(wanted)
    {
        return Err(CliError::Message(format!(
            "dvb is connected to the GitHub account {connected}, not {wanted}"
        )));
    }
    let mut page = None;
    if !status.connected {
        let owner = args
            .owner
            .clone()
            .or_else(|| status.organization.clone())
            .ok_or_else(|| {
                CliError::Message(
                    "name the GitHub account: dvb github connect --owner <org-or-user>".into(),
                )
            })?;
        let port = crate::app::desk_port(target).await?;
        let created = matches!(status.create_app, GithubSetupStep::Succeeded);
        match status.organization.as_deref() {
            Some(pending) if created && !pending.eq_ignore_ascii_case(&owner) => {
                return Err(CliError::Message(format!(
                    "the App for {pending} is created but not installed; finish it with: dvb github connect --owner {pending}"
                )));
            }
            // A fresh state gives a fresh installation link for the created App.
            Some(_) if created => {
                let _: GithubConnection = client
                    .post("/github/install", &GithubInstallRequest { state: token() })
                    .await?;
            }
            _ => {
                let (url, server) = create_app(client, &owner, port).await?;
                page = Some(server);
                link(
                    &mut print,
                    args.json,
                    "create_app",
                    &format!(
                        "Create the GitHub App for {owner}: open {url} in a browser on this machine and approve it on GitHub."
                    ),
                    &url,
                )?;
            }
        }
    }
    let wait = follow(client, &args, poll, &mut page, &mut print);
    let timed_out = match tokio::time::timeout(limit, wait).await {
        Ok(result) => result.map(|()| false)?,
        Err(_) => true,
    };
    drop(page);
    let status: GithubConnection = client.get("/github").await?;
    let repositories = if status.connected {
        Some(
            client
                .get::<GithubRepositories>("/github/repositories")
                .await
                .map_err(|e| e.to_string()),
        )
    } else {
        None
    };
    finish(&status, repositories, &args, timed_out, &mut print)
}

/// Polls until GitHub reports the App installed with the wanted repositories,
/// or a step failed; prints each link once it exists.
async fn follow(
    client: &Client,
    args: &Connect,
    poll: Duration,
    page: &mut Option<AbortOnDrop>,
    print: &mut impl FnMut(&str) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let (mut install_shown, mut access_shown) = (false, false);
    loop {
        let status: GithubConnection = client.get("/github").await?;
        if failed(&status) {
            return Ok(());
        }
        if status.connected {
            *page = None;
            let granted: GithubRepositories = client.get("/github/repositories").await?;
            let missing = missing(&args.repos, &granted);
            if missing.is_empty() {
                return Ok(());
            }
            if !access_shown {
                let url = &granted.repository_access_url;
                let text = format!("Give the App access to {}: {url}", missing.join(", "));
                link(print, args.json, "repository_access", &text, url)?;
                access_shown = true;
            }
        } else if matches!(status.create_app, GithubSetupStep::Succeeded) {
            *page = None;
            if let Some(url) = status.install_url.as_deref().filter(|_| !install_shown) {
                let text =
                    format!("Install the App and choose the repositories it may access: {url}");
                link(print, args.json, "install_app", &text, url)?;
                install_shown = true;
            }
        }
        tokio::time::sleep(poll).await;
    }
}

fn failed(status: &GithubConnection) -> bool {
    [&status.create_app, &status.install_app]
        .into_iter()
        .any(|step| matches!(step, GithubSetupStep::Failed { .. }))
}

/// The wanted repositories the installation does not reach; a bare name is
/// in the connected account.
fn missing(wanted: &[String], granted: &GithubRepositories) -> Vec<String> {
    wanted
        .iter()
        .map(|repo| {
            if repo.contains('/') {
                repo.clone()
            } else {
                format!("{}/{repo}", granted.organization)
            }
        })
        .filter(|repo| {
            !granted
                .repositories
                .iter()
                .any(|r| r.repo.eq_ignore_ascii_case(repo))
        })
        .collect()
}

fn finish(
    status: &GithubConnection,
    repositories: Option<Result<GithubRepositories, String>>,
    args: &Connect,
    timed_out: bool,
    print: &mut impl FnMut(&str) -> Result<(), CliError>,
) -> Result<i32, CliError> {
    let granted: Vec<String> = repositories
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|r| r.repositories.iter().map(|r| r.repo.clone()).collect())
        .unwrap_or_default();
    let mut items = vec![doctor::github_app(status)];
    if let Some(repositories) = repositories {
        items.push(match &repositories {
            Ok(r) if !args.repos.is_empty() => {
                let missing = missing(&args.repos, r);
                if missing.is_empty() {
                    (true, format!("GitHub repository access: {}", granted.join(", ")))
                } else {
                    (
                        false,
                        format!(
                            "GitHub repository access: {} not granted. Fix: give the App access at {}",
                            missing.join(", "),
                            r.repository_access_url
                        ),
                    )
                }
            }
            Ok(_) | Err(_) => doctor::repository_access(status, repositories),
        });
    }
    let unmet = items.iter().filter(|(met, _)| !met).count();
    let owner = status.organization.as_deref().unwrap_or("the account");
    let text = if args.json {
        let mut result = doctor::json(&items);
        result["owner"] = status.organization.clone().into();
        result["repositories"] = granted.into();
        format!("{result}\n")
    } else if unmet == 0 {
        let list = if granted.is_empty() {
            "none".to_owned()
        } else {
            granted.join(", ")
        };
        format!("Connected to {owner}; repositories: {list}\n")
    } else if timed_out {
        format!("{}Timed out waiting for GitHub.\n", doctor::lines(&items))
    } else {
        doctor::lines(&items)
    };
    print(&text)?;
    Ok(if unmet == 0 { 0 } else { 1 })
}

fn link(
    print: &mut impl FnMut(&str) -> Result<(), CliError>,
    json: bool,
    name: &str,
    text: &str,
    url: &str,
) -> Result<(), CliError> {
    if json {
        print(&format!(
            "{}\n",
            serde_json::json!({"link": name, "url": url})
        ))
    } else {
        print(&format!("{text}\n"))
    }
}

/// Starts Create App and serves the page that posts its manifest to GitHub.
async fn create_app(
    client: &Client,
    owner: &str,
    port: u16,
) -> Result<(String, AbortOnDrop), CliError> {
    let mut identity: GithubPrefill = client.get("/github/prefill").await?;
    // Like the desk's prefill: the owner's git identity when dvb stored none.
    let identity = tokio::task::spawn_blocking(move || {
        fill_from_git(&mut identity, |key| git_config(None, key));
        identity
    })
    .await
    .map_err(message)?;
    let (Some(owner_name), Some(owner_email)) = (identity.owner_name, identity.owner_email) else {
        return Err(CliError::Message(
            "GitHub setup records your name and email; set them with git config --global user.name and user.email, then retry".into(),
        ));
    };
    let manifest: GithubManifest = client
        .post(
            "/github/manifest",
            &GithubManifestRequest {
                organization: owner.to_owned(),
                redirect_url: format!("http://localhost:{port}/github/callback"),
                state: token(),
                owner_name,
                owner_email,
            },
        )
        .await?;
    serve(create_page(&manifest)).await
}

/// GitHub creates an App only from a form post, which a printed link cannot
/// make; this page makes it, as the wizard's window does.
fn create_page(manifest: &GithubManifest) -> String {
    format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Create the dvb GitHub App</title><main><h1>Create the dvb GitHub App</h1><form id="create" method="post" action="{}"><input type="hidden" name="manifest" value="{}"><button type="submit">Continue to GitHub</button></form></main><script>document.getElementById("create").submit()</script></html>"#,
        escape(&manifest.create_url),
        escape(&manifest.manifest)
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Serves `html` on loopback until the returned task is dropped.
async fn serve(html: String) -> Result<(String, AbortOnDrop), CliError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(CliError::Connection)?;
    let port = listener.local_addr().map_err(CliError::Connection)?.port();
    let html: std::sync::Arc<str> = html.into();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // A browser's idle preconnect socket must not hold up the page.
            tokio::spawn(answer(stream, html.clone()));
        }
    });
    Ok((format!("http://127.0.0.1:{port}/"), AbortOnDrop(server)))
}

/// Answers one request; a socket that sends nothing for ten seconds is closed.
async fn answer(mut stream: tokio::net::TcpStream, html: std::sync::Arc<str>) {
    let mut request = vec![0; 8192];
    let mut read = 0;
    let head = async {
        while read < request.len() && !request[..read].windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut request[read..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(10), head)
        .await
        .is_err()
    {
        return;
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    // A browser that went away needs no answer.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// 32 hex digits, the form GitHub's callback state takes.
pub fn token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fills unknown owner fields from the owner's git config.
pub fn fill_from_git(values: &mut GithubPrefill, git: impl Fn(&str) -> Option<String>) {
    if values.owner_name.is_none() {
        values.owner_name = git("user.name");
    }
    if values.owner_email.is_none() {
        values.owner_email = git("user.email");
    }
}

/// The owner's global git setting; `global` replaces the file only in tests.
pub fn git_config(global: Option<&Path>, key: &str) -> Option<String> {
    let mut git = std::process::Command::new("git");
    git.args(["config", "--global", "--get", key])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(path) = global {
        git.env("GIT_CONFIG_GLOBAL", path);
    }
    let out = git.output().ok()?;
    let value = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::tests::{fake_engine, write};
    use dvb_core::api::GithubRepository;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone, Copy)]
    enum Stage {
        New,
        Creating,
        Created,
        Installed,
    }

    fn status(stage: Stage, state: &str) -> GithubConnection {
        let org = Some("example-org".to_owned());
        let (organization, create_app, install_app) = match stage {
            Stage::New => (
                None,
                GithubSetupStep::NotStarted,
                GithubSetupStep::NotStarted,
            ),
            Stage::Creating => (
                org,
                GithubSetupStep::AwaitingCallback,
                GithubSetupStep::NotStarted,
            ),
            Stage::Created => (
                org,
                GithubSetupStep::Succeeded,
                GithubSetupStep::AwaitingCallback,
            ),
            Stage::Installed => (org, GithubSetupStep::Succeeded, GithubSetupStep::Succeeded),
        };
        let connected = matches!(stage, Stage::Installed);
        GithubConnection {
            organization,
            account_type: None,
            install_url: matches!(stage, Stage::Created).then(|| {
                format!("https://github.com/apps/dvb-example/installations/new?state={state}")
            }),
            connected,
            repository_access_url: connected.then(|| {
                "https://github.com/organizations/example-org/settings/installations/7".into()
            }),
            create_app,
            install_app,
        }
    }

    /// The engine's setup routes, moved through the wizard's steps by the
    /// test as GitHub's callbacks would, and a desk control socket on port 7300.
    fn scripted_engine(dir: &Path) -> (Target, Arc<Mutex<Stage>>) {
        use axum::{
            Json,
            routing::{get, post},
        };
        let stage = Arc::new(Mutex::new(Stage::New));
        let state = Arc::new(Mutex::new(String::new()));
        let (read_stage, read_state) = (stage.clone(), state.clone());
        let (set_stage, set_state) = (stage.clone(), state);
        let engine = axum::Router::new()
            .route(
                "/version",
                get(|| async { ([(dvb_core::api::VERSION_HEADER, dvb_core::VERSION)], "{}") }),
            )
            .route(
                "/github",
                get(move || async move {
                    let stage = *read_stage.lock().unwrap();
                    Json(status(stage, &read_state.lock().unwrap()))
                }),
            )
            .route(
                "/github/prefill",
                get(|| async {
                    Json(GithubPrefill {
                        organization: None,
                        owner_name: Some("Example Owner".into()),
                        owner_email: Some("owner@example.invalid".into()),
                    })
                }),
            )
            .route(
                "/github/manifest",
                post(move |Json(body): Json<GithubManifestRequest>| async move {
                    assert_eq!(body.organization, "example-org");
                    assert_eq!(body.redirect_url, "http://localhost:7300/github/callback");
                    assert_eq!(body.owner_name, "Example Owner");
                    assert!(body.state.len() == 32 && body.state.chars().all(|c| c.is_ascii_hexdigit()));
                    *set_stage.lock().unwrap() = Stage::Creating;
                    *set_state.lock().unwrap() = body.state.clone();
                    Json(GithubManifest {
                        create_url: format!(
                            "https://github.com/organizations/example-org/settings/apps/new?state={}",
                            body.state
                        ),
                        manifest: r#"{"name":"dvb <example>"}"#.into(),
                    })
                }),
            )
            .route(
                "/github/repositories",
                get(|| async {
                    let repo = |name: &str| GithubRepository {
                        repo: format!("example-org/{name}"),
                        private: true,
                        default_branch: "main".into(),
                    };
                    Json(GithubRepositories {
                        organization: "example-org".into(),
                        repository_access_url:
                            "https://github.com/organizations/example-org/settings/installations/7"
                                .into(),
                        repositories: vec![repo("app"), repo("web")],
                    })
                }),
            );
        let sock = dir.join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move { axum::serve(listener, engine).await.unwrap() });
        let desk = axum::Router::new().route(
            "/app",
            get(|| async { Json("http://127.0.0.1:7300/launch?token=offline") }),
        );
        let listener = tokio::net::UnixListener::bind(crate::app::control_socket(&sock)).unwrap();
        tokio::spawn(async move { axum::serve(listener, desk).await.unwrap() });
        (
            Target {
                sock: Some(sock),
                dev: None,
            },
            stage,
        )
    }

    /// Runs the command; its lines arrive on the receiver as it prints them.
    fn start(
        target: Target,
        args: Connect,
        limit: Duration,
    ) -> (
        tokio::task::JoinHandle<Result<i32, CliError>>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (lines, received) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            let client = Client::new(target.sock());
            connect(
                &target,
                &client,
                args,
                Duration::from_millis(1),
                limit,
                |text| lines.send(text.to_owned()).map_err(message),
            )
            .await
        });
        (run, received)
    }

    fn args(owner: Option<&str>, repos: &[&str], json: bool) -> Connect {
        Connect {
            owner: owner.map(str::to_owned),
            repos: repos.iter().map(|r| (*r).to_owned()).collect(),
            json,
        }
    }

    async fn fetch(url: &str) -> String {
        let (host, path) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap();
        let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
        stream
            .write_all(format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut page = String::new();
        stream.read_to_string(&mut page).await.unwrap();
        page
    }

    #[tokio::test]
    async fn connect_prints_the_create_and_install_links_then_the_connected_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let (target, stage) = scripted_engine(dir.path());
        let (run, mut lines) = start(
            target,
            args(Some("example-org"), &["app"], false),
            Duration::from_secs(60),
        );
        let create = lines.recv().await.unwrap();
        let url = create
            .strip_prefix("Create the GitHub App for example-org: open ")
            .and_then(|rest| {
                rest.strip_suffix(" in a browser on this machine and approve it on GitHub.\n")
            })
            .unwrap_or_else(|| panic!("{create}"));
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        // A browser's preconnect socket that sends nothing does not hold up the page.
        let host = url
            .strip_prefix("http://")
            .unwrap()
            .split_once('/')
            .unwrap()
            .0;
        let _idle = tokio::net::TcpStream::connect(host).await.unwrap();
        let page = tokio::time::timeout(Duration::from_secs(5), fetch(url))
            .await
            .expect("the page answers beside an idle socket");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
        assert!(
            page.contains(r#"<form id="create" method="post" action="https://github.com/organizations/example-org/settings/apps/new?state="#),
            "{page}"
        );
        assert!(
            page.contains(r#"<input type="hidden" name="manifest" value="{&quot;name&quot;:&quot;dvb &lt;example&gt;&quot;}">"#),
            "{page}"
        );
        // GitHub created the App and returned to the desk's callback.
        *stage.lock().unwrap() = Stage::Created;
        let install = lines.recv().await.unwrap();
        assert!(
            install.starts_with("Install the App and choose the repositories it may access: https://github.com/apps/dvb-example/installations/new?state="),
            "{install}"
        );
        *stage.lock().unwrap() = Stage::Installed;
        assert_eq!(run.await.unwrap().unwrap(), 0);
        assert_eq!(
            lines.recv().await.unwrap(),
            "Connected to example-org; repositories: example-org/app, example-org/web\n"
        );
        assert!(lines.recv().await.is_none());
    }

    #[tokio::test]
    async fn json_timeout_prints_the_link_then_what_is_missing_and_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let (target, _stage) = scripted_engine(dir.path());
        let (run, mut lines) = start(
            target,
            args(Some("example-org"), &[], true),
            Duration::from_millis(100),
        );
        assert_eq!(run.await.unwrap().unwrap(), 1);
        let link: serde_json::Value = serde_json::from_str(&lines.recv().await.unwrap()).unwrap();
        assert_eq!(link["link"], "create_app");
        assert!(
            link["url"]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:")
        );
        assert_eq!(link.as_object().unwrap().len(), 2);
        let result: serde_json::Value = serde_json::from_str(&lines.recv().await.unwrap()).unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "items": [{"met": false, "text": "GitHub App: waiting for GitHub to create the App for example-org. Fix: dvb github connect"}],
                "unmet": 1,
                "owner": "example-org",
                "repositories": [],
            })
        );
        assert!(lines.recv().await.is_none());
    }

    #[tokio::test]
    async fn without_an_owner_or_a_pending_app_connect_asks_for_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let (target, _stage) = scripted_engine(dir.path());
        let (run, _lines) = start(target, args(None, &[], false), LIMIT);
        let error = run.await.unwrap().unwrap_err().to_string();
        assert_eq!(
            error,
            "name the GitHub account: dvb github connect --owner <org-or-user>"
        );
    }

    /// A Fake engine whose App is installed on example-org and reaches example-org/app.
    fn connected_fake_engine(dir: &Path) -> Target {
        let forge = Arc::new(dvb_forge::FakeForge::default());
        forge.set_checks(
            &dvb_core::forge::Repo("example-org/app".into()),
            "h",
            vec![],
        );
        fake_engine(dir, forge);
        write(
            dir.join("state/github-app.json"),
            r#"{"app_id":1,"installation_id":2,"organization":"example-org"}"#,
        );
        Target {
            sock: Some(dir.join("o.sock")),
            dev: None,
        }
    }

    #[tokio::test]
    async fn already_connected_prints_the_account_and_refuses_another() {
        let dir = tempfile::tempdir().unwrap();
        let target = connected_fake_engine(dir.path());
        let (run, mut lines) = start(
            target.clone(),
            args(Some("example-org"), &[], false),
            Duration::from_secs(10),
        );
        assert_eq!(run.await.unwrap().unwrap(), 0);
        assert_eq!(
            lines.recv().await.unwrap(),
            "Connected to example-org; repositories: example-org/app\n"
        );
        let (run, mut lines) = start(
            target.clone(),
            args(None, &["example-org/app"], true),
            Duration::from_secs(10),
        );
        assert_eq!(run.await.unwrap().unwrap(), 0);
        let result: serde_json::Value = serde_json::from_str(&lines.recv().await.unwrap()).unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "items": [
                    {"met": true, "text": "GitHub App: connected to example-org"},
                    {"met": true, "text": "GitHub repository access: example-org/app"},
                ],
                "unmet": 0,
                "owner": "example-org",
                "repositories": ["example-org/app"],
            })
        );
        assert!(lines.recv().await.is_none());
        let (run, _lines) = start(target, args(Some("other-org"), &[], false), LIMIT);
        assert_eq!(
            run.await.unwrap().unwrap_err().to_string(),
            "dvb is connected to the GitHub account example-org, not other-org"
        );
    }

    #[tokio::test]
    async fn a_repository_never_granted_prints_the_access_link_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let target = connected_fake_engine(dir.path());
        let (run, mut lines) = start(
            target,
            args(None, &["example-org/app", "example-org/other"], false),
            Duration::from_millis(100),
        );
        assert_eq!(run.await.unwrap().unwrap(), 1);
        assert_eq!(
            lines.recv().await.unwrap(),
            "Give the App access to example-org/other: https://github.com/organizations/fake/settings/installations/1\n"
        );
        assert_eq!(
            lines.recv().await.unwrap(),
            "ok       GitHub App: connected to example-org\n\
             missing  GitHub repository access: example-org/other not granted. Fix: give the App access at https://github.com/organizations/fake/settings/installations/1\n\
             1 setup item needs you.\n\
             Timed out waiting for GitHub.\n"
        );
    }

    #[test]
    fn prefill_keeps_stored_identity_then_falls_back_to_git_config() {
        let git = |key: &str| Some(format!("git {key}"));
        let mut stored = GithubPrefill {
            organization: Some("example-org".into()),
            owner_name: Some("Stored Owner".into()),
            owner_email: Some("stored@example.invalid".into()),
        };
        let kept = stored.clone();
        fill_from_git(&mut stored, git);
        assert_eq!(stored, kept);
        let mut fresh = GithubPrefill::default();
        fill_from_git(&mut fresh, git);
        assert_eq!(fresh.organization, None);
        assert_eq!(fresh.owner_name.as_deref(), Some("git user.name"));
        assert_eq!(fresh.owner_email.as_deref(), Some("git user.email"));
        let mut none = GithubPrefill::default();
        fill_from_git(&mut none, |_| None);
        assert_eq!(none, GithubPrefill::default());
    }

    #[test]
    fn git_config_reads_the_owners_identity_and_nothing_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("gitconfig");
        assert_eq!(git_config(Some(&config), "user.name"), None);
        std::fs::write(
            &config,
            "[user]\n\tname = Example Owner\n\temail = owner@example.invalid\n",
        )
        .unwrap();
        assert_eq!(
            git_config(Some(&config), "user.name").as_deref(),
            Some("Example Owner")
        );
        assert_eq!(
            git_config(Some(&config), "user.email").as_deref(),
            Some("owner@example.invalid")
        );
        std::fs::write(&config, "[user]\n\tname = Example Owner\n").unwrap();
        assert_eq!(git_config(Some(&config), "user.email"), None);
    }
}
