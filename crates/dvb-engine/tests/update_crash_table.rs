//! Every update recovery case, as files and a real SQLite database in a
//! temporary directory. `recover` is what each binary runs first at start; the
//! tests call it as the old binary ("0.1.0") and as the target ("0.2.0").

use dvb_core::{Actor, EventId, EventKind};
use dvb_engine::update::handover::{
    Layout, Next, Rollback, UpdateRecord, prepare, recover, set_current, write_durable,
};
use dvb_store::{NewEvent, Store};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const OLD: &str = "0.1.0";
const NEW: &str = "0.2.0";

struct Box_ {
    _dir: tempfile::TempDir,
    l: Layout,
}

/// An install in a temp dir: `current` -> OLD, both versions unpacked, a
/// database with two events, and an older version and snapshot lying around.
fn install() -> Box_ {
    let dir = tempfile::tempdir().unwrap();
    let l = Layout {
        opt: dir.path().join("opt"),
        state: dir.path().join("state"),
    };
    std::fs::create_dir_all(&l.state).unwrap();
    for v in [OLD, NEW, "0.0.9"] {
        std::fs::create_dir_all(l.versions().join(v)).unwrap();
        std::fs::write(l.version_bin(v).unwrap(), b"#!/bin/sh\n").unwrap();
    }
    set_current(&l, OLD).unwrap();
    std::fs::create_dir_all(l.backups()).unwrap();
    std::fs::write(l.backups().join("0.0.9-1.sqlite"), b"old snapshot").unwrap();
    let store = Store::open(&l.db()).unwrap();
    for v in ["a", "b"] {
        event(&store, EventKind::EngineStarted { version: v.into() });
    }
    Box_ { _dir: dir, l }
}

fn event(store: &Store, kind: EventKind) {
    store
        .append(
            dvb_core::Timestamp(1),
            vec![NewEvent {
                actor: Actor::Engine,
                project: None,
                kind,
                evidence: None,
            }],
        )
        .unwrap();
}

/// The migration the target brings: a table. Applied in one transaction, and
/// only when it is not there (a restart after the commit applies nothing).
fn apply_migration(db: &Path) -> Result<Store, dvb_engine::update::handover::HandoverError> {
    let mut c = Connection::open(db).map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute("CREATE TABLE IF NOT EXISTS probe (x INTEGER)", [])
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    drop(c);
    Store::open(db).map_err(dvb_engine::update::handover::HandoverError::from)
}

/// A migration that changes something, then fails: nothing of it may stay.
fn failing_migration(db: &Path) -> Result<Store, dvb_engine::update::handover::HandoverError> {
    let _ = apply_migration(db)?;
    Err("syntax error near 'ALTER'".into())
}

fn has_probe(l: &Layout) -> bool {
    Connection::open(l.db())
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'probe'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

fn kinds(l: &Layout) -> Vec<String> {
    Store::open_unmigrated(&l.db())
        .unwrap()
        .after(EventId(0))
        .unwrap()
        .iter()
        .map(|e| e.kind.kind().to_owned())
        .collect()
}

fn count(l: &Layout, kind: &str) -> usize {
    kinds(l).iter().filter(|k| *k == kind).count()
}

fn failure_reason(l: &Layout) -> String {
    Store::open_unmigrated(&l.db())
        .unwrap()
        .after(EventId(0))
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|e| {
            if let EventKind::EngineUpdateFailed { reason, .. } = e.kind {
                Some(reason)
            } else {
                None
            }
        })
        .expect("engine.update_failed was recorded")
}

fn current(l: &Layout) -> PathBuf {
    std::fs::read_link(l.current()).unwrap()
}

fn exists(l: &Layout) -> (bool, bool) {
    (l.update_json().exists(), l.rollback().exists())
}

/// Step 4 done: the snapshot and `update.json` exist, as the old binary left them.
fn handed_over(b: &Box_) -> UpdateRecord {
    let store = Store::open(&b.l.db()).unwrap();
    let rec = prepare(&b.l, &store, OLD, NEW).unwrap();
    drop(store);
    rec
}

fn as_old(
    l: &Layout,
    m: &dyn Fn(&Path) -> Result<Store, dvb_engine::update::handover::HandoverError>,
) -> Next {
    recover(l, OLD, false, m).unwrap()
}

fn as_new(
    l: &Layout,
    delegated: bool,
    m: &dyn Fn(&Path) -> Result<Store, dvb_engine::update::handover::HandoverError>,
) -> Next {
    recover(l, NEW, delegated, m).unwrap()
}

fn assert_finished(l: &Layout) {
    assert_eq!(exists(l), (false, false));
    assert_eq!(current(l), PathBuf::from(format!("versions/{NEW}")));
    assert_eq!(count(l, "engine.updated"), 1);
    assert!(has_probe(l), "the migration is applied");
}

