//! dvb-setup's pre-install backup (`dvb store-snapshot`) of an engine stopped
//! by SIGTERM, which leaves committed events in the WAL.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::EventKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

/// Whether `db` holds the `max_runs` 6 setting; an error when it holds no log.
fn has_max_runs_6(db: &Path) -> Result<bool, dvb_store::StoreError> {
    let events = dvb_store::Store::open_unmigrated(db)?.after(dvb_core::EventId(0))?;
    Ok(events.into_iter().any(|e| {
        matches!(e.kind, EventKind::SettingsChanged { key, new, .. }
            if key == "max_runs" && new == 6)
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backup_of_a_terminated_engine_holds_its_wal_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = support::Engine::spawn(support::engine_cmd(dir.path()));
    let c = Client::new(dir.path().join("engine.sock"));
    support::engine_up(&c).await;
    let set = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(&c.sock)
        .args(["settings", "set", "max_runs", "6"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{set:?}");

    // What `systemctl stop` sends; the engine exits without a checkpoint.
    let pid = engine.0.id().to_string();
    let kill = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(kill.success());
    assert_eq!(engine.0.wait().unwrap().signal(), Some(15));

    let state = dir.path().join("state");
    let db = state.join("dvb.sqlite");
    assert!(
        std::fs::metadata(state.join("dvb.sqlite-wal"))
            .unwrap()
            .len()
            > 0
    );
    // The plain copy dvb-setup used to take misses the event.
    let copy = dir.path().join("copy.sqlite");
    std::fs::copy(&db, &copy).unwrap();
    assert!(!has_max_runs_6(&copy).unwrap_or(false));

    let backup = dir.path().join("pre-install.sqlite");
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("store-snapshot")
        .arg(&db)
        .arg(&backup)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!dir.path().join("pre-install.sqlite-wal").exists());
    assert!(has_max_runs_6(&backup).unwrap());

    // A missing database is an error, not an empty backup.
    let missing = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("store-snapshot")
        .arg(dir.path().join("missing.sqlite"))
        .arg(dir.path().join("empty.sqlite"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(!dir.path().join("missing.sqlite").exists());
}
