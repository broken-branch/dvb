//! `POST /update` end to end. The new binary's half is
//! `handover::recover`, run at the next start.

use super::fetch::{Origin, fetch};
use super::handover::{self, Layout};
use super::{available, release_public_key};
use crate::InternalError;
use crate::engine::{Engine, Fail};
use dvb_core::api::{StartUpdate, UpdateInfo, UpdateStatus};
use dvb_core::{Actor, Clock, EventKind, Refusal, RefusalCode};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn refused(why: impl Into<String>) -> Fail {
    Fail::Refused(Refusal::new(RefusalCode::UpdateRefused, why))
}

/// Runs still going: live runs that have a unit (those waiting in `admit`, and
/// those queued for their vendor or held by a pause, have none).
pub fn runs_active(e: &Engine) -> usize {
    let proj = e.proj();
    let queued = e.queued.lock().unwrap_or_else(dvb_core::sync::recover);
    proj.live_runs()
        .filter(|run| !queued.contains_key(&run.assignment.run))
        .count()
}

/// `GET /update`'s answer while an update is under way.
pub fn status(e: &Engine) -> Option<UpdateStatus> {
    e.update.target().map(|to| UpdateStatus {
        to,
        runs_active: u32::try_from(runs_active(e)).unwrap_or(u32::MAX),
    })
}

/// How long the drain waits for running work before stopping it.
pub const DRAIN_MILLIS: i64 = 30_000;

/// Waits for running work, up to the drain bound on the injected clock.
pub async fn drain(active: impl Fn() -> usize, clock: &dyn Clock, poll: Duration) {
    let deadline = clock.now().plus_millis(DRAIN_MILLIS);
    while active() > 0 {
        if clock.now() >= deadline {
            break;
        }
        tokio::time::sleep(poll).await;
    }
}

/// What the old binary does after the drain: quiesce, snapshot, `update.json`,
/// then `exec`. `exec` returns only when it failed; the update is then undone and
/// the failure recorded. Returns when the update did not take.
pub async fn handover(e: &Arc<Engine>, l: &Layout, to: &str, exec: &Exec) -> String {
    drain(
        || runs_active(e),
        e.clock.as_ref(),
        Duration::from_millis(500),
    )
    .await;
    // Runs still going are stopped, resumable, so none spans the update.
    crate::dispatch::stop_for_update(e, to).await;
    // Preserve receipts for forge operations already in flight before taking
    // the snapshot. Closed admission holds all subsequent effects.
    while e.update.effects_inflight() > 0 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    e.quiesce();
    let outcome = match handover::prepare(l, &e.store, dvb_core::VERSION, to) {
        Ok(rec) => {
            let err = match l.version_bin(&rec.to) {
                Ok(path) => exec(&path).to_string(),
                Err(why) => why.to_string(),
            };
            let _ = handover::abandon(l);
            format!("could not start {}: {err}", rec.to)
        }
        Err(why) => why.to_string(),
    };
    e.update.lift_quiesce();
    e.update.reopen_admission();
    e.update.end();
    let _ = e.append_checked(None, Actor::Engine, |_, _| {
        Ok(vec![EventKind::EngineUpdateFailed {
            from: dvb_core::VERSION.to_owned(),
            to: to.to_owned(),
            reason: outcome.clone(),
        }])
    });
    outcome
}

/// Replaces this process with the target binary, same arguments. Returns only on
/// failure. `listen` is `LISTEN_FDNAMES` when systemd gave us our sockets: they
/// stay open across the exec and the target is told where they are (the pid does
/// not change), so no connection attempt is ever refused.
pub fn exec_target(target: &Path, listen: Option<&str>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(target);
    cmd.args(std::env::args_os().skip(1))
        .env("DVB_HANDOVER", "1");
    if let Some(names) = listen {
        cmd.env("LISTEN_FDS", names.split(':').count().to_string())
            .env("LISTEN_PID", std::process::id().to_string())
            .env("LISTEN_FDNAMES", names);
    }
    cmd.exec()
}

/// What `exec` is: replace the process, or (in tests) pretend to.
pub type Exec = std::sync::Arc<dyn Fn(&Path) -> std::io::Error + Send + Sync>;

/// Where an update reads releases and what it trusts.
#[derive(Debug, Clone)]
pub struct Source {
    /// The release list (GitHub's API shape); none in a dev-mode engine unless
    /// `DVB_RELEASES_URL` names one.
    pub releases_url: Option<String>,
    pub token: Option<String>,
    /// Where a release's files are read from.
    pub origin: Origin,
    /// The pinned minisign public key.
    pub key: String,
}

impl Source {
    pub fn from_env(e: &Engine) -> Self {
        Self::for_mode(e.cfg.dev)
    }

    /// From the environment: `dev` engines have no release list unless
    /// `DVB_RELEASES_URL` names one.
    pub fn for_mode(dev: bool) -> Self {
        Self {
            releases_url: std::env::var("DVB_RELEASES_URL")
                .ok()
                .or_else(|| (!dev).then(super::releases_url)),
            token: super::github_token(),
            origin: Origin::from_env(),
            key: release_public_key().to_owned(),
        }
    }

