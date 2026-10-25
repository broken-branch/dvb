//! Owner replies wake Fake runs; stale goal questions disappear from open views.
use dvb_engine::owner;
mod support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use dvb_core::api::{QuestionView, Status};
use dvb_core::event::{Answer, EndReason, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::launcher::{UnitId, UnitState};
use dvb_core::{Actor, EventId, EventKind, GoalId, ProjectId, QuestionId, Vendor};
use dvb_engine::prompt::render;
use dvb_engine::{AppState, Engine, api, dispatch, publish, reconcile, wake};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tower::ServiceExt;

use support::launcher::FakeLauncher as Started;

async fn fixture() -> (tempfile::TempDir, AppState, Arc<Started>, ProjectId) {
    fixture_with(Default::default()).await
}

async fn fixture_with(
    mut options: dvb_engine::EngineOptions,
) -> (tempfile::TempDir, AppState, Arc<Started>, ProjectId) {
    options
        .forge
        .get_or_insert_with(|| Arc::new(dvb_forge::FakeForge::local_git()));
    let dir = tempfile::tempdir().unwrap();
    let launcher = Arc::new(Started {
        stop_changes_state: false,
        ..Started::new(UnitState::Active)
    });
    let e = support::engine::open_with(
        dir.path(),
        "unused-dvb",
        launcher.readings.options(dvb_engine::EngineOptions {
            launcher: Some(launcher.clone()),
            ..options
        }),
    );
    // Reply delivery tests supply disk explicitly rather than probing /tmp.
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    let repo = dir.path().join("repo");
    support::git::seed_repository(&repo, "main", None, "Seed");
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("project"))
        .await
        .unwrap();
    let p = ProjectId::new("p");
    let mut events = vec![support::events::project_event(
        &p,
        "project",
        &repo.to_string_lossy(),
    )];
    let (roles, hires) = support::events::default_role_hire_events();
    for (role, hire) in roles.into_iter().zip(hires) {
        events.push(hire);
        events.push(role);
    }
    events.push(EventKind::GoalStated {
        goal: GoalId::new("g"),
        text: "First goal".into(),
        from_issue: None,
        issues: Vec::new(),
        after: None,
    });
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(events))
        .unwrap();
    (dir, e, launcher, p)
}

#[tokio::test]
async fn concurrent_goal_wakes_bind_the_prompt_to_the_goal_version_at_admission() {
    let forge = Arc::new(support::InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::local_git(),
    )));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *forge.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        forge: Some(forge),
        ..Default::default()
    })
    .await;
    let waking = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    barrier.wait().await;
    // While fetch is pending, the owner changes the selected goal and states
    // another one. The admission transaction must select the current version.
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "Plan the parser".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::GoalStated {
                goal: GoalId::new("g2"),
                text: "Plan the exporter".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ])
    })
    .unwrap();
    let other = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    barrier.wait().await;
    waking.await.unwrap().unwrap();
    other.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    let unit = launcher.units.lock().unwrap()[0].clone();
    let text = prompt(&e, &unit);
    let own = text
        .split_once("## This run's goal\n\n")
        .unwrap()
        .1
        .split_once("## Your notes")
        .unwrap()
        .0;
    assert!(own.contains("Goal id: g;"));
    assert!(own.contains("\n\nPlan the parser\n\n"));
    assert!(!own.contains("Plan the exporter"));
    assert!(own.contains("Other goals waiting for plans: g2"));
    assert!(!text.contains("{{"), "{text}");
    let version = e.proj().goals[&GoalId::new("g")].version;
    assert!(own.contains(&format!("version: {version};")));
    assert_eq!(e.proj().runs[&unit.run].goal_version, Some(version));
    let reopened = Engine::open(
        e.cfg.clone(),
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(reopened.proj().runs[&unit.run].goal_version, Some(version));
}

fn open(e: &AppState, p: &ProjectId, id: &str, lapses_on: LapsesOn) {
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new(id),
            subject: "What next?".into(),
            format: QuestionFormat::Simple,
            options: vec![
                QuestionOption {
                    id: 1,
                    text: "Repair the existing task".into(),
                    effect: "Repair".into(),
                    approves_plan: false,
                    no_effect: false,
                    retries_task: None,
                },
                QuestionOption {
                    id: 2,
                    text: "Yes, start".into(),
                    effect: "Start the plan".into(),
                    approves_plan: true,
                    no_effect: false,
                    retries_task: None,
                },
            ],
            recommended: 1,
            lapses_on,
        }])
    })
    .unwrap();
}

async fn request(e: &AppState, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().uri(path);
    let req = match body {
        Some(body) => builder
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = api::router(e.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn wait_for(signal: &Semaphore) {
    support::wait::io(tokio::time::timeout(
        Duration::from_secs(10),
        signal.acquire(),
    ))
    .await
    .unwrap()
    .unwrap()
    .forget();
}

fn prompt(e: &AppState, unit: &UnitId) -> String {
    std::fs::read_to_string(e.cfg.paths.work.join(unit.run.as_str()).join("prompt.md")).unwrap()
}

/// A prompt fragment as `prompts/fragments.toml` holds it.
fn fragment(name: &str) -> String {
    let fragments: std::collections::BTreeMap<String, String> =
        toml::from_str(include_str!("../../../prompts/fragments.toml")).unwrap();
    fragments[name].clone()
}

fn done(e: &AppState, p: &ProjectId) {
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
}

fn fake_orchestrator(run: &str, goal_version: Option<EventId>) -> dvb_core::RunAssignment {
    support::assignment::Assignment::new(dvb_core::RunId::new(run))
        .goal_version(goal_version)
        .role(dvb_core::RoleId::new("orchestrator"))
        .hire(dvb_core::HireId::new("other"))
        .effort(dvb_core::Effort::Medium)
        .reason("Other goal".into())
        .build()
}

fn fill_run_cap(e: &AppState) -> Vec<dvb_core::RunId> {
    let limit = 3;
    e.append_checked(None, Actor::Owner, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "max_runs".into(),
            old: json!(null),
            new: json!(limit),
            restored_from: None,
        }])
    })
    .unwrap();
    let assignments: Vec<_> = (0..limit)
        .map(|i| fake_orchestrator(&format!("busy-{i}"), None))
        .collect();
    let runs = assignments.iter().map(|a| a.run.clone()).collect();
    e.append_checked(Some(&ProjectId::new("other")), Actor::Engine, |_, _| {
        Ok(assignments.into_iter().map(EventKind::RunStarted).collect())
    })
    .unwrap();
    runs
}

#[tokio::test]
async fn an_answer_while_paused_is_delivered_once_on_resume() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    open(&e, &p, "q", LapsesOn::ValueVersion(EventId(0)));
    owner::pause(&e, &p, true).unwrap();
    let (status, _) = request(
        &e,
        "/questions/q/answer",
        Some(json!({"reply": "Paused reply"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert!(launcher.units.lock().unwrap().is_empty());
    owner::pause(&e, &p, false).unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(prompt(&e, &unit).contains("What next? -> Paused reply"));
    dispatch::run_ended(&e, &unit.run, EndReason::Completed)
        .await
        .unwrap();
    wake::wake_if_missed(&e, &p);
    wake::wake(e.clone(), p, wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn restart_recovers_an_answer_committed_before_its_wake() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    open(&e, &p, "q", LapsesOn::ValueVersion(EventId(0)));
    // The engine died after this transaction, before scheduling the wake.
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question: QuestionId::new("q"),
            answer: Answer::Reply("Recovered reply".into()),
        }])
    })
    .unwrap();
    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(prompt(&e, &unit).contains("What next? -> Recovered reply"));
    dispatch::run_ended(&e, &unit.run, EndReason::Completed)
        .await
        .unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    wake::wake(e, p, wake::Wake::Answer, 0, 0).await.unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_answers_reach_exactly_one_prompt_each_without_overlapping_runs() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    for q in ["q1", "q2"] {
        open(&e, &p, q, LapsesOn::ValueVersion(EventId(0)));
    }
    launcher.block_start.store(true, Ordering::SeqCst);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut requests = vec![];
    for (q, reply) in [("q1", "Concurrent first"), ("q2", "Concurrent second")] {
        let e = e.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            request(
                &e,
                &format!("/questions/{q}/answer"),
                Some(json!({"reply": reply})),
            )
            .await
        }));
    }
    for request in requests {
        assert_eq!(request.await.unwrap().0, StatusCode::OK);
    }
    wait_for(&launcher.starting).await;
    // A wake during StartUnit is coalesced, as is one during a running unit.
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(e.proj().live_runs().count(), 1);
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    launcher.block_start.store(false, Ordering::SeqCst);
    launcher.release.add_permits(1);
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    let mut prompts = vec![prompt(&e, &first)];
    dispatch::run_ended(&e, &first.run, EndReason::Completed)
        .await
        .unwrap();
    if !prompts[0].contains("What next? -> Concurrent first")
        || !prompts[0].contains("What next? -> Concurrent second")
    {
        // An answer after the first start belongs to the end-of-run wake.
        wait_for(&launcher.ready).await;
        let second = launcher.units.lock().unwrap()[1].clone();
        assert_eq!(e.proj().live_runs().count(), 1);
        prompts.push(prompt(&e, &second));
        dispatch::run_ended(&e, &second.run, EndReason::Completed)
            .await
            .unwrap();
    }
    wake::wake(e.clone(), p, wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(e.proj().live_runs().count(), 0);
    assert_eq!(launcher.units.lock().unwrap().len(), prompts.len());
    for reply in ["Concurrent first", "Concurrent second"] {
        assert_eq!(
            prompts
                .iter()
                .filter(|text| text.contains(&format!("What next? -> {reply}")))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn an_answer_after_run_start_waits_for_the_end_of_that_run() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    open(&e, &p, "q1", LapsesOn::ValueVersion(EventId(0)));
    open(&e, &p, "q2", LapsesOn::ValueVersion(EventId(0)));
    let (status, _) = request(
        &e,
        "/questions/q1/answer",
        Some(json!({"reply": "First reply"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    let (status, _) = request(&e, "/questions/q2/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    // Read before the run ends: an ended orchestrator's directory is removed.
    let first_prompt = prompt(&e, &first);
    dispatch::run_ended(&e, &first.run, EndReason::Completed)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let second = launcher.units.lock().unwrap()[1].clone();
    let second_prompt = prompt(&e, &second);
    assert!(first_prompt.contains("What next? -> First reply"));
    assert!(!first_prompt.contains("What next? -> Yes, start"));
    assert!(!second_prompt.contains("What next? -> First reply"));
    assert!(second_prompt.contains("What next? -> Yes, start"));
    assert_eq!(
        e.proj().runs[&second.run].assignment.reason,
        "The owner approved the plan."
    );
}

#[tokio::test]
async fn approval_and_other_replies_wait_at_the_global_cap_and_recover_after_restart() {
    for choice in [1, 2] {
        let (_dir, e, launcher, p) = fixture().await;
        done(&e, &p);
        let version = e.proj().goals[&GoalId::new("g")].version;
        open(&e, &p, "q", LapsesOn::GoalVersion(version));
        let busy = fill_run_cap(&e);
        let (status, _) = request(&e, "/questions/q/answer", Some(json!({"choice": choice}))).await;
        assert_eq!(status, StatusCode::OK);
        wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
            .await
            .unwrap();
        assert!(launcher.units.lock().unwrap().is_empty());
        assert_eq!(e.proj().live_runs().count(), busy.len());

        // No run was admitted, so the saved answer remains undelivered when
        // the store is reopened. A different project's slot release recovers it.
        let cfg = e.cfg.clone();
        drop(e);
        let e = Engine::open(
            cfg,
            launcher.readings.options(dvb_engine::EngineOptions {
                forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
                launcher: Some(launcher.clone()),
                ..Default::default()
            }),
        )
        .unwrap();
        *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
        e.remember();
        e.end_run(&busy[0], EndReason::Completed, None).unwrap();
        wait_for(&launcher.ready).await;
        assert_eq!(launcher.units.lock().unwrap().len(), 1);
        assert_eq!(e.proj().live_runs().count(), busy.len());
        let unit = launcher.units.lock().unwrap()[0].clone();
        let run = e.proj().runs[&unit.run].clone();
        assert_eq!(run.goal_version, Some(version));
        assert_eq!(
            run.assignment.reason,
            if choice == 2 {
                "The owner approved the plan."
            } else {
                "The owner answered a question."
            }
        );
        let expected = if choice == 2 {
            "- What next? [goal: First goal] -> Yes, start (Start the plan)"
        } else {
            "- What next? [goal: First goal] -> Repair the existing task (Repair)"
        };
        let text = prompt(&e, &unit);
        assert_eq!(
            text.lines().filter(|line| *line == expected).count(),
            1,
            "Expected exactly one reply line {expected:?} for choice {choice}; prompt:\n{text}"
        );
        dispatch::run_ended(&e, &unit.run, EndReason::Completed)
            .await
            .unwrap();
        wake::wake(e.clone(), p, wake::Wake::Answer, 0, 0)
            .await
            .unwrap();
        assert_eq!(launcher.units.lock().unwrap().len(), 1);
        assert_eq!(e.proj().live_runs().count(), busy.len() - 1);
    }
}

#[tokio::test]
async fn a_completed_goal_waits_at_the_global_cap_and_recovers_once_after_restart() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    let version = e.proj().goals[&GoalId::new("g")].version;

    // A later orchestrator for another goal did not receive this completion.
    state_goal(&e, &p, "other-goal", "Unrelated goal");
    let other_version = e.proj().goals[&GoalId::new("other-goal")].version;
    let unrelated = fake_orchestrator("unrelated", Some(other_version));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(unrelated.clone()),
            EventKind::RunEnded {
                run: unrelated.run,
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
            EventKind::GoalDropped {
                goal: GoalId::new("other-goal"),
            },
        ])
    })
    .unwrap();
    let busy = fill_run_cap(&e);
    wake::wake(e.clone(), p.clone(), wake::Wake::GoalDone, 0, 0)
        .await
        .unwrap();
    assert!(launcher.units.lock().unwrap().is_empty());
    assert_eq!(e.proj().live_runs().count(), busy.len());

    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    e.remember();
    e.end_run(&busy[0], EndReason::Completed, None).unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(e.proj().live_runs().count(), busy.len());
    let unit = launcher.units.lock().unwrap()[0].clone();
    let run = e.proj().runs[&unit.run].clone();
    assert_eq!(run.assignment.goal_version, Some(version));
    assert_eq!(run.assignment.reason, "The goal is done.");
    assert!(prompt(&e, &unit).contains("First goal"));
    dispatch::run_ended(&e, &unit.run, EndReason::Completed)
        .await
        .unwrap();

    // Successful delivery also survives reopening and repeated wake attempts.
    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    wake::wake(e.clone(), p.clone(), wake::Wake::GoalDone, 0, 0)
        .await
        .unwrap();
    wake::wake(e.clone(), p, wake::Wake::GoalDone, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(e.proj().live_runs().count(), busy.len() - 1);
}

#[tokio::test]
async fn an_approval_reply_stays_on_its_goal_after_task_admission_with_another_goal_waiting() {
    let (_dir, e, launcher, p) = fixture().await;
    plan_and_ask(&e, &p, "g", "q");
    state_goal(&e, &p, "g2", "Another goal needs a hire");
    plan_only(&e, &p, "g2");
    let version = e.proj().goals[&GoalId::new("g")].version;
    let mut hire = e.proj().projects[&p]
        .hire(&dvb_core::RoleId::new("backend"), None)
        .unwrap()
        .clone();

    // Hold reply delivery at admission. Its approved task starts before the
    // reply reaches the orchestrator; another approved goal now needs a hire.
    e.update.close_admission();
    let (status, _) = request(&e, "/questions/q/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    let task = dvb_core::TaskId::new("t-g");
    let builder = support::assignment::Assignment::new(dvb_core::RunId::new("already-admitted"))
        .task(Some(task.clone()))
        .role(hire.role.clone())
        .hire(hire.id.clone())
        .model(hire.model.clone())
        .effort(hire.effort)
        .reason("Continue the approved plan".into())
        .build();
    hire.status = dvb_core::roles::HireStatus::Retired;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(builder),
            EventKind::TaskStateChanged {
                task,
                from: dvb_core::TaskState::Planned,
                to: dvb_core::TaskState::Running,
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g2"),
            },
            EventKind::HireRetired(hire),
        ])
    })
    .unwrap();
    assert_eq!(
        e.proj()
            .goal_for_wake(&p, Some(wake::Wake::Approved), &e.pool)
            .unwrap()
            .id,
        GoalId::new("g2")
    );
    e.update.reopen_admission();
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    let unit = launcher.units.lock().unwrap()[0].clone();
    let run = e.proj().runs[&unit.run].clone();
    assert_eq!(run.goal_version, Some(version));
    assert_eq!(run.assignment.reason, "The owner approved the plan.");
    let text = prompt(&e, &unit);
    assert_eq!(
        text.lines().filter(|line| *line == "- What next? [goal: First goal] -> Yes, start (Start the plan)").count(),
        1,
        "{text}"
    );
    // A consumed approval reply does not admit another reply run.
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: unit.run,
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        }])
    })
    .unwrap();
    wake::wake(e.clone(), p, wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(
        e.proj()
            .runs
            .values()
            .filter(|r| r.assignment.task.is_none())
            .count(),
        1
    );
}

