//! Owner settings through the actual CLI/socket.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::Settings;
use dvb_engine::{Config, Engine};
use std::path::Path;
use std::process::{Command, Output};

async fn cli(socket: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dvb"));
    command.arg("--sock").arg(socket).args(args);
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_settings_lists_and_sets_max_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), env!("CARGO_BIN_EXE_dvb").into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    let c = Client::new(cfg.paths.engine_sock.clone());
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    support::engine_up(&c).await;
    for (args, limit) in [
        (vec!["settings"], 3),
        (vec!["settings", "set", "max_runs", "6"], 6),
        (vec!["settings", "get"], 6),
        (vec!["settings", "set", "max_runs", "2"], 2),
        (vec!["settings", "set", "notify_completed", "true"], 2),
    ] {
        let out = cli(&cfg.paths.engine_sock, &args).await;
        assert!(out.status.success(), "{args:?}: {out:?}");
        let saved: Settings = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(saved.values["max_runs"], limit);
    }
    serving.abort();
}

/// `dvb settings publication` with `args`: exit code, stdout and stderr.
async fn publication(socket: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = cli(socket, &[&["settings", "publication"], args].concat()).await;
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_publication_goes_back_to_the_default_with_neutral_alone_after_as_owner() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), env!("CARGO_BIN_EXE_dvb").into());
    std::fs::create_dir_all(&cfg.paths.state).unwrap();
    dvb_engine::publish::PublicationSettings {
        owner_name: "Example Owner".into(),
        owner_email: "owner@example.invalid".into(),
        ..Default::default()
    }
    .save(&cfg.paths.state)
    .unwrap();
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    let c = Client::new(cfg.paths.engine_sock.clone());
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    support::engine_up(&c).await;
    let socket = cfg.paths.engine_sock.as_path();
    let owner = [
        "--name",
        "Example Owner",
        "--email",
        "owner@example.invalid",
    ];
    let (code, _, stderr) = publication(socket, &owner).await;
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "dvb: refused: INVALID_VALUE (The commit author name can't contain your name \
         (Example Owner) unless you publish as yourself. Give another with --name or \
         --email, or publish as yourself with --as-owner.)\n"
    );
    let (code, _, stderr) = publication(socket, &["--as-owner", "--name", ""]).await;
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "dvb: refused: INVALID_VALUE (The commit author name is empty. Give another with \
         --name or --email.)\n"
    );
    let (code, stdout, _) = publication(
        socket,
        &[
            "--as-owner",
            "--name",
            "Example Owner",
            "--email",
            "owner@example.invalid",
        ],
    )
    .await;
    assert_eq!(code, Some(0));
    assert_eq!(
        stdout,
        "dvb publishes commits as Example Owner <owner@example.invalid>, as you: they count on that GitHub account\n"
    );
    assert_eq!(
        publication(socket, &["--neutral"]).await,
        (
            Some(0),
            "dvb publishes commits as dvb <dvb@dvb.invalid> (the default)\n".into(),
            String::new()
        )
    );
    serving.abort();
}