#[test]
fn a_snapshot_is_a_private_consistent_copy() {
    let b = install();
    let rec = handed_over(&b);
    let snap = b.l.backups().join(&rec.snapshot);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&snap).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let c = Connection::open(&snap).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    assert!(rec.snapshot.starts_with("0.1.0-"));
    let json: UpdateRecord =
        serde_json::from_slice(&std::fs::read(b.l.update_json()).unwrap()).unwrap();
    assert_eq!(json, rec);
}

// Row 1: before step 4's rename.
#[test]
fn crash_before_update_json_exists_is_a_normal_start() {
    let b = install();
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
    assert_eq!(exists(&b.l), (false, false));
    assert!(!has_probe(&b.l));
    assert_eq!(current(&b.l), PathBuf::from(format!("versions/{OLD}")));
    // The update is offered again and goes through.
    handed_over(&b);
    assert_eq!(as_new(&b.l, true, &apply_migration), Next::Run);
    assert_finished(&b.l);
}

// Row 2: after the rename, before the migration commits.
#[test]
fn crash_before_the_migration_the_old_binary_delegates_and_the_target_migrates() {
    let b = install();
    handed_over(&b);
    assert_eq!(
        as_old(&b.l, &apply_migration),
        Next::Exec(b.l.version_bin(NEW).unwrap())
    );
    assert!(!has_probe(&b.l), "the old binary never touches the store");
    assert_eq!(count(&b.l, "engine.updated"), 0);
    assert_eq!(as_new(&b.l, true, &apply_migration), Next::Run);
    assert_finished(&b.l);
    // The previous version and its snapshot stay; older ones go.
    assert!(b.l.versions().join(OLD).exists() && b.l.versions().join(NEW).exists());
    assert!(!b.l.versions().join("0.0.9").exists());
    assert!(!b.l.backups().join("0.0.9-1.sqlite").exists());
    assert_eq!(std::fs::read_dir(b.l.backups()).unwrap().count(), 1);
}

#[test]
fn crash_before_the_migration_the_target_may_be_the_one_systemd_starts() {
    let b = install();
    handed_over(&b);
    assert_eq!(as_new(&b.l, false, &apply_migration), Next::Run);
    assert_finished(&b.l);
}

// Row 3: after the commit, before `current` moves.
#[test]
fn crash_after_the_migration_commit_the_target_only_finishes() {
    let b = install();
    handed_over(&b);
    apply_migration(&b.l.db()).unwrap();
    assert!(has_probe(&b.l));
    assert_eq!(current(&b.l), PathBuf::from(format!("versions/{OLD}")));
    assert_eq!(
        as_old(&b.l, &apply_migration),
        Next::Exec(b.l.version_bin(NEW).unwrap())
    );
    assert_eq!(as_new(&b.l, true, &apply_migration), Next::Run);
    assert_finished(&b.l);
}

// Row 4: after `current` moves, before the delete.
#[test]
fn crash_after_current_moved_the_target_records_and_deletes() {
    let b = install();
    handed_over(&b);
    apply_migration(&b.l.db()).unwrap();
    set_current(&b.l, NEW).unwrap();
    assert_eq!(as_new(&b.l, false, &apply_migration), Next::Run);
    assert_finished(&b.l);
}

#[test]
fn crash_after_it_was_recorded_does_not_record_it_twice() {
    let b = install();
    let rec = handed_over(&b);
    assert_eq!(as_new(&b.l, true, &apply_migration), Next::Run);
    // The delete did not happen: the record is back.
    write_durable(&b.l.update_json(), &serde_json::to_vec(&rec).unwrap()).unwrap();
    assert_eq!(as_new(&b.l, false, &apply_migration), Next::Run);
    assert_finished(&b.l);
}

// Row 5: after `rollback` is written, during or after the restore.
fn rollback_written(b: &Box_) -> UpdateRecord {
    let rec = handed_over(b);
    // The failed migration left its mark, then rollback was written.
    apply_migration(&b.l.db()).unwrap();
    let rb = Rollback {
        snapshot: rec.snapshot.clone(),
        reason: "the migration failed: boom".into(),
    };
    write_durable(&b.l.rollback(), &serde_json::to_vec(&rb).unwrap()).unwrap();
    rec
}

#[test]
fn rollback_wins_and_the_old_binary_carries_on() {
    let b = install();
    rollback_written(&b);
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
    assert_eq!(exists(&b.l), (false, false));
    assert!(!has_probe(&b.l), "the database is the snapshot again");
    assert_eq!(count(&b.l, "engine.started"), 2);
    assert_eq!(failure_reason(&b.l), "the migration failed: boom");
    assert_eq!(current(&b.l), PathBuf::from(format!("versions/{OLD}")));
    assert_eq!(count(&b.l, "engine.updated"), 0);
}