#[tokio::test]
async fn every_answer_wakes_a_fake_orchestrator_with_the_reply_in_its_prompt() {
    for (answer, approves, reply) in [
        (Answer::Choice(1), false, "Repair the existing task"),
        (
            Answer::Reply("Next: add a one-line .editorconfig. Plan it and ask me.".into()),
            false,
            ".editorconfig",
        ),
        (Answer::Choice(2), true, "Yes, start"),
    ] {
        let (_dir, e, launcher, p) = fixture().await;
        let version = e.proj().latest_goal(&p).unwrap().version;
        if matches!(answer, Answer::Reply(_)) {
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::GoalDone {
                    goal: GoalId::new("g"),
                }])
            })
            .unwrap();
        }
        open(&e, &p, "q", LapsesOn::GoalVersion(version));
        let (status, view) = request(
            &e,
            "/questions/q/answer",
            Some(serde_json::to_value(&answer).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let view: QuestionView = serde_json::from_value(view).unwrap();
        assert_eq!(view.answer, Some(answer));
        assert!(!view.open);
        tokio::time::timeout(Duration::from_secs(10), launcher.ready.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert_eq!(unit.vendor, Vendor::Fake);
        let r = e.proj().runs[&unit.run].clone();
        assert!(r.assignment.task.is_none());
        assert_eq!(
            r.assignment.reason,
            if approves {
                "The owner approved the plan."
            } else {
                "The owner answered a question."
            }
        );
        let dir = e.cfg.paths.work.join(unit.run.as_str());
        let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap();
        assert!(
            prompt
                .split("## The owner's reply")
                .nth(1)
                .unwrap()
                .contains(reply)
        );
        let script: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("fake-script.json")).unwrap())
                .unwrap();
        assert!(script.is_array());
        let events = support::events::all(&e);
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::GoalApproved { .. }))
                .count(),
            usize::from(approves)
        );
        let (status, body) = request(&e, "/questions/q/answer", Some(json!({"choice": 1}))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "ALREADY_TERMINAL");
        assert_eq!(launcher.units.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn lapsed_questions_are_closed_in_status_api_and_replay_but_answers_still_refuse_as_lapsed() {
    // The goal stated again ("change course"); a different goal lapses nothing.
    for goal in ["g"] {
        let (_dir, e, _launcher, p) = fixture().await;
        let version = e.proj().latest_goal(&p).unwrap().version;
        open(&e, &p, "old", LapsesOn::GoalVersion(version));
        open(&e, &p, "value", LapsesOn::ValueVersion(EventId(0)));
        let other = ProjectId::new("other");
        e.append_checked(Some(&other), Actor::Owner, |_, _| {
            Ok(vec![EventKind::GoalStated {
                goal: GoalId::new("other"),
                text: "Other project".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            }])
        })
        .unwrap();
        let other_version = e.proj().latest_goal(&other).unwrap().version;
        open(&e, &other, "other", LapsesOn::GoalVersion(other_version));
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::GoalStated {
                goal: GoalId::new(goal),
                text: "Changed goal".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            }])
        })
        .unwrap();
        // Also cover a question opened with an already stale version.
        open(&e, &p, "already-stale", LapsesOn::GoalVersion(version));
        let (_, body) = request(&e, "/questions?open=true", None).await;
        let views: Vec<QuestionView> = serde_json::from_value(body).unwrap();
        assert_eq!(views.len(), 2);
        assert!(
            views
                .iter()
                .all(|q| q.id == QuestionId::new("value") || q.id == QuestionId::new("other"))
        );
        let (_, body) = request(&e, "/questions", None).await;
        let views: Vec<QuestionView> = serde_json::from_value(body).unwrap();
        assert!(
            !views
                .iter()
                .find(|q| q.id == QuestionId::new("old"))
                .unwrap()
                .open
        );
        let (_, body) = request(&e, "/status?project=p", None).await;
        let status: Status = serde_json::from_value(body).unwrap();
        assert_eq!(status.needs_you, 1);
        let (status, body) = request(
            &e,
            "/questions/old/answer",
            Some(json!({"reply": "Too late"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "QUESTION_LAPSED");
        let rebuilt = dvb_engine::proj::Projections::rebuild(&support::events::all(&e));
        assert!(!rebuilt.questions[&QuestionId::new("old")].view.open);
        assert!(
            !rebuilt.questions[&QuestionId::new("already-stale")]
                .view
                .open
        );
        assert!(rebuilt.questions[&QuestionId::new("value")].view.open);
        assert!(rebuilt.questions[&QuestionId::new("other")].view.open);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_approvals_of_one_question_commit_one_answer_and_one_approval() {
    let (_dir, e, _launcher, p) = fixture().await;
    owner::pause(&e, &p, true).unwrap();
    for round in 0..32 {
        let id = format!("race-{round}");
        let version = e.proj().latest_goal(&p).unwrap().version;
        open(&e, &p, &id, LapsesOn::GoalVersion(version));
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut requests = Vec::new();
        for _ in 0..8 {
            let (e, id, barrier) = (e.clone(), id.clone(), barrier.clone());
            requests.push(tokio::spawn(async move {
                barrier.wait().await;
                request(
                    &e,
                    &format!("/questions/{id}/answer"),
                    Some(json!({"choice": 2})),
                )
                .await
            }));
        }
        let mut accepted = 0;
        for request in requests {
            let (status, body) = request.await.unwrap();
            if status == StatusCode::OK {
                accepted += 1;
            } else {
                assert_eq!(status, StatusCode::CONFLICT);
                assert_eq!(body["code"], "ALREADY_TERMINAL");
            }
        }
        assert_eq!(accepted, 1);
        let events = support::events::all(&e);
        assert_eq!(events.iter().filter(|ev| matches!(&ev.kind, EventKind::QuestionAnswered { question, .. } if question.as_str() == id)).count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::GoalApproved { .. }))
                .count(),
            round + 1
        );
    }
}

#[tokio::test]
async fn an_orchestrator_that_keeps_crashing_is_retried_then_the_owner_is_asked() {
    let (_dir, e, launcher, p) = fixture().await;
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    for n in 0..3 {
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[n].clone();
        // A crash in prepare: the run ends without a plan.
        dispatch::run_ended(&e, &unit.run, EndReason::Crashed)
            .await
            .unwrap();
    }
    // Retried twice after the first run; the third crash asks instead.
    let (_, body) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (s, body) = request(&e, "/questions?open=true", None).await;
            if body.as_array().is_some_and(|a| !a.is_empty()) {
                return (s, body);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let views: Vec<QuestionView> = serde_json::from_value(body).unwrap();
    assert_eq!(views.len(), 1);
    assert!(views[0].subject.contains("stopped 3 times"));
    assert_eq!(launcher.units.lock().unwrap().len(), 3);
    // While the question stands nothing wakes it on its own: not a restart's
    // reconcile, not an unpause.
    owner::pause(&e, &p, true).unwrap();
    owner::pause(&e, &p, false).unwrap();
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    wake::wake_if_missed(&e, &p);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 3);
    // Answering wakes it again.
    let (status, _) = request(
        &e,
        &format!("/questions/{}/answer", views[0].id),
        Some(json!({"choice": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn a_wake_dropped_while_an_orchestrator_ran_is_given_when_that_run_crashes() {
    let (_dir, e, launcher, p) = fixture().await;
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    // The owner changes the goal while the run is live: its wake is dropped.
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("g2"),
            text: "Second goal".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    dispatch::run_ended(&e, &first.run, EndReason::Crashed)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let second = launcher.units.lock().unwrap()[1].clone();
    assert!(prompt(&e, &second).contains("Second goal"));
}

#[tokio::test]
async fn a_crashed_answer_wake_is_retried_with_the_reply() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    open(&e, &p, "q", LapsesOn::ValueVersion(EventId(0)));
    let (status, _) = request(&e, "/questions/q/answer", Some(json!({"reply": "Do it"}))).await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    dispatch::run_ended(&e, &first.run, EndReason::Crashed)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let second = launcher.units.lock().unwrap()[1].clone();
    assert!(prompt(&e, &second).contains("What next? -> Do it"));
}

#[tokio::test]
async fn a_crashed_goal_done_wake_is_retried() {
    let (_dir, e, launcher, p) = fixture().await;
    done(&e, &p);
    wake::wake(e.clone(), p.clone(), wake::Wake::GoalDone, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    dispatch::run_ended(&e, &first.run, EndReason::Crashed)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let second = launcher.units.lock().unwrap()[1].clone();
    assert_eq!(
        e.proj().runs[&second.run].assignment.reason,
        "The goal is done."
    );
}

/// A plan and its question for goal `g`, as a finished orchestrator run leaves them.
fn plan_and_ask(e: &AppState, p: &ProjectId, g: &str, question: &str) {
    plan_only(e, p, g);
    let version = e.proj().goals[&GoalId::new(g)].version;
    open(e, p, question, LapsesOn::GoalVersion(version));
}

/// A saved plan whose question the orchestrator never got to ask.
fn plan_only(e: &AppState, p: &ProjectId, g: &str) {
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PlanProposed {
            goal: GoalId::new(g),
            tasks: vec![PlannedTask {
                after: vec![],
                task: dvb_core::TaskId::new(format!("t-{g}")),
                title: format!("Task of {g}"),
                role: dvb_core::RoleId::new("backend"),
                review: dvb_core::roles::ReviewKind::Code,
                summary: "One edit".into(),
                routine: false,
            }],
        }])
    })
    .unwrap();
}

fn state_goal(e: &AppState, p: &ProjectId, g: &str, text: &str) {
    e.append_checked(Some(p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new(g),
            text: text.into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
}

fn goal_of_run(e: &AppState, unit: &UnitId) -> Option<EventId> {
    e.proj().runs[&unit.run].goal_version
}

#[tokio::test]
async fn a_written_plan_reply_replans_its_goal_and_leaves_the_other_plan_untouched() {
    use dvb_core::provider::{Provider, RunItem};
    use dvb_core::run_verbs::{AskBody, AskReply, PlanBody, PlanReply};

    for held in [false, true] {
        let (_dir, e, launcher, p) = fixture().await;
        state_goal(&e, &p, "g2", "Second goal");
        plan_and_ask(&e, &p, "g2", "q-second");
        plan_and_ask(&e, &p, "g", "q-first");
        let first_version = e.proj().goals[&GoalId::new("g")].version;
        if held {
            // A failed fetch can hold planning independently of a saved plan.
            // Its engine-restored approval question still carries the goal.
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::QuestionOpened {
                    kind: Some(dvb_core::event::QuestionKind::PlanningHold {
                        attempt: dvb_core::RunId::new("fetch-first"),
                        fetch: true,
                    }),
                    main_ci: None,
                    task: None,
                    question: QuestionId::new(format!("planning-{first_version}-fetch-first")),
                    subject: "Planning fetch failed".into(),
                    format: QuestionFormat::Simple,
                    options: vec![],
                    recommended: 1,
                    lapses_on: LapsesOn::GoalVersion(first_version),
                }])
            })
            .unwrap();
        }
        let other_plan = format!("{:?}", e.proj().goals[&GoalId::new("g2")]);
        let other_task = format!("{:?}", e.proj().tasks[&dvb_core::TaskId::new("t-g2")]);
        let other_question = e.proj().questions[&QuestionId::new("q-second")]
            .view
            .clone();
        let reply = "Add streaming parser coverage";
        let (status, body) = request(
            &e,
            "/questions/q-first/answer",
            Some(json!({"reply": reply})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            e.proj()
                .goal_for_wake(&p, Some(wake::Wake::Answer), &e.pool)
                .map(|g| g.id.clone()),
            Some(GoalId::new("g")),
            "held: {held}"
        );
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert_eq!(goal_of_run(&e, &unit), Some(first_version));
        let text = prompt(&e, &unit);
        assert!(text.contains("Goal id: g;"), "{text}");
        assert!(
            text.contains(&format!("[goal: First goal] -> {reply}")),
            "{text}"
        );

        // The Fake receives that prompt and returns the revised plan. Submit
        // its tool bodies through the run router, so binding and revision
        // authorization must both agree with the delivered owner reply.
        let dir = e.cfg.paths.work.join(unit.run.as_str());
        let revised = json!({"tasks": [{
            "title": "Parser coverage", "role": "backend", "review": "code",
            "summary": reply
        }]});
        let script = dir.join("replan-fake.json");
        std::fs::write(
            &script,
            json!([
                {"write": {"path": "revised-plan.json", "text": revised.to_string()}},
                {"exit": 0}
            ])
            .to_string(),
        )
        .unwrap();
        let fake = dvb_run::fake::Fake {
            script,
            dvb: "unused-dvb".into(),
            stderr: dvb_run::StderrTail::default(),
        };
        let mut session = fake
            .start(&dir, &dir, &text, "fake-1", dvb_core::Effort::Medium)
            .unwrap();
        assert!(matches!(session.next(), Some(RunItem::ModelServed(_))));
        assert!(matches!(session.next(), Some(RunItem::ToolCall { .. })));
        assert!(matches!(session.next(), Some(RunItem::Ended(_))));
        let caller = || dvb_engine::serve::Caller {
            run: e.proj().runs[&unit.run].clone(),
        };
        let plan: PlanBody =
            serde_json::from_slice(&std::fs::read(dir.join("revised-plan.json")).unwrap()).unwrap();
        let revised = support::http::run::<PlanReply, _>(
            "plan",
            axum::extract::State(e.clone()),
            caller(),
            axum::Json(plan),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(revised.tasks.len(), 1);
        let ask: AskBody = serde_json::from_value(json!({
            "subject": "Start the revised plan?",
            "options": [{"text": "Start", "effect": "Build it", "approves_plan": true}],
            "recommended": 1
        }))
        .unwrap();
        let question = support::http::run::<AskReply, _>(
            "ask",
            axum::extract::State(e.clone()),
            caller(),
            axum::Json(ask),
        )
        .await
        .unwrap()
        .0
        .question;
        dispatch::run_ended(&e, &unit.run, EndReason::Completed)
            .await
            .unwrap();
        let proj = e.proj();
        let asked = &proj.questions[&question];
        assert!(asked.view.open);
        assert_eq!(asked.lapses_on, LapsesOn::GoalVersion(first_version));
        assert_eq!(asked.view.plan[0].summary, reply);
        assert_eq!(format!("{:?}", proj.goals[&GoalId::new("g2")]), other_plan);
        assert_eq!(
            format!("{:?}", proj.tasks[&dvb_core::TaskId::new("t-g2")]),
            other_task
        );
        assert_eq!(
            proj.questions[&QuestionId::new("q-second")].view,
            other_question
        );
        assert_eq!(launcher.units.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn a_second_goal_stated_back_to_back_is_planned_after_the_first_and_loses_nothing() {
    let (_dir, e, launcher, p) = fixture().await;
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    let first_version = e.proj().goals[&GoalId::new("g")].version;
    assert_eq!(goal_of_run(&e, &first), Some(first_version));
    // Stated while the first is being planned: its wake is dropped (a run is live).
    let (status, body) = request(
        &e,
        "/projects/p/goals",
        Some(json!({"text": "Second goal"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let second = GoalId::new(body["id"].as_str().unwrap());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    // The first run finishes its plan and asks; the second goal does not lapse it.
    plan_and_ask(&e, &p, "g", "q-first");
    let (_, body) = request(&e, "/questions?open=true", None).await;
    let open_ids: Vec<QuestionId> = serde_json::from_value::<Vec<QuestionView>>(body)
        .unwrap()
        .into_iter()
        .map(|q| q.id)
        .collect();
    assert_eq!(open_ids, [QuestionId::new("q-first")]);
    // The run ends: the second goal gets its wake, bound to itself.
    dispatch::run_ended(&e, &first.run, EndReason::Completed)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let next = launcher.units.lock().unwrap()[1].clone();
    let second_version = e.proj().goals[&second].version;
    assert_eq!(goal_of_run(&e, &next), Some(second_version));
    assert!(prompt(&e, &next).contains("Second goal"));
    assert!(prompt(&e, &next).contains("First goal"));
    // The first goal's question still stands, and its yes approves the first goal.
    let (status, _) = request(&e, "/questions/q-first/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    let proj = e.proj();
    assert_eq!(
        proj.goals[&GoalId::new("g")].state,
        dvb_core::api::GoalState::Approved
    );
    assert_eq!(proj.goals[&second].state, dvb_core::api::GoalState::Stated);
}

#[tokio::test]
async fn restart_wakes_an_older_unplanned_goal_behind_a_newer_planned_one() {
    let (_dir, e, launcher, p) = fixture().await;
    // The older goal never got its plan; the newer one was planned and asked.
    state_goal(&e, &p, "g2", "Second goal");
    plan_and_ask(&e, &p, "g2", "q-second");
    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    let first_version = e.proj().goals[&GoalId::new("g")].version;
    assert_eq!(goal_of_run(&e, &unit), Some(first_version));
    // The newer goal's question was not lost meanwhile.
    assert!(e.proj().questions[&QuestionId::new("q-second")].view.open);
}

fn script(e: &AppState, unit: &UnitId) -> String {
    std::fs::read_to_string(
        e.cfg
            .paths
            .work
            .join(unit.run.as_str())
            .join("fake-script.json"),
    )
    .unwrap()
}

#[tokio::test]
async fn approving_the_second_of_two_planned_goals_starts_its_own_task() {
    let (_dir, e, launcher, p) = fixture().await;
    state_goal(&e, &p, "g2", "Second goal");
    plan_and_ask(&e, &p, "g", "q-first");
    plan_and_ask(&e, &p, "g2", "q-second");
    let (status, _) = request(&e, "/questions/q-second/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    // The engine starts t-g2 itself; the orchestrator's reply run is on g2.
    wait_for(&launcher.ready).await;
    wait_for(&launcher.ready).await;
    let units = launcher.units.lock().unwrap().clone();
    let builder = units
        .iter()
        .find(|u| e.proj().runs[&u.run].assignment.task.is_some())
        .unwrap();
    assert_eq!(
        e.proj().runs[&builder.run].assignment.task,
        Some(dvb_core::TaskId::new("t-g2"))
    );
    let unit = units
        .iter()
        .find(|u| e.proj().runs[&u.run].assignment.task.is_none())
        .unwrap();
    assert_eq!(
        goal_of_run(&e, unit),
        Some(e.proj().goals[&GoalId::new("g2")].version)
    );
    let script = script(&e, unit);
    assert!(!script.contains("\"t-g\""), "{script}");
    assert!(
        e.proj().tasks[&dvb_core::TaskId::new("t-g")]
            .facts
            .run
            .is_none()
    );
}

/// #674: the owner approves goal r's plan while the orchestrator plans goal
/// g. r's task starts, at the latest when that run ends.
#[tokio::test]
async fn approving_a_plan_while_another_goals_orchestrator_runs_starts_its_task() {
    let (_dir, e, _launcher, p) = fixture().await;
    state_goal(&e, &p, "r", "Release");
    plan_and_ask(&e, &p, "r", "q-r");
    let planner = fake_orchestrator("planner-g", Some(e.proj().goals[&GoalId::new("g")].version));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(planner.clone())])
    })
    .unwrap();
    let (status, _) = request(&e, "/questions/q-r/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    e.end_run(&planner.run, EndReason::Completed, None).unwrap();
    support::orchestrator::started(&e, &dvb_core::TaskId::new("t-r")).await;
}

/// An approved plan whose goal is not the ended run's starts its task when a
/// run ends, as when the engine stopped before the approval's own start.
#[tokio::test]
async fn a_run_end_starts_another_approved_goals_planned_task() {
    let (_dir, e, _launcher, p) = fixture().await;
    e.remember();
    state_goal(&e, &p, "r", "Release");
    plan_only(&e, &p, "r");
    let planner = fake_orchestrator("planner-g", Some(e.proj().goals[&GoalId::new("g")].version));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(planner.clone()),
            EventKind::GoalApproved {
                goal: GoalId::new("r"),
            },
        ])
    })
    .unwrap();
    e.end_run(&planner.run, EndReason::Completed, None).unwrap();
    support::orchestrator::started(&e, &dvb_core::TaskId::new("t-r")).await;
}

/// At the run cap, the approved task waits for a slot rather than starting.
#[tokio::test]
async fn an_approved_task_at_the_run_cap_waits_for_a_run_to_end() {
    let (_dir, e, _launcher, p) = fixture().await;
    state_goal(&e, &p, "r", "Release");
    plan_and_ask(&e, &p, "r", "q-r");
    let busy = fill_run_cap(&e);
    let task = dvb_core::TaskId::new("t-r");
    let (status, _) = request(&e, "/questions/q-r/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK);
    support::wait::FAST
        .until("the approved task waits", || {
            e.proj().tasks[&task].waiting.is_some()
        })
        .await;
    assert!(e.proj().tasks[&task].facts.run.is_none());
    assert_eq!(e.proj().live_runs().count(), busy.len());
    e.end_run(&busy[0], EndReason::Completed, None).unwrap();
    support::orchestrator::started(&e, &task).await;
}

#[tokio::test]
async fn a_reply_to_the_second_goals_question_stays_on_the_second_goal() {
    let (_dir, e, launcher, p) = fixture().await;
    state_goal(&e, &p, "g2", "Second goal");
    plan_and_ask(&e, &p, "g", "q-first");
    plan_and_ask(&e, &p, "g2", "q-second");
    let (status, _) = request(
        &e,
        "/questions/q-second/answer",
        Some(json!({"reply": "Explain this plan first"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert_eq!(
        goal_of_run(&e, &unit),
        Some(e.proj().goals[&GoalId::new("g2")].version)
    );
    let text = prompt(&e, &unit);
    assert!(text.contains("[goal: Second goal]"), "{text}");
    assert!(
        text.contains(&format!("{}: Second goal", fragment("bound-goal"))),
        "{text}"
    );
}

#[tokio::test]
async fn restoring_a_saved_question_goes_on_to_the_next_unplanned_goal() {
    let (_dir, e, launcher, p) = fixture().await;
    // The older goal has a saved plan without its question; a newer goal has no plan.
    plan_only(&e, &p, "g");
    state_goal(&e, &p, "g2", "Second goal");
    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    let second = e.proj().goals[&GoalId::new("g2")].version;
    let proj = e.proj();
    assert_eq!(proj.runs[&unit.run].goal_version, Some(second));
    // And the first goal's question was asked meanwhile.
    let v = proj.goals[&GoalId::new("g")].version;
    assert!(
        proj.questions
            .values()
            .any(|q| q.lapses_on == LapsesOn::GoalVersion(v) && q.view.open)
    );
    drop(proj);
}

/// answers `question`, expects `QUESTION_LAPSED`, and checks that the
/// recorded refusal is the only change: the event log gains just that event,
/// and the projections, question and status views and started runs are as before.
async fn answer_lapsed_and_check_nothing_changed(
    e: &AppState,
    launcher: &Started,
    question: &str,
    answer: Value,
) {
    let snapshot = || async {
        (
            format!("{:?}", *e.proj()),
            request(e, "/questions", None).await,
            request(e, "/status?project=p", None).await,
            launcher.units.lock().unwrap().clone(),
        )
    };
    let before_events = support::events::all(e);
    let before = snapshot().await;
    let (status, body) = request(e, &format!("/questions/{question}/answer"), Some(answer)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "QUESTION_LAPSED");
    let after_events = support::events::all(e);
    assert_eq!(after_events[..before_events.len()], before_events[..]);
    let [refused] = &after_events[before_events.len()..] else {
        panic!("expected only the refusal, got {after_events:?}");
    };
    assert!(matches!(
        &refused.kind,
        EventKind::CommandRefused(r)
            if r.command == "answer" && r.refusal.current().unwrap().code == dvb_core::RefusalCode::QuestionLapsed
    ));
    assert_eq!(snapshot().await, before);
}

#[tokio::test]
async fn approving_a_plan_whose_goal_was_stated_again_is_refused_and_changes_nothing() {
    let (_dir, e, launcher, p) = fixture().await;
    plan_and_ask(&e, &p, "g", "plan");
    state_goal(&e, &p, "g", "Changed goal");
    answer_lapsed_and_check_nothing_changed(&e, &launcher, "plan", json!({"choice": 2})).await;
    assert_ne!(
        e.proj().goals[&GoalId::new("g")].state,
        dvb_core::api::GoalState::Approved
    );
}

#[tokio::test]
async fn historical_value_questions_do_not_lapse_when_a_value_is_set_again() {
    let (_dir, e, _launcher, p) = fixture().await;
    let events = support::events::all(&e);
    let hire_version = |hire: &dvb_core::roles::HireRecord| {
        events
            .iter()
            .rev()
            .find(|ev| matches!(&ev.kind, EventKind::HireAdded(h) if h.id == hire.id))
            .unwrap()
            .id
    };
    let hires = e.proj().projects[&p].hires.clone();
    let (changed, untouched) = (&hires[0], &hires[1]);
    open(
        &e,
        &p,
        "value",
        LapsesOn::ValueVersion(hire_version(changed)),
    );
    open(
        &e,
        &p,
        "untouched",
        LapsesOn::ValueVersion(hire_version(untouched)),
    );
    let mut hire = changed.clone();
    hire.effort = dvb_core::Effort::High;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::HireChanged {
            hire,
            restored_from: None,
        }])
    })
    .unwrap();
    let (status, _) = request(
        &e,
        "/questions/value/answer",
        Some(json!({"reply": "Fine"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &e,
        "/questions/untouched/answer",
        Some(json!({"reply": "Fine"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Answers question `q` with `body`; returns the events it recorded (without
/// ids and times) and the reason and reply section of the prompt of the orchestrator run it woke.
async fn answered_with(
    answer: &Answer,
    body: Value,
) -> (Vec<(Actor, Option<ProjectId>, EventKind)>, String, String) {
    let (_dir, e, launcher, p) = fixture().await;
    let version = e.proj().latest_goal(&p).unwrap().version;
    if matches!(answer, Answer::Reply(_)) {
        done(&e, &p);
    }
    open(&e, &p, "q", LapsesOn::GoalVersion(version));
    let before = support::events::all(&e).len();
    let (status, _) = request(&e, "/questions/q/answer", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(10), launcher.ready.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let unit = launcher.units.lock().unwrap()[0].clone();
    let reason = e.proj().runs[&unit.run].assignment.reason.clone();
    let dir = e.cfg.paths.work.join(unit.run.as_str());
    let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap();
    let reply = prompt.split("## The owner's reply").nth(1).unwrap();
    let reply = reply.split("The verbs:").next().unwrap().to_string();
    let events = support::events::all(&e)[before..]
        .iter()
        .filter(|ev| {
            !matches!(
                ev.kind,
                EventKind::RunStarted { .. }
                    | EventKind::RunLaunched { .. }
                    | EventKind::RunEnded { .. }
            )
        })
        .map(|ev| (ev.actor.clone(), ev.project.clone(), ev.kind.clone()))
        .collect();
    (events, reason, reply)
}

#[tokio::test]
async fn the_app_and_dvb_answer_record_the_same_events_and_wake_the_orchestrator_alike() {
    // Accept the recommendation, pick another option, write a reply.
    for (answer, app_body) in [
        (Answer::Choice(1), json!({"choice": 1})),
        (Answer::Choice(2), json!({"choice": 2})),
        (
            Answer::Reply("Add an .editorconfig".into()),
            json!({"reply": "Add an .editorconfig"}),
        ),
    ] {
        // `dvb answer` sends the serialized `Answer`; the app sends its own body.
        let cli = answered_with(&answer, serde_json::to_value(&answer).unwrap()).await;
        let app = answered_with(&answer, app_body).await;
        assert_eq!(cli.0, app.0, "{answer:?}");
        assert!(matches!(cli.0[0].2, EventKind::QuestionAnswered { .. }));
        let approves = answer == Answer::Choice(2);
        assert_eq!(
            cli.0
                .iter()
                .filter(|(_, _, k)| matches!(k, EventKind::GoalApproved { .. }))
                .count(),
            usize::from(approves)
        );
        assert_eq!(cli.1, app.1);
        assert_eq!(cli.2, app.2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_orchestrator_wake_is_held_for_disk_space_and_starts_when_it_frees() {
    let (_dir, e, launcher, p) = fixture().await;
    let gb = 1024 * 1024 * 1024;
    *launcher.readings.disk.lock().unwrap() = Some(10 * gb);
    let wake = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(launcher.units.lock().unwrap().is_empty());
    *launcher.readings.disk.lock().unwrap() = Some(30 * gb);
    e.disk_freed.notify_waiters();
    wake.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_freed_from_low_disk_still_waits_for_a_vendor_limit() {
    let (_dir, e, launcher, p) = fixture().await;
    let gb = 1024 * 1024 * 1024;
    *launcher.readings.disk.lock().unwrap() = Some(10 * gb);
    let wake = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let reset = dvb_core::Timestamp(e.clock.now().0 + 1500);
    let body = dvb_core::run_verbs::RateLimitBody {
        used_pct: 100.0,
        window: "5h".into(),
        resets_at: Some(reset),
    };
    dvb_engine::vendors::record(&e, Vendor::Fake, &body).unwrap();
    *launcher.readings.disk.lock().unwrap() = Some(30 * gb);
    e.disk_freed.notify_waiters();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(e.vendors.is_limited(Vendor::Fake));
    assert!(launcher.units.lock().unwrap().is_empty());
    wake.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert!(!e.vendors.is_limited(Vendor::Fake));
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_rechecks_disk_space_after_update_admission() {
    let (_dir, e, launcher, p) = fixture().await;
    let gb = 1024 * 1024 * 1024;
    *launcher.readings.disk.lock().unwrap() = Some(30 * gb);
    e.update.close_admission();
    let wake = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    *launcher.readings.disk.lock().unwrap() = Some(10 * gb);
    e.update.reopen_admission();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(launcher.units.lock().unwrap().is_empty());
    *launcher.readings.disk.lock().unwrap() = Some(30 * gb);
    e.disk_freed.notify_waiters();
    wake.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_delayed_approved_wake_stays_idle_after_an_owner_question_opens() {
    let recorded = Arc::new(support::InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::local_git(),
    )));
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        forge: Some(recorded.clone()),
        ..Default::default()
    })
    .await;
    plan_only(&e, &p, "g");
    let backend = dvb_core::RoleId::new("backend");
    let mut hire = e.proj().projects[&p].hire(&backend, None).unwrap().clone();
    hire.status = dvb_core::roles::HireStatus::Retired;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::HireRetired(hire),
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
        ])
    })
    .unwrap();

    // Poll this wake through its initial checks and fetch, then keep it
    // suspended at admission while another wake gets to ask the owner.
    e.update.close_admission();
    let mut delayed = Box::pin(wake::wake(e.clone(), p.clone(), wake::Wake::Approved, 0, 0));
    tokio::select! {
        result = &mut delayed => panic!("wake ended before admission reopened: {result:?}"),
        () = recorded.fetch_done.notified() => {}
    }
    assert!(launcher.units.lock().unwrap().is_empty());
    e.update.reopen_admission();
    wake::wake(e.clone(), p.clone(), wake::Wake::Approved, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    let version = e.proj().goals[&GoalId::new("g")].version;
    open(&e, &p, "hire", LapsesOn::GoalVersion(version));
    dispatch::run_ended(&e, &first.run, EndReason::Completed)
        .await
        .unwrap();
    assert_eq!(e.proj().live_runs().count(), 0);
    delayed.await.unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(e.proj().runs.len(), 1);
    assert_eq!(e.proj().questions.len(), 1);
    assert!(e.proj().questions[&QuestionId::new("hire")].view.open);
    assert!(
        e.proj()
            .goal_for_wake(&p, Some(wake::Wake::Approved), &e.pool)
            .is_none()
    );

    // Suppressing approval wakes must not suppress the owner's reply.
    let (status, _) = request(&e, "/questions/hire/answer", Some(json!({"choice": 1}))).await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 2);
    assert_eq!(e.proj().runs.len(), 2);
    assert_eq!(e.proj().questions.len(), 1);
    assert!(!e.proj().questions[&QuestionId::new("hire")].view.open);
    let answering = launcher.units.lock().unwrap()[1].clone();
    assert_eq!(e.proj().runs[&answering.run].goal_version, Some(version));
    let answering_prompt = prompt(&e, &answering);
    assert_eq!(
        answering_prompt
            .lines()
            .filter(|line| {
                *line == "- What next? [goal: First goal] -> Repair the existing task (Repair)"
            })
            .count(),
        1,
        "{answering_prompt}"
    );
}

#[tokio::test]
async fn a_new_approved_wake_persists_its_missing_hire_goal_and_prompt_across_restart() {
    let (_dir, e, launcher, p) = fixture().await;
    plan_only(&e, &p, "g");
    state_goal(&e, &p, "g2", "Frontend work");
    let frontend = dvb_core::RoleId::new("frontend");
    let mut hire = e.proj().projects[&p].hire(&frontend, None).unwrap().clone();
    hire.status = dvb_core::roles::HireStatus::Retired;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::HireRetired(hire),
            EventKind::PlanProposed {
                goal: GoalId::new("g2"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: dvb_core::TaskId::new("t2"),
                    title: "Frontend work".into(),
                    role: frontend,
                    review: dvb_core::roles::ReviewKind::Code,
                    summary: "One edit".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g2"),
            },
        ])
    })
    .unwrap();
    wake::wake(e.clone(), p.clone(), wake::Wake::Approved, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    let version = e.proj().goals[&GoalId::new("g2")].version;
    let text = prompt(&e, &unit);
    assert!(
        text.contains(&format!("{}: Frontend work", fragment("bound-goal"))),
        "{text}"
    );
    assert_eq!(
        e.proj().runs[&unit.run].assignment.goal_version,
        Some(version)
    );
    let cfg = e.cfg.clone();
    drop(e);
    let e = Engine::open(
        cfg,
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
            launcher: Some(launcher.clone()),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(e.proj().runs[&unit.run].goal_version, Some(version));
    assert_eq!(
        e.proj().runs[&unit.run].assignment.goal_version,
        Some(version)
    );
    assert_eq!(prompt(&e, &unit), text);
}

#[tokio::test]
async fn a_delayed_planning_wake_rechecks_the_orchestrators_open_question_at_admission() {
    let forge = Arc::new(support::InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::local_git(),
    )));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *forge.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        forge: Some(forge),
        ..Default::default()
    })
    .await;
    let delayed = tokio::spawn(wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0));
    // The delayed wake passed the initial eligibility check and is fetching.
    barrier.wait().await;
    let version = e.proj().goals[&GoalId::new("g")].version;
    let role = e.proj().projects[&p]
        .role_by_slug("orchestrator")
        .unwrap()
        .clone();
    let hire = e.proj().projects[&p].hire(&role.id, None).unwrap().clone();
    let run = dvb_core::RunId::new("planner-that-asked");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::new(run.clone())
                .goal_version(Some(version))
                .role(role.id)
                .hire(hire.id)
                .model(hire.model)
                .effort(hire.effort)
                .reason("A goal was stated.".into())
                .build(),
        )])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Run(run.clone()), |_, _| {
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("grammar"),
            subject: "Which grammar?".into(),
            format: QuestionFormat::Simple,
            options: vec![QuestionOption {
                id: 1,
                text: "Current".into(),
                effect: "Keep it".into(),
                approves_plan: false,
                no_effect: false,
                retries_task: None,
            }],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(version),
        }])
    })
    .unwrap();
    dispatch::run_ended(&e, &run, EndReason::Completed)
        .await
        .unwrap();
    barrier.wait().await;
    delayed.await.unwrap().unwrap();
    assert!(launcher.units.lock().unwrap().is_empty());
    assert_eq!(e.proj().runs.len(), 1);
    assert!(e.proj().questions[&QuestionId::new("grammar")].view.open);
    assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
    let (status, _) = request(
        &e,
        "/questions/grammar/answer",
        Some(json!({"reply": "Use Unicode grammar"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let answering = launcher.units.lock().unwrap()[0].clone();
    assert_eq!(e.proj().runs[&answering.run].goal_version, Some(version));
    assert!(
        prompt(&e, &answering).contains("Which grammar? [goal: First goal] -> Use Unicode grammar")
    );
    assert_eq!(e.proj().runs.len(), 2);
}

#[tokio::test]
async fn a_delivered_planner_answer_does_not_release_the_next_empty_completion() {
    let (_dir, e, launcher, p) = fixture().await;
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let first = launcher.units.lock().unwrap()[0].clone();
    let version = e.proj().goals[&GoalId::new("g")].version;
    e.append_checked(Some(&p), Actor::Run(first.run.clone()), |_, _| {
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("grammar"),
            subject: "Which grammar?".into(),
            format: QuestionFormat::Simple,
            options: vec![QuestionOption {
                id: 1,
                text: "Current".into(),
                effect: "Keep it".into(),
                approves_plan: false,
                no_effect: false,
                retries_task: None,
            }],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(version),
        }])
    })
    .unwrap();
    dispatch::run_ended(&e, &first.run, EndReason::Completed)
        .await
        .unwrap();
    let (status, _) = request(
        &e,
        "/questions/grammar/answer",
        Some(json!({"reply": "Use Unicode grammar"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let answering = launcher.units.lock().unwrap()[1].clone();
    assert!(
        prompt(&e, &answering).contains("Which grammar? [goal: First goal] -> Use Unicode grammar")
    );
    dispatch::run_ended(&e, &answering.run, EndReason::Completed)
        .await
        .unwrap();
    assert!(e.proj().goals[&GoalId::new("g")].planning_held);
    for wake in [wake::Wake::Answer, wake::Wake::Goal, wake::Wake::Approved] {
        wake::wake(e.clone(), p.clone(), wake, 0, 0).await.unwrap();
    }
    wake::wake_if_missed(&e, &p);
    assert_eq!(e.proj().runs.len(), 2);
    assert_eq!(launcher.units.lock().unwrap().len(), 2);
    assert!(
        e.proj()
            .questions
            .values()
            .any(|q| q.view.open && q.planning_run.is_some())
    );
}

#[tokio::test]
async fn invalid_choices_are_recorded_as_refusals_for_ordinary_and_planning_questions() {
    let (_dir, e, launcher, p) = fixture().await;
    let version = e.proj().goals[&GoalId::new("g")].version;
    open(&e, &p, "ordinary", LapsesOn::GoalVersion(version));
    let before = e.store.last_id().unwrap();
    let (status, body) = request(
        &e,
        "/questions/ordinary/answer",
        Some(json!({"choice": 99})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "INVALID_VALUE");
    assert!(e.proj().questions[&QuestionId::new("ordinary")].view.open);
    let events = e.store.after(before).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0].kind, EventKind::CommandRefused(r) if r.command == "answer"));

    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let run = launcher.units.lock().unwrap()[0].run.clone();
    dispatch::run_ended(&e, &run, EndReason::Completed)
        .await
        .unwrap();
    let question = e
        .proj()
        .questions
        .values()
        .find(|q| q.view.open && q.planning_run.is_some())
        .unwrap()
        .view
        .id
        .clone();
    for answer in [json!({"choice": 3}), json!({"reply": "Plan it again"})] {
        let before = e.store.last_id().unwrap();
        let (status, body) =
            request(&e, &format!("/questions/{question}/answer"), Some(answer)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "INVALID_VALUE");
        let events = e.store.after(before).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0].kind, EventKind::CommandRefused(r) if r.command == "answer"));
        assert!(e.proj().questions[&question].view.open);
        assert!(e.proj().goals[&GoalId::new("g")].planning_held);
    }
}

