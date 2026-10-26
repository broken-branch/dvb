//! The old binary's half of an update and the signed-release
//! checks. Releases are directories read through `file://`; keys are throwaway,
//! made inside the test and never written anywhere else.

use dvb_core::api::StartUpdate;
use dvb_core::{Actor, EventKind, RefusalCode};
use dvb_engine::update::State;
use dvb_engine::update::fetch::{Origin, fetch};
use dvb_engine::update::handover::Layout;
use dvb_engine::update::protocol::{self, Source};
use dvb_engine::{Config, Engine, Fail};
use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod support;

struct Keys {
    pk: String,
    sk: minisign::SecretKey,
    pk_box: minisign::PublicKey,
}

fn keys() -> Keys {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    Keys {
        pk: kp.pk.to_box().unwrap().to_string(),
        pk_box: kp.pk,
        sk: kp.sk,
    }
}

/// `<root>/v<version>/` with the tarball, `SHA256SUMS` and, when `sign` names a
/// key, `SHA256SUMS.minisig`. The release's `dvb` is a script that prints `says`.
fn release(root: &Path, version: &str, says: &str, schema: u32, sign: Option<&Keys>) {
    let rel = root.join(format!("v{version}"));
    let pack = root.join(format!("pack-{version}"));
    let top = pack.join(format!("dvb-{version}"));
    std::fs::create_dir_all(&top).unwrap();
    std::fs::create_dir_all(&rel).unwrap();
    let bin = top.join("dvb");
    std::fs::write(&bin, format!("#!/bin/sh\necho 'dvb {says}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(top.join("schema"), schema.to_string()).unwrap();
    pack_release(root, version, sign);
}

fn pack_release(root: &Path, version: &str, sign: Option<&Keys>) {
    let rel = root.join(format!("v{version}"));
    let pack = root.join(format!("pack-{version}"));
    let tarball = format!("dvb-{version}-x86_64-linux.tar.gz");
    assert!(
        Command::new("tar")
            .args(["-czf"])
            .arg(rel.join(&tarball))
            .arg("-C")
            .arg(&pack)
            .arg(format!("dvb-{version}"))
            .status()
            .unwrap()
            .success()
    );
    let sum = {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(std::fs::read(rel.join(&tarball)).unwrap());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let sums = format!("{sum}  {tarball}\n");
    std::fs::write(rel.join("SHA256SUMS"), &sums).unwrap();
    if let Some(k) = sign {
        let sig = minisign::sign(
            Some(&k.pk_box),
            &k.sk,
            Cursor::new(sums.as_bytes()),
            None,
            None,
        )
        .unwrap();
        std::fs::write(rel.join("SHA256SUMS.minisig"), sig.to_string()).unwrap();
    }
}

fn layout(dir: &Path) -> Layout {
    Layout {
        opt: dir.join("opt"),
        state: dir.join("state"),
    }
}

fn mirror(dir: &Path) -> Origin {
    Origin::Base(format!("file://{}", dir.join("rel").display()))
}

async fn fetch_ok(
    dir: &Path,
    version: &str,
    key: &str,
    schema: u32,
    downgrade: bool,
) -> Result<(), dvb_engine::InternalError> {
    let l = layout(dir);
    std::fs::create_dir_all(&l.opt).unwrap();
    fetch(&l, &mirror(dir), version, key, schema, downgrade).await
}

#[tokio::test]
async fn a_release_signed_with_the_pinned_key_is_unpacked() {
    for version in ["0.2.0", "v0.2.0"] {
        let dir = tempfile::tempdir().unwrap();
        let k = keys();
        release(&dir.path().join("rel"), "0.2.0", "0.2.0", 1, Some(&k));
        fetch_ok(dir.path(), version, &k.pk, 1, false)
            .await
            .unwrap();
        let l = layout(dir.path());
        assert!(l.version_bin("0.2.0").unwrap().exists());
        assert!(!l.opt.join(".staging-0.2.0").exists());
    }
}

#[tokio::test]
async fn a_release_signed_with_another_key_is_refused_and_nothing_is_unpacked() {
    let dir = tempfile::tempdir().unwrap();
    let (pinned, other) = (keys(), keys());
    release(&dir.path().join("rel"), "0.2.0", "0.2.0", 1, Some(&other));
    let err = fetch_ok(dir.path(), "0.2.0", &pinned.pk, 1, false)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not signed with dvb's release key"),
        "{err}"
    );
    assert!(!layout(dir.path()).versions().exists());
}

#[tokio::test]
async fn an_unsigned_release_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    release(&dir.path().join("rel"), "0.2.0", "0.2.0", 1, None);
    let err = fetch_ok(dir.path(), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not signed"), "{err}");
}

#[tokio::test]
async fn a_tampered_tarball_fails_its_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    let rel = dir.path().join("rel");
    release(&rel, "0.2.0", "0.2.0", 1, Some(&k));
    let tarball = rel.join("v0.2.0/dvb-0.2.0-x86_64-linux.tar.gz");
    let mut bytes = std::fs::read(&tarball).unwrap();
    bytes.push(0);
    std::fs::write(&tarball, bytes).unwrap();
    let err = fetch_ok(dir.path(), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("checksum"), "{err}");
}

#[tokio::test]
async fn tampered_sums_fail_the_signature() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    let rel = dir.path().join("rel");
    release(&rel, "0.2.0", "0.2.0", 1, Some(&k));
    let sums = rel.join("v0.2.0/SHA256SUMS");
    let mut text = std::fs::read_to_string(&sums).unwrap();
    text.push_str("00  evil\n");
    std::fs::write(&sums, text).unwrap();
    let err = fetch_ok(dir.path(), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not signed with dvb's release key"),
        "{err}"
    );
}

