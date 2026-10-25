//! `POST /logins/{vendor}` in dev mode through the real binary: the login run is
//! a child process of the engine running the vendor's login command (a stub here),
//! and the URL and result come back through the API.

pub mod support;

#[path = "../../../tests/support/executable.rs"]
pub mod executable;
use dvb_cli::client::{CliError, Client};
use dvb_core::RunId;
use dvb_core::api::{LoginCode, LoginStarted};
use std::path::Path;
use support::{Engine, engine_cmd, engine_up};

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    executable::write_executable(&p, &format!("#!/bin/sh\n{body}\n"));
}

async fn engine(tmp: &Path) -> (Engine, Client) {
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // `claude auth login` prints a URL and takes the code on stdin; `codex login
    // --device-auth` prints a URL and a device code and ends when the page is done.
    stub(
        &bin,
        "claude",
        "echo 'Visit https://claude.example/authorize?s=1 to sign in'\n\
         read code\n\
         test \"$code\" = pasted || { echo 'bad code'; exit 1; }\n\
         mkdir -p \"$HOME/.claude\" && echo '{}' > \"$HOME/.claude/.credentials.json\"",
    );
    stub(
        &bin,
        "codex",
        "echo '  https://auth.example/device'\necho '  ABCD-12345'\nsleep 0.3\n\
         mkdir -p \"$HOME/.codex\" && echo '{}' > \"$HOME/.codex/auth.json\"",
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let dir = tmp.join("e");
    let mut cmd = engine_cmd(&dir);
    cmd.env("PATH", path);
    let engine = Engine::spawn(cmd);
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    (engine, c)
}

async fn code(c: &Client, vendor: &str, run: &RunId, code: &str) -> LoginStarted {
    c.post(
        &format!("/logins/{vendor}"),
        &LoginCode {
            run: run.clone(),
            code: code.into(),
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_claude_login_returns_the_url_then_takes_the_code() {
    let tmp = tempfile::Builder::new().prefix("dvbl").tempdir().unwrap();
    let (_e, c) = engine(tmp.path()).await;
    let s: LoginStarted = c.post("/logins/claude", &()).await.unwrap();
    assert_eq!(s.url, "https://claude.example/authorize?s=1");
    assert!(s.prompt.contains("paste the code"), "{}", s.prompt);

    let wrong_run = s.run.clone();
    let wrong = code(&c, "claude", &s.run, "wrong").await;
    assert!(wrong.prompt.starts_with("Login failed"), "{}", wrong.prompt);

    // A second login run, with the right code.
    let s: LoginStarted = c.post("/logins/claude", &()).await.unwrap();
    let done = code(&c, "claude", &s.run, "pasted").await;
    assert_eq!(done.prompt, "Logged in.");
    let creds = tmp
        .path()
        .join("e/work")
        .join(s.run.as_str())
        .join("home/.claude/.credentials.json");
    assert!(creds.is_file(), "{}", creds.display());

    // The run has ended: a repeated submission is turned away and leaves no code.
    let again = code(&c, "claude", &s.run, "pasted").await;
    assert!(again.prompt.contains("already ended"), "{}", again.prompt);
    let after = code(&c, "claude", &wrong_run, "late").await;
    assert!(after.prompt.contains("already ended"), "{}", after.prompt);
    for run in [&s.run, &wrong_run] {
        assert!(
            !tmp.path()
                .join("e/work")
                .join(run.as_str())
                .join("code")
                .exists()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_codex_device_login_shows_the_code_and_waits() {
    let tmp = tempfile::Builder::new().prefix("dvbl").tempdir().unwrap();
    let (_e, c) = engine(tmp.path()).await;
    let s: LoginStarted = c.post("/logins/codex", &()).await.unwrap();
    assert_eq!(s.url, "https://auth.example/device");
    assert!(s.prompt.contains("ABCD-12345"), "{}", s.prompt);
    let done = code(&c, "codex", &s.run, "").await;
    assert_eq!(done.prompt, "Logged in.");
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_the_fake_vendor_and_unknown_runs_have_no_login() {
    let tmp = tempfile::Builder::new().prefix("dvbl").tempdir().unwrap();
    let (_e, c) = engine(tmp.path()).await;
    let r: Result<LoginStarted, CliError> = c.post("/logins/fake", &()).await;
    assert!(
        matches!(r, Err(CliError::Refused(ref r)) if r.code == dvb_core::RefusalCode::NotFound),
        "{r:?}"
    );
    let r: Result<LoginStarted, CliError> = c
        .post(
            "/logins/claude",
            &LoginCode {
                run: RunId::new("nope"),
                code: "x".into(),
            },
        )
        .await;
    assert!(
        matches!(r, Err(CliError::Refused(ref r)) if r.code == dvb_core::RefusalCode::NotFound),
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_typed_cli_vendor_logins_keep_code_and_device_handoffs() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tmp = tempfile::Builder::new().prefix("dvbl").tempdir().unwrap();
    let (_e, _c) = engine(tmp.path()).await;
    for vendor in ["claude", "codex"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(tmp.path().join("e"))
            .args(["login", vendor])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if vendor == "claude" {
            child.stdin.take().unwrap().write_all(b"pasted\n").unwrap();
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("Open this link:"), "{text}");
        assert!(text.ends_with("Logged in.\n"), "{text}");
    }
    let runs: Vec<_> = std::fs::read_dir(tmp.path().join("e/work"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 2);
    for (vendor, file) in [("claude", ".credentials.json"), ("codex", "auth.json")] {
        assert!(
            runs.iter()
                .any(|run| run.join(format!("home/.{vendor}/{file}")).is_file())
        );
    }
    for run in runs {
        assert_eq!(
            std::fs::read_to_string(run.join("login-result")).unwrap(),
            "ok"
        );
        assert!(!run.join("code").exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_failed_cli_login_prints_failure_once() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tmp = tempfile::Builder::new().prefix("dvbl").tempdir().unwrap();
    let (_e, _c) = engine(tmp.path()).await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(tmp.path().join("e"))
        .args(["login", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"wrong\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stdout.matches("Login failed").count() + stderr.matches("Login failed").count(),
        1,
        "{stdout}\n{stderr}"
    );
    assert!(stderr.contains("bad code"), "{stderr}");
}