// Paused timers exercise the same delayed wake as failed launches, without
// waiting in real time or contacting a remote forge/model.
async fn fetch_fixture() -> (
    tempfile::TempDir,
    AppState,
    Arc<Started>,
    ProjectId,
    Arc<support::InterruptedForge>,
) {
    let forge = Arc::new(support::InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::local_git(),
    )));
    let (dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        forge: Some(forge.clone()),
        ..Default::default()
    })
    .await;
    (dir, e, launcher, p, forge)
}

fn fetches(forge: &support::InterruptedForge) -> usize {
    forge
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| **c == "fetch")
        .count()
}

fn pending_reply(e: &AppState, p: &ProjectId, late_empty_end: bool) {
    let version = e.proj().goals[&GoalId::new("g")].version;
    let a = fake_orchestrator("question-run", Some(version));
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(a.clone())])
    })
    .unwrap();
    e.append_checked(Some(p), Actor::Run(a.run.clone()), |_, _| {
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("fetch-reply"),
            subject: "Which parser?".into(),
            format: QuestionFormat::Simple,
            options: vec![],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(version),
        }])
    })
    .unwrap();
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: a.run,
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        }])
    })
    .unwrap();
    let successor = fake_orchestrator("empty-successor", Some(version));
    if late_empty_end {
        e.append_checked(Some(p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(successor.clone())])
        })
        .unwrap();
    }
    e.append_checked(Some(p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question: QuestionId::new("fetch-reply"),
            answer: Answer::Reply("Use the streaming parser".into()),
        }])
    })
    .unwrap();
    if late_empty_end {
        e.append_checked(Some(p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunEnded {
                run: successor.run,
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            }])
        })
        .unwrap();
    }
}