#[tokio::test]
async fn a_release_whose_binary_says_another_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    release(&dir.path().join("rel"), "0.2.0", "0.3.0", 1, Some(&k));
    let err = fetch_ok(dir.path(), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("says 'dvb 0.3.0'"), "{err}");
}

#[tokio::test]
async fn going_back_needs_the_same_schema() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    release(&dir.path().join("rel"), "0.0.9", "0.0.9", 1, Some(&k));
    let err = fetch_ok(dir.path(), "0.0.9", &k.pk, 2, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("same database schema"), "{err}");
    fetch_ok(dir.path(), "0.0.9", &k.pk, 1, true).await.unwrap();
}

use support::engine::engine;

fn started() -> Vec<EventKind> {
    vec![EventKind::EngineStarted {
        version: "t".into(),
    }]
}

#[test]
fn a_quiesced_engine_appends_nothing_and_still_reads() {
    let (_dir, e) = engine();
    let before = support::events::all(&e).len();
    e.update.quiesce();
    let Err(Fail::Refused(r)) = e.append_checked(None, Actor::Engine, |_, _| Ok(started())) else {
        panic!("an append was taken while quiesced")
    };
    assert_eq!(r.code, RefusalCode::EngineUpdating);
    assert_eq!(support::events::all(&e).len(), before);
    let _ = e.proj().live_runs().count();
    e.update.lift_quiesce();
    e.append_checked(None, Actor::Engine, |_, _| Ok(started()))
        .unwrap();
    assert_eq!(support::events::all(&e).len(), before + 1);
}