    /// The newest release if it is newer than this build.
    pub async fn newest(&self) -> Result<Option<String>, InternalError> {
        let url = self
            .releases_url
            .as_deref()
            .ok_or("a dev-mode engine has no release source (set DVB_RELEASES_URL)")?;
        available(url, self.token.as_deref(), dvb_core::VERSION).await
    }
}

/// A normalized version and its relation to the running version.
#[derive(Debug)]
pub(super) struct Target {
    pub version: String,
    pub downgrade: bool,
}

impl Source {
    /// Equal explicit targets and an exhausted release list both mean no update.
    pub(super) async fn target(
        &self,
        current: &str,
        to: Option<String>,
    ) -> Result<Option<Target>, InternalError> {
        let to = match to {
            Some(to) => to,
            None => match self.newest().await? {
                Some(to) => to,
                None => return Ok(None),
            },
        };
        let parsed = super::parse_version(&to)
            .ok_or_else(|| InternalError::from(format!("{to} is not a version")))?;
        let running = super::parse_version(current)
            .ok_or_else(|| InternalError::from(format!("{current} is not a version")))?;
        if parsed == running {
            return Ok(None);
        }
        Ok(Some(Target {
            version: parsed.to_string(),
            downgrade: parsed < running,
        }))
    }
}

/// `POST /update`: choose the release, fetch and verify it (a refusal is
/// shown at once), then drain and hand over in the background.
pub async fn start(e: Arc<Engine>, body: StartUpdate) -> Result<UpdateInfo, Fail> {
    let src = Source::from_env(&e);
    let listen = e.update.listen();
    let exec: Exec = Arc::new(move |target| exec_target(target, listen.as_deref()));
    start_with(e, body, src, exec).await
}

/// `start`, with the source and the `exec` given (tests pass a stand-in).
pub async fn start_with(
    e: Arc<Engine>,
    body: StartUpdate,
    src: Source,
    exec: Exec,
) -> Result<UpdateInfo, Fail> {
    let l = Layout::of(&e.cfg);
    let current = dvb_core::VERSION;
    let explicit = body.to.is_some();
    let target = src
        .target(current, body.to)
        .await
        .map_err(|error| refused(error.to_string()))?
        .ok_or_else(|| {
            refused(if explicit {
                format!("dvb {current} is already running")
            } else {
                format!("dvb {current} is the newest release")
            })
        })?;
    let to = target.version;
    let downgrade = target.downgrade;
    if !e.update.begin(&to) {
        return Err(refused("an update is already under way"));
    }
    let schema = dvb_store::schema_version();
    if let Err(why) = fetch(&l, &src.origin, &to, &src.key, schema, downgrade).await {
        e.update.end();
        return Err(refused(why.to_string()));
    }
    e.update.close_admission();
    let bg = e.clone();
    let to_bg = to.clone();
    crate::spawn("update", async move {
        let why = handover(&bg, &l, &to_bg, &exec).await;
        eprintln!("dvb engine: update to {to_bg} did not take: {why}");
        Ok(())
    });
    Ok(UpdateInfo {
        current: current.to_owned(),
        available: Some(to.clone()),
        updating: Some(status(&e).unwrap_or(UpdateStatus { to, runs_active: 0 })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, engine::Queued};
    use dvb_core::{ProjectId, RunId, TaskId};

    #[tokio::test]
    async fn shared_target_normalizes_versions_and_detects_downgrades_offline() {
        let source = Source {
            releases_url: None,
            token: None,
            origin: Origin::Base("file:///unused".into()),
            key: release_public_key().into(),
        };
        for (requested, expected, downgrade) in [
            ("v1.0.0-rc.2", "1.0.0-rc.2", true),
            ("0.9.9", "0.9.9", true),
        ] {
            let target = source
                .target("1.0.0", Some(requested.into()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(target.version, expected);
            assert_eq!(target.downgrade, downgrade);
        }
        assert!(
            source
                .target("1.0.0", Some("v1.0.0".into()))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            source
                .target("1.0.0", Some("1.0.0-../../../state".into()))
                .await
                .unwrap_err()
                .to_string()
                .contains("not a version")
        );
        assert!(
            source
                .target("invalid-current", Some("1.0.0".into()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_queued_run_waiting_in_admit_does_not_hide_a_running_builder() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        for name in ["running", "queued"] {
            e.append_checked(Some(&ProjectId::new("project")), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunStarted(
                    crate::test_support::assignment::Assignment::new(RunId::new(name))
                        .task(Some(TaskId::new(name)))
                        .build(),
                )])
            })
            .unwrap();
        }
        e.queued.lock().unwrap().insert(
            RunId::new("queued"),
            Queued {
                from: None,
                held: false,
            },
        );
        e.update.close_admission();
        // Poll admission once: the queued vendor has opened, but admission is closed.
        let admission = e.update.admit();
        tokio::pin!(admission);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), &mut admission)
                .await
                .is_err()
        );
        assert_eq!(e.proj().live_runs().count(), 2);
        assert_eq!(runs_active(&e), 1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                drain(
                    || runs_active(&e),
                    e.clock.as_ref(),
                    Duration::from_millis(1)
                )
            )
            .await
            .is_err()
        );
        e.update.reopen_admission();
        admission.await;
    }
}