fn fake_receives_prompt(e: &AppState, unit: &UnitId) -> String {
    use dvb_core::provider::{Provider, RunItem};
    let dir = e.cfg.paths.work.join(unit.run.as_str());
    let text = prompt(e, unit);
    let script = dir.join("test-fake.json");
    std::fs::write(&script, r#"[{"exit":0}]"#).unwrap();
    let fake = dvb_run::fake::Fake {
        script,
        dvb: "unused-dvb".into(),
        stderr: dvb_run::StderrTail::default(),
    };
    let mut session = fake
        .start(&dir, &dir, &text, "fake-1", dvb_core::Effort::Medium)
        .unwrap();
    assert!(matches!(session.next(), Some(RunItem::ModelServed(_))));
    assert!(matches!(session.next(), Some(RunItem::Ended(_))));
    text
}

async fn fetch_backoff(failed: u8) {
    // Let the spawned delayed wake register its timer before advancing it.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(
        dvb_core::state::backoff_millis(failed, 2_000, 300_000).unsigned_abs(),
    ))
    .await;
}

#[tokio::test(start_paused = true)]
async fn retryable_wake_fetch_delivers_the_actual_pending_owner_answer() {
    let (_dir, e, launcher, p, forge) = fetch_fixture().await;
    pending_reply(&e, &p, false);
    forge.fetch_fails_once.store(true, Ordering::SeqCst);
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(fetches(&forge), 1);
    assert!(launcher.units.lock().unwrap().is_empty());
    assert_eq!(e.proj().runs.len(), 1); // Only the question's original planner.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(
        dvb_core::state::backoff_millis(1, 2_000, 300_000).unsigned_abs() - 1,
    ))
    .await;
    assert_eq!(fetches(&forge), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for(&launcher.ready).await;
    assert_eq!(fetches(&forge), 2);
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(
        fake_receives_prompt(&e, &unit)
            .contains("Which parser? [goal: First goal] -> Use the streaming parser")
    );
    assert_eq!(
        e.proj().runs[&unit.run].assignment.reason,
        "The owner answered a question."
    );
    wake::wake(e.clone(), p, wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
}

fn assert_fetch_hold(e: &AppState, detail: &str) -> QuestionId {
    let proj = e.proj();
    assert!(proj.goals[&GoalId::new("g")].planning_held);
    let questions: Vec<_> = proj.questions.values().filter(|q| q.view.open).collect();
    assert_eq!(questions.len(), 1);
    assert_eq!(
        proj.questions
            .values()
            .filter(|q| q.planning_run.is_some())
            .count(),
        1
    );
    let q = questions[0];
    assert!(
        matches!(&q.kind, dvb_core::event::QuestionKind::PlanningHold { attempt, fetch: true }
        if q.planning_run.as_ref() == Some(attempt))
    );

    assert!(q.view.subject.contains(detail), "{}", q.view.subject);
    assert!(q.planning_run.is_some());
    assert_eq!(
        q.view
            .options
            .iter()
            .map(|o| o.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Plan it again", "Drop the goal"]
    );
    q.view.id.clone()
}

#[tokio::test(start_paused = true)]
async fn rate_limited_planning_fetch_and_gate_waits_keep_the_answer_and_attempts() {
    use dvb_core::forge::{ForgeError, RateLimit};
    use dvb_core::{Clock, FakeClock, Timestamp};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let forge = Arc::new(dvb_forge::FakeForge::local_git().with_clock(clock.clone()));
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        clock: Some(clock.clone()),
        forge: Some(forge.clone()),
        ..Default::default()
    })
    .await;
    pending_reply(&e, &p, false);
    let reset = clock.now().plus_millis(3_600_000);
    forge.fail_next(
        "fetch",
        ForgeError {
            retryable: true,
            unreachable: false,
            detail: "GitHub HTTP 403 (rate limit)".into(),
            rate_limit: Some(RateLimit { reset: Some(reset) }),
        },
    );
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    // Other wakes encounter the actual shared forge gate. They must neither
    // consume attempts nor cause the ordinary 2s/4s retry to hold planning.
    for elapsed in [2_000, 4_000] {
        tokio::task::yield_now().await;
        clock.advance_millis(elapsed);
        tokio::time::advance(Duration::from_millis(elapsed as u64)).await;
        wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
            .await
            .unwrap();
        assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
        assert!(e.proj().questions.values().all(|q| !q.view.open));
        assert_eq!(forge.calls(), vec!["fetch"]);
    }
    tokio::task::yield_now().await;
    let at = reset.plus_millis(1_000);
    let wait = at.millis() - clock.now().millis();
    clock.set(at.plus_millis(-1));
    tokio::time::advance(Duration::from_millis((wait - 1) as u64)).await;
    assert!(launcher.units.lock().unwrap().is_empty());
    assert_eq!(forge.calls(), vec!["fetch"]);
    clock.set(at);
    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for(&launcher.ready).await;
    let units = launcher.units.lock().unwrap().clone();
    assert_eq!(units.len(), 1);
    assert_eq!(
        forge
            .calls()
            .iter()
            .filter(|op| op.as_str() == "fetch")
            .count(),
        2
    );
    assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
    assert!(
        fake_receives_prompt(&e, &units[0])
            .contains("Which parser? [goal: First goal] -> Use the streaming parser")
    );
}