#[tokio::test]
async fn closed_admission_holds_a_run_until_it_reopens() {
    let s = std::sync::Arc::new(State::default());
    s.close_admission();
    let held = tokio::spawn({
        let s = s.clone();
        async move { s.admit().await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!held.is_finished());
    s.reopen_admission();
    held.await.unwrap();
    s.admit().await;
}

#[tokio::test]
async fn the_drain_waits_for_running_work() {
    let polls = AtomicUsize::new(0);
    let active = || usize::from(polls.fetch_add(1, Ordering::SeqCst) < 3);
    protocol::drain(
        active,
        &dvb_core::FakeClock::default(),
        Duration::from_millis(1),
    )
    .await;
    assert_eq!(polls.load(Ordering::SeqCst), 4);
}

#[derive(Default)]
struct DrainClock {
    clock: dvb_core::FakeClock,
    reads: AtomicUsize,
}

impl dvb_core::Clock for DrainClock {
    fn now(&self) -> dvb_core::Timestamp {
        let now = self.clock.now();
        self.reads.fetch_add(1, Ordering::SeqCst);
        now
    }
}

/// A run still going at the drain's bound is
/// stopped the owner's way before the exec (resumable, the update as its
/// reason), so no run spans the update; the target resumes it once with its
/// workspace and branch, held by the project's pause.
#[tokio::test]
async fn a_run_still_going_at_the_bound_is_stopped_before_the_exec() {
    use dvb_core::{Command as TaskCommand, ProjectId, RunId, TaskId, TaskState};
    use dvb_engine::update::handover::{Next, recover};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(DrainClock::default());
    let cfg = Config::dev(dir.path(), "dvb".into());
    let e = Engine::open(
        cfg.clone(),
        dvb_engine::EngineOptions {
            clock: Some(clock.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    e.manual_dispatch();
    let (p, t, r) = (ProjectId::new("p"), TaskId::new("t"), RunId::new("stuck"));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        let mut events = support::events::approved_task_events();
        let (roles, hires) = support::events::default_role_hire_events();
        events.extend(roles);
        events.extend(hires);
        Ok(events)
    })
    .unwrap();
    e.decide(
        e.proj(),
        &t,
        Actor::Engine,
        dvb_engine::Decision::new(
            "start",
            TaskCommand::Start(
                support::assignment::Assignment::new(r.clone())
                    .task(Some(t.clone()))
                    .base(Some("base".into()))
                    .build(),
            ),
        ),
        |_| Ok(()),
    )
    .unwrap();
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_active", TaskCommand::UnitActive),
    )
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::BranchPublished {
            task: t.clone(),
            branch: "dvb/t-1".into(),
            head: "published-head".into(),
            round: 1,
        }])
    })
    .unwrap();
    let workspace = e.cfg.paths.work.join(r.as_str()).join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("work-in-progress"), "keep this work").unwrap();
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    let l = Layout::of(&cfg);
    let k = keys();
    let to = next_version(dvb_core::VERSION);
    release(
        &dir.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&k),
    );
    let stopped = TaskState::Stopped {
        was: Box::new(TaskState::Running),
    };
    let next = Arc::new(Mutex::new(None));
    let probe: protocol::Exec = {
        let (e, l, to, next, stopped) = (
            e.clone(),
            l.clone(),
            to.clone(),
            next.clone(),
            stopped.clone(),
        );
        Arc::new(move |_: &Path| {
            assert!(e.update.quiesced());
            // Stopped and ended before the exec, with the update as its reason.
            assert_eq!(protocol::runs_active(&e), 0);
            assert_eq!(
                e.proj().runs[&RunId::new("stuck")].ended,
                Some(dvb_core::event::EndReason::Stopped)
            );
            assert_eq!(e.proj().tasks[&TaskId::new("t")].view.state, stopped);
            let reason = support::events::all(&e)
                .into_iter()
                .find_map(|ev| {
                    if let EventKind::TaskStopped { reason, .. } = ev.kind {
                        reason
                    } else {
                        None
                    }
                })
                .unwrap();
            assert!(reason.contains(&format!("update to dvb {to}")), "{reason}");
            // The target's startup recovery completes the prepared handover.
            assert_eq!(
                recover(&l, &to, true, &|db| {
                    dvb_store::Store::open(db)
                        .map_err(dvb_engine::update::handover::HandoverError::from)
                })
                .unwrap(),
                Next::Run
            );
            *next.lock().unwrap() = Some(
                Engine::open(
                    cfg.clone(),
                    dvb_engine::EngineOptions {
                        forge: None,
                        launcher: Some(Arc::new(support::launcher::FakeLauncher {
                            forbid_start: Some("the resumed run must wait for unpause"),
                            forbid_stop: Some("the run was stopped before the exec"),
                            forbid_state: Some("the run ended before the exec"),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
            std::io::Error::other("test: target startup simulated")
        })
    };
    let before = clock.reads.load(Ordering::SeqCst);
    protocol::start_with(e.clone(), up(&to), source(dir.path(), &k.pk), probe)
        .await
        .unwrap();
    support::wait::SLOW
        .until("the drain to start reading its clock", || {
            clock.reads.load(Ordering::SeqCst) > before
        })
        .await;
    clock.clock.advance_millis(protocol::DRAIN_MILLIS - 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        next.lock().unwrap().is_none(),
        "drain must wait until its bound"
    );
    assert_eq!(protocol::runs_active(&e), 1);
    assert_eq!(e.proj().tasks[&t].view.state, TaskState::Running);
    clock.clock.advance_millis(1);
    support::wait::SLOW
        .until("the bounded handover", || e.update.target().is_none())
        .await;
    let next = next.lock().unwrap().take().expect("target startup reached");
    assert_eq!(
        std::fs::read_link(l.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
    assert!(support::events::all(&next).iter().any(|ev| {
        matches!(&ev.kind, EventKind::EngineUpdated { to: updated, .. } if updated == &to)
    }));
    dvb_engine::reconcile::mark(&next).unwrap();
    assert!(matches!(
        next.proj().tasks[&t].view.state,
        TaskState::Unknown { .. }
    ));
    dvb_engine::reconcile::reconcile(next.clone())
        .await
        .unwrap();
    support::wait::SLOW
        .until("the update's task to reconcile into the pause", || {
            next.proj().tasks[&t].view.state == stopped
        })
        .await;
    assert_eq!(next.proj().live_runs().count(), 0);
    assert!(next.proj().tasks[&t].update_stop.is_some());
    assert_eq!(protocol::completion(&next).unwrap().pending, 1);
    next.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    dvb_engine::dispatch::step_now(&next, &t).await.unwrap();
    next.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    assert_eq!(next.proj().tasks[&t].view.state, TaskState::Preparing);
    assert_ne!(next.proj().tasks[&t].facts.run.as_ref(), Some(&r));
    assert!(next.proj().tasks[&t].update_stop.is_none());
    assert_eq!(
        support::events::all(&next)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::TaskResumed { .. }))
            .count(),
        1
    );
    assert_eq!(next.proj().tasks[&t].branch.as_deref(), Some("dvb/t-1"));
    assert_eq!(
        next.proj().runs[&r].assignment.base.as_deref(),
        Some("base")
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("work-in-progress")).unwrap(),
        "keep this work"
    );
}

/// The same recovery runs after a successful handover and a rollback.
#[tokio::test]
async fn update_restarts_only_its_own_stops_once_even_in_a_paused_project() {
    use dvb_core::{Command as TaskCommand, ProjectId, RunId, TaskId, TaskState};
    use dvb_engine::update::handover::{Next, prepare, recover};
    use dvb_engine::{Decision, dispatch};
    use std::sync::Arc;

    for rollback in [false, true] {
        for (stop, later_stop) in [
            (None, None),
            (Some(Actor::Owner), None),
            (Some(Actor::Engine), None),
            (None, Some(Actor::Owner)),
            (None, Some(Actor::Engine)),
        ] {
            let resumes = stop.is_none() && later_stop.is_none();
            let dir = tempfile::tempdir().unwrap();
            let cfg = Config::dev(dir.path(), "dvb".into());
            let e = Engine::open(cfg.clone(), Default::default()).unwrap();
            e.manual_dispatch();
            let (p, t, r) = (ProjectId::new("p"), TaskId::new("t"), RunId::new("before"));
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                let mut events = support::events::approved_task_events();
                let (roles, hires) = support::events::default_role_hire_events();
                events.extend(roles);
                events.extend(hires);
                Ok(events)
            })
            .unwrap();
            e.decide(
                e.proj(),
                &t,
                Actor::Engine,
                Decision::new(
                    "start",
                    TaskCommand::Start(
                        support::assignment::Assignment::new(r.clone())
                            .task(Some(t.clone()))
                            .base(Some("base".into()))
                            .build(),
                    ),
                ),
                |_| Ok(()),
            )
            .unwrap();
            if let Some(actor) = stop.clone() {
                // Even the old update wording is not an update marker.
                e.decide(
                    e.proj(),
                    &t,
                    actor,
                    Decision::new(
                        "stop",
                        TaskCommand::Stop(Some(
                            "stopped for the update to dvb 99.0.0; resume it to carry on".into(),
                        )),
                    ),
                    |_| Ok(()),
                )
                .unwrap();
                e.end_run(&r, dvb_core::event::EndReason::Stopped, None)
                    .unwrap();
            }
            dispatch::stop_for_update(&e, "99.0.0").await;
            if later_stop == Some(Actor::Engine) {
                dispatch::resume(&e, &t, Actor::Owner).await.unwrap();
                dispatch::stop(&e, &t, Actor::Engine).await.unwrap();
            }
            e.append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::ProjectPaused {}])
            })
            .unwrap();
            if later_stop == Some(Actor::Owner) {
                let before = support::events::all(&e).len();
                let stopped = e.proj().tasks[&t].view.state.clone();
                dispatch::stop(&e, &t, Actor::Owner).await.unwrap();
                assert_eq!(e.proj().tasks[&t].view.state, stopped);
                let events = support::events::all(&e);
                assert_eq!(events.len(), before + 1);
                assert_eq!(events[before].actor, Actor::Owner);
                assert!(matches!(
                    &events[before].kind,
                    EventKind::TaskStopped { task, update: false, .. } if task == &t
                ));
                assert!(e.proj().tasks[&t].update_stop.is_none());
                assert!(matches!(
                    dispatch::stop(&e, &t, Actor::Owner).await,
                    Err(dvb_engine::Fail::Refused(r)) if r.code == dvb_core::RefusalCode::AlreadyStopped
                ));
            }
            let l = Layout::of(&cfg);
            std::fs::create_dir_all(l.versions()).unwrap();
            prepare(&l, &e.store, dvb_core::VERSION, "99.0.0").unwrap();
            let migrate = |db: &Path| {
                if rollback {
                    Err(dvb_engine::update::handover::HandoverError::Message(
                        "test rollback".into(),
                    ))
                } else {
                    dvb_store::Store::open(db).map_err(Into::into)
                }
            };
            let next = recover(&l, "99.0.0", true, &migrate).unwrap();
            assert_eq!(
                next,
                if rollback {
                    Next::Exec(l.version_bin(dvb_core::VERSION).unwrap())
                } else {
                    Next::Run
                }
            );
            drop(e);
            let next = Engine::open(
                cfg.clone(),
                dvb_engine::EngineOptions {
                    launcher: Some(Arc::new(support::launcher::FakeLauncher {
                        forbid_start: Some("paused project must not create a run"),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .unwrap();
            next.manual_dispatch();
            let receipt = protocol::completion(&next).unwrap();
            assert_eq!(
                (receipt.stopped, receipt.resumed, receipt.pending),
                (u32::from(stop.is_none()), 0, u32::from(resumes))
            );
            dispatch::step_now(&next, &t).await.unwrap();
            assert!(matches!(
                next.proj().tasks[&t].view.state,
                TaskState::Stopped { .. }
            ));
            assert_eq!(next.proj().live_runs().count(), 0);
            assert_eq!(next.proj().tasks[&t].update_stop.is_some(), resumes);
            assert_eq!(
                protocol::completion(&next).unwrap().pending,
                u32::from(resumes)
            );
            // A restart during the pause preserves the marker without creating an attempt.
            drop(next);
            let next = Engine::open(cfg, Default::default()).unwrap();
            next.manual_dispatch();
            dvb_engine::reconcile::mark(&next).unwrap();
            dvb_engine::reconcile::reconcile(next.clone())
                .await
                .unwrap();
            support::wait::SLOW
                .until("stopped recovery", || {
                    matches!(next.proj().tasks[&t].view.state, TaskState::Stopped { .. })
                })
                .await;
            dispatch::step_now(&next, &t).await.unwrap();
            assert_eq!(next.proj().live_runs().count(), 0);
            assert_eq!(next.proj().tasks[&t].update_stop.is_some(), resumes);
            // Another update must leave the pending marker intact as well.
            dispatch::stop_for_update(&next, "99.0.1").await;
            assert_eq!(next.proj().tasks[&t].update_stop.is_some(), resumes);
            next.append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::ProjectResumed {}])
            })
            .unwrap();
            dispatch::step_now(&next, &t).await.unwrap();
            next.append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::ProjectPaused {}])
            })
            .unwrap();
            if resumes {
                assert_eq!(next.proj().tasks[&t].view.state, TaskState::Preparing);
                assert_ne!(next.proj().tasks[&t].facts.run.as_ref(), Some(&r));
            } else {
                assert!(matches!(
                    next.proj().tasks[&t].view.state,
                    TaskState::Stopped { .. }
                ));
            }
            // Replaying and kicking again never creates a second resume.
            dispatch::step_now(&next, &t).await.unwrap();
            let resumed = support::events::all(&next)
                .iter()
                .filter(|ev| {
                    ev.actor == Actor::Engine && matches!(ev.kind, EventKind::TaskResumed { .. })
                })
                .count();
            assert_eq!(resumed, usize::from(resumes));
            let replay = dvb_engine::proj::Projections::rebuild(&support::events::all(&next));
            assert!(replay.tasks[&t].update_stop.is_none());
            let receipt = protocol::completion(&next).unwrap();
            assert_eq!(
                (receipt.stopped, receipt.resumed, receipt.pending),
                (u32::from(stop.is_none()), u32::from(resumes), 0)
            );
        }
    }
}

