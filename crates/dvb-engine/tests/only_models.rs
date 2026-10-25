//! Timed model selection uses FakeClock, the real dispatcher/orchestrator paths,
//! an observed launcher and Fake sessions; it never calls a model.
mod support;

use axum::http::{Method, StatusCode};
use dvb_core::api::{SetOnly, Settings};
use dvb_core::event::{EndReason, OutputItem};
use dvb_core::launcher::UnitId;
use dvb_core::model::OnlyModels;
use dvb_core::provider::Provider;
use dvb_core::roles::{HireRecord, HireStatus};
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{
    Actor, Command, Effort, EventKind, FakeClock, GoalId, HireId, PoolModel, ProjectId, RoleId,
    RunAssignment, TaskId, Timestamp, Vendor,
};
use dvb_engine::{AppState, dispatch, proj::Projections, publish, vendors, wake};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{http::call, wait::FAST as wait};

struct Observed {
    launcher: Arc<support::launcher::FakeLauncher>,
    starts: Arc<Mutex<Vec<(UnitId, String)>>>,
}

impl Observed {
    fn new(work: PathBuf) -> Self {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let launcher = Arc::new(support::launcher::FakeLauncher {
            stop_changes_state: false,
            start_hook: Some(Arc::new({
                let starts = starts.clone();
                move |unit| {
                    let dir = work.join(unit.run.as_str());
                    let env = std::fs::read_to_string(dir.join("env")).unwrap();
                    let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap();
                    // Exercise the Fake provider with the delivered prompt even for tests
                    // whose model catalogue simulates other vendors.
                    let script = dir.join("test-fake.json");
                    std::fs::write(&script, r#"[{"exit":0}]"#).unwrap();
                    let model = env
                        .lines()
                        .find_map(|line| line.strip_prefix("DVB_MODEL="))
                        .unwrap();
                    let fake = dvb_run::fake::Fake {
                        script,
                        dvb: "dvb".into(),
                        stderr: dvb_run::StderrTail::default(),
                    };
                    let mut session = fake
                        .start(&dir, &dir, &prompt, model, Effort::Medium)
                        .unwrap();
                    assert_eq!(
                        session.next(),
                        Some(dvb_core::provider::RunItem::ModelServed(
                            dvb_core::event::ServedModel::Confirmed(model.into())
                        ))
                    );
                    starts.lock().unwrap().push((unit.clone(), env));
                    Ok(())
                }
            })),
            ..Default::default()
        });
        Self { launcher, starts }
    }

    async fn wait_for_count(&self, count: usize) {
        support::wait::io(async {
            loop {
                let started = self.launcher.started.notified();
                if self.starts.lock().unwrap().len() >= count {
                    return;
                }
                started.await;
            }
        })
        .await;
    }
}

fn empty(dir: &Path, clock: Arc<FakeClock>, options: dvb_engine::EngineOptions) -> AppState {
    let e = support::engine::open_with(
        dir,
        "dvb",
        dvb_engine::EngineOptions {
            clock: Some(clock),
            disk_reading: Some(Arc::new(|| Some(u64::MAX))),
            ..options
        },
    );
    // Model routing needs healthy resources on every construction and reopen.
    assert_eq!(e.available_memory_bytes(), Some(u64::MAX));
    e
}

async fn fixture(dir: &Path, clock: Arc<FakeClock>) -> (AppState, Arc<Observed>) {
    let observed = Arc::new(Observed::new(dir.join("work")));
    let mut pool = support::engine::fake_pool();
    pool.models.extend([
        PoolModel {
            id: "codex-test".into(),
            vendor: Vendor::Codex,
            efforts: vec![Effort::Medium, Effort::High],
        },
        PoolModel {
            id: "claude-test".into(),
            vendor: Vendor::Claude,
            efforts: vec![Effort::Low, Effort::High],
        },
    ]);
    let e = empty(
        dir,
        clock,
        dvb_engine::EngineOptions {
            launcher: Some(observed.launcher.clone()),
            pool: Some(pool),
            ..Default::default()
        },
    );
    let repo = dir.join("repo");
    support::git::seed_repository(&repo, "main", None, "Seed");
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("p"))
        .await
        .unwrap();
    let (roles, hires) = support::events::default_role_hire_events();
    let mut seed = vec![support::events::project_event(
        &ProjectId::new("p"),
        "p",
        &repo.to_string_lossy(),
    )];
    seed.extend(roles.into_iter().chain(hires));
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(seed))
        .unwrap();
    (e, observed)
}