#[tokio::test(start_paused = true)]
async fn untimed_planning_limits_double_backoff_without_spending_failure_attempts() {
    use dvb_core::forge::{ForgeError, RateLimit};
    use dvb_core::{FakeClock, Timestamp};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let forge = Arc::new(dvb_forge::FakeForge::local_git().with_clock(clock.clone()));
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        clock: Some(clock.clone()),
        forge: Some(forge.clone()),
        ..Default::default()
    })
    .await;
    // Preserve one ordinary failure across all the following budget waits.
    forge.fail_next(
        "fetch",
        ForgeError {
            retryable: true,
            unreachable: false,
            detail: "unavailable".into(),
            rate_limit: None,
        },
    );
    for _ in 0..10 {
        forge.fail_next(
            "fetch",
            ForgeError {
                retryable: true,
                unreachable: false,
                detail: "GitHub HTTP 429 (rate limit)".into(),
                rate_limit: Some(RateLimit { reset: None }),
            },
        );
    }
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    for retry in 1..=11 {
        let delay = dvb_core::state::backoff_millis(retry, 2_000, 300_000);
        tokio::task::yield_now().await;
        clock.advance_millis(delay - 1);
        tokio::time::advance(Duration::from_millis((delay - 1) as u64)).await;
        assert_eq!(forge.calls().len(), usize::from(retry));
        assert!(launcher.units.lock().unwrap().is_empty());
        assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
        clock.advance_millis(1);
        tokio::time::advance(Duration::from_millis(1)).await;
        support::wait::FAST
            .until("fetch retry", || {
                forge.calls().len() == usize::from(retry + 1)
            })
            .await;
    }
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
}