#[tokio::test]
async fn an_update_without_running_work_resumes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), "dvb".into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    e.manual_dispatch();
    dvb_engine::dispatch::stop_for_update(&e, "99.0.0").await;
    let l = Layout::of(&cfg);
    std::fs::create_dir_all(l.versions()).unwrap();
    dvb_engine::update::handover::prepare(&l, &e.store, dvb_core::VERSION, "99.0.0").unwrap();
    assert_eq!(
        dvb_engine::update::handover::recover(&l, "99.0.0", true, &|db| {
            dvb_store::Store::open(db).map_err(Into::into)
        })
        .unwrap(),
        dvb_engine::update::handover::Next::Run
    );
    drop(e);
    let e = Engine::open(cfg, Default::default()).unwrap();
    let receipt = protocol::completion(&e).unwrap();
    assert_eq!(
        (receipt.stopped, receipt.resumed, receipt.pending),
        (0, 0, 0)
    );
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TaskResumed { .. }))
    );
}

fn source(dir: &Path, key: &str) -> Source {
    Source {
        releases_url: None,
        token: None,
        origin: mirror(dir),
        key: key.to_owned(),
    }
}

/// The version after this build's, so the update is a step forward.
fn next_version(version: &str) -> String {
    let mut v: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
    v[1] += 1;
    format!("{}.{}.{}", v[0], v[1], v[2])
}