fn set(e: &AppState, models: &[&str], hours: u16) -> OnlyModels {
    e.set_only(SetOnly {
        models: models.iter().map(|m| (*m).into()).collect(),
        hours,
    })
    .unwrap()
}

fn hire(e: &AppState, role: &str, model: &str, effort: Effort) {
    let mut old = e.proj().projects[&ProjectId::new("p")]
        .hire(&RoleId::new(role), None)
        .unwrap()
        .clone();
    old.status = HireStatus::Retired;
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::HireRetired(old),
            EventKind::HireAdded(HireRecord {
                id: HireId::new(format!("new-{role}")),
                role: RoleId::new(role),
                model: model.into(),
                effort,
                status: HireStatus::Active,
                routine: false,
            }),
        ])
    })
    .unwrap();
}

fn task(e: &AppState, name: &str) -> TaskId {
    reviewed_task(e, name, dvb_core::roles::ReviewKind::Code)
}

fn reviewed_task(e: &AppState, name: &str, review: dvb_core::roles::ReviewKind) -> TaskId {
    let id = TaskId::new(name);
    let goal = GoalId::new(format!("goal-{name}"));
    let mut plan = support::events::backend_plan_event(&goal, &id);
    if let EventKind::PlanProposed { tasks, .. } = &mut plan {
        tasks[0].review = review;
    }
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::goal_event(&goal, name, None),
            plan,
            EventKind::GoalApproved { goal },
        ])
    })
    .unwrap();
    id
}

async fn start(e: &AppState, id: &TaskId, observed: &Observed) -> RunAssignment {
    let n = observed.starts.lock().unwrap().len();
    support::wait::io(dispatch::start_waiting(e.clone()))
        .await
        .unwrap();
    // Automatic approved admission uses the same path as explicit waiting starts.
    observed.wait_for_count(n + 1).await;
    let proj = e.proj();
    let run = proj.tasks[id].facts.run.as_ref().unwrap();
    proj.runs[run].assignment.clone()
}

fn request_start(e: &AppState, id: &TaskId) {
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskWaiting {
            task: id.clone(),
            model: None,
            effort: None,
            reason: "Build".into(),
            routine: false,
        }])
    })
    .unwrap();
}

fn kinds(e: &AppState) -> Vec<EventKind> {
    support::events::all(e)
        .into_iter()
        .map(|ev| ev.kind)
        .collect()
}