#[tokio::test(start_paused = true)]
async fn repeated_wake_fetch_failures_stop_at_the_cap_with_one_durable_hold() {
    let (_dir, e, launcher, p, forge) = fetch_fixture().await;
    pending_reply(&e, &p, false);
    *forge.fetch_error.lock().unwrap() = Some(dvb_core::forge::ForgeError {
        rate_limit: None,
        retryable: true,
        unreachable: false,
        detail: "fetch transport unavailable".into(),
    });
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    for failed in 1..dvb_core::caps::ATTEMPTS_PER_TASK {
        fetch_backoff(failed).await;
        support::wait::FAST
            .until("next fetch", || fetches(&forge) == usize::from(failed + 1))
            .await;
    }
    let question = assert_fetch_hold(&e, "fetch transport unavailable");
    assert_eq!(
        fetches(&forge),
        usize::from(dvb_core::caps::ATTEMPTS_PER_TASK)
    );
    assert!(launcher.units.lock().unwrap().is_empty());
    let reopened = Engine::open(
        e.cfg.clone(),
        launcher.readings.options(dvb_engine::EngineOptions {
            forge: Some(forge.clone()),
            launcher: Some(launcher.clone()),
            disk_reading: Some(Arc::new(|| Some(u64::MAX))),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(
        assert_fetch_hold(&reopened, "fetch transport unavailable"),
        question
    );
    for _ in 0..3 {
        wake::wake_if_missed(&reopened, &p);
        wake::wake(reopened.clone(), p.clone(), wake::Wake::Answer, 0, 0)
            .await
            .unwrap();
        wake::wake(reopened.clone(), p.clone(), wake::Wake::Goal, 0, 0)
            .await
            .unwrap();
    }
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(
        fetches(&forge),
        usize::from(dvb_core::caps::ATTEMPTS_PER_TASK)
    );
    assert_eq!(
        assert_fetch_hold(&reopened, "fetch transport unavailable"),
        question
    );
    // The hold answer releases one attempt, retaining the original words.
    *forge.fetch_error.lock().unwrap() = None;
    let (status, _) = request(
        &reopened,
        &format!("/questions/{question}/answer"),
        Some(json!({"choice":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(
        fake_receives_prompt(&reopened, &unit)
            .contains("Which parser? [goal: First goal] -> Use the streaming parser")
    );
    assert!(!reopened.proj().questions[&question].view.open);
}

#[tokio::test(start_paused = true)]
async fn non_retryable_wake_fetch_holds_immediately_and_drop_uses_existing_action() {
    let (_dir, e, launcher, p, forge) = fetch_fixture().await;
    *forge.fetch_error.lock().unwrap() = Some(dvb_core::forge::ForgeError {
        rate_limit: None,
        retryable: false,
        unreachable: false,
        detail: "fetch repository refused".into(),
    });
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    assert_eq!(fetches(&forge), 1);
    assert!(e.proj().runs.is_empty());
    let question = assert_fetch_hold(&e, "fetch repository refused");
    wake::wake_if_missed(&e, &p);
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(fetches(&forge), 1);
    let (status, _) = request(
        &e,
        &format!("/questions/{question}/answer"),
        Some(json!({"choice":2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        e.proj().goals[&GoalId::new("g")].state,
        dvb_core::api::GoalState::Dropped
    );
    assert!(!e.proj().questions[&question].view.open);
    assert!(launcher.units.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn successful_wake_fetch_starts_the_planner_without_retry_or_hold() {
    let (_dir, e, launcher, p, forge) = fetch_fixture().await;
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(fake_receives_prompt(&e, &unit).contains("First goal"));
    assert_eq!(
        e.proj().runs[&unit.run].assignment.reason,
        "A goal was stated."
    );
    assert!(e.proj().runs[&unit.run].assignment.base.is_some());
    assert!(!e.proj().goals[&GoalId::new("g")].planning_held);
    assert!(e.proj().questions.is_empty());
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(fetches(&forge), 1);
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn terminal_fetch_replaces_an_empty_hold_released_by_a_pending_reply() {
    let (_dir, e, launcher, p, forge) = fetch_fixture().await;
    // The owner's reply releases its question's planner, but a still-live
    // empty successor completes afterward and imposes its older hold.
    pending_reply(&e, &p, true);
    assert!(e.proj().goals[&GoalId::new("g")].planning_held);
    *forge.fetch_error.lock().unwrap() = Some(dvb_core::forge::ForgeError {
        rate_limit: None,
        retryable: false,
        unreachable: false,
        detail: "fetch access refused after reply".into(),
    });
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    let question = assert_fetch_hold(&e, "fetch access refused after reply");
    assert_eq!(fetches(&forge), 1);
    wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
        .await
        .unwrap();
    assert_eq!(fetches(&forge), 1);
    *forge.fetch_error.lock().unwrap() = None;
    assert_eq!(
        request(
            &e,
            &format!("/questions/{question}/answer"),
            Some(json!({"choice":1}))
        )
        .await
        .0,
        StatusCode::OK
    );
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(
        fake_receives_prompt(&e, &unit)
            .contains("Which parser? [goal: First goal] -> Use the streaming parser")
    );
}

fn memory_notices(e: &AppState) -> usize {
    support::events::all(e)
        .iter()
        .filter(|ev| {
            matches!(
                &ev.kind,
                EventKind::RunOutput {
                    resource: Some(dvb_core::event::ResourceWait::Memory),
                    ..
                }
            )
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn taskless_orchestrator_memory_hold_polls_once_and_keeps_vendor_and_update_guards() {
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        clock: Some(Arc::new(dvb_core::FakeClock::at(dvb_core::Timestamp(
            1_000_000,
        )))),
        ..Default::default()
    })
    .await;
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    *launcher.readings.memory.lock().unwrap() = Some(0);
    e.update.close_admission();
    let wake = tokio::spawn(wake::wake(e.clone(), p, wake::Wake::Goal, 0, 0));
    support::wait::FAST
        .until("orchestrator memory notice", || memory_notices(&e) == 1)
        .await;
    // Resource wait frees into the closed update gate, then memory falls again.
    *launcher.readings.memory.lock().unwrap() = Some(10 * 1024 * 1024 * 1024);
    e.disk_freed.notify_waiters();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    *launcher.readings.memory.lock().unwrap() = Some(0);
    e.update.reopen_admission();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(launcher.units.lock().unwrap().is_empty());
    assert!(e.proj().runs.is_empty());
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(memory_notices(&e), 1);
    dvb_engine::vendors::record(
        &e,
        Vendor::Fake,
        &dvb_core::run_verbs::RateLimitBody {
            used_pct: 100.0,
            window: "test".into(),
            resets_at: Some(dvb_core::Timestamp(e.clock.now().0 + 60_000)),
        },
    )
    .unwrap();
    *launcher.readings.memory.lock().unwrap() = Some(10 * 1024 * 1024 * 1024);
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(launcher.units.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(31)).await;
    wake.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(memory_notices(&e), 1);
}

#[tokio::test(start_paused = true)]
async fn taskless_orchestrator_load_hold_polls_once_and_keeps_vendor_and_update_guards() {
    let (_dir, e, launcher, p) = fixture_with(dvb_engine::EngineOptions {
        clock: Some(Arc::new(dvb_core::FakeClock::at(dvb_core::Timestamp(
            1_000_000,
        )))),
        ..Default::default()
    })
    .await;
    let notices = || {
        support::events::all(&e)
            .iter()
            .filter(|ev| {
                matches!(
                    &ev.kind,
                    EventKind::RunOutput {
                        resource: Some(dvb_core::event::ResourceWait::HostLoad),
                        ..
                    }
                )
            })
            .count()
    };
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    *launcher.readings.load.lock().unwrap() = Some((2.01, 2));
    e.update.close_admission();
    let wake = tokio::spawn(wake::wake(e.clone(), p, wake::Wake::Goal, 0, 0));
    support::wait::FAST
        .until("orchestrator load notice", || notices() == 1)
        .await;
    // Resource wait frees into the closed update gate, then load rises again.
    *launcher.readings.load.lock().unwrap() = Some((1.99, 2));
    e.disk_freed.notify_waiters();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    *launcher.readings.load.lock().unwrap() = Some((2.01, 2));
    e.update.reopen_admission();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(launcher.units.lock().unwrap().is_empty());
    assert!(e.proj().runs.is_empty());
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(notices(), 1);
    dvb_engine::vendors::record(
        &e,
        Vendor::Fake,
        &dvb_core::run_verbs::RateLimitBody {
            used_pct: 100.0,
            window: "test".into(),
            resets_at: Some(dvb_core::Timestamp(e.clock.now().0 + 60_000)),
        },
    )
    .unwrap();
    *launcher.readings.load.lock().unwrap() = Some((1.99, 2));
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(launcher.units.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(31)).await;
    wake.await.unwrap().unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    assert_eq!(notices(), 1);
}

#[tokio::test(start_paused = true)]
async fn taskless_memory_hold_releases_after_an_actual_orchestrator_in_another_project_ends() {
    let (_dir, e, launcher, p) = fixture().await;
    *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let active = launcher.units.lock().unwrap()[0].run.clone();
    let other = ProjectId::new("memory-other");
    let repo = e.proj().projects[&p].view.repo.clone();
    support::wait::io(publish::create_mirror(&repo, &e.mirror("memory-other")))
        .await
        .unwrap();
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds = vec![support::events::project_event(
        &other,
        "memory-other",
        &repo,
    )];
    kinds.extend(roles);
    kinds.extend(hires);
    kinds.push(support::events::goal_event(
        &GoalId::new("memory-goal"),
        "Plan work",
        None,
    ));
    e.append_checked(Some(&other), Actor::Owner, |_, _| Ok(kinds))
        .unwrap();
    *launcher.readings.memory.lock().unwrap() = Some(0);
    let wake = tokio::spawn(wake::wake(e.clone(), other, wake::Wake::Goal, 0, 0));
    support::wait::FAST
        .until("second project's memory notice", || memory_notices(&e) == 1)
        .await;
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    *launcher.readings.memory.lock().unwrap() = Some(10 * 1024 * 1024 * 1024);
    launcher.end(&active);
    // The actual unit watcher records completion and notifies other projects.
    // Ten-second test deadline is shorter than the thirty-second resource poll.
    tokio::time::timeout(Duration::from_secs(10), wake)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    wait_for(&launcher.ready).await;
    assert_eq!(launcher.units.lock().unwrap().len(), 2);
    assert_eq!(memory_notices(&e), 1);
}

#[tokio::test]
async fn ordered_goal_does_not_launch_a_planner_before_its_predecessor_finishes() {
    let (_dir, e, launcher, p) = fixture().await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
            EventKind::ProjectPaused {},
        ])
    })
    .unwrap();
    let (status, body) = request(
        &e,
        "/projects/p/goals",
        Some(json!({
            "text": "Second goal", "after": "g", "from_issue": 248
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let waiting = GoalId::new(body["id"].as_str().unwrap());
    {
        let proj = e.proj();
        for wake in [
            None,
            Some(wake::Wake::Goal),
            Some(wake::Wake::Approved),
            Some(wake::Wake::Answer),
        ] {
            assert!(
                proj.goal_for_wake(&p, wake, &e.pool)
                    .is_none_or(|g| g.id != waiting)
            );
        }
        drop(proj);
    }
    let redirected = owner::redirect(&e, &waiting, "Second goal redirected".into()).unwrap();
    assert_eq!(redirected.waits_for, Some(GoalId::new("g")));
    let replay = dvb_engine::proj::Projections::rebuild(&support::events::all(&e));
    assert_eq!(replay.goals[&waiting].after, Some(GoalId::new("g")));
    assert_eq!(replay.goals[&waiting].issues(), vec![248]);
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    wake::wake(e.clone(), p, wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    assert!(
        e.proj().runs.is_empty(),
        "a waiting goal must not be admitted"
    );
    assert!(
        launcher.units.lock().unwrap().is_empty(),
        "a waiting goal must not launch a planner: {body}"
    );
}

#[tokio::test]
async fn redirect_and_drop_durably_close_only_their_task_questions() {
    for (action, restarting) in [("redirect", false), ("redirect", true), ("drop", false)] {
        let (_dir, e, launcher, p) = fixture().await;
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![
                EventKind::ProjectPaused {},
                EventKind::PlanProposed {
                    goal: GoalId::new("g"),
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: dvb_core::TaskId::new("stopped"),
                        title: "Stopped work".into(),
                        role: dvb_core::RoleId::new("backend"),
                        review: dvb_core::roles::ReviewKind::Code,
                        summary: "x".into(),
                        routine: false,
                    }],
                },
                EventKind::BranchPublished {
                    task: dvb_core::TaskId::new("stopped"),
                    branch: "dvb/kept-1".into(),
                    head: "head".into(),
                    round: dvb_core::caps::REVIEW_ROUNDS_PER_PR,
                },
                EventKind::VerdictRecorded {
                    review_stage: None,
                    verdict: dvb_core::VerdictId::new("v"),
                    task: dvb_core::TaskId::new("stopped"),
                    head: "head".into(),
                    run: dvb_core::RunId::new("review"),
                    role: dvb_core::RoleId::new("code-reviewer"),
                    model: "fake-1".into(),
                    vendor: Vendor::Fake,
                    findings: vec![dvb_core::run_verbs::Finding {
                        severity: dvb_core::run_verbs::Severity::Medium,
                        file: "src/lib.rs".into(),
                        line: None,
                        title: "Open defect".into(),
                        detail: "Needs another fix round".into(),
                        open: true,
                    }],
                    summary: "Changes needed".into(),
                },
                EventKind::TaskStateChanged {
                    task: dvb_core::TaskId::new("stopped"),
                    from: dvb_core::TaskState::Planned,
                    to: dvb_core::TaskState::Stopped {
                        was: Box::new(dvb_core::TaskState::InReview),
                    },
                },
            ])
        })
        .unwrap();
        // ValueVersion questions do not lapse with a goal version. Their task attribution must close them.
        open(&e, &p, "unrelated", LapsesOn::ValueVersion(EventId(0)));
        let mut q = support::events::all(&e)
            .into_iter()
            .find_map(|ev| matches!(ev.kind, EventKind::QuestionOpened { .. }).then_some(ev.kind))
            .unwrap();
        if let EventKind::QuestionOpened {
            task,
            question,
            options,
            recommended,
            ..
        } = &mut q
        {
            *task = Some(dvb_core::TaskId::new("stopped"));
            *question = QuestionId::new("task-question");
            options[0].text = "Leave it stopped".into();
            options[1].approves_plan = false;
            *recommended = 1;
        }
        let mut other_recommendation = q.clone();
        if let EventKind::QuestionOpened {
            question,
            recommended,
            ..
        } = &mut other_recommendation
        {
            *question = QuestionId::new("other-recommendation");
            *recommended = 2;
        }
        let mut historical = q.clone();
        if let EventKind::QuestionOpened {
            task,
            question,
            lapses_on,
            ..
        } = &mut historical
        {
            *task = None;
            *question = QuestionId::new("historical-question");
            *lapses_on = LapsesOn::GoalVersion(e.proj().goals[&GoalId::new("g")].version);
        }
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                q,
                other_recommendation,
                EventKind::TaskStopped {
                    update: false,
                    task: dvb_core::TaskId::new("stopped"),
                    reason: None,
                },
                historical,
            ])
        })
        .unwrap();
        let url = format!("/goals/g/{action}");
        let (status, _) = request(
            &e,
            &url,
            Some(if action == "redirect" {
                json!({"text":"Keep work from the old branch; change direction"})
            } else {
                Value::Null
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events = support::events::all(&e);
        assert!(
            events.iter().any(
                |ev| matches!(&ev.kind, EventKind::QuestionClosed { question, task }
            if question.as_str() == "task-question" && task.as_str() == "stopped")
            ),
            "{action} needs a durable closure"
        );
        assert!(
            events.iter().any(
                |ev| matches!(&ev.kind, EventKind::QuestionClosed { question, .. }
            if question.as_str() == "historical-question")
            ),
            "{action} needs durable historical attribution closure"
        );
        let replay = dvb_engine::proj::Projections::rebuild(&events);
        assert!(
            !replay.questions[&QuestionId::new("task-question")]
                .view
                .open
        );
        assert!(replay.questions[&QuestionId::new("unrelated")].view.open);
        assert!(
            replay.questions[&QuestionId::new("other-recommendation")]
                .view
                .open
        );
        if action == "redirect" {
            if restarting {
                reconcile::mark(&e).unwrap();
                assert_eq!(
                    e.proj().tasks[&dvb_core::TaskId::new("stopped")].view.state,
                    dvb_core::TaskState::Unknown {
                        last: Box::new(dvb_core::TaskState::Stopped {
                            was: Box::new(dvb_core::TaskState::InReview),
                        }),
                    }
                );
            }
            e.append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::ProjectResumed {}])
            })
            .unwrap();
            wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
                .await
                .unwrap();
            wait_for(&launcher.ready).await;
            let unit = launcher.units.lock().unwrap()[0].clone();
            let actual = prompt(&e, &unit);
            let exhausted = fragment("exhausted-task")
                + &render(
                    &fragment("old-branch"),
                    &[("branch", "dvb/kept-1".into())],
                    false,
                );
            assert_eq!(actual.matches(&exhausted).count(), 1, "{actual}");
        }
    }
}

#[tokio::test]
async fn ordered_goal_releases_on_done_or_drop_and_recovers_across_restart() {
    for (dropped, restart_at_release) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let (_dir, e, launcher, p) = fixture().await;
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![
                EventKind::GoalApproved {
                    goal: GoalId::new("g"),
                },
                EventKind::ProjectPaused {},
            ])
        })
        .unwrap();
        let (status, body) = request(
            &e,
            "/projects/p/goals",
            Some(json!({"text":"Ordered second", "after":"g"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["waits_for"], "g");
        let next = GoalId::new(body["id"].as_str().unwrap());
        let cfg = e.cfg.clone();
        drop(e);
        let e = Engine::open(
            cfg.clone(),
            launcher.readings.options(dvb_engine::EngineOptions {
                forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
                launcher: Some(launcher.clone()),
                ..Default::default()
            }),
        )
        .unwrap();
        *launcher.readings.disk.lock().unwrap() = Some(u64::MAX);
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::ProjectResumed {}])
        })
        .unwrap();
        wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
            .await
            .unwrap();
        assert!(launcher.units.lock().unwrap().is_empty());
        assert_eq!(
            e.proj().goals[&next].state,
            dvb_core::api::GoalState::Stated
        );
        // Use the ordinary durable append/recovery path, without another owner action on the waiter.
        if !restart_at_release {
            e.remember();
        }
        if dropped {
            owner::drop_goal(&e, &GoalId::new("g")).unwrap();
        } else {
            done(&e, &p);
        }
        let e = if restart_at_release {
            drop(e);
            let reopened = Engine::open(
                cfg.clone(),
                launcher.readings.options(dvb_engine::EngineOptions {
                    forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
                    launcher: Some(launcher.clone()),
                    ..Default::default()
                }),
            )
            .unwrap();

            reopened.remember();
            wake::wake_if_missed(&reopened, &p);
            reopened
        } else {
            e
        };
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert!(prompt(&e, &unit).contains("Ordered second"));
        let version = e.proj().goals[&next].version;
        assert_eq!(e.proj().runs[&unit.run].goal_version, Some(version));
        assert_eq!(launcher.units.lock().unwrap().len(), 1);
        // A released dependency remains eligible after replay too, with the current live run retained.
        let replay = Engine::open(
            cfg,
            launcher.readings.options(dvb_engine::EngineOptions {
                launcher: Some(launcher.clone()),
                ..Default::default()
            }),
        )
        .unwrap();
        {
            let proj = replay.proj();
            assert!(proj.plan_wake_ready(&proj.goals[&next]));
            drop(proj);
        }
        assert_eq!(replay.proj().runs[&unit.run].goal_version, Some(version));
    }
}