fn up(to: &str) -> StartUpdate {
    StartUpdate {
        to: Some(to.to_owned()),
    }
}

#[tokio::test]
async fn an_update_drains_quiesces_snapshots_and_execs_the_target() {
    let (dir, e) = engine();
    let l = Layout::of(&e.cfg);
    let k = keys();
    let to = next_version(dvb_core::VERSION);
    release(
        &dir.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&k),
    );
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    // Inside exec: the engine is quiesced, the snapshot exists and update.json names it.
    let probe = {
        let l = l.clone();
        let e = e.clone();
        let to = to.clone();
        move |p: &Path| {
            assert!(e.update.quiesced());
            let rec: dvb_engine::update::handover::UpdateRecord =
                serde_json::from_slice(&std::fs::read(l.update_json()).unwrap()).unwrap();
            assert!(l.backups().join(&rec.snapshot).exists());
            assert_eq!(
                (rec.from.as_str(), rec.to.as_str()),
                (dvb_core::VERSION, to.as_str())
            );
            assert!(matches!(
                e.append_checked(None, Actor::Engine, |_, _| Ok(started())),
                Err(Fail::Refused(_))
            ));
            SEEN.lock().unwrap().push(p.display().to_string());
            std::io::Error::other("test: no exec")
        }
    };
    let probe: protocol::Exec = std::sync::Arc::new(probe);
    let info = protocol::start_with(e.clone(), up(&to), source(dir.path(), &k.pk), probe)
        .await
        .unwrap();
    assert_eq!(info.updating.unwrap().to, to);
    // The background task runs to the failed exec and undoes the update.
    for _ in 0..200 {
        if e.update.target().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(e.update.target().is_none());
    assert_eq!(SEEN.lock().unwrap().len(), 1);
    assert!(SEEN.lock().unwrap()[0].ends_with(&format!("versions/{to}/dvb")));
    assert!(!e.update.quiesced());
    assert!(!l.update_json().exists());
    // The failed handover was recorded and the engine writes again.
    let log = support::events::all(&e);
    assert!(log.iter().any(|ev| matches!(&ev.kind, EventKind::EngineUpdateFailed { reason, .. } if reason.contains("no exec"))));
    e.append_checked(None, Actor::Engine, |_, _| Ok(started()))
        .unwrap();
}

#[tokio::test]
async fn an_update_to_a_release_signed_by_another_key_is_refused_up_front() {
    let (dir, e) = engine();
    let (pinned, other) = (keys(), keys());
    let to = next_version(dvb_core::VERSION);
    release(
        &dir.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&other),
    );
    let before = support::events::all(&e).len();
    let never =
        || -> protocol::Exec { std::sync::Arc::new(|_: &Path| panic!("exec must not run")) };
    let err = protocol::start_with(e.clone(), up(&to), source(dir.path(), &pinned.pk), never())
        .await
        .unwrap_err();
    let Fail::Refused(r) = err else {
        panic!("{err:?}")
    };
    assert_eq!(r.code, RefusalCode::UpdateRefused);
    assert!(
        r.detail.contains("not signed with dvb's release key"),
        "{}",
        r.detail
    );
    assert!(e.update.target().is_none());
    assert!(!e.update.quiesced());
    assert_eq!(support::events::all(&e).len(), before);
    // The engine still takes writes, and a new run would start (admission is open).
    e.update.admit().await;
}

