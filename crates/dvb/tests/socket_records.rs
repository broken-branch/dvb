//! `dvb history`, `dvb restore` and `dvb report` through the real binary against
//! a dev engine: a setting's versions newest first, a restore, a stale restore
//! refused with VALUE_CHANGED, and a report that only prints.

pub mod support;

use dvb_cli::client::Client;
use std::process::{Command, Output};
use support::{Engine, engine_cmd, engine_up};

fn dvb(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_history_restore_and_report_from_the_command_line() {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let dir = tmp.path().join("e");
    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;

    for v in [r#""Technical""#, r#""Expert""#] {
        assert!(
            dvb(&dir, &["settings", "set", "response_style", v])
                .status
                .success()
        );
    }
    let out = dvb(&dir, &["history", "setting", "response_style"]);
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<String> = text(&out).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with("\"Expert\""), "{lines:?}");
    assert!(lines[1].ends_with("\"Technical\""), "{lines:?}");
    let oldest = lines[1].split_whitespace().next().unwrap().to_owned();

    // A restore naming a stale `current` is refused; nothing changes.
    let out = dvb(
        &dir,
        &[
            "restore",
            "setting",
            "response_style",
            &oldest,
            "--current",
            &oldest,
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("VALUE_CHANGED"),
        "{out:?}"
    );

    let out = dvb(&dir, &["restore", "setting", "response_style", &oldest]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).contains("\"Technical\""), "{}", text(&out));
    let out = dvb(&dir, &["history", "setting", "response_style"]);
    assert_eq!(text(&out).lines().count(), 3);
    assert!(
        text(&out)
            .lines()
            .next()
            .unwrap()
            .ends_with("\"Technical\"")
    );

    let out = dvb(
        &dir,
        &["report", "bug", "--title", "It breaks", "Steps", "one"],
    );
    assert!(out.status.success(), "{out:?}");
    let shown = text(&out);
    assert!(shown.contains("Steps one"), "{shown}");
    assert!(
        shown.contains("https://github.com/broken-branch/dvb/issues/new?title=It%20breaks"),
        "{shown}"
    );
}