#[test]
fn rollback_wins_and_the_target_hands_back_to_the_old_binary() {
    let b = install();
    rollback_written(&b);
    assert_eq!(
        as_new(&b.l, true, &apply_migration),
        Next::Exec(b.l.version_bin(OLD).unwrap())
    );
    assert_eq!(exists(&b.l), (false, false));
    assert!(!has_probe(&b.l));
    assert_eq!(count(&b.l, "engine.update_failed"), 1);
}

#[test]
fn the_restore_can_be_repeated() {
    let b = install();
    let rec = rollback_written(&b);
    assert_eq!(
        as_new(&b.l, true, &apply_migration),
        Next::Exec(b.l.version_bin(OLD).unwrap())
    );
    // Crashed after the restore but before the deletes: both files are back.
    let _ = apply_migration(&b.l.db());
    write_durable(&b.l.update_json(), &serde_json::to_vec(&rec).unwrap()).unwrap();
    let rb = Rollback {
        snapshot: rec.snapshot,
        reason: "again".into(),
    };
    write_durable(&b.l.rollback(), &serde_json::to_vec(&rb).unwrap()).unwrap();
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
    assert!(!has_probe(&b.l));
    assert_eq!(exists(&b.l), (false, false));
    assert_eq!(count(&b.l, "engine.started"), 2);
}

#[test]
fn a_rollback_marker_alone_means_the_restore_finished() {
    let b = install();
    let rb = Rollback {
        snapshot: "x".into(),
        reason: "y".into(),
    };
    write_durable(&b.l.rollback(), &serde_json::to_vec(&rb).unwrap()).unwrap();
    apply_migration(&b.l.db()).unwrap();
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
    assert_eq!(exists(&b.l), (false, false));
    assert!(has_probe(&b.l), "no restore is repeated");
}

// Step 6, failure: the target's migration fails.
#[test]
fn a_failed_migration_restores_the_snapshot_and_returns_to_the_old_binary() {
    let b = install();
    handed_over(&b);
    assert_eq!(
        as_new(&b.l, true, &failing_migration),
        Next::Exec(b.l.version_bin(OLD).unwrap())
    );
    assert_eq!(exists(&b.l), (false, false));
    assert!(!has_probe(&b.l));
    assert_eq!(current(&b.l), PathBuf::from(format!("versions/{OLD}")));
    assert_eq!(count(&b.l, "engine.updated"), 0);
    assert_eq!(count(&b.l, "engine.started"), 2);
    assert!(failure_reason(&b.l).contains("syntax error near 'ALTER'"));
    // The old binary then starts normally.
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
}

#[test]
fn a_missing_target_is_undone_by_the_old_binary() {
    let b = install();
    handed_over(&b);
    std::fs::remove_dir_all(b.l.versions().join(NEW)).unwrap();
    assert_eq!(as_old(&b.l, &apply_migration), Next::Run);
    assert_eq!(exists(&b.l), (false, false));
    assert!(failure_reason(&b.l).contains("is missing"));
}

#[test]
fn a_target_that_is_not_the_version_it_was_named_for_is_undone() {
    let b = install();
    handed_over(&b);
    // Started by exec, yet it is some other version: it must not hand back and forth.
    assert_eq!(
        recover(&b.l, "0.0.9", true, &apply_migration).unwrap(),
        Next::Exec(b.l.version_bin(OLD).unwrap())
    );
    assert!(failure_reason(&b.l).contains("not that version"));
    assert_eq!(exists(&b.l), (false, false));
}

#[test]
fn going_back_to_the_old_version_after_a_failure_loses_no_event() {
    let b = install();
    handed_over(&b);
    as_new(&b.l, true, &failing_migration);
    assert_eq!(
        kinds(&b.l),
        ["engine.started", "engine.started", "engine.update_failed"]
    );
}

#[test]
fn durable_replacement_keeps_mode_whole_bytes_and_cleans_failed_staging() {
    use std::error::Error;
    use std::os::unix::fs::{MetadataExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    std::fs::write(&original, "kept").unwrap();
    let mode = std::fs::metadata(&original).unwrap().mode() & 0o777;
    let target = dir.path().join("update.json");
    symlink(&original, &target).unwrap();
    write_durable(&target, b"complete marker").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"complete marker");
    assert_eq!(std::fs::read(&original).unwrap(), b"kept");
    assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, mode);
    let directory = dir.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    let before = std::fs::read_dir(dir.path()).unwrap().count();
    let error = write_durable(&directory, b"cannot install").unwrap_err();
    assert!(error.source().unwrap().is::<std::io::Error>());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before);
    assert!(directory.is_dir());
}