#[tokio::test]
async fn the_running_version_and_a_second_update_are_refused() {
    let (dir, e) = engine();
    let never =
        || -> protocol::Exec { std::sync::Arc::new(|_: &Path| panic!("exec must not run")) };
    let same = protocol::start_with(
        e.clone(),
        up(dvb_core::VERSION),
        source(dir.path(), dvb_engine::update::release_public_key()),
        never(),
    )
    .await;
    assert!(matches!(same, Err(Fail::Refused(r)) if r.code == RefusalCode::UpdateRefused));
    assert!(e.update.begin("9.9.9"));
    let second = protocol::start_with(
        e.clone(),
        up("9.9.8"),
        source(dir.path(), dvb_engine::update::release_public_key()),
        never(),
    )
    .await;
    assert!(matches!(second, Err(Fail::Refused(r)) if r.detail.contains("already under way")));
}

#[tokio::test]
async fn a_cockpit_only_box_updates_its_own_opt() {
    use dvb_engine::update::local;
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let k = keys();
    let to = next_version(dvb_core::VERSION);
    release(
        &dir.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&k),
    );
    std::fs::create_dir_all(l.versions().join(dvb_core::VERSION)).unwrap();
    std::fs::create_dir_all(l.versions().join("0.0.1")).unwrap();
    dvb_engine::update::handover::set_current(&l, dvb_core::VERSION).unwrap();
    let got = local::install(&l, &source(dir.path(), &k.pk), Some(to.clone()))
        .await
        .unwrap();
    assert_eq!(got.as_deref(), Some(to.as_str()));
    assert_eq!(
        std::fs::read_link(l.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
    assert!(
        l.versions().join(dvb_core::VERSION).exists(),
        "the previous version stays"
    );
    assert!(!l.versions().join("0.0.1").exists());
    // Another key's release changes nothing.
    let other = keys();
    let third = next_version(&to);
    release(
        &dir.path().join("rel"),
        &third,
        &third,
        dvb_store::schema_version(),
        Some(&other),
    );
    let err = local::install(&l, &source(dir.path(), &k.pk), Some(third))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("not signed with dvb's release key"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_link(l.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
}

#[tokio::test]
async fn get_update_without_check_never_needs_the_network() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let (_dir, e) = engine();
    let get = |uri: &'static str| {
        let router = dvb_engine::api::router(e.clone());
        async move {
            let res = router
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = res.status();
            let body = axum::body::to_bytes(res.into_body(), 1 << 16)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        }
    };
    // A dev-mode engine has no release list: the plain answer still comes.
    let (status, body) = get("/update").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"], dvb_core::VERSION);
    assert!(body["updating"].is_null());
    // Asking for a check says why it cannot.
    let (status, body) = get("/update?check=true").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "UPDATE_REFUSED");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("no release source")
    );
    // While an update is under way the answer is its progress.
    assert!(e.update.begin("9.9.9"));
    let (_, body) = get("/update?check=true").await;
    assert_eq!(body["updating"]["to"], "9.9.9");
}

#[tokio::test]
async fn an_unpacked_release_is_readable_and_runnable_but_not_writable_by_others() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    let rel = dir.path().join("rel");
    release(&rel, "0.2.0", "0.2.0", 1, Some(&k));
    // A release packed by someone with a tight umask: owner-only everywhere. (The
    // service's umask 0027 does this to a plain extraction.)
    let top = rel.join("pack-0.2.0/dvb-0.2.0");
    std::fs::set_permissions(&top, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(top.join("dvb"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(top.join("schema"), std::fs::Permissions::from_mode(0o600)).unwrap();
    pack_release(&rel, "0.2.0", Some(&k));
    fetch_ok(dir.path(), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap();
    let l = layout(dir.path());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&l.versions()), 0o755);
    assert_eq!(mode(&l.versions().join("0.2.0")), 0o755);
    assert_eq!(mode(&l.version_bin("0.2.0").unwrap()), 0o755);
    assert_eq!(mode(&l.versions().join("0.2.0/schema")), 0o644);
}

/// A private GitHub in miniature: the release API and the asset API on a loopback
/// port; every request needs `Authorization: Bearer <token>` or gets 404, and an
/// asset needs `Accept: application/octet-stream`. Returns the port and the
/// requests' authorization headers it saw.
fn fake_github(
    root: std::path::PathBuf,
    token: &'static str,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split(' ').nth(1).unwrap_or("").to_owned();
            let (mut auth, mut accept) = (String::new(), String::new());
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                let lower = h.to_ascii_lowercase();
                if lower.starts_with("authorization:") {
                    h[14..].trim().clone_into(&mut auth);
                } else if lower.starts_with("accept:") {
                    h[7..].trim().clone_into(&mut accept);
                }
            }
            log.lock().unwrap().push(auth.clone());
            let mut w = stream;
            let respond = |w: &mut std::net::TcpStream, code: &str, body: &[u8]| {
                write!(
                    w,
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                w.write_all(body).unwrap();
            };
            if auth != format!("Bearer {token}") {
                respond(&mut w, "404 Not Found", b"{}");
                continue;
            }
            if let Some(tag) = path.strip_prefix("/repos/broken-branch/dvb/releases/tags/") {
                let dir = root.join(tag);
                let mut assets = Vec::new();
                for e in std::fs::read_dir(&dir).unwrap().flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.path().is_file() {
                        assets.push(serde_json::json!({"url": format!("http://127.0.0.1:{port}/assets/{tag}/{name}"), "name": name}));
                    }
                }
                respond(
                    &mut w,
                    "200 OK",
                    serde_json::json!({"tag_name": tag, "assets": assets})
                        .to_string()
                        .as_bytes(),
                );
            } else if let Some(rest) = path.strip_prefix("/assets/") {
                let file = root.join(rest);
                if accept != "application/octet-stream" {
                    respond(&mut w, "415 Unsupported Media Type", b"{}");
                } else {
                    let mut bytes = Vec::new();
                    std::fs::File::open(file)
                        .unwrap()
                        .read_to_end(&mut bytes)
                        .unwrap();
                    respond(&mut w, "200 OK", &bytes);
                }
            } else {
                respond(&mut w, "404 Not Found", b"{}");
            }
        }
    });
    (port, seen)
}