async fn settings(e: &AppState) -> Settings {
    let (status, value) = call(e, Method::GET, "/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_value(value).unwrap()
}

fn assert_env(observed: &Observed, a: &RunAssignment) {
    let starts = observed.starts.lock().unwrap();
    let (unit, env) = starts.iter().find(|(unit, _)| unit.run == a.run).unwrap();
    assert_eq!(unit.vendor, a.vendor);
    assert!(env.contains(&format!("DVB_MODEL={}\n", a.model)), "{env}");
    assert!(
        env.contains(&format!(
            "DVB_EFFORT={}\n",
            serde_json::to_value(a.effort).unwrap().as_str().unwrap()
        )),
        "{env}"
    );
    drop(starts);
}

#[tokio::test(start_paused = true)]
async fn named_hire_stays_and_other_hire_uses_first_model_nearest_effort_with_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let (e, observed) = fixture(dir.path(), clock).await;
    hire(&e, "backend", "codex-test", Effort::Medium);
    let before = settings(&e).await;
    let hires = e.proj().projects[&ProjectId::new("p")].hires.clone();
    set(&e, &["claude-test", "codex-test"], 1);
    let named = task(&e, "named");
    request_start(&e, &named);
    let a = start(&e, &named, &observed).await;
    assert_eq!(
        (a.model.as_str(), a.effort, a.vendor),
        ("codex-test", Effort::Medium, Vendor::Codex)
    );
    assert!(
        !kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::RunRerouted { run, .. } if run == &a.run))
    );
    assert_env(&observed, &a);
    e.clear_only().unwrap();
    set(&e, &["claude-test", "fake-next"], 1);
    let switched = task(&e, "switched");
    request_start(&e, &switched);
    let b = start(&e, &switched, &observed).await;
    assert_eq!(
        (b.model.as_str(), b.effort, b.vendor),
        ("claude-test", Effort::Low, Vendor::Claude)
    );
    assert_eq!(a.hire, b.hire);
    assert_env(&observed, &b);
    let events = support::events::all(&e);
    assert!(events.iter().any(|ev| matches!(&ev.kind, EventKind::RunStarted(original)
        if original.run == b.run && original.model == "codex-test" && original.effort == Effort::Medium)));
    let reroute = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::RunRerouted {run, ..} if run == &b.run))
        .unwrap();
    assert!(
        matches!(&reroute.kind, EventKind::RunRerouted { from_model, to_model, limited: None, .. }
        if from_model == "codex-test" && to_model == "claude-test")
    );
    assert!(dvb_cli::line(reroute).contains("hired for codex-test, runs on claude-test"));
    assert_eq!(Projections::rebuild(&events).runs[&b.run].assignment, b);
    e.clear_only().unwrap();
    assert_eq!(settings(&e).await, before);
    assert_eq!(e.proj().projects[&ProjectId::new("p")].hires, hires);
    // Clearing changes no assignment already launched.
    assert_eq!(e.proj().runs[&b.run].assignment, b);
}

