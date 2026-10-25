//! The handover between two binaries and crash recovery.
//!
//! `state/update.json` says "a handover is in progress, to this target, from this
//! snapshot", and while it exists the target binary owns recovery. `state/rollback`
//! says "restore wins". Otherwise the database's own migration table says whether
//! the migration happened. `recover` runs first at every start and is the whole
//! crash table; `prepare` is what the old binary does before it `exec`s.

use crate::engine::Config;
use dvb_core::{Actor, Clock, Event, EventId, EventKind, SystemClock};
use dvb_store::{NewEvent, Store};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Store(#[from] dvb_store::StoreError),
    #[error("invalid version: {0}")]
    InvalidVersion(String),
    #[error("{0}: no directory")]
    NoDirectory(PathBuf),
    #[error("{0}")]
    Message(String),
}
impl From<String> for HandoverError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}
impl From<&str> for HandoverError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}

/// Where the versions, `current` and the update files live.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `/opt/dvb`: `versions/<v>/dvb` and the `current` symlink.
    pub opt: PathBuf,
    pub state: PathBuf,
}

impl Layout {
    pub fn install() -> Self {
        Self {
            opt: "/opt/dvb".into(),
            state: "/var/lib/dvb/state".into(),
        }
    }

    /// An install, or `<dev dir>/opt` beside the dev state.
    pub fn of(cfg: &Config) -> Self {
        if cfg.dev {
            let dir = cfg.paths.state.parent().unwrap_or(&cfg.paths.state);
            Self {
                opt: dir.join("opt"),
                state: cfg.paths.state.clone(),
            }
        } else {
            Self::install()
        }
    }

    /// install.sh leaves `role-engine` and `role-cockpit` in `/opt/dvb`: what this
    /// box was installed as, independent of any socket forwarded to it.
    pub fn has_role(&self, role: &str) -> bool {
        self.opt.join(format!("role-{role}")).exists()
    }

    /// A cockpit with no engine of its own: its engine is elsewhere, and the desk
    /// keeps its own `/opt/dvb`.
    pub fn cockpit_only(&self) -> bool {
        self.has_role("cockpit") && !self.has_role("engine")
    }

    pub fn versions(&self) -> PathBuf {
        self.opt.join("versions")
    }

    pub fn version_bin(&self, v: &str) -> Result<PathBuf, HandoverError> {
        let v = super::parse_version(v).ok_or_else(|| HandoverError::InvalidVersion(v.into()))?;
        Ok(self.versions().join(v.to_string()).join("dvb"))
    }

    pub fn current(&self) -> PathBuf {
        self.opt.join("current")
    }

    pub fn backups(&self) -> PathBuf {
        self.state.join("backups")
    }

    pub fn db(&self) -> PathBuf {
        self.state.join("dvb.sqlite")
    }

    pub fn update_json(&self) -> PathBuf {
        self.state.join("update.json")
    }

    pub fn rollback(&self) -> PathBuf {
        self.state.join("rollback")
    }
}

/// `state/update.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRecord {
    pub from: String,
    pub to: String,
    /// The snapshot's file name under `state/backups/`.
    pub snapshot: String,
}

/// `state/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rollback {
    pub snapshot: String,
    pub reason: String,
}

fn io(path: &Path, source: std::io::Error) -> HandoverError {
    HandoverError::Io {
        path: path.into(),
        source,
    }
}

fn sync_dir(dir: &Path) -> Result<(), HandoverError> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io(dir, e))
}

/// Writes a file so it either exists whole or not at all: temp file, `fsync`,
/// rename, `fsync` of the directory.
pub fn write_durable(path: &Path, bytes: &[u8]) -> Result<(), HandoverError> {
    let dir = path
        .parent()
        .ok_or_else(|| HandoverError::NoDirectory(path.into()))?;
    dvb_core::io::write_atomic(path, bytes, 0o666, true).map_err(|e| io(path, e))?;
    sync_dir(dir)
}