#[tokio::test]
async fn socket_a_private_release_is_fetched_through_the_asset_api_with_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let k = keys();
    release(&dir.path().join("rel"), "0.2.0", "0.2.0", 1, Some(&k));
    let (port, seen) = fake_github(dir.path().join("rel"), "s3cret");
    let l = layout(dir.path());
    std::fs::create_dir_all(&l.opt).unwrap();
    let api = |token: &str| Origin::Api {
        api: format!("http://127.0.0.1:{port}"),
        token: token.to_owned(),
    };
    // The browser URL of a private repository, and a wrong token: refused.
    let err = fetch(&l, &api("wrong"), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot read"), "{err}");
    assert!(!l.versions().exists());
    fetch(&l, &api("s3cret"), "0.2.0", &k.pk, 1, false)
        .await
        .unwrap();
    assert!(l.version_bin("0.2.0").unwrap().exists());
    // The release record and every asset went with the token.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().filter(|a| *a == "Bearer s3cret").count() >= 4,
        "{seen:?}"
    );
    drop(seen);
}

/// A clock whose next `now()` can be held, to freeze a writer inside `append`.
struct GateClock {
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl dvb_core::Clock for GateClock {
    fn now(&self) -> dvb_core::Timestamp {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.lock().unwrap().send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        dvb_core::Timestamp(1)
    }
}

fn events_in(db: &Path, like: &str) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE payload LIKE ?1",
            [format!("%{like}%")],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn quiesce_waits_for_a_writer_that_is_already_inside_append() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let clock = std::sync::Arc::new(GateClock {
        armed: false.into(),
        entered: entered_tx.into(),
        release: release_rx.into(),
    });
    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            clock: Some(clock.clone()),
            ..Default::default()
        },
    );
    clock.armed.store(true, Ordering::SeqCst);
    let writer = {
        let e = e.clone();
        std::thread::spawn(move || {
            e.append_checked(None, Actor::Engine, |_, _| {
                Ok(vec![EventKind::EngineStarted {
                    version: "in-flight".into(),
                }])
            })
        })
    };
    entered_rx.recv().unwrap(); // the writer is past the quiesce check, frozen before its commit
    let quiesced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let q = {
        let (e, quiesced) = (e.clone(), quiesced.clone());
        std::thread::spawn(move || {
            e.quiesce();
            quiesced.store(true, Ordering::SeqCst);
        })
    };
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !quiesced.load(Ordering::SeqCst),
        "quiesce returned while a writer was mid-append"
    );
    release_tx.send(()).unwrap();
    writer
        .join()
        .unwrap()
        .expect("the writer that began before the quiesce commits");
    q.join().unwrap();
    // The snapshot taken now has the writer's event; a later writer is refused.
    let l = Layout::of(&e.cfg);
    let rec = dvb_engine::update::handover::prepare(&l, &e.store, "0.1.0", "0.2.0").unwrap();
    assert_eq!(events_in(&l.backups().join(rec.snapshot), "in-flight"), 1);
    assert!(matches!(
        e.append_checked(None, Actor::Engine, |_, _| Ok(started())),
        Err(Fail::Refused(_))
    ));
}

