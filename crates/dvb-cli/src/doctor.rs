//! `dvb doctor`: the setup status the engine serves for the app's setup screens,
//! one line per item, each unmet item with the command that fixes it.

use crate::client::{CliError, Client, message};
use crate::github::{fill_from_git, git_config};
use crate::write_stdout;
use dvb_core::Vendor;
use dvb_core::api::{
    GithubConnection, GithubPrefill, GithubRepositories, GithubSetupStep, Logins,
    PublishingIdentity,
};
use std::path::PathBuf;

/// Prints every setup item, as lines or as one JSON object; the exit code is
/// 0 when all are met, 1 otherwise. `git_global` replaces the global git config
/// file only in tests.
pub(crate) async fn doctor(
    client: &Client,
    as_json: bool,
    git_global: Option<PathBuf>,
) -> Result<i32, CliError> {
    let items = read(client, git_global).await?;
    if as_json {
        write_stdout(format_args!("{}\n", json(&items)))?;
    } else {
        write_stdout(format_args!("{}", lines(&items)))?;
    }
    Ok(if items.iter().all(|(met, _)| *met) {
        0
    } else {
        1
    })
}

async fn read(
    client: &Client,
    git_global: Option<PathBuf>,
) -> Result<Vec<(bool, String)>, CliError> {
    let mut owner: GithubPrefill = client.get("/github/prefill").await?;
    // The values `dvb github connect` records: stored ones, else the owner's git config.
    let owner = tokio::task::spawn_blocking(move || {
        fill_from_git(&mut owner, |key| git_config(git_global.as_deref(), key));
        owner
    })
    .await
    .map_err(message)?;
    let github: GithubConnection = client.get("/github").await?;
    // The setup screen lists the repositories the App reaches once it is installed.
    let repositories = if github.connected {
        Some(
            client
                .get::<GithubRepositories>("/github/repositories")
                .await
                .map_err(|e| e.to_string()),
        )
    } else {
        None
    };
    let logins: Logins = client.get("/logins").await?;
    let identity: PublishingIdentity = client.get("/settings/publication").await?;
    Ok(items(&owner, &github, repositories, &logins, &identity))
}

/// Each setup item: whether it is met, and its line.
fn items(
    owner: &GithubPrefill,
    github: &GithubConnection,
    repositories: Option<Result<GithubRepositories, String>>,
    logins: &Logins,
    identity: &PublishingIdentity,
) -> Vec<(bool, String)> {
    let mut items = vec![git_identity(owner), github_app(github)];
    if let Some(repositories) = repositories {
        items.push(repository_access(github, repositories));
    }
    items.push((true, crate::publication_text(identity)));
    for (vendor, signed_in) in [
        (Vendor::Claude, logins.claude),
        (Vendor::Codex, logins.codex),
    ] {
        items.push(login(vendor, signed_in, logins.needed.contains(&vendor)));
    }
    items
}