fn remove_durable(path: &Path) -> Result<(), HandoverError> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(path, e)),
    }
    path.parent().map_or(Ok(()), sync_dir)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, HandoverError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| HandoverError::Json {
                    path: path.into(),
                    source,
                })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

/// Points `current` at `versions/<v>`, atomically.
pub fn set_current(l: &Layout, v: &str) -> Result<(), HandoverError> {
    let v = super::parse_version(v).ok_or_else(|| HandoverError::InvalidVersion(v.into()))?;
    let tmp = l.opt.join("current.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(format!("versions/{v}"), &tmp).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, l.current()).map_err(|e| io(&l.current(), e))?;
    sync_dir(&l.opt)
}

/// Replaces the database with a snapshot, atomically: copy beside it, `fsync`,
/// rename over. Repeating it gives the same file (nothing was written since the
/// snapshot, and a leftover journal of the failed run is removed).
fn restore(l: &Layout, snapshot: &str) -> Result<(), HandoverError> {
    let from = l.backups().join(snapshot);
    let db = l.db();
    let tmp = l.state.join("dvb.sqlite.restore");
    std::fs::copy(&from, &tmp).map_err(|e| io(&from, e))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| io(&tmp, e))?;
    for suffix in ["-wal", "-shm"] {
        let mut side = db.clone().into_os_string();
        side.push(suffix);
        match std::fs::remove_file(PathBuf::from(&side)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(Path::new(&side), e)),
        }
    }
    std::fs::rename(&tmp, &db).map_err(|e| io(&db, e))?;
    sync_dir(&l.state)
}

/// What the caller does after `recover`.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Carry on starting.
    Run,
    /// `exec` this binary instead (with the same arguments).
    Exec(PathBuf),
}

/// Runs first at every engine start; recovers interrupted handovers. `running` is
/// this binary's version; `delegated` is true when the previous binary `exec`ed
/// us, which stops a mislabelled target from handing back and forth. `migrate`
/// opens the database and applies the migrations in one transaction
/// (`Store::open`); tests pass a stand-in.
pub fn recover(
    l: &Layout,
    running: &str,
    delegated: bool,
    migrate: &dyn Fn(&Path) -> Result<Store, HandoverError>,
) -> Result<Next, HandoverError> {
    let record: Option<UpdateRecord> = read_json(&l.update_json())?;
    let rollback: Option<Rollback> = read_json(&l.rollback())?;
    match (record, rollback) {
        (None, None) => Ok(Next::Run),
        // The restore finished and only the marker is left.
        (None, Some(_)) => {
            remove_durable(&l.rollback())?;
            Ok(Next::Run)
        }
        // Restore wins over the migration table: repeat it.
        (Some(rec), Some(rb)) => finish_rollback(l, &rec, &rb.reason, running),
        (Some(rec), None) if running != rec.to => {
            let target = l.version_bin(&rec.to)?;
            if delegated || !target.exists() {
                let why = if delegated {
                    format!(
                        "{} was started as {} but is not that version",
                        target.display(),
                        rec.to
                    )
                } else {
                    format!("{} is missing", target.display())
                };
                return roll_back(l, &rec, &why, running);
            }
            Ok(Next::Exec(target))
        }
        (Some(rec), None) => match migrate(&l.db()) {
            Ok(store) => finish(l, &rec, &store),
            Err(why) => roll_back(l, &rec, &format!("the migration failed: {why}"), running),
        },
    }
}

/// The target has migrated: move `current`, record it once, drop `update.json`,
/// and clear what is older than the previous version.
fn finish(l: &Layout, rec: &UpdateRecord, store: &Store) -> Result<Next, HandoverError> {
    set_current(l, &rec.to)?;
    let seen = store
        .select(EventId(0), dvb_store::Select::Kinds(&["engine.updated"]))
        ?
        .iter()
        .any(|e| matches!(&e.kind, EventKind::EngineUpdated { snapshot, .. } if *snapshot == rec.snapshot));
    if !seen {
        append(
            store,
            EventKind::EngineUpdated {
                from: rec.from.clone(),
                to: rec.to.clone(),
                snapshot: rec.snapshot.clone(),
            },
        )?;
    }
    remove_durable(&l.update_json())?;
    prune(l, rec);
    Ok(Next::Run)
}