#[tokio::test]
async fn ordered_goals_reject_missing_and_other_project_predecessors() {
    let (_dir, e, _launcher, p) = fixture().await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    e.append_checked(Some(&ProjectId::new("other")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("foreign"),
            text: "Other project".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    for after in ["missing", "foreign"] {
        let (status, body) = request(
            &e,
            "/projects/p/goals",
            Some(json!({"text":"Waiting", "after":after})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "INVALID_VALUE");
    }
    assert_eq!(e.proj().goals.len(), 2);
}

async fn state_after(e: &AppState, text: &str, after: Option<&GoalId>) -> GoalId {
    let (status, body) = request(
        e,
        "/projects/p/goals",
        Some(json!({"text": text, "after": after})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    GoalId::new(body["id"].as_str().unwrap())
}

async fn set_after(e: &AppState, goal: &GoalId, after: Option<&GoalId>) -> (StatusCode, Value) {
    request(
        e,
        &format!("/goals/{goal}/after"),
        Some(json!({ "after": after })),
    )
    .await
}

#[tokio::test]
async fn a_reordered_saved_plan_cannot_be_approved_before_its_new_predecessor_finishes() {
    let (_dir, e, launcher, p) = fixture().await;
    // Keep the first goal busy while the second goal's planner saves its plan.
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalApproved {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    state_goal(&e, &p, "g2", "Second goal");
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let planner = launcher.units.lock().unwrap()[0].clone();
    plan_and_ask(&e, &p, "g2", "q-second");
    let (status, body) = set_after(&e, &GoalId::new("g2"), Some(&GoalId::new("g"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) =
        request(&e, "/questions/q-second/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "VALUE_CHANGED");
    assert!(body["detail"].as_str().unwrap().contains("g"));
    assert!(e.proj().questions[&QuestionId::new("q-second")].view.open);
    dispatch::run_ended(&e, &planner.run, EndReason::Completed)
        .await
        .unwrap();
    dispatch::start_waiting(e.clone()).await.unwrap();
    assert!(
        e.proj().tasks[&dvb_core::TaskId::new("t-g2")]
            .facts
            .run
            .is_none()
    );
    assert_eq!(launcher.units.lock().unwrap().len(), 1);

    done(&e, &p);
    let (status, body) =
        request(&e, "/questions/q-second/answer", Some(json!({"choice": 2}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[1].clone();
    assert_eq!(
        e.proj().runs[&unit.run].assignment.task,
        Some(dvb_core::TaskId::new("t-g2"))
    );
}

#[tokio::test]
async fn a_reordered_plan_reply_cannot_block_its_prerequisites_planner() {
    for restart in [false, true] {
        let (_dir, e, launcher, p) = fixture().await;
        owner::pause(&e, &p, true).unwrap();
        state_goal(&e, &p, "g2", "Second goal");
        plan_and_ask(&e, &p, "g2", "q-second");
        let first_version = e.proj().goals[&GoalId::new("g")].version;
        let second_version = e.proj().goals[&GoalId::new("g2")].version;
        let (status, body) = set_after(&e, &GoalId::new("g2"), Some(&GoalId::new("g"))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let question = e.proj().questions[&QuestionId::new("q-second")]
            .view
            .clone();
        let reply = "Add streaming parser coverage";
        let (status, body) = request(
            &e,
            "/questions/q-second/answer",
            Some(json!({"reply": reply})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "VALUE_CHANGED");
        assert_eq!(
            e.proj().questions[&QuestionId::new("q-second")].view,
            question
        );
        assert!(!support::events::all(&e).iter().any(|ev| {
            matches!(&ev.kind, EventKind::QuestionAnswered { question, .. }
                if question == &QuestionId::new("q-second"))
        }));

        let e = if restart {
            // Resume was committed, but the engine died before giving A its wake.
            e.append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::ProjectResumed {}])
            })
            .unwrap();
            let cfg = e.cfg.clone();
            drop(e);
            let reopened = Engine::open(
                cfg,
                launcher.readings.options(dvb_engine::EngineOptions {
                    forge: Some(Arc::new(dvb_forge::FakeForge::local_git())),
                    launcher: Some(launcher.clone()),
                    ..Default::default()
                }),
            )
            .unwrap();
            reconcile::mark(&reopened).unwrap();
            reconcile::reconcile(reopened.clone()).await.unwrap();
            reopened
        } else {
            owner::pause(&e, &p, false).unwrap();
            e
        };
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert_eq!(goal_of_run(&e, &unit), Some(first_version));
        assert_eq!(
            e.proj().questions[&QuestionId::new("q-second")].view,
            question
        );
        plan_and_ask(&e, &p, "g", "q-first");
        dispatch::run_ended(&e, &unit.run, EndReason::Completed)
            .await
            .unwrap();
        assert_eq!(launcher.units.lock().unwrap().len(), 1);

        done(&e, &p);
        let (status, body) = request(
            &e,
            "/questions/q-second/answer",
            Some(json!({"reply": reply})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[1].clone();
        assert_eq!(goal_of_run(&e, &unit), Some(second_version));
        assert!(prompt(&e, &unit).contains(reply));
    }
}

#[tokio::test]
async fn clearing_a_reordered_saved_plan_recovers_its_missing_approval_question() {
    let (_dir, e, launcher, p) = fixture().await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalApproved {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    state_goal(&e, &p, "g2", "Second goal");
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wait_for(&launcher.ready).await;
    let planner = launcher.units.lock().unwrap()[0].clone();
    plan_only(&e, &p, "g2");
    let (status, body) = set_after(&e, &GoalId::new("g2"), Some(&GoalId::new("g"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    dispatch::run_ended(&e, &planner.run, EndReason::Completed)
        .await
        .unwrap();
    assert!(e.proj().questions.is_empty());
    let (status, body) = set_after(&e, &GoalId::new("g2"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, questions) = request(&e, "/questions?open=true", None).await;
    assert_eq!(status, StatusCode::OK);
    let questions: Vec<QuestionView> = serde_json::from_value(questions).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(launcher.units.lock().unwrap().len(), 1);
    let question = &questions[0];
    let approval = question
        .options
        .iter()
        .find(|o| o.approves_plan)
        .unwrap()
        .id;
    let (status, body) = request(
        &e,
        &format!("/questions/{}/answer", question.id),
        Some(json!({"choice": approval})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for(&launcher.ready).await;
    assert!(script(&e, &launcher.units.lock().unwrap()[1]).contains("t-g2"));
}

#[tokio::test]
async fn a_reordered_chain_does_not_start_the_third_goal_when_the_middle_one_is_dropped() {
    let (_dir, e, launcher, p) = fixture().await;
    let first = GoalId::new("g");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalApproved {
                goal: first.clone(),
            },
            EventKind::ProjectPaused {},
        ])
    })
    .unwrap();
    let second = state_after(&e, "Second goal", Some(&first)).await;
    let third = state_after(&e, "Third goal", Some(&second)).await;
    let (status, body) = set_after(&e, &third, Some(&first)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["waits_for"], "g");
    owner::drop_goal(&e, &second).unwrap();
    {
        let proj = e.proj();
        assert_eq!(proj.waits_for(&proj.goals[&third]), Some(&first));
        drop(proj);
    }
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    wake::wake(e.clone(), p.clone(), wake::Wake::GoalDone, 0, 0)
        .await
        .unwrap();
    assert!(
        launcher.units.lock().unwrap().is_empty(),
        "the third goal must wait for the first"
    );
    let events = support::events::all(&e);
    let set = events
        .iter()
        .find(|ev| matches!(ev.kind, EventKind::GoalAfterSet { .. }))
        .unwrap();
    assert_eq!(
        set.kind,
        EventKind::GoalAfterSet {
            goal: third.clone(),
            after: Some(first.clone()),
        }
    );
    assert_eq!(
        dvb_core::wording::event_text(&set.kind, |t| t.0.to_string()).unwrap(),
        format!("Goal {third} now follows goal g")
    );
    let replay = dvb_engine::proj::Projections::rebuild(&events);
    assert_eq!(replay.goals[&third].after, Some(first));
    e.remember();
    done(&e, &p);
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(prompt(&e, &unit).contains("Third goal"));
    let version = e.proj().goals[&third].version;
    assert_eq!(e.proj().runs[&unit.run].goal_version, Some(version));
}

#[tokio::test]
async fn none_clears_a_goal_order_and_plans_the_goal() {
    let (_dir, e, launcher, p) = fixture().await;
    let first = GoalId::new("g");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalApproved {
                goal: first.clone(),
            },
            EventKind::ProjectPaused {},
        ])
    })
    .unwrap();
    let next = state_after(&e, "Unordered now", Some(&first)).await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    e.remember();
    let (status, body) = set_after(&e, &next, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("waits_for").is_none(), "{body}");
    assert_eq!(e.proj().goals[&next].after, None);
    wait_for(&launcher.ready).await;
    let unit = launcher.units.lock().unwrap()[0].clone();
    assert!(prompt(&e, &unit).contains("Unordered now"));
    // Setting the same order again records nothing.
    let before = support::events::all(&e).len();
    let (status, _) = set_after(&e, &next, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(support::events::all(&e).len(), before);
}

#[tokio::test]
async fn goal_order_changes_are_refused_with_codes() {
    let (_dir, e, _launcher, p) = fixture().await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    e.append_checked(Some(&ProjectId::new("other")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("foreign"),
            text: "Other project".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let first = GoalId::new("g");
    let second = state_after(&e, "Second", Some(&first)).await;
    let third = state_after(&e, "Third", Some(&second)).await;
    let started = state_after(&e, "Started", None).await;
    let approved = state_after(&e, "Approved", None).await;
    let finished = state_after(&e, "Finished", None).await;
    let dropped = state_after(&e, "Dropped", None).await;
    let task = dvb_core::TaskId::new("running");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: started.clone(),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Work".into(),
                    role: dvb_core::RoleId::new("backend"),
                    review: dvb_core::roles::ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                }],
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: dvb_core::TaskState::Planned,
                to: dvb_core::TaskState::Running,
            },
            EventKind::GoalApproved {
                goal: approved.clone(),
            },
            EventKind::GoalDone {
                goal: finished.clone(),
            },
            EventKind::GoalDropped {
                goal: dropped.clone(),
            },
        ])
    })
    .unwrap();
    let missing = GoalId::new("missing");
    let foreign = GoalId::new("foreign");
    for (goal, after, code) in [
        (&missing, Some(&first), "NOT_FOUND"),
        (&third, Some(&missing), "INVALID_VALUE"),
        (&third, Some(&foreign), "INVALID_VALUE"),
        (&started, None, "GOAL_STARTED"),
        (&approved, None, "GOAL_STARTED"),
        (&finished, None, "ALREADY_TERMINAL"),
        (&dropped, None, "ALREADY_TERMINAL"),
        (&third, Some(&third), "GOAL_ORDER_CYCLE"),
        (&first, Some(&third), "GOAL_ORDER_CYCLE"),
        (&second, Some(&third), "GOAL_ORDER_CYCLE"),
    ] {
        let (status, body) = set_after(&e, goal, after).await;
        assert_eq!(body["code"], code, "{goal} after {after:?}: {body}");
        let expected = if code == "NOT_FOUND" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(status, expected, "{body}");
    }
    let events = support::events::all(&e);
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::GoalAfterSet { .. }))
    );
    let refused = events
        .iter()
        .filter(|ev| matches!(&ev.kind, EventKind::CommandRefused(r) if r.command == "after"))
        .count();
    assert_eq!(refused, 9);
}

/// What keeps goal Y's pending task recovery from taking a wake, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryGoal {
    Free,
    Held,
    Asking,
}

#[tokio::test]
async fn stopped_task_recovery_wake_names_the_task_and_its_problem() {
    use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
    for (outcome, expected) in [
        (
            BuilderOutcome::Blocked {
                prerequisite: "engine API".into(),
            },
            "Task recovery: Build it stopped: blocked on engine API".to_owned(),
        ),
        (
            BuilderOutcome::Partial {
                reason: PartialReason::Other,
                detail: "Needs another tool".into(),
            },
            "Task recovery: Build it stopped: partial (Needs another tool)".to_owned(),
        ),
        (
            BuilderOutcome::Partial {
                reason: PartialReason::Other,
                detail: "é".repeat(200),
            },
            format!(
                "Task recovery: Build it stopped: partial ({}…)",
                "é".repeat(160)
            ),
        ),
        (
            BuilderOutcome::Partial {
                reason: PartialReason::OutOfRole,
                detail: "Needs engine API".into(),
            },
            "Task recovery: Build it stopped: out of its role".to_owned(),
        ),
    ] {
        let (_dir, e, launcher, p) = fixture().await;
        let goal = GoalId::new("g");
        let task = dvb_core::TaskId::new("t");
        let builder = support::assignment::Assignment::new(dvb_core::RunId::new("builder"))
            .task(Some(task.clone()))
            .build();
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::PlanProposed {
                    goal: goal.clone(),
                    tasks: vec![PlannedTask {
                        task: task.clone(),
                        title: "Build it".into(),
                        role: dvb_core::RoleId::new("backend"),
                        review: dvb_core::roles::ReviewKind::Code,
                        summary: "One edit".into(),
                        after: vec![],
                        routine: false,
                    }],
                },
                EventKind::GoalApproved { goal: goal.clone() },
                EventKind::RunStarted(builder.clone()),
                EventKind::RunCollected {
                    run: builder.run.clone(),
                    head: None,
                    summary: None,
                    outcome: Some(outcome.clone()),
                },
                EventKind::RunEnded {
                    run: builder.run.clone(),
                    reason: EndReason::Completed,
                    detail: None,
                    stderr: None,
                },
                EventKind::TaskStateChanged {
                    task: task.clone(),
                    from: dvb_core::TaskState::Running,
                    to: dvb_core::TaskState::Stopped {
                        was: Box::new(dvb_core::TaskState::Publishing),
                    },
                },
            ])
        })
        .unwrap();
        wake::wake_if_missed(&e, &p);
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        let replay = dvb_engine::proj::Projections::rebuild(&support::events::all(&e));
        let run = &replay.runs[&unit.run];
        assert_eq!(run.assignment.reason, expected);
        assert_eq!(run.wake, Some(wake::Wake::Approved));
        assert_eq!(run.goal_version, Some(replay.goals[&goal].version));
        let script: Value = serde_json::from_str(
            &std::fs::read_to_string(
                e.cfg
                    .paths
                    .work
                    .join(unit.run.as_str())
                    .join("fake-script.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(script[0]["run"][0], "task start");
    }
}

#[tokio::test]
async fn prerequisite_recovery_wake_names_the_waiting_task_and_prerequisite() {
    for (state, status) in [
        (
            dvb_core::TaskState::Failed {
                reason: "CI failed".into(),
            },
            "failed",
        ),
        (
            dvb_core::TaskState::Stopped {
                was: Box::new(dvb_core::TaskState::Running),
            },
            "stopped",
        ),
    ] {
        let (_dir, e, launcher, p) = fixture().await;
        let goal = GoalId::new("g");
        let prerequisite = dvb_core::TaskId::new("api");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::PlanProposed {
                    goal: goal.clone(),
                    tasks: vec![PlannedTask {
                        task: prerequisite.clone(),
                        title: "Build it".into(),
                        role: dvb_core::RoleId::new("backend"),
                        review: dvb_core::roles::ReviewKind::Code,
                        summary: "One edit".into(),
                        after: vec![],
                        routine: false,
                    }],
                },
                EventKind::PlanProposed {
                    goal: goal.clone(),
                    tasks: vec![PlannedTask {
                        task: dvb_core::TaskId::new("caller"),
                        title: "Use the API".into(),
                        role: dvb_core::RoleId::new("backend"),
                        review: dvb_core::roles::ReviewKind::Code,
                        summary: "Implement caller".into(),
                        after: vec![prerequisite.clone()],
                        routine: false,
                    }],
                },
                EventKind::GoalApproved { goal: goal.clone() },
                EventKind::TaskStateChanged {
                    task: prerequisite.clone(),
                    from: dvb_core::TaskState::Running,
                    to: state.clone(),
                },
            ])
        })
        .unwrap();
        wake::wake_if_missed(&e, &p);
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert_eq!(
            e.proj().runs[&unit.run].assignment.reason,
            format!("Task recovery: Use the API is waiting: its prerequisite Build it {status}")
        );
    }
}