#[tokio::test]
async fn an_update_waits_for_an_effect_and_holds_back_the_next_until_it_is_undone() {
    let (dir, e) = engine();
    let l = Layout::of(&e.cfg);
    let k = keys();
    let to = next_version(dvb_core::VERSION);
    release(
        &dir.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&k),
    );
    // A delayed merge: inside its section, then its evidence is appended.
    let inside = std::sync::Arc::new(tokio::sync::Notify::new());
    let merge = {
        let (e, inside) = (e.clone(), inside.clone());
        tokio::spawn(async move {
            let guard = e.update.effect().await;
            inside.notify_one();
            tokio::time::sleep(Duration::from_millis(600)).await;
            e.append_checked(None, Actor::Engine, |_, _| {
                Ok(vec![EventKind::EngineStarted {
                    version: "merge-evidence".into(),
                }])
            })
            .unwrap();
            drop(guard);
        })
    };
    inside.notified().await;
    // At exec time: the merge and its evidence are settled and in the snapshot, and
    // the next effect has not run.
    let next_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe: protocol::Exec = {
        let (l, e, next_ran) = (l.clone(), e.clone(), next_ran.clone());
        std::sync::Arc::new(move |_: &Path| {
            assert_eq!(e.update.effects_inflight(), 0);
            let rec: dvb_engine::update::handover::UpdateRecord =
                serde_json::from_slice(&std::fs::read(l.update_json()).unwrap()).unwrap();
            assert_eq!(
                events_in(&l.backups().join(&rec.snapshot), "merge-evidence"),
                1
            );
            assert!(
                !next_ran.load(Ordering::SeqCst),
                "a queued effect ran during the update"
            );
            std::io::Error::other("test: no exec")
        })
    };
    protocol::start_with(e.clone(), up(&to), source(dir.path(), &k.pk), probe)
        .await
        .unwrap();
    // Queued behind the update.
    let next = {
        let (e, next_ran) = (e.clone(), next_ran.clone());
        tokio::spawn(async move {
            let _g = e.update.effect().await;
            next_ran.store(true, Ordering::SeqCst);
        })
    };
    merge.await.unwrap();
    // The exec failed: the update is undone and the queued effect goes on.
    tokio::time::timeout(Duration::from_secs(10), next)
        .await
        .expect("the queued effect resumes after the rollback")
        .unwrap();
    assert!(next_ran.load(Ordering::SeqCst));
    assert!(e.update.target().is_none());
    assert!(!e.update.quiesced());
    assert_eq!(e.update.effects_inflight(), 0);
}

#[tokio::test]
async fn a_cockpit_follows_an_engine_that_updated_on_another_box() {
    use dvb_engine::update::handover::{Next, recover};
    use dvb_engine::update::local;
    let root = tempfile::tempdir().unwrap();
    let (engine_box, cockpit_box) = (
        root.path().join("engine-box"),
        root.path().join("cockpit-box"),
    );
    let (ea, cb) = (layout(&engine_box), layout(&cockpit_box));
    for l in [&ea, &cb] {
        std::fs::create_dir_all(&l.state).unwrap();
        std::fs::create_dir_all(l.versions().join(dvb_core::VERSION)).unwrap();
        dvb_engine::update::handover::set_current(l, dvb_core::VERSION).unwrap();
    }
    // What install.sh leaves: the engine box is an engine and a cockpit, the other only a cockpit.
    std::fs::write(ea.opt.join("role-engine"), "").unwrap();
    std::fs::write(ea.opt.join("role-cockpit"), "").unwrap();
    std::fs::write(cb.opt.join("role-cockpit"), "").unwrap();
    assert!(!ea.cockpit_only() && cb.cockpit_only());
    // The release is published once; both boxes read it.
    let k = keys();
    let to = next_version(dvb_core::VERSION);
    release(
        &root.path().join("rel"),
        &to,
        &to,
        dvb_store::schema_version(),
        Some(&k),
    );
    let origin = Origin::Base(format!("file://{}", root.path().join("rel").display()));
    let src = Source {
        releases_url: None,
        token: None,
        origin,
        key: k.pk.clone(),
    };
    // The engine box updates: its handover is finished by the target at start.
    let store = dvb_store::Store::open(&ea.db()).unwrap();
    fetch(
        &ea,
        &src.origin,
        &to,
        &src.key,
        dvb_store::schema_version(),
        false,
    )
    .await
    .unwrap();
    dvb_engine::update::handover::prepare(&ea, &store, dvb_core::VERSION, &to).unwrap();
    drop(store);
    let next = recover(&ea, &to, true, &|db| {
        dvb_store::Store::open(db).map_err(dvb_engine::update::handover::HandoverError::from)
    })
    .unwrap();
    assert_eq!(next, Next::Run);
    assert_eq!(
        std::fs::read_link(ea.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
    // The cockpit box still has the old desk. It aligns to the engine's version, even
    // though it did not start the update, with its own verified copy.
    assert_eq!(
        std::fs::read_link(cb.current()).unwrap(),
        Path::new(&format!("versions/{}", dvb_core::VERSION))
    );
    let got = local::install(&cb, &src, Some(to.clone())).await.unwrap();
    assert_eq!(got.as_deref(), Some(to.as_str()));
    assert_eq!(
        std::fs::read_link(cb.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
    assert!(cb.version_bin(&to).unwrap().exists());
    // A version the cockpit cannot verify is refused and nothing moves.
    let other = keys();
    let to2 = next_version(&to);
    release(
        &root.path().join("rel"),
        &to2,
        &to2,
        dvb_store::schema_version(),
        Some(&other),
    );
    assert!(local::install(&cb, &src, Some(to2)).await.is_err());
    assert_eq!(
        std::fs::read_link(cb.current()).unwrap(),
        Path::new(&format!("versions/{to}"))
    );
}
