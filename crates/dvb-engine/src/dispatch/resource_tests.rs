use super::*;
use crate::proj::Projections;
use crate::{Config, Engine};
use dvb_core::event::OutputItem;
use dvb_core::{HireId, RoleId};

#[tokio::test(start_paused = true)]
async fn resource_polls_do_not_read_retained_output_and_notices_survive_reentry_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let readings = crate::test_support::engine::Readings::default();
    let e = Engine::open(
        Config::dev(dir.path(), "unused-dvb".into()),
        readings.options(crate::EngineOptions {
            forge: None,
            launcher: None,
            ..Default::default()
        }),
    )
    .unwrap();
    let project = ProjectId::new("p");
    let other = ProjectId::new("other");
    let retained = e
        .append_checked(Some(&other), Actor::Run(RunId::new("old-run")), |_, _| {
            Ok((0..1_024)
                .map(|_| EventKind::RunOutput {
                    resource: None,
                    run: RunId::new("old-run"),
                    item: OutputItem::Text("x".repeat(4_096)),
                })
                .collect())
        })
        .unwrap();
    let retained_id = i64::try_from(retained[0].id.0).unwrap();
    drop(retained);
    // A deterministic read tripwire: after projection folding, make one retained
    // output undecodable. Any full-log read now fails, with no timing/allocation
    // assertion needed. Restore it before checking replay below.
    let conn = rusqlite::Connection::open(e.cfg.paths.state.join("dvb.sqlite")).unwrap();
    let payload: String = conn
        .query_row(
            "SELECT payload FROM events WHERE id = ?1",
            [retained_id],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE events SET payload = 'invalid json' WHERE id = ?1",
        [retained_id],
    )
    .unwrap();
    assert!(e.store.after(EventId(0)).is_err());

    for (id, task, reviews) in [
        ("builder", Some(TaskId::new("t")), None),
        ("reviewer", Some(TaskId::new("t")), Some("head".into())),
        ("orchestrator", None, None),
    ] {
        let a = crate::test_support::assignment::Assignment::new(RunId::new(id))
            .task(task)
            .role(RoleId::new(id))
            .hire(HireId::new("hire"))
            .base(Some("base".into()))
            .reviews(reviews)
            .build();
        // Worker-authored lookalikes must not suppress the engine's notices.
        e.append_checked(Some(&project), Actor::Run(a.run.clone()), |_, _| {
            Ok(vec![
                EventKind::RunOutput {
                    resource: None,
                    run: a.run.clone(),
                    item: OutputItem::Note("Waiting for memory: worker text".into()),
                },
                EventKind::RunOutput {
                    resource: None,
                    run: a.run.clone(),
                    item: OutputItem::Note("Waiting for host load: worker text".into()),
                },
            ])
        })
        .unwrap();
        let cursor = e.store.last_id().unwrap();
        let notices = || {
            e.store
                .after(cursor)
                .unwrap()
                .iter()
                .filter(|event| event.actor == Actor::Engine)
                .count()
        };
        for _ in 0..2 {
            *readings.disk.lock().unwrap() = Some(0);
            *readings.memory.lock().unwrap() = Some(0);
            *readings.load.lock().unwrap() = Some((4.01, 4));
            let waiter = tokio::spawn({
                let (e, project, a) = (e.clone(), project.clone(), a.clone());
                async move { wait_launch(&e, &project, &a).await }
            });
            tokio::task::yield_now().await;
            assert_eq!(notices(), 3);
            for _ in 0..3 {
                tokio::time::advance(RESOURCE_POLL).await;
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                assert_eq!(notices(), 3);
            }
            *readings.memory.lock().unwrap() = Some(MIN_AVAILABLE_MEMORY_BYTES);
            tokio::time::advance(RESOURCE_POLL).await;
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished()); // Disk still holds admission.
            *readings.disk.lock().unwrap() = Some(MIN_FREE_BYTES);
            tokio::time::advance(RESOURCE_POLL).await;
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished()); // Load alone still holds every run kind.
            *readings.load.lock().unwrap() = Some((3.99, 4));
            tokio::time::advance(RESOURCE_POLL).await;
            waiter.await.unwrap().unwrap();
            assert_eq!(notices(), 3);
        }
        // Taskless notices precede RunStarted; that event must not reset them.
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(a)])
        })
        .unwrap();
    }
    conn.execute(
        "UPDATE events SET payload = ?1 WHERE id = ?2",
        rusqlite::params![payload, retained_id],
    )
    .unwrap();
    *e.proj() = Projections::rebuild(&crate::test_support::events::all(&e));
    let before = e.store.last_id().unwrap();
    let runs: Vec<_> = e
        .proj()
        .runs
        .values()
        .map(|r| r.assignment.clone())
        .collect();
    for run in runs {
        for reason in [
            ResourceWait::Memory,
            ResourceWait::Disk,
            ResourceWait::HostLoad,
        ] {
            tell_waiting_for_resource(&e, &project, &run, reason, "still held").unwrap();
        }
    }
    assert_eq!(e.store.last_id().unwrap(), before);
}
