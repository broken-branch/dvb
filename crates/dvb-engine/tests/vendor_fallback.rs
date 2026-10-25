//! A limited vendor's run moves to the orchestrator's vendor when `vendor_fallback`
//! is on, and says so with `run.rerouted`; with it off, or when the orchestrator's
//! own vendor is the limited one, the run waits.
//! After reset, independent new work uses the unchanged original hire; a live
//! fallback run keeps its assignment.

mod support;
use dvb_core::launcher::{Launcher, UnitId, UnitState};
use dvb_core::roles::{HireRecord, HireStatus};
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{
    Actor, Clock, Effort, EventKind, FakeClock, GoalId, HireId, PoolModel, ProjectId, RoleId,
    RunAssignment, RunId, SystemClock, TaskId, TaskState, Timestamp, Vendor,
};
use dvb_engine::{AppState, dispatch, proj::Projections, publish, vendors};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use support::wait::FAST as wait;

use support::launcher::FakeLauncher as Stub;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Started {
    unit: UnitId,
    env: Option<String>,
}

/// Two vendors: the orchestrator is on `fake-1` (Fake), the backend builder is
/// hired on `codex-x` (Codex).
fn engine(dir: &Path, stub: &Arc<Stub>, fallback: bool) -> AppState {
    engine_with_clock(dir, stub, fallback, Arc::new(SystemClock))
}

fn engine_with_clock(
    dir: &Path,
    stub: &Arc<Stub>,
    fallback: bool,
    clock: Arc<dyn Clock>,
) -> AppState {
    let mut pool = support::engine::fake_pool();
    pool.models.push(PoolModel {
        id: "codex-x".into(),
        vendor: Vendor::Codex,
        efforts: vec![Effort::Low, Effort::High],
    });
    let e = support::engine::open_with(
        dir,
        "dvb",
        dvb_engine::EngineOptions {
            clock: Some(clock),
            launcher: Some(stub.clone()),
            pool: Some(pool),
            disk_reading: Some(Arc::new(|| Some(u64::MAX))),
            ..Default::default()
        },
    );
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds = support::events::approved_task_events();
    kinds[0] = support::events::project_event(
        &ProjectId::new("p"),
        "p",
        &dir.join("repo").to_string_lossy(),
    );
    kinds.splice(1..1, roles.into_iter().chain(hires));
    kinds.push(EventKind::HireAdded(HireRecord {
        id: HireId::new("h-codex"),
        role: RoleId::new("backend"),
        model: "codex-x".into(),
        effort: Effort::High,
        status: HireStatus::Active,
        routine: false,
    }));
    kinds.push(EventKind::HireRetired(HireRecord {
        id: HireId::new("h-backend"),
        role: RoleId::new("backend"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: HireStatus::Retired,
        routine: false,
    }));
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    if !fallback {
        e.append_checked(None, Actor::Owner, |_, _| {
            Ok(vec![EventKind::SettingsChanged {
                key: "vendor_fallback".into(),
                old: serde_json::json!(true),
                new: serde_json::json!(false),
                restored_from: None,
            }])
        })
        .unwrap();
    }
    e
}

fn builder() -> RunAssignment {
    support::assignment::Assignment::new(RunId::new("b1"))
        .task(Some(TaskId::new("t")))
        .hire(HireId::new("h-codex"))
        .model("codex-x".into())
        .vendor(Vendor::Codex)
        .effort(Effort::High)
        .base(Some("x".into()))
        .build()
}

fn limit(e: &AppState, vendor: Vendor) {
    limit_for(e, vendor, 60_000);
}

fn limit_for(e: &AppState, vendor: Vendor, ms: i64) {
    dvb_engine::vendors::record(
        e,
        vendor,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(e.clock.now().plus_millis(ms)),
        },
    )
    .unwrap();
}

fn state(e: &AppState) -> TaskState {
    e.proj().tasks[&TaskId::new("t")].view.state.clone()
}

fn reroutes(e: &AppState) -> Vec<EventKind> {
    support::events::all(e)
        .into_iter()
        .map(|ev| ev.kind)
        .filter(|k| matches!(k, EventKind::RunRerouted { .. }))
        .collect()
}

fn assert_launch(
    started: &Started,
    run: &RunId,
    task: &TaskId,
    vendor: Vendor,
    model: &str,
    effort: &str,
) {
    assert_eq!(
        started.unit,
        UnitId {
            vendor,
            run: run.clone()
        }
    );
    let env = started.env.as_ref().unwrap();
    for line in [
        format!("DVB_RUN_ID={run}"),
        format!("DVB_TASK_ID={task}"),
        format!("DVB_VENDOR={}", vendor.as_str()),
        format!("DVB_MODEL={model}"),
        format!("DVB_EFFORT={effort}"),
        "DVB_RESUME=0".into(),
    ] {
        assert!(env.lines().any(|actual| actual == line), "missing {line}");
    }
}