#[tokio::test(start_paused = true)]
async fn live_expiry_returns_new_work_to_hire_and_keeps_running_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let (e, observed) = fixture(dir.path(), clock.clone()).await;
    let hires = e.proj().projects[&ProjectId::new("p")].hires.clone();
    let before = settings(&e).await;
    let only = set(&e, &["fake-next"], 1);
    tokio::task::yield_now().await; // arm the actual deadline timer
    let first = task(&e, "first");
    request_start(&e, &first);
    let a = start(&e, &first, &observed).await;
    assert_eq!(a.model, "fake-next");
    clock.set(only.ends_at);
    tokio::time::advance(Duration::from_secs(3600)).await;
    wait.until("automatic only.ended event", || {
        kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::OnlyEnded { .. }))
    })
    .await;
    // Assert the timer, before a read or new admission can expire it lazily.
    assert!(e.proj().only.is_none());
    assert_eq!(e.proj().runs[&a.run].assignment, a);
    let second = task(&e, "second");
    request_start(&e, &second);
    let b = start(&e, &second, &observed).await;
    assert_eq!((b.model.as_str(), b.effort), ("fake-1", Effort::Medium));
    assert_env(&observed, &b);
    assert_eq!(settings(&e).await, before);
    assert_eq!(e.proj().projects[&ProjectId::new("p")].hires, hires);
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyStarted(_)))
            .count(),
        1
    );
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn replacement_old_deadline_and_repeated_clear_end_each_generation_once() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let e = empty(dir.path(), clock.clone(), Default::default());
    let first = set(&e, &["fake-1"], 1);
    tokio::task::yield_now().await;
    let replacement = set(&e, &["fake-next", "fake-1"], 2);
    tokio::task::yield_now().await;
    clock.set(first.ends_at);
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::task::yield_now().await;
    assert_eq!(e.only_models().unwrap(), Some(replacement.clone()));
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        1
    );
    e.clear_only().unwrap();
    e.clear_only().unwrap();
    clock.set(replacement.ends_at);
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::task::yield_now().await;
    assert!(e.only_models().unwrap().is_none());
    let events = support::events::all(&e);
    let starts: Vec<_> = events
        .iter()
        .filter(|ev| matches!(ev.kind, EventKind::OnlyStarted(_)))
        .map(|ev| ev.id)
        .collect();
    let ends: Vec<_> = events
        .iter()
        .filter_map(|ev| {
            if let EventKind::OnlyEnded { started } = ev.kind {
                Some(started)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(starts, ends);
}

#[tokio::test(start_paused = true)]
async fn replay_before_deadline_and_overdue_startup_then_new_run_on_hire() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let (e, _) = fixture(dir.path(), clock.clone()).await;
    call(
        &e,
        Method::PUT,
        "/settings",
        Some(json!({"values":{"model_pool":["fake-1","fake-next"]}})),
    )
    .await;
    let before = settings(&e).await;
    let hires = e.proj().projects[&ProjectId::new("p")].hires.clone();
    let only = set(&e, &["fake-next", "fake-1"], 1);
    tokio::task::yield_now().await;
    drop(e);
    let e = empty(dir.path(), clock.clone(), Default::default());
    assert_eq!(e.only_models().unwrap(), Some(only.clone()));
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        0
    );
    drop(e);
    clock.set(only.ends_at.plus_millis(1));
    let observed = Arc::new(Observed::new(dir.path().join("work")));
    let e = empty(
        dir.path(),
        clock.clone(),
        dvb_engine::EngineOptions {
            launcher: Some(observed.launcher.clone()),
            ..Default::default()
        },
    );
    // Engine::open itself ended the overdue switch, before any status read.
    assert!(e.proj().only.is_none());
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        1
    );
    let t = task(&e, "after-restart");
    request_start(&e, &t);
    let a = start(&e, &t, &observed).await;
    assert_eq!(a.model, "fake-1");
    assert_eq!(settings(&e).await, before);
    assert_eq!(e.proj().projects[&ProjectId::new("p")].hires, hires);
    drop(e);
    let e = empty(dir.path(), clock, Default::default());
    assert!(e.proj().only.is_none());
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_pool_selection_and_hours_are_coded_and_leave_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let e = empty(
        dir.path(),
        Arc::new(FakeClock::at(Timestamp(1_000_000))),
        Default::default(),
    );
    let (_, value) = call(
        &e,
        Method::PUT,
        "/settings",
        Some(json!({"values":{"model_pool":["fake-1"]}})),
    )
    .await;
    assert!(value.get("values").is_some());
    let before = settings(&e).await;
    let active = set(&e, &["fake-1"], 1);
    for id in ["fake-next", "fake", "outside-pool", "FAKE-1"] {
        let (status, value) = call(
            &e,
            Method::PUT,
            "/only",
            Some(json!({"models":[id],"hours":1})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(value["code"], "MODEL_NOT_IN_POOL");
        assert_eq!(value["detail"], "valid model ids: fake-1");
        assert_eq!(e.only_models().unwrap(), Some(active.clone()));
    }
    for hours in [
        json!(0),
        json!(169),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(65536),
    ] {
        let (status, value) = call(
            &e,
            Method::PUT,
            "/only",
            Some(json!({"models":["fake-1"],"hours":hours})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(value["code"], "INVALID_VALUE");
        assert!(
            value["detail"]
                .as_str()
                .unwrap()
                .contains("whole number from 1 to 168")
        );
    }
    assert_eq!(e.only_models().unwrap(), Some(active));
    for hours in [1, 168] {
        set(&e, &["fake-1"], hours);
        e.clear_only().unwrap();
    }
    assert_eq!(settings(&e).await, before);
}

fn limit(e: &AppState, vendor: Vendor) {
    vendors::record(
        e,
        vendor,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(e.clock.now().plus_millis(7_200_000)),
        },
    )
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn limited_effective_vendor_waits_without_fallback_and_replacement_wakes_queued_run() {
    let dir = tempfile::tempdir().unwrap();
    let (e, observed) = fixture(dir.path(), Arc::new(FakeClock::at(Timestamp(1_000_000)))).await;
    set(&e, &["codex-test", "fake-next"], 1);
    limit(&e, Vendor::Codex);
    let t = task(&e, "queued");
    request_start(&e, &t);
    support::wait::io(dispatch::start_waiting(e.clone()))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert!(observed.starts.lock().unwrap().is_empty());
    assert!(
        !kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::RunRerouted { .. }))
    );
    // The available orchestrator/Fake vendor and second named model must not
    // cause usage fallback. A replacement changes the waiting run at launch.
    set(&e, &["fake-next"], 2);
    wait.until("replacement unblocks queued run", || {
        !observed.starts.lock().unwrap().is_empty()
    })
    .await;
    let a = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.task.as_ref() == Some(&t))
        .unwrap()
        .assignment
        .clone();
    assert_eq!(a.model, "fake-next");
    assert_env(&observed, &a);
    // Switch change affects queued assignments; already-running one is fixed.
    e.clear_only().unwrap();
    assert_eq!(e.proj().runs[&a.run].assignment, a);
}

