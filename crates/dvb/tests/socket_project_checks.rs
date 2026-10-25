//! Owner project check commands against a Fake engine, including durable history.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::{Actor, EventKind, ProjectId, api::ProjectView, event::CiMode};
use dvb_engine::{Config, Engine};
use std::path::Path;
use std::process::{Command, Output};

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn success(dir: &Path, args: &[&str]) -> String {
    let out = cli(dir, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn socket_project_checks_set_print_no_ci_refuse_empty_and_show_history() {
    let dir = tempfile::Builder::new().prefix("dvbc").tempdir().unwrap();
    let e = Engine::open(
        Config::dev(dir.path(), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let id = ProjectId::new("p");
    e.append_checked(Some(&id), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectAdded {
            id: id.clone(),
            name: "project".into(),
            repo: "/nowhere".into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        }])
    })
    .unwrap();
    drop(e);
    let _engine = support::Engine::spawn(support::engine_cmd(dir.path()));
    let client = Client::new(dir.path().join("engine.sock"));
    support::engine_up(&client).await;
    assert_eq!(
        success(dir.path(), &["project", "checks", "project"]),
        "no CI\n"
    );
    assert_eq!(
        success(dir.path(), &["project", "checks", "project", "rust", "ui"]),
        "rust\nui\n"
    );
    assert_eq!(
        success(dir.path(), &["project", "checks", "p"]),
        "rust\nui\n"
    );
    let names = ["contracts", "e2e", "identity", "rust", "ui", "upgrade"];
    let mut args = vec!["project", "checks", "project"];
    args.extend(names);
    assert_eq!(success(dir.path(), &args), names.join("\n") + "\n");
    let out = cli(dir.path(), &["project", "checks", "project", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("CI_EVIDENCE_MISSING")
    );
    let read: ProjectView = client.get("/projects/p").await.unwrap();
    assert_eq!(
        read.ci,
        CiMode::Expected(names.iter().map(|s| (*s).into()).collect())
    );
    assert_eq!(
        success(dir.path(), &["project", "checks", "project", "--no-ci"]),
        "no CI\n"
    );
    assert_eq!(success(dir.path(), &["project", "checks", "p"]), "no CI\n");
    let conflict = cli(
        dir.path(),
        &["project", "checks", "project", "--no-ci", "rust"],
    );
    assert_eq!(conflict.status.code(), Some(2));
    let h = success(dir.path(), &["history", "checks", "project"]);
    let lines: Vec<_> = h.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "reads and rejected writes add no versions: {h}"
    );
    assert!(lines[0].contains("no_ci"));
    assert!(lines[1].contains("upgrade"));
    assert!(lines[2].contains("rust") && lines[2].contains("ui"));
    assert!(lines[3].contains("no_ci"));
    assert!(lines.iter().all(|l| l.contains("owner")));
}