#[tokio::test(start_paused = true)]
async fn new_work_returns_to_the_original_hire_after_vendor_reset() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let work = dir.path().join("work");
    let stub = Arc::new(Stub {
        stop_changes_state: false,
        start_hook: Some(Arc::new({
            let recorded = recorded.clone();
            move |unit| {
                let env = Some(
                    std::fs::read_to_string(work.join(unit.run.as_str()).join("env")).unwrap(),
                );
                recorded.lock().unwrap().push(Started {
                    unit: unit.clone(),
                    env,
                });
                Ok(())
            }
        })),
        ..Stub::default()
    });
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let e = engine_with_clock(dir.path(), &stub, true, clock.clone());
    let repo = dir.path().join("repo");
    support::git::seed_repository(&repo, "main", None, "Seed");
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("p"))
        .await
        .unwrap();
    let project = ProjectId::new("p");
    let role = RoleId::new("backend");
    let hires = e.proj().projects[&project].hires.clone();
    let hire = e.proj().projects[&project]
        .hire(&role, None)
        .unwrap()
        .clone();
    assert_eq!(
        (hire.model.as_str(), hire.effort),
        ("codex-x", Effort::High)
    );
    assert_eq!(hire.id, HireId::new("h-codex"));

    let reset = clock.now().plus_millis(60_000);
    vendors::record(
        &e,
        Vendor::Codex,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(reset),
        },
    )
    .unwrap();
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert!(!e.vendors.is_limited(Vendor::Fake));
    // Arm the resume timer while the injected clock is still before reset.
    tokio::task::yield_now().await;

    let first_task = TaskId::new("t");
    // Deferred requests omit model and effort, exercising start_as_asked's
    // real active-hire selection rather than constructing run assignments.
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskWaiting {
            task: first_task.clone(),
            model: None,
            effort: None,
            reason: "First independent task".into(),
            routine: false,
        }])
    })
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        support::wait::io(dispatch::start_waiting(e.clone())),
    )
    .await
    .unwrap()
    .unwrap();
    wait.until("the first task to launch on the fallback vendor", || {
        stub.starts.load(Ordering::SeqCst) == 1 && state(&e) == TaskState::Running
    })
    .await;
    let first = e.proj().tasks[&first_task].facts.run.clone().unwrap();
    let launched_first = recorded.lock().unwrap()[0].clone();
    assert_launch(
        &launched_first,
        &first,
        &first_task,
        Vendor::Fake,
        "fake-1",
        "high",
    );
    let fallback = e.proj().runs[&first].assignment.clone();
    assert_eq!(
        (fallback.model.as_str(), fallback.vendor, fallback.effort),
        ("fake-1", Vendor::Fake, Effort::High)
    );
    assert_eq!(fallback.hire, hire.id);
    let reroute = EventKind::RunRerouted {
        run: first.clone(),
        task: Some(first_task.clone()),
        role: role.clone(),
        from_model: hire.model.clone(),
        to_model: "fake-1".into(),
        vendor: Vendor::Fake,
        effort: hire.effort,
        limited: Some(Vendor::Codex),
        resets_at: Some(reset),
    };
    assert_eq!(reroutes(&e), vec![reroute.clone()]);
    assert_eq!(e.proj().projects[&project].hires, hires);
    assert!(e.vendors.is_limited(Vendor::Codex));

    // Moving FakeClock is not completion: let the production Tokio timer
    // finish and observe its durable vendor.resumed event before new work.
    clock.set(reset);
    assert!(e.vendors.is_limited(Vendor::Codex));
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    wait.until("the vendor resume timer to record the reset", || {
        !e.vendors.is_limited(Vendor::Codex)
            && support::events::all(&e).iter().any(|ev| {
                matches!(
                    ev.kind,
                    EventKind::VendorResumed {
                        vendor: Vendor::Codex
                    }
                ) && ev.at == reset
            })
    })
    .await;
    assert!(
        !e.vendors
            .windows()
            .iter()
            .find(|w| w.vendor == Vendor::Codex)
            .unwrap()
            .limited
    );

    let second_task = TaskId::new("after-reset");
    let goal = GoalId::new("after-reset-goal");
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::goal_event(&goal, "Independent work after reset", None),
            support::events::backend_plan_event(&goal, &second_task),
            EventKind::GoalApproved { goal },
            EventKind::TaskWaiting {
                task: second_task.clone(),
                model: None,
                effort: None,
                reason: "Second independent task".into(),
                routine: false,
            },
        ])
    })
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        support::wait::io(dispatch::start_waiting(e.clone())),
    )
    .await
    .unwrap()
    .unwrap();
    wait.until("the second task to launch on its original hire", || {
        stub.starts.load(Ordering::SeqCst) == 2
            && e.proj().tasks[&second_task].view.state == TaskState::Running
    })
    .await;
    let second = e.proj().tasks[&second_task].facts.run.clone().unwrap();
    assert_ne!(second, first);
    let launches = recorded.lock().unwrap().clone();
    assert_eq!(
        launches.len(),
        2,
        "reset must not relaunch the fallback session"
    );
    assert_eq!(launches[0], launched_first);
    assert_eq!(stub.stops.load(Ordering::SeqCst), 0);
    assert_launch(
        &launches[1],
        &second,
        &second_task,
        Vendor::Codex,
        &hire.model,
        "high",
    );
    assert_eq!(
        stub.state(&launches[0].unit).await.unwrap(),
        UnitState::Active
    );

    // Replay the durable log too: run.started preserves the original request,
    // while the first run's effective assignment and reroute stay on fallback.
    let events = support::events::all(&e);
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(
                ev.kind,
                EventKind::VendorResumed {
                    vendor: Vendor::Codex
                }
            ))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(&ev.kind, EventKind::RunStarted(a)
        if a.run == first && a.hire == hire.id && a.model == hire.model
            && a.vendor == Vendor::Codex && a.effort == hire.effort))
    );
    assert_eq!(reroutes(&e), vec![reroute]);
    let replay = Projections::rebuild(&events);
    for proj in [&*e.proj(), &replay] {
        assert_eq!(proj.projects[&project].hires, hires);
        assert_eq!(proj.projects[&project].hire(&role, None), Some(&hire));
        assert_eq!(proj.runs[&first].assignment, fallback);
        assert!(proj.runs[&first].ended.is_none());
        assert_eq!(proj.tasks[&first_task].view.state, TaskState::Running);
        let a = &proj.runs[&second].assignment;
        assert_eq!(
            (&a.hire, a.model.as_str(), a.vendor, a.effort),
            (&hire.id, hire.model.as_str(), Vendor::Codex, hire.effort)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limited_vendors_run_moves_to_the_orchestrators_vendor() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub {
        stop_changes_state: false,
        ..Default::default()
    });
    let e = engine(dir.path(), &stub, true);
    limit(&e, Vendor::Codex);
    dispatch::start_task(&e, builder(), Actor::Engine)
        .await
        .unwrap();
    wait.until("the run to start on the other vendor", || {
        stub.starts.load(Ordering::SeqCst) == 1 && state(&e) == TaskState::Running
    })
    .await;
    assert!(e.vendors.is_limited(Vendor::Codex));
    let events = reroutes(&e);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        EventKind::RunRerouted { from_model, to_model, vendor: Vendor::Fake, limited: Some(Vendor::Codex), resets_at: Some(_), .. }
            if from_model == "codex-x" && to_model == "fake-1"
    ));
    let proj = e.proj();
    let a = &proj.runs[&RunId::new("b1")].assignment;
    assert_eq!((a.model.as_str(), a.vendor), ("fake-1", Vendor::Fake));
    drop(proj);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_switch_off_the_run_waits_for_its_own_vendor() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub {
        stop_changes_state: false,
        ..Default::default()
    });
    let e = engine(dir.path(), &stub, false);
    limit(&e, Vendor::Codex);
    dispatch::start_task(&e, builder(), Actor::Engine)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    assert!(reroutes(&e).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn when_only_the_orchestrators_vendor_is_limited_everything_waits() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub {
        stop_changes_state: false,
        ..Default::default()
    });
    let e = engine(dir.path(), &stub, true);
    limit_for(&e, Vendor::Fake, 1_000);
    dispatch::start_task(&e, builder(), Actor::Engine)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // The builder's own vendor is open, but the orchestrator's is out.
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    wait.until("the run to start once it is back", || {
        stub.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    // The move back is automatic: it started as hired, with no reroute.
    assert!(reroutes(&e).is_empty());
    assert_eq!(
        e.proj().runs[&RunId::new("b1")].assignment.vendor,
        Vendor::Codex
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn both_limited_then_the_fallback_vendor_resumes_first() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Arc::new(Stub {
        stop_changes_state: false,
        ..Default::default()
    });
    let e = engine(dir.path(), &stub, true);
    limit_for(&e, Vendor::Codex, 60_000);
    limit_for(&e, Vendor::Fake, 1_000);
    dispatch::start_task(&e, builder(), Actor::Engine)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(stub.starts.load(Ordering::SeqCst), 0);
    wait.until("the run to move when the fallback vendor reopens", || {
        stub.starts.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(e.vendors.is_limited(Vendor::Codex));
    assert_eq!(reroutes(&e).len(), 1);
    assert_eq!(
        e.proj().runs[&RunId::new("b1")].assignment.vendor,
        Vendor::Fake
    );
}