/// One line per item, `ok` or `missing`, then how many need the owner.
pub(crate) fn lines(items: &[(bool, String)]) -> String {
    let unmet = items.iter().filter(|(met, _)| !met).count();
    let mut text = String::new();
    for (met, line) in items {
        text.push_str(if *met { "ok       " } else { "missing  " });
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(&match unmet {
        0 => "Everything is set up.\n".to_owned(),
        1 => "1 setup item needs you.\n".to_owned(),
        n => format!("{n} setup items need you.\n"),
    });
    text
}

/// `--json`: `{"items": [{"met", "text"}], "unmet"}`.
pub(crate) fn json(items: &[(bool, String)]) -> serde_json::Value {
    serde_json::json!({
        "items": items
            .iter()
            .map(|(met, text)| serde_json::json!({"met": met, "text": text}))
            .collect::<Vec<_>>(),
        "unmet": items.iter().filter(|(met, _)| !met).count(),
    })
}

const CONNECT: &str = "Fix: dvb github connect";

/// Unmet exactly when `dvb github connect` refuses for a missing name or email.
fn git_identity(owner: &GithubPrefill) -> (bool, String) {
    match (&owner.owner_name, &owner.owner_email) {
        (Some(name), Some(email)) => (true, format!("Git identity: {name} <{email}>")),
        (None, _) | (_, None) => (
            false,
            "Git identity: no name or email; dvb github connect records both. \
             Fix: git config --global user.name \"Your Name\" and \
             git config --global user.email you@example.com"
                .to_owned(),
        ),
    }
}

pub(crate) fn github_app(g: &GithubConnection) -> (bool, String) {
    let account = g.organization.as_deref().unwrap_or("the account");
    if g.connected {
        return (true, format!("GitHub App: connected to {account}"));
    }
    let state = match &g.create_app {
        GithubSetupStep::NotStarted => "not created.".to_owned(),
        GithubSetupStep::AwaitingCallback => {
            format!("waiting for GitHub to create the App for {account}.")
        }
        GithubSetupStep::Failed { failure } => failure.message.clone(),
        GithubSetupStep::Succeeded => match &g.install_app {
            GithubSetupStep::NotStarted => format!("created, not installed on {account}."),
            GithubSetupStep::AwaitingCallback => {
                format!("created, waiting for its installation on {account}.")
            }
            GithubSetupStep::Failed { failure } => failure.message.clone(),
            GithubSetupStep::Succeeded => "installed, but not connected.".to_owned(),
        },
    };
    // Without an account to continue, the command needs one.
    let fix = if g.organization.is_some() {
        CONNECT
    } else {
        "Fix: dvb github connect --owner <org-or-user>"
    };
    (false, format!("GitHub App: {state} {fix}"))
}

pub(crate) fn repository_access(
    g: &GithubConnection,
    repositories: Result<GithubRepositories, String>,
) -> (bool, String) {
    let fix = |url: Option<&str>| {
        url.map_or_else(
            || CONNECT.to_owned(),
            |url| format!("{CONNECT}; give the App access at {url}"),
        )
    };
    match repositories {
        Ok(r) if r.repositories.is_empty() => (
            false,
            format!(
                "GitHub repository access: the App reaches no repository. {}",
                fix(Some(&r.repository_access_url))
            ),
        ),
        Ok(r) => (
            true,
            format!(
                "GitHub repository access: {}",
                r.repositories
                    .iter()
                    .map(|repo| repo.repo.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Err(error) => (
            false,
            format!(
                "GitHub repository access: cannot be read ({error}). {}",
                fix(g.repository_access_url.as_deref())
            ),
        ),
    }
}

fn login(vendor: Vendor, signed_in: bool, needed: bool) -> (bool, String) {
    let name = match vendor {
        Vendor::Claude => "Claude",
        Vendor::Codex => "Codex",
        Vendor::Fake => "Fake",
    };
    let fix = format!("dvb login {}", vendor.as_str());
    match (signed_in, needed) {
        (true, _) => (true, format!("{name} login: signed in")),
        (false, false) => (
            true,
            format!("{name} login: not signed in; the default hires do not need it ({fix})"),
        ),
        (false, true) => (
            false,
            format!("{name} login: not signed in; the default hires need it. Fix: {fix}"),
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dvb_core::api::{GithubFailure, GithubFailureCode, GithubRepository};
    use std::sync::Arc;

    /// A Fake engine's owner API served on a socket in `dir`.
    pub(crate) fn fake_engine(dir: &std::path::Path, forge: Arc<dvb_forge::FakeForge>) -> Client {
        let e = dvb_engine::Engine::open(
            dvb_engine::Config::dev(dir, "dvb".into()),
            dvb_engine::EngineOptions {
                forge: Some(forge),
                ..Default::default()
            },
        )
        .unwrap();
        let sock = dir.join("o.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            axum::serve(listener, dvb_engine::api::router(e))
                .await
                .unwrap();
        });
        Client::new(sock)
    }

    pub(crate) fn write(path: std::path::PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Doctor's lines and exit code with `dir/gitconfig` as the global git config.
    async fn doctor_text(client: &Client, dir: &std::path::Path) -> (String, i32) {
        let git = Some(dir.join("gitconfig"));
        let items = read(client, git.clone()).await.unwrap();
        (lines(&items), doctor(client, false, git).await.unwrap())
    }

    #[tokio::test]
    async fn a_fake_engine_with_everything_set_up_prints_met_lines_and_exits_0() {
        let dir = tempfile::tempdir().unwrap();
        let forge = Arc::new(dvb_forge::FakeForge::default());
        forge.set_checks(
            &dvb_core::forge::Repo("example-org/app".into()),
            "h",
            vec![],
        );
        let client = fake_engine(dir.path(), forge);
        write(
            dir.path().join("state/github-app.json"),
            r#"{"app_id":1,"installation_id":2,"organization":"example-org"}"#,
        );
        write(dir.path().join("logins/claude/.credentials.json"), "{}");
        write(dir.path().join("logins/codex/auth.json"), "{}");
        write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Example Owner\n\temail = owner@example.invalid\n",
        );
        let (text, code) = doctor_text(&client, dir.path()).await;
        assert_eq!(
            text,
            "ok       Git identity: Example Owner <owner@example.invalid>\n\
             ok       GitHub App: connected to example-org\n\
             ok       GitHub repository access: example-org/app\n\
             ok       dvb publishes commits as dvb <dvb@dvb.invalid> (the default)\n\
             ok       Claude login: signed in\n\
             ok       Codex login: signed in\n\
             Everything is set up.\n"
        );
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn a_fake_engine_without_github_prints_the_fix_and_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let client = fake_engine(dir.path(), Arc::new(dvb_forge::FakeForge::default()));
        // A fresh machine: no git config at all, so connect would refuse.
        let (text, code) = doctor_text(&client, dir.path()).await;
        assert!(
            text.starts_with(
                "missing  Git identity: no name or email; dvb github connect records both. \
                 Fix: git config --global user.name \"Your Name\" and \
                 git config --global user.email you@example.com\n\
                 missing  GitHub App: not created. Fix: dvb github connect --owner <org-or-user>\n"
            ),
            "{text}"
        );
        // The Fake engine's default hires use the Fake vendor, which needs no login.
        assert!(
            text.contains(
                "ok       Codex login: not signed in; the default hires do not need it (dvb login codex)\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("2 setup items need you.\n"), "{text}");
        assert_eq!(code, 1);
        // A name without an email is still refused.
        write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Example Owner\n",
        );
        let (text, _) = doctor_text(&client, dir.path()).await;
        assert!(text.starts_with("missing  Git identity: "), "{text}");
    }

    fn connection(
        connected: bool,
        create_app: GithubSetupStep,
        install_app: GithubSetupStep,
    ) -> GithubConnection {
        GithubConnection {
            organization: Some("example-org".into()),
            account_type: None,
            install_url: None,
            connected,
            repository_access_url: connected
                .then(|| "https://github.com/settings/installations/2".into()),
            create_app,
            install_app,
        }
    }

    #[test]
    fn each_unmet_item_names_its_fix() {
        let failure = GithubFailure {
            code: GithubFailureCode::InstallAppFailed,
            message: "Could not verify the installation.".into(),
            retryable: true,
        };
        for (create, install, expected) in [
            (
                GithubSetupStep::Succeeded,
                GithubSetupStep::NotStarted,
                "GitHub App: created, not installed on example-org. Fix: dvb github connect",
            ),
            (
                GithubSetupStep::Succeeded,
                GithubSetupStep::Failed { failure },
                "GitHub App: Could not verify the installation. Fix: dvb github connect",
            ),
        ] {
            assert_eq!(
                github_app(&connection(false, create, install)),
                (false, expected.into())
            );
        }
        let connected = connection(true, GithubSetupStep::Succeeded, GithubSetupStep::Succeeded);
        let none = GithubRepositories {
            organization: "example-org".into(),
            repository_access_url: "https://github.com/settings/installations/2".into(),
            repositories: Vec::<GithubRepository>::new(),
        };
        let logins = Logins {
            claude: false,
            codex: true,
            needed: vec![Vendor::Claude],
        };
        let identity = PublishingIdentity {
            name: "dvb".into(),
            email: "dvb@dvb.invalid".into(),
            is_default: true,
            as_owner: false,
        };
        let owner = GithubPrefill {
            organization: None,
            owner_name: Some("Example Owner".into()),
            owner_email: Some("owner@example.invalid".into()),
        };
        let found = items(&owner, &connected, Some(Ok(none)), &logins, &identity);
        let text = lines(&found);
        assert_eq!(
            text,
            "ok       Git identity: Example Owner <owner@example.invalid>\n\
             ok       GitHub App: connected to example-org\n\
             missing  GitHub repository access: the App reaches no repository. Fix: dvb github connect; give the App access at https://github.com/settings/installations/2\n\
             ok       dvb publishes commits as dvb <dvb@dvb.invalid> (the default)\n\
             missing  Claude login: not signed in; the default hires need it. Fix: dvb login claude\n\
             ok       Codex login: signed in\n\
             2 setup items need you.\n"
        );
        assert_eq!(
            json(&found),
            serde_json::json!({"items": [
                {"met": true, "text": "Git identity: Example Owner <owner@example.invalid>"},
                {"met": true, "text": "GitHub App: connected to example-org"},
                {"met": false, "text": "GitHub repository access: the App reaches no repository. Fix: dvb github connect; give the App access at https://github.com/settings/installations/2"},
                {"met": true, "text": "dvb publishes commits as dvb <dvb@dvb.invalid> (the default)"},
                {"met": false, "text": "Claude login: not signed in; the default hires need it. Fix: dvb login claude"},
                {"met": true, "text": "Codex login: signed in"},
            ], "unmet": 2})
        );
        let text = lines(&items(
            &owner,
            &connected,
            Some(Err("HTTP 500: down".into())),
            &logins,
            &identity,
        ));
        assert!(
            text.contains("missing  GitHub repository access: cannot be read (HTTP 500: down). Fix: dvb github connect; give the App access at https://github.com/settings/installations/2\n"),
            "{text}"
        );
    }
}