#[tokio::test]
async fn an_answer_on_one_goal_is_acted_on_while_another_goal_has_a_pending_recovery() {
    for (y, approval) in [RecoveryGoal::Free, RecoveryGoal::Held, RecoveryGoal::Asking]
        .into_iter()
        .flat_map(|y| [false, true].map(|approval| (y, approval)))
    {
        let (_dir, e, launcher, p) = fixture().await;
        let (gy, gx, ty) = (
            GoalId::new("g"),
            GoalId::new("g2"),
            dvb_core::TaskId::new("ty"),
        );
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                support::events::backend_plan_event(&gy, &ty),
                EventKind::GoalApproved { goal: gy.clone() },
            ])
        })
        .unwrap();
        let vy = e.proj().goals[&gy].version;
        let question = |id: &str, kind| EventKind::QuestionOpened {
            kind: Some(kind),
            main_ci: None,
            task: None,
            question: QuestionId::new(id),
            subject: "About the first goal".into(),
            format: QuestionFormat::Simple,
            options: vec![QuestionOption::owner(1, "Go on", "Go on")],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(vy),
        };
        match y {
            RecoveryGoal::Free => {}
            RecoveryGoal::Held => e
                .append_checked(Some(&p), Actor::Engine, |_, _| {
                    Ok(vec![question(
                        "held",
                        dvb_core::event::QuestionKind::PlanningHold {
                            attempt: dvb_core::RunId::new("fetch-1"),
                            fetch: true,
                        },
                    )])
                })
                .map(drop)
                .unwrap(),
            RecoveryGoal::Asking => {
                let asker = fake_orchestrator("asker", Some(vy));
                e.append_checked(Some(&p), Actor::Engine, |_, _| {
                    Ok(vec![EventKind::RunStarted(asker.clone())])
                })
                .unwrap();
                e.append_checked(Some(&p), Actor::Run(asker.run.clone()), |_, _| {
                    Ok(vec![question(
                        "asking",
                        dvb_core::event::QuestionKind::Ordinary,
                    )])
                })
                .unwrap();
                e.append_checked(Some(&p), Actor::Engine, |_, _| {
                    Ok(vec![EventKind::RunEnded {
                        run: asker.run.clone(),
                        reason: EndReason::Completed,
                        detail: None,
                        stderr: None,
                    }])
                })
                .unwrap();
            }
        }
        // The first goal's builder then stops out of role: its recovery is pending.
        let builder = support::assignment::Assignment::new(dvb_core::RunId::new("by"))
            .task(Some(ty.clone()))
            .build();
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::RunStarted(builder.clone()),
                EventKind::RunCollected {
                    run: builder.run.clone(),
                    head: None,
                    summary: None,
                    outcome: Some(dvb_core::run_verbs::BuilderOutcome::Partial {
                        reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                        detail: "Needs engine API".into(),
                    }),
                },
                EventKind::RunEnded {
                    run: builder.run.clone(),
                    reason: EndReason::Completed,
                    detail: None,
                    stderr: None,
                },
                EventKind::TaskStateChanged {
                    task: ty.clone(),
                    from: dvb_core::TaskState::Planned,
                    to: dvb_core::TaskState::Stopped {
                        was: Box::new(dvb_core::TaskState::Publishing),
                    },
                },
                EventKind::GoalStated {
                    goal: gx.clone(),
                    text: "Second goal".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
            ])
        })
        .unwrap();
        let vx = e.proj().goals[&gx].version;
        open(&e, &p, "qx", LapsesOn::GoalVersion(vx));
        let (status, _) = request(
            &e,
            "/questions/qx/answer",
            Some(if approval {
                json!({"choice": 2})
            } else {
                json!({"reply": "Second goal first"})
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wait_for(&launcher.ready).await;
        let unit = launcher.units.lock().unwrap()[0].clone();
        assert_eq!(e.proj().runs[&unit.run].goal_version, Some(vx), "{y:?}");
        let text = prompt(&e, &unit);
        let reply = if approval {
            "Yes, start"
        } else {
            "Second goal first"
        };
        assert!(
            text.contains(&format!("What next? [goal: Second goal] -> {reply}")),
            "{text}"
        );
        if approval {
            assert_eq!(
                e.proj().runs[&unit.run].assignment.reason,
                "The owner approved the plan."
            );
        }
        if y == RecoveryGoal::Free {
            // The recovery is then handled for its own goal.
            dispatch::run_ended(&e, &unit.run, EndReason::Completed)
                .await
                .unwrap();
            wait_for(&launcher.ready).await;
            let next = launcher.units.lock().unwrap()[1].clone();
            assert_eq!(e.proj().runs[&next.run].goal_version, Some(vy));
        }
    }
}

/// The what-next skip records the answer and closes the question without a
/// run; a reply in words wakes the orchestrator with it.
#[tokio::test]
async fn a_no_effect_answer_wakes_no_orchestrator_and_a_reply_in_words_does() {
    use dvb_core::run_verbs::{AskBody, AskOption, AskReply};
    for (answer, wakes) in [
        (Answer::Choice(2), false),
        (Answer::Reply("Next: show the version".into()), true),
    ] {
        let (_dir, e, launcher, p) = fixture().await;
        done(&e, &p);
        wake::wake(e.clone(), p.clone(), wake::Wake::GoalDone, 0, 0)
            .await
            .unwrap();
        wait_for(&launcher.ready).await;
        let asking = launcher.units.lock().unwrap()[0].clone();
        let option = |text: &str, no_effect| AskOption {
            text: text.into(),
            effect: text.into(),
            approves_plan: false,
            no_effect,
            retries_task: None,
        };
        let caller = dvb_engine::serve::Caller {
            run: e.proj().runs[&asking.run].clone(),
        };
        let question = support::http::run::<AskReply, _>(
            "ask",
            axum::extract::State(e.clone()),
            caller,
            axum::Json(AskBody {
                subject: "What next?".into(),
                options: vec![option("Work on #12", false), option("Skip", true)],
                recommended: 1,
                format: None,
                issues: vec![],
            }),
        )
        .await
        .unwrap()
        .0
        .question;
        dispatch::run_ended(&e, &asking.run, EndReason::Completed)
            .await
            .unwrap();
        let (status, view) = request(
            &e,
            &format!("/questions/{question}/answer"),
            Some(serde_json::to_value(&answer).unwrap()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let view: QuestionView = serde_json::from_value(view).unwrap();
        assert_eq!(view.answer, Some(answer));
        assert!(!view.open);
        if wakes {
            wait_for(&launcher.ready).await;
            let unit = launcher.units.lock().unwrap()[1].clone();
            assert_eq!(
                e.proj().runs[&unit.run].assignment.reason,
                "The owner answered a question."
            );
        }
        // Nothing is left to deliver later either.
        wake::wake(e.clone(), p.clone(), wake::Wake::Answer, 0, 0)
            .await
            .unwrap();
        assert_eq!(e.proj().runs.len(), 1 + usize::from(wakes));
    }
}

/// A planned task with a dependent, approved and waiting as the engine
/// leaves it at approval.
fn prerequisite_and_dependent(e: &AppState, p: &ProjectId) {
    let task = |id: &str, after: Vec<dvb_core::TaskId>| PlannedTask {
        task: dvb_core::TaskId::new(id),
        title: id.into(),
        role: dvb_core::RoleId::new("backend"),
        summary: id.into(),
        review: dvb_core::roles::ReviewKind::Code,
        after,
        routine: false,
    };
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![
                    task("t1", vec![]),
                    task("t2", vec![dvb_core::TaskId::new("t1")]),
                ],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
            EventKind::TaskWaiting {
                task: dvb_core::TaskId::new("t2"),
                model: None,
                effort: None,
                reason: "The owner approved the plan.".into(),
                routine: false,
            },
        ])
    })
    .unwrap();
}

/// An owner's stop with a reason is recorded and wakes no one; without a
/// reason, its dependent's goal wakes the orchestrator as before.
#[tokio::test]
async fn a_stop_with_a_reason_wakes_no_orchestrator_and_one_without_does() {
    for reason in [Some("Hold until #442 merges"), None] {
        let (_dir, e, launcher, p) = fixture().await;
        prerequisite_and_dependent(&e, &p);
        let (status, _) = request(
            &e,
            "/tasks/t1/stop",
            Some(reason.map_or(json!(null), |r| json!({ "reason": r }))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let recorded = support::events::all(&e).into_iter().find_map(|ev| {
            if let EventKind::TaskStopped { reason, .. } = ev.kind {
                Some(reason)
            } else {
                None
            }
        });
        assert_eq!(recorded, Some(reason.map(str::to_owned)));
        if reason.is_none() {
            wait_for(&launcher.ready).await;
        }
        wake::wake(e.clone(), p.clone(), wake::Wake::Approved, 0, 0)
            .await
            .unwrap();
        let runs: Vec<_> = e.proj().runs.values().cloned().collect();
        assert_eq!(runs.len(), usize::from(reason.is_none()));
        assert!(runs.iter().all(|r| r.assignment.is_orchestrator()));
    }
}

/// A skip on one goal after a real reply on another leaves the wake on the
/// reply's goal: the skip has nothing for the run to act on.
#[tokio::test]
async fn a_no_effect_answer_chooses_no_goal_for_the_answer_wake() {
    let (_dir, e, _launcher, p) = fixture().await;
    state_goal(&e, &p, "g2", "Second goal");
    let version = |g: &str| e.proj().goals[&GoalId::new(g)].version;
    let (first, second) = (version("g"), version("g2"));
    open(&e, &p, "reply", LapsesOn::GoalVersion(second));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("skip"),
            subject: "What next?".into(),
            format: QuestionFormat::Simple,
            options: vec![QuestionOption {
                id: 1,
                text: "Skip".into(),
                effect: "The team waits".into(),
                approves_plan: false,
                no_effect: true,
                retries_task: None,
            }],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(first),
        }])
    })
    .unwrap();
    let answer = |id: &str, answer: Answer| {
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::QuestionAnswered {
                question: QuestionId::new(id),
                answer,
            }])
        })
        .unwrap();
    };
    answer("reply", Answer::Reply("Plan the exporter first".into()));
    answer("skip", Answer::Choice(1));
    let goal = (e.proj())
        .goal_for_wake(&p, Some(wake::Wake::Answer), &e.pool)
        .map(|g| g.id.clone());
    assert_eq!(goal, Some(GoalId::new("g2")));
}