/// Keeps the previous version and its snapshot until the next successful update.
fn prune(l: &Layout, rec: &UpdateRecord) {
    prune_versions(l, &[&rec.from, &rec.to]);
    prune_dir(&l.backups(), &[&rec.snapshot]);
}

/// Removes every unpacked version except `keep`.
pub fn prune_versions(l: &Layout, keep: &[&str]) {
    prune_dir(&l.versions(), keep);
}

fn prune_dir(dir: &Path, keep: &[&str]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !keep.contains(&entry.file_name().to_string_lossy().as_ref()) {
            let _ = std::fs::remove_dir_all(entry.path());
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Writes `state/rollback`, then does the rest.
fn roll_back(
    l: &Layout,
    rec: &UpdateRecord,
    reason: &str,
    running: &str,
) -> Result<Next, HandoverError> {
    let rb = Rollback {
        snapshot: rec.snapshot.clone(),
        reason: reason.to_owned(),
    };
    let bytes = serde_json::to_vec(&rb).map_err(|source| HandoverError::Json {
        path: l.rollback(),
        source,
    })?;
    write_durable(&l.rollback(), &bytes)?;
    finish_rollback(l, rec, reason, running)
}

/// Restore, delete `update.json` then `rollback`, record the failure, and go
/// back to the old binary. Every step can be repeated.
fn finish_rollback(
    l: &Layout,
    rec: &UpdateRecord,
    reason: &str,
    running: &str,
) -> Result<Next, HandoverError> {
    restore(l, &rec.snapshot)?;
    remove_durable(&l.update_json())?;
    remove_durable(&l.rollback())?;
    let store = Store::open_unmigrated(&l.db())?;
    append(
        &store,
        EventKind::EngineUpdateFailed {
            from: rec.from.clone(),
            to: rec.to.clone(),
            reason: reason.to_owned(),
        },
    )?;
    Ok(if running == rec.from {
        Next::Run
    } else {
        Next::Exec(l.version_bin(&rec.from)?)
    })
}

fn append(store: &Store, kind: EventKind) -> Result<Vec<Event>, HandoverError> {
    store
        .append(
            SystemClock.now(),
            vec![NewEvent {
                actor: Actor::Engine,
                project: None,
                kind,
                evidence: None,
            }],
        )
        .map_err(HandoverError::from)
}

/// The old binary's last steps before `exec`: snapshot the
/// database into `state/backups/<from>-<time>.sqlite` (mode 0600) and write
/// `update.json` naming it. Returns the record.
pub fn prepare(
    l: &Layout,
    store: &Store,
    from: &str,
    to: &str,
) -> Result<UpdateRecord, HandoverError> {
    let from = super::parse_version(from)
        .ok_or_else(|| HandoverError::InvalidVersion(from.into()))?
        .to_string();
    let to = super::parse_version(to)
        .ok_or_else(|| HandoverError::InvalidVersion(to.into()))?
        .to_string();
    std::fs::create_dir_all(l.backups()).map_err(|e| io(&l.backups(), e))?;
    let secs = SystemClock.now().0 / 1000;
    let snapshot = format!("{from}-{secs}.sqlite");
    let path = l.backups().join(&snapshot);
    let _ = std::fs::remove_file(&path);
    store.snapshot(&path)?;
    sync_dir(&l.backups())?;
    let rec = UpdateRecord { from, to, snapshot };
    let bytes = serde_json::to_vec(&rec).map_err(|source| HandoverError::Json {
        path: l.update_json(),
        source,
    })?;
    write_durable(&l.update_json(), &bytes)?;
    Ok(rec)
}

/// Undoes `prepare` when `exec` did not happen.
pub fn abandon(l: &Layout) -> Result<(), HandoverError> {
    remove_durable(&l.update_json())
}