#[tokio::test(start_paused = true)]
async fn excluded_limited_orchestrator_vendor_does_not_block_switched_builder_or_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let (e, observed) = fixture(dir.path(), Arc::new(FakeClock::at(Timestamp(1_000_000)))).await;
    hire(&e, "orchestrator", "codex-test", Effort::High);
    limit(&e, Vendor::Codex);
    set(&e, &["fake-next"], 1);
    let t = task(&e, "builder");
    request_start(&e, &t);
    let b = start(&e, &t, &observed).await;
    assert_eq!(b.model, "fake-next");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![support::events::goal_event(
            &GoalId::new("planning"),
            "Plan",
            None,
        )])
    })
    .unwrap();
    wake::wake(e.clone(), ProjectId::new("p"), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    let a = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.task.is_none())
        .unwrap()
        .assignment
        .clone();
    assert_eq!((a.model.as_str(), a.effort), ("fake-next", Effort::High));
    assert_env(&observed, &a);
    assert!(kinds(&e).iter().any(
        |k| matches!(k, EventKind::RunRerouted {run, task:None, from_model, limited:None,..}
        if run == &a.run && from_model == "codex-test")
    ));
}

#[tokio::test(start_paused = true)]
async fn reviewer_switch_uses_actual_builder_vendor_and_flags_same_vendor_review() {
    for (role, review, switched) in [
        ("code-reviewer", dvb_core::roles::ReviewKind::Code, true),
        ("code-reviewer", dvb_core::roles::ReviewKind::Design, true),
        ("code-reviewer", dvb_core::roles::ReviewKind::Code, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (e, observed) =
            fixture(dir.path(), Arc::new(FakeClock::at(Timestamp(1_000_000)))).await;
        hire(&e, role, "codex-test", Effort::High);
        set(&e, &["fake-next"], 1);
        let t = reviewed_task(&e, "review", review);
        request_start(&e, &t);
        let b = start(&e, &t, &observed).await;
        let repo = dir.path().join("repo");
        std::fs::write(repo.join("change.txt"), "changed\n").unwrap();
        support::git::git(&repo, &["add", "."]);
        support::git::git(&repo, &["commit", "-qm", "Change"]);
        let head = support::wait::io(e.fetch(
            e.forge().unwrap().as_ref(),
            &dvb_core::forge::Repo(repo.to_string_lossy().into()),
            &e.mirror("p"),
            "main",
        ))
        .await
        .unwrap();
        if !switched {
            e.clear_only().unwrap();
            limit(&e, Vendor::Codex);
        }
        let forge = e.forge().unwrap();
        let remote = dvb_core::forge::Repo(repo.to_string_lossy().into());
        let pr = support::wait::io(async {
            forge
                .push(&remote, &e.mirror("p"), "dvb/review-1", &head)
                .await
                .unwrap();
            forge
                .open_pr(&remote, "dvb/review-1", "Review", "Review fixture")
                .await
                .unwrap()
        })
        .await;
        // Enter the genuine review dispatch path without a live builder.
        let task_lock = e.lock(&t).await;
        e.run_command(
            &b.run,
            dvb_engine::Decision::new("collected", Command::HeadWritten),
        )
        .unwrap();
        e.end_run(&b.run, EndReason::Completed, None).unwrap();
        e.decide(
            e.proj(),
            &t,
            Actor::Engine,
            dvb_engine::Decision {
                pre: vec![EventKind::BranchPublished {
                    task: t.clone(),
                    round: 1,
                    branch: "dvb/review-1".into(),
                    head: head.clone(),
                }],
                ..dvb_engine::Decision::new("published", Command::Published)
            },
            |_| Ok(()),
        )
        .unwrap();
        e.decide(
            e.proj(),
            &t,
            Actor::Engine,
            dvb_engine::Decision {
                pre: vec![EventKind::PrOpened {
                    task: t.clone(),
                    pr: pr.number,
                    url: pr.url,
                }],
                ..dvb_engine::Decision::new("pr", Command::PrOpened)
            },
            |_| Ok(()),
        )
        .unwrap();
        drop(task_lock);
        dispatch::step_now(&e, &t).await.unwrap();
        observed.wait_for_count(2).await;
        let r = e
            .proj()
            .runs
            .values()
            .find(|r| r.assignment.reviews.is_some())
            .unwrap()
            .assignment
            .clone();
        assert_eq!(
            (r.model.as_str(), r.effort, r.vendor),
            (
                if switched { "fake-next" } else { "fake-1" },
                Effort::High,
                Vendor::Fake
            )
        );
        assert_eq!(r.hire, HireId::new(format!("new-{role}")));
        assert_env(&observed, &r);
        assert!(kinds(&e).iter().any(
            |k| matches!(k,EventKind::RunOutput {run,item:OutputItem::Note(text), ..}
        if run==&r.run && text=="Same-vendor review: builder and reviewer both run on fake")
        ));
        assert!(kinds(&e).iter().any(
            |k| matches!(k,EventKind::RunStarted(a) if a.run==r.run && a.model=="codex-test")
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn a_restart_before_deadline_restores_a_live_expiry_timer() {
    let dir = tempfile::Builder::new()
        .prefix("dvbonly")
        .tempdir()
        .unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let e = empty(dir.path(), clock.clone(), Default::default());
    let only = set(&e, &["fake-next"], 1);
    tokio::task::yield_now().await;
    drop(e);
    let e = empty(dir.path(), clock.clone(), Default::default());
    assert_eq!(e.proj().only.as_ref().unwrap().1, only);
    let engine = tokio::spawn(dvb_engine::start(e.clone()));
    wait.until("restarted engine socket", || {
        e.cfg.paths.engine_sock.exists()
    })
    .await;
    tokio::task::yield_now().await;
    clock.set(only.ends_at);
    tokio::time::advance(Duration::from_secs(3600)).await;
    wait.until("recovered timer's end event", || {
        kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::OnlyEnded { .. }))
    })
    .await;
    assert!(e.proj().only.is_none());
    assert_eq!(
        kinds(&e)
            .iter()
            .filter(|k| matches!(k, EventKind::OnlyEnded { .. }))
            .count(),
        1
    );
    engine.abort();
}

#[tokio::test(start_paused = true)]
async fn queued_before_switch_uses_switch_at_launch_and_clear_releases_limited_target() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let (e, observed) = fixture(dir.path(), clock.clone()).await;
    // A run admitted earlier waits for its hired vendor's usage window.
    vendors::record(
        &e,
        Vendor::Fake,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(e.clock.now().plus_millis(60_000)),
        },
    )
    .unwrap();
    let t = task(&e, "before-switch");
    request_start(&e, &t);
    support::wait::io(dispatch::start_waiting(e.clone()))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert!(observed.starts.lock().unwrap().is_empty());
    set(&e, &["claude-test"], 1);
    observed.wait_for_count(1).await;
    let a = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.task.as_ref() == Some(&t))
        .unwrap()
        .assignment
        .clone();
    assert_eq!(a.model, "claude-test");
    assert_env(&observed, &a);
    // The next queued run waits for the effective target. Clearing selects its
    // hire again, and an expired vendor window then permits that original hire.
    limit(&e, Vendor::Codex);
    set(&e, &["codex-test"], 1);
    let t2 = task(&e, "clear-queued");
    request_start(&e, &t2);
    support::wait::io(dispatch::start_waiting(e.clone()))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(observed.starts.lock().unwrap().len(), 1);
    // First reset the hired/orchestrator vendor while codex stays limited.
    clock.advance_millis(60_000);
    tokio::time::advance(Duration::from_secs(60)).await;
    e.clear_only().unwrap();
    observed.wait_for_count(2).await;
    let b = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.task.as_ref() == Some(&t2))
        .unwrap()
        .assignment
        .clone();
    assert_eq!(b.model, "fake-1");
    assert_env(&observed, &b);
    assert_eq!(e.proj().runs[&a.run].assignment, a);
}

#[tokio::test(start_paused = true)]
async fn an_unselected_hire_is_switched_without_changing_selected_models_or_hires() {
    let dir = tempfile::tempdir().unwrap();
    let (e, observed) = fixture(dir.path(), Arc::new(FakeClock::at(Timestamp(1_000_000)))).await;
    call(
        &e,
        Method::PUT,
        "/settings",
        Some(json!({"values":{"model_pool":["fake-next"]}})),
    )
    .await;
    let before = settings(&e).await;
    let hires = e.proj().projects[&ProjectId::new("p")].hires.clone();
    set(&e, &["fake-next"], 1);
    let t = task(&e, "unselected-hire");
    request_start(&e, &t);
    let a = start(&e, &t, &observed).await;
    assert_eq!(a.model, "fake-next");
    assert_eq!(a.hire, HireId::new("h-backend"));
    assert_eq!(settings(&e).await, before);
    assert_eq!(e.proj().projects[&ProjectId::new("p")].hires, hires);
}

#[tokio::test(start_paused = true)]
async fn a_waiting_orchestrator_uses_current_switch_and_expiry_returns_it_to_its_hire() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let (e, observed) = fixture(dir.path(), clock.clone()).await;
    // The hired model is available. The effective first named model is limited;
    // neither another named model nor the original hire may bypass that wait.
    limit(&e, Vendor::Codex);
    let only = set(&e, &["codex-test", "fake-next"], 1);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![support::events::goal_event(
            &GoalId::new("planning"),
            "Plan",
            None,
        )])
    })
    .unwrap();
    let wake = tokio::spawn(wake::wake(
        e.clone(),
        ProjectId::new("p"),
        wake::Wake::Goal,
        0,
        0,
    ));
    tokio::task::yield_now().await;
    assert!(observed.starts.lock().unwrap().is_empty());
    clock.set(only.ends_at);
    tokio::time::advance(Duration::from_secs(3600)).await;
    wake.await.unwrap().unwrap();
    let a = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.task.is_none())
        .unwrap()
        .assignment
        .clone();
    assert_eq!((a.model.as_str(), a.effort), ("fake-1", Effort::Medium));
    assert_env(&observed, &a);
    assert!(
        !kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::RunRerouted { .. }))
    );
    assert!(e.proj().only.is_none());
}
