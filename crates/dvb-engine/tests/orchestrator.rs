use dvb_engine::wake;
mod support;
use axum::Json;
use axum::extract::State;
use dvb_core::api::TaskView;
use dvb_core::event::{Answer, LapsesOn};
use dvb_core::run_verbs::{AskBody, HireBody, PlanBody, RetireBody, TaskRetryBody, TaskStartBody};
use dvb_core::{Actor, EventKind, QuestionId, Refusal, RefusalCode, RunId, TaskId};
use dvb_engine::AppState;
use dvb_engine::Fail;
use dvb_engine::serve::Caller;
use support::orchestrator::{
    caller, end_all_runs, fake_repo, fake_run, settings, start_body, started,
    three_tasks_at_the_cap, two_tasks_at_the_cap,
};

use dvb_core::event::QuestionFormat;
use dvb_core::roles::{HireRecord, HireStatus, default_records};
use dvb_core::{Effort, EventId, GoalId, HireId, ProjectId, RoleId, Vendor};
use dvb_engine::Config;
use serde_json::json;

#[tokio::test]
async fn orchestrator_hires_and_retires_with_owner_validation_and_recorded_attribution() {
    let (_dir, e, _c, _) = fake_run();
    let request = || HireBody {
        role: "backend".into(),
        model: "fake-next".into(),
        effort: None,
    };
    let id = support::http::run::<dvb_core::run_verbs::HireReply, _>(
        "hire",
        State(e.clone()),
        caller(&e),
        Json(request()),
    )
    .await
    .unwrap()
    .0
    .hire;
    let record = e.proj().projects[&ProjectId::new("p")]
        .hires
        .iter()
        .find(|h| h.id == id)
        .unwrap()
        .clone();
    assert_eq!(record.role, RoleId::new("backend"));
    assert_eq!(record.model, "fake-next");
    assert_eq!(record.effort, Effort::Medium);
    assert_eq!(
        e.proj().projects[&ProjectId::new("p")]
            .hires
            .iter()
            .filter(|h| h.role == record.role && h.status == HireStatus::Active)
            .count(),
        1
    );
    assert_eq!(
        e.proj().projects[&ProjectId::new("p")]
            .hires
            .iter()
            .find(|h| h.id == HireId::new("h-backend"))
            .unwrap()
            .status,
        HireStatus::Retired
    );
    let hired = support::events::all(&e)
        .into_iter()
        .find(|ev| matches!(&ev.kind, EventKind::HireAdded(h) if h.id == id))
        .unwrap();
    assert_eq!(hired.actor, Actor::Run(RunId::new("orchestrator-run")));
    assert_eq!(hired.project, Some(ProjectId::new("p")));
    let retired = support::http::run::<dvb_core::roles::HireRecord, _>(
        "retire",
        State(e.clone()),
        caller(&e),
        Json(RetireBody { hire: id.clone() }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(retired.status, HireStatus::Retired);
    let ev = e.store.after(hired.id).unwrap().pop().unwrap();
    assert_eq!(ev.actor, hired.actor);
    assert_eq!(ev.kind, EventKind::HireRetired(retired.clone()));
    let before = e.store.last_id().unwrap();
    assert_eq!(
        support::http::run::<dvb_core::roles::HireRecord, _>(
            "retire",
            State(e.clone()),
            caller(&e),
            Json(RetireBody { hire: id })
        )
        .await
        .unwrap()
        .0,
        retired
    );
    assert_eq!(before, e.store.last_id().unwrap());

    for (role, model, effort, code) in [
        ("missing", "fake-1", None, RefusalCode::RoleNotAllowed),
        ("backend", "alias", None, RefusalCode::ModelNotInPool),
        (
            "backend",
            "fake-1",
            Some(Effort::Low),
            RefusalCode::ModelNotInPool,
        ),
    ] {
        assert!(
            matches!(support::http::run::<dvb_core::run_verbs::HireReply, _>("hire", State(e.clone()), caller(&e), Json(HireBody { role: role.into(), model: model.into(), effort })).await, Err(Fail::Refused(r)) if r.code == code)
        );
    }
    settings(&e, "model_pool", json!(["fake-1"])).await;
    assert!(
        matches!(support::http::run::<dvb_core::run_verbs::HireReply, _>("hire", State(e.clone()), caller(&e), Json(request())).await, Err(Fail::Refused(r)) if r.code == RefusalCode::ModelNotInPool)
    );
    let mut builder = e.proj().runs[&RunId::new("orchestrator-run")]
        .assignment
        .clone();
    builder.run = RunId::new("builder");
    builder.role = RoleId::new("backend");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(builder)])
    })
    .unwrap();
    let builder = || Caller {
        run: e.proj().runs[&RunId::new("builder")].clone(),
    };
    assert!(
        matches!(support::http::run::<dvb_core::run_verbs::HireReply, _>("hire", State(e.clone()), builder(), Json(request())).await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
    );
    assert!(
        matches!(support::http::run::<dvb_core::roles::HireRecord, _>("retire", State(e.clone()), builder(), Json(RetireBody { hire: HireId::new("h-backend") })).await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
    );
    let foreign = ProjectId::new("foreign");
    e.append_checked(Some(&foreign), Actor::Owner, |_, _| {
        Ok(vec![
            support::events::project_event(&foreign, "foreign", "local"),
            EventKind::HireAdded(HireRecord {
                id: HireId::new("foreign-hire"),
                ..record
            }),
        ])
    })
    .unwrap();
    assert!(matches!(
        support::http::run::<dvb_core::roles::HireRecord, _>(
            "retire",
            State(e.clone()),
            caller(&e),
            Json(RetireBody {
                hire: HireId::new("foreign-hire")
            })
        )
        .await,
        Err(Fail::NotFound(_))
    ));
    let stale = caller(&e);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("orchestrator-run"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    let before = e.store.last_id().unwrap();
    assert!(
        matches!(support::http::run::<dvb_core::run_verbs::HireReply, _>("hire", State(e.clone()), caller(&e), Json(HireBody {
            role: "backend".into(), model: "fake-1".into(), effort: Some(Effort::High),
        })).await, Err(Fail::Refused(r)) if r.code == RefusalCode::NotARun)
    );
    assert!(
        matches!(support::http::run::<dvb_core::roles::HireRecord, _>("retire", State(e.clone()), stale, Json(RetireBody { hire: HireId::new("h-backend") })).await, Err(Fail::Refused(r)) if r.code == RefusalCode::NotARun)
    );
    assert_eq!(e.store.last_id().unwrap(), before);
}

#[tokio::test]
async fn replay_keeps_historical_and_persisted_asks_on_their_goal_until_redirect_or_drop() {
    // Exercise real run_goal/ask authorization after reopening the store,
    // including a surviving caller whose prompt predates the upgrade.
    for recorded in [false, true] {
        for dropped in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = Config::dev(dir.path(), "unused-dvb".into());
            let e = support::engine::open_config(
                cfg.clone(),
                dvb_engine::EngineOptions {
                    forge: None,
                    launcher: None,
                    ..Default::default()
                },
            );
            let project = ProjectId::new("p");
            let events: Vec<dvb_core::Event> =
                serde_json::from_str(include_str!("fixtures/legacy_orchestrator_binding.json"))
                    .unwrap();
            for ev in events {
                let mut kind = ev.kind;
                if recorded && let EventKind::RunStarted(a) = &mut kind {
                    a.goal_version = Some(e.proj().goals[&GoalId::new("g2")].version);
                }
                e.append_checked(Some(&project), ev.actor, |_, _| Ok(vec![kind]))
                    .unwrap();
            }
            e.append_checked(Some(&project), Actor::Engine, |_, _| {
                Ok(default_records(|r| RoleId::new(r.slug))
                    .into_iter()
                    .map(EventKind::RoleAdded)
                    .collect())
            })
            .unwrap();
            let run = RunId::new("historical");
            let surviving = e.proj().runs[&run].clone();
            let goal = GoalId::new(if recorded { "g2" } else { "g1" });
            let version = e.proj().goals[&goal].version;
            drop(e);
            let e = support::engine::open_config(
                cfg,
                dvb_engine::EngineOptions {
                    forge: None,
                    launcher: None,
                    ..Default::default()
                },
            );
            assert_eq!(e.proj().runs[&run].goal_version, Some(version));
            let body = || AskBody {
                subject: "Hire for this goal?".into(),
                options: vec![],
                recommended: 1,
                format: None,
                issues: vec![],
            };
            let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
                "ask",
                State(e.clone()),
                Caller {
                    run: surviving.clone(),
                },
                Json(body()),
            )
            .await
            .unwrap()
            .0
            .question;
            assert_eq!(
                e.proj().questions[&question].lapses_on,
                LapsesOn::GoalVersion(version)
            );
            let change = if dropped {
                EventKind::GoalDropped { goal }
            } else {
                EventKind::GoalStated {
                    goal,
                    text: "New direction".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                }
            };
            e.append_checked(Some(&project), Actor::Owner, |_, _| Ok(vec![change]))
                .unwrap();
            assert!(!e.proj().questions[&question].view.open);
            assert!(matches!(
                support::http::run::<dvb_core::run_verbs::AskReply, _>("ask", State(e.clone()), Caller { run: surviving }, Json(body())).await,
                Err(Fail::Refused(r)) if r.code == RefusalCode::ValueChanged
            ));
        }
    }
}

#[tokio::test]
async fn fake_orchestrator_ask_uses_the_current_default_and_explicit_format_overrides_it() {
    let (_dir, e, c, _task) = fake_run();
    let body = || AskBody {
        subject: "Start?".into(),
        options: vec![],
        recommended: 1,
        format: None,
        issues: vec![],
    };
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        c,
        Json(body()),
    )
    .await
    .unwrap()
    .0
    .question;
    assert_eq!(
        e.proj().questions[&question].view.format,
        QuestionFormat::QuickCompare
    );
    for (value, format) in [
        ("simple", QuestionFormat::Simple),
        ("swot", QuestionFormat::Swot),
    ] {
        settings(&e, "question_format", json!(value)).await;
        let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
            "ask",
            State(e.clone()),
            caller(&e),
            Json(body()),
        )
        .await
        .unwrap()
        .0
        .question;
        assert_eq!(e.proj().questions[&question].view.format, format);
        let (r, p) = {
            let proj = e.proj();
            (
                proj.runs[&RunId::new("orchestrator-run")].clone(),
                proj.projects[&ProjectId::new("p")].clone(),
            )
        };
        let vars = dvb_engine::prompt::context(&e, &r, &p).await.unwrap();
        let text = dvb_engine::prompt::render(
            include_str!("../../../prompts/orchestrator.md"),
            &vars,
            false,
        );
        assert!(text.contains(&format!("default question format is `{value}`")));
    }
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(AskBody {
            format: Some(QuestionFormat::Simple),
            issues: vec![],
            ..body()
        }),
    )
    .await
    .unwrap()
    .0
    .question;
    assert_eq!(
        e.proj().questions[&question].view.format,
        QuestionFormat::Simple
    );
    assert_eq!(e.proj().settings.question_format(), QuestionFormat::Swot);
}

fn removed(result: Result<(), Fail>) {
    let Err(Fail::Refused(refusal)) = result else {
        panic!("expected selected-pool refusal")
    };
    assert_eq!(refusal.code, RefusalCode::ModelNotInPool);
    assert!(refusal.detail.contains("selected model pool"));
    assert!(refusal.detail.contains("valid model ids: fake-next"));
}

/// A plan's routine tasks are stored as routine, and the approval question
/// marks them.
#[tokio::test]
async fn the_approval_question_shows_which_tasks_are_routine() {
    let (_dir, e, c, _) = fake_run();
    e.append_checked(Some(&c.run.project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    let next: PlanBody = serde_json::from_value(json!({
        "goal": "Parse and test the config",
        "tasks": [
            {"title": "Add the parser", "role": "backend", "summary": "New module"},
            {"title": "Test the parser", "role": "backend", "summary": "Tests", "routine": true}
        ]
    }))
    .unwrap();
    let tasks = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        c,
        Json(next),
    )
    .await
    .unwrap()
    .0
    .tasks;
    let routine: Vec<_> = tasks
        .iter()
        .map(|t| e.proj().tasks[t].view.routine)
        .collect();
    assert_eq!(routine, [false, true]);
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(AskBody {
            subject: "Start the new plan?".into(),
            options: vec![dvb_core::run_verbs::AskOption {
                text: "Yes".into(),
                effect: "Build it".into(),
                approves_plan: true,
                no_effect: false,
                retries_task: None,
            }],
            recommended: 1,
            format: None,
            issues: vec![],
        }),
    )
    .await
    .unwrap()
    .0
    .question;
    let q = e.proj().questions[&question].view.clone();
    assert!(
        q.subject
            .contains("1. backend — Add the parser\n2. backend — Test the parser (routine)\n"),
        "{}",
        q.subject
    );
    assert_eq!(
        q.plan.iter().map(|t| t.routine).collect::<Vec<_>>(),
        [false, true]
    );
}

#[tokio::test]
async fn a_completed_goal_can_be_followed_by_a_separate_plan_and_approval_question() {
    let (_dir, e, c, old_task) = fake_run();
    e.append_checked(Some(&c.run.project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    let next: PlanBody = serde_json::from_value(json!({
        "goal": "Add a one-line .editorconfig",
        "tasks": [{"title": "Add .editorconfig", "role": "backend", "summary": "One line"}]
    }))
    .unwrap();
    let task = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        c,
        Json(next.clone()),
    )
    .await
    .unwrap()
    .0
    .tasks[0]
        .clone();
    let goal = {
        let proj = e.proj();
        assert_eq!(
            proj.goals[&GoalId::new("g")].state,
            dvb_core::api::GoalState::Done
        );
        assert_eq!(proj.goals[&GoalId::new("g")].tasks, [old_task]);
        let g = proj.latest_goal(&ProjectId::new("p")).unwrap();
        assert_ne!(g.id, GoalId::new("g"));
        assert_eq!(g.text, "Add a one-line .editorconfig");
        assert_eq!(g.state, dvb_core::api::GoalState::Stated);
        assert_eq!(g.tasks, std::slice::from_ref(&task));
        assert!(!proj.tasks[&task].approved);
        let result = g.clone();
        drop(proj);
        result
    };
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(AskBody {
            subject: "Start the new plan?".into(),
            options: vec![dvb_core::run_verbs::AskOption {
                text: "Yes".into(),
                effect: "Build it".into(),
                approves_plan: true,
                no_effect: false,
                retries_task: None,
            }],
            recommended: 1,
            format: None,
            issues: vec![],
        }),
    )
    .await
    .unwrap()
    .0
    .question;
    assert_eq!(
        e.proj().questions[&question].lapses_on,
        LapsesOn::GoalVersion(goal.version)
    );
    let repeat = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        caller(&e),
        Json(next.clone()),
    )
    .await;
    assert!(matches!(repeat, Err(Fail::Refused(r)) if r.code == RefusalCode::ValueChanged));

    // Replay must keep the proposing run bound to its new goal.
    let rebuilt = dvb_engine::proj::Projections::rebuild(&support::events::all(&e));
    assert_eq!(
        rebuilt.runs[&RunId::new("orchestrator-run")].goal_version,
        Some(goal.version)
    );
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: goal.id.clone(),
            text: "A different direction".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let stale = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        caller(&e),
        Json(next),
    )
    .await;
    assert!(matches!(stale, Err(Fail::Refused(r)) if r.code == RefusalCode::ValueChanged));
}

#[tokio::test]
async fn a_goal_stated_meanwhile_does_not_make_the_runs_plan_stale() {
    let (_dir, e, _c, _) = fake_run();
    let p = ProjectId::new("p");
    let owner = |goal: &str| {
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::GoalStated {
                goal: GoalId::new(goal),
                text: format!("Goal {goal}"),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            }])
        })
        .unwrap();
    };
    owner("a");
    let mut a = e.proj().runs[&RunId::new("orchestrator-run")]
        .assignment
        .clone();
    a.run = RunId::new("planner");
    a.reason = "A goal was stated.".into();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(a)])
    })
    .unwrap();
    // Stated while the run plans goal a: it waits its turn.
    owner("b");
    let caller_of = |e: &AppState, run: &str| Caller {
        run: e.proj().runs[&RunId::new(run)].clone(),
    };
    let body: PlanBody = serde_json::from_value(json!({
        "tasks": [{"title": "Do a", "role": "backend", "summary": "One edit"}]
    }))
    .unwrap();
    let task = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        caller_of(&e, "planner"),
        Json(body),
    )
    .await
    .unwrap()
    .0
    .tasks[0]
        .clone();
    let _ = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller_of(&e, "planner"),
        Json(AskBody {
            subject: "Start it?".into(),
            options: vec![],
            recommended: 1,
            format: None,
            issues: vec![],
        }),
    )
    .await
    .unwrap();
    let proj = e.proj();
    assert_eq!(proj.goals[&GoalId::new("a")].tasks, [task]);
    assert!(proj.goals[&GoalId::new("b")].tasks.is_empty());
    assert!(proj.questions.values().all(|q| q.view.open));
    drop(proj);
}

#[tokio::test]
async fn a_completed_goal_requires_next_goal_text_and_leaves_no_partial_plan() {
    let (_dir, e, c, _) = fake_run();
    e.append_checked(Some(&c.run.project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    for (goal, role) in [
        (None, "backend"),
        (Some(" "), "backend"),
        (Some("Next goal"), "missing-role"),
    ] {
        let body = serde_json::from_value::<PlanBody>(json!({
            "goal": goal,
            "tasks": [{"title": "Next task", "role": role, "summary": "One edit"}]
        }))
        .unwrap();
        let Err(Fail::Refused(r)) = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
            "plan",
            State(e.clone()),
            caller(&e),
            Json(body),
        )
        .await
        else {
            panic!("an incomplete next-goal plan must be refused");
        };
        if role == "backend" {
            assert_eq!(r.code, RefusalCode::ValueChanged);
            assert!(r.detail.contains("the completed goal needs a next goal"));
        } else {
            assert_eq!(r.code, RefusalCode::RoleNotAllowed);
            assert_eq!(r.detail, "no role missing-role");
        }
        assert_eq!(e.proj().goals.len(), 1);
        assert_eq!(e.proj().tasks.len(), 1);
    }
}

#[tokio::test]
async fn a_done_bound_plan_names_the_owner_goal_waiting_for_its_own_run() {
    let (_dir, e, c, _) = fake_run();
    e.append_checked(Some(&c.run.project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
    e.append_checked(Some(&c.run.project), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("owner-goal"),
            text: "Add a parser".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    // Different text must be refused too: no next-goal creation while any
    // stated goal waits, even though this run is still valid for Done.
    for text in ["Add a parser", "Unrelated work"] {
        let before = e.store.last_id().unwrap();
        let body = serde_json::from_value::<PlanBody>(json!({"goal": text,
            "tasks": [{"title": "Next task", "role": "backend", "summary": "One edit"}]
        }))
        .unwrap();
        let Err(Fail::Refused(r)) = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
            "plan",
            State(e.clone()),
            Caller { run: c.run.clone() },
            Json(body),
        )
        .await
        else {
            panic!("a waiting owner goal must prevent next-goal creation");
        };
        assert_eq!(r.code, RefusalCode::ValueChanged);
        assert!(r.detail.contains("owner-goal"));
        assert_eq!(e.store.last_id().unwrap(), before);
        assert_eq!(e.proj().goals.len(), 2);
        assert!(e.proj().goals[&GoalId::new("owner-goal")].planned.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_plans_use_the_bound_waiting_goal_only_once() {
    let (_dir, e, c, _) = fake_run();
    e.append_checked(Some(&c.run.project), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("a"),
            text: "Goal a".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let mut assignment = c.run.assignment.clone();
    assignment.run = RunId::new("planner");
    assignment.reason = "A goal was stated.".into();
    assignment.goal_version = Some(e.proj().goals[&GoalId::new("a")].version);
    e.append_checked(Some(&c.run.project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment)])
    })
    .unwrap();
    e.append_checked(Some(&c.run.project), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("b"),
            text: "Goal b".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let bound = Caller {
        run: e.proj().runs[&RunId::new("planner")].clone(),
    };
    let body = |goal: Option<&str>| {
        serde_json::from_value::<PlanBody>(json!({"goal": goal,
            "tasks": [{"title": "Do a", "role": "backend", "summary": "Goal a"}]
        }))
        .unwrap()
    };
    let Err(Fail::Refused(r)) = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        Caller {
            run: bound.run.clone(),
        },
        Json(body(Some("Goal b"))),
    )
    .await
    else {
        panic!("the newest goal cannot replace the run's goal");
    };
    assert_eq!(r.code, RefusalCode::ValueChanged);
    assert!(r.detail.contains("a"));
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let submit = |e: AppState, c: Caller, barrier: std::sync::Arc<tokio::sync::Barrier>| {
        tokio::spawn(async move {
            barrier.wait().await;
            let body = serde_json::from_value::<PlanBody>(json!({
                "tasks": [{"title": "Do a", "role": "backend", "summary": "Goal a"}]
            }))
            .unwrap();
            support::http::run::<dvb_core::run_verbs::PlanReply, _>("plan", State(e), c, Json(body))
                .await
        })
    };
    let (a, b) = tokio::join!(
        submit(
            e.clone(),
            Caller {
                run: bound.run.clone()
            },
            barrier.clone()
        ),
        submit(e.clone(), bound, barrier)
    );
    let results = [a.unwrap(), b.unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|f| matches!(f, Fail::Refused(r) if r.code == RefusalCode::ValueChanged))
    );
    assert_eq!(e.proj().goals[&GoalId::new("a")].tasks.len(), 1);
    assert!(e.proj().goals[&GoalId::new("b")].planned.is_none());
    assert_eq!(e.proj().goals.len(), 3);
}

#[tokio::test]
async fn omitted_model_starts_with_the_roles_active_hire_and_effort() {
    let (dir, e, c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::HireAdded(HireRecord {
            id: HireId::new("h-next"),
            role: RoleId::new("backend"),
            model: "fake-next".into(),
            effort: Effort::High,
            status: HireStatus::Active,
            routine: false,
        })])
    })
    .unwrap();
    // The earlier active hire is outside the selected pool; use the usable one.
    settings(&e, "model_pool", json!(["fake-next"])).await;
    let body: TaskStartBody = serde_json::from_value(json!({
        "task": task,
        "role": "backend",
        "reason": "The owner approved the plan."
    }))
    .unwrap();
    let run = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        c,
        Json(body),
    )
    .await
    .unwrap()
    .0
    .run
    .unwrap();
    let proj = e.proj();
    let a = &proj.runs[&run].assignment;
    assert_eq!(a.hire, HireId::new("h-next"));
    assert_eq!(a.model, "fake-next");
    assert_eq!(a.vendor, Vendor::Fake);
    assert_eq!(a.effort, Effort::High);
    assert_eq!(proj.tasks[&task].view.state, dvb_core::TaskState::Preparing);
}

/// A routine task, marked by the plan or by its start, starts on the role's
/// routine hire; with no routine hire it starts on the default hire, as before.
#[tokio::test]
async fn a_routine_task_starts_on_the_roles_routine_hire() {
    for routine_hire in [true, false] {
        let (dir, e, _c, task) = fake_run();
        fake_repo(dir.path(), &e).await;
        let planned = TaskId::new("t-routine");
        let mut kinds = vec![EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![dvb_core::event::PlannedTask {
                after: vec![],
                task: planned.clone(),
                title: "Add tests".into(),
                role: RoleId::new("backend"),
                review: dvb_core::roles::ReviewKind::Code,
                summary: "Tests".into(),
                routine: true,
            }],
        }];
        if routine_hire {
            kinds.push(EventKind::HireAdded(HireRecord {
                id: HireId::new("h-routine"),
                role: RoleId::new("backend"),
                model: "fake-next".into(),
                effort: Effort::Low,
                status: HireStatus::Active,
                routine: true,
            }));
        }
        e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(kinds))
            .unwrap();
        let mut marked = start_body(&task);
        marked.routine = true;
        for body in [marked, start_body(&planned)] {
            let run = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
                "task/start",
                State(e.clone()),
                caller(&e),
                Json(body.clone()),
            )
            .await
            .unwrap()
            .0
            .run
            .unwrap();
            let a = e.proj().runs[&run].assignment.clone();
            let t = e.proj().tasks[&body.task].view.clone();
            assert!(a.routine && t.routine, "{}", body.task);
            let (hire, model) = if routine_hire {
                ("h-routine", "fake-next")
            } else {
                ("h-backend", "fake-1")
            };
            assert_eq!((a.hire.as_str(), a.model.as_str()), (hire, model));
            assert_eq!(t.round_model.as_deref(), Some(model));
        }
        // The role's default hire stays the one it had.
        let default = e.proj().projects[&ProjectId::new("p")]
            .hire(&RoleId::new("backend"), None)
            .map(|h| h.id.clone());
        assert_eq!(default, Some(HireId::new("h-backend")));
    }
}

/// The engine starts an approved task itself; the orchestrator starting it
/// again gets the same run back and nothing is refused or started.
#[tokio::test]
async fn a_second_start_of_a_started_task_is_a_no_op() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    let start = || {
        support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            caller(&e),
            Json(start_body(&task)),
        )
    };
    let first = start().await.unwrap().0.run.unwrap();
    let before = e.store.last_id().unwrap();
    let again = start().await.unwrap().0;
    assert_eq!(again.run, Some(first));
    assert!(again.waiting_for.is_empty());
    assert_eq!(
        e.store.last_id().unwrap(),
        before,
        "no event, refusal or start"
    );
    assert_eq!(
        e.proj().tasks[&task].view.state,
        dvb_core::TaskState::Preparing
    );
}

/// A task in review has started too: starting it again leaves its review
/// as it is, with no fix round and nothing refused.
#[tokio::test]
async fn a_start_of_a_task_in_review_is_a_no_op() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    let start = || {
        support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            caller(&e),
            Json(start_body(&task)),
        )
    };
    let first = start().await.unwrap().0.run.unwrap();
    let from = e.proj().tasks[&task].view.state.clone();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from,
            to: dvb_core::TaskState::InReview,
        }])
    })
    .unwrap();
    let before = e.store.last_id().unwrap();
    let again = start().await.unwrap().0;
    assert_eq!(again.run, Some(first));
    assert_eq!(
        e.store.last_id().unwrap(),
        before,
        "no event, refusal or round"
    );
    assert_eq!(
        e.proj().tasks[&task].view.state,
        dvb_core::TaskState::InReview
    );
}

#[test]
fn recovery_rejects_a_selected_hire_missing_from_the_release_pool() {
    let (_dir, e, _c, task) = fake_run();
    let mut hire = e.proj().projects[&ProjectId::new("p")]
        .hire(&RoleId::new("backend"), None)
        .unwrap()
        .clone();
    hire.model = "removed-from-release".into();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::HireChanged {
                hire,
                restored_from: None,
            },
            EventKind::SettingsChanged {
                key: "model_pool".into(),
                old: json!(["fake-1"]),
                new: json!(["removed-from-release"]),
                restored_from: None,
            },
        ])
    })
    .unwrap();
    let proj = e.proj();
    assert!(proj.needs_orchestrator(&proj.tasks[&task], &e.pool));
    drop(proj);
}

#[tokio::test]
async fn admission_uses_the_same_selected_hire_rule_as_recovery() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    settings(&e, "model_pool", json!(["fake-next"])).await;
    // The log and locked snapshot deliberately disagree, as in the model-mode fixture.
    e.proj().apply(&dvb_core::Event {
        id: EventId(1000),
        at: e.clock.now(),
        actor: Actor::Owner,
        project: None,
        evidence: None,
        kind: EventKind::SettingsChanged {
            key: "model_pool".into(),
            old: json!(["fake-next"]),
            new: json!(["fake-1"]),
            restored_from: None,
        },
    });
    {
        let proj = e.proj();
        assert!(!proj.needs_orchestrator(&proj.tasks[&task], &e.pool));
        drop(proj);
    }
    let result = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(start_body(&task)),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn admission_uses_the_model_mode_in_its_locked_projection() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    // Deterministic torn-read fixture: the log reports Auto while the
    // admission snapshot is Custom, as a concurrent setting can change
    // between an unlocked log read and locking the projection.
    settings(&e, "model_mode", json!("Auto")).await;
    e.proj().settings.values.insert(
        dvb_core::api::SettingsKey::ModelMode,
        serde_json::json!("Custom"),
    );
    let refused = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(TaskStartBody {
            task,
            role: "backend".into(),
            model: Some("fake-next".into()),
            effort: Some(Effort::High),
            reason: "Model choice".into(),
            routine: false,
        }),
    )
    .await;
    assert!(matches!(refused, Err(Fail::Refused(r)) if r.code == RefusalCode::NoActiveHire));
    assert!(
        !e.proj().projects[&ProjectId::new("p")]
            .hires
            .iter()
            .any(|h| h.model == "fake-next")
    );
}

#[tokio::test]
async fn auto_mode_hires_the_named_pool_model_for_the_orchestrator() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    let body = || TaskStartBody {
        task: task.clone(),
        role: "backend".into(),
        model: Some("fake-next".into()),
        effort: Some(Effort::High),
        reason: "A small edit; the cheaper model will do.".into(),
        routine: false,
    };
    // Custom: no hire on that model, so a refusal and no hire.
    let refused = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(body()),
    )
    .await;
    assert!(matches!(refused, Err(Fail::Refused(r)) if r.code == RefusalCode::NoActiveHire));
    // Auto: the engine hires it, as the orchestrator's run, and starts on it.
    settings(&e, "model_mode", json!("Auto")).await;
    let run = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(body()),
    )
    .await
    .unwrap()
    .0
    .run
    .unwrap();
    let events = support::events::all(&e);
    let retired = events
        .iter()
        .find(
            |ev| matches!(&ev.kind, EventKind::HireRetired(h) if h.id == HireId::new("h-backend")),
        )
        .unwrap();
    let started = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.run == run))
        .unwrap();
    assert_eq!(
        (retired.actor.clone(), retired.at),
        (started.actor.clone(), started.at)
    );
    let hired = events.iter().find_map(|ev| {
        if let EventKind::HireAdded(h) = &ev.kind
            && h.model == "fake-next"
        {
            Some((h.clone(), ev.actor.clone()))
        } else {
            None
        }
    });
    let (hire, actor) = hired.expect("hire.added");
    assert_eq!(actor, Actor::Run(RunId::new("orchestrator-run")));
    assert_eq!(hire.effort, Effort::High);
    let proj = e.proj();
    assert_eq!(
        proj.projects[&ProjectId::new("p")]
            .hires
            .iter()
            .filter(|h| h.role == RoleId::new("backend") && h.status == HireStatus::Active)
            .count(),
        1
    );
    let a = &proj.runs[&run].assignment;
    assert_eq!((a.hire.clone(), a.model.as_str()), (hire.id, "fake-next"));
    assert_eq!(a.reason, "A small edit; the cheaper model will do.");
    drop(proj);
}

#[tokio::test]
async fn a_refused_auto_start_hires_no_one() {
    let (dir, e, _c, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    settings(&e, "model_mode", json!("Auto")).await;
    dvb_engine::owner::pause(&e, &ProjectId::new("p"), true).unwrap();
    let body = TaskStartBody {
        task: task.clone(),
        role: "backend".into(),
        model: Some("fake-next".into()),
        effort: None,
        reason: "Build".into(),
        routine: false,
    };
    let before = e.proj().projects[&ProjectId::new("p")].hires.clone();
    let refused = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(body),
    )
    .await;
    assert!(refused.is_err());
    assert_eq!(e.proj().projects[&ProjectId::new("p")].hires, before);
    assert!(
        !e.proj().projects[&ProjectId::new("p")]
            .hires
            .iter()
            .any(|h| h.model == "fake-next")
    );
}

#[tokio::test]
async fn auto_initial_hiring_uses_medium_without_an_active_hire() {
    let (dir, e, _, task) = fake_run();
    fake_repo(dir.path(), &e).await;
    let _ = support::http::owner::<dvb_core::roles::HireRecord>(
        &e,
        axum::http::Method::POST,
        "/hires/h-backend/retire",
        None,
    )
    .await
    .unwrap();
    settings(&e, "model_mode", json!("Auto")).await;
    let before = e.store.last_id().unwrap();
    let run = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(TaskStartBody {
            task,
            role: "backend".into(),
            model: Some("fake-next".into()),
            effort: None,
            reason: "edit".into(),
            routine: false,
        }),
    )
    .await
    .unwrap()
    .0
    .run
    .unwrap();
    let events = e.store.after(before).unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::HireRetired(_)))
    );
    let proj = e.proj();
    assert_eq!(proj.runs[&run].assignment.effort, Effort::Medium);
    assert_eq!(
        proj.projects[&ProjectId::new("p")]
            .hire(&RoleId::new("backend"), None)
            .unwrap()
            .id,
        proj.runs[&run].assignment.hire
    );
}

#[tokio::test]
async fn no_usable_active_hire_refuses_and_guides_the_orchestrator_to_hire() {
    use axum::response::IntoResponse;
    for (retired, removed, model) in [
        (true, false, None),
        (false, false, Some("fake-next")),
        (false, true, None),
        (false, true, Some("fake-next")),
    ] {
        let (dir, e, c, task) = fake_run();
        fake_repo(dir.path(), &e).await;
        if retired {
            e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
                Ok(vec![EventKind::HireRetired(HireRecord {
                    id: HireId::new("h-backend"),
                    role: RoleId::new("backend"),
                    model: "fake-1".into(),
                    effort: Effort::Medium,
                    status: HireStatus::Retired,
                    routine: false,
                })])
            })
            .unwrap();
        }
        if removed {
            settings(&e, "model_pool", json!(["fake-next"])).await;
        }
        let before = e.proj().runs.len();
        let response = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            c,
            Json(TaskStartBody {
                task: task.clone(),
                role: "backend".into(),
                model: model.map(str::to_owned),
                effort: None,
                reason: "Build".into(),
                routine: false,
            }),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let reply: Refusal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply.code, RefusalCode::NoActiveHire);
        assert!(reply.detail.contains("backend"));
        assert!(reply.detail.contains("dvb run ask"));
        assert_eq!(e.proj().runs.len(), before);
        assert_eq!(
            e.proj().tasks[&task].view.state,
            dvb_core::TaskState::Planned
        );
        assert!(support::events::all(&e).iter().any(|ev| matches!(
            &ev.kind, EventKind::CommandRefused(r) if r.refusal.current() == Some(&reply)
        )));
        let (r, p) = {
            let proj = e.proj();
            (
                proj.runs[&RunId::new("orchestrator-run")].clone(),
                proj.projects[&ProjectId::new("p")].clone(),
            )
        };
        let vars = dvb_engine::prompt::context(&e, &r, &p).await.unwrap();
        let text = dvb_engine::prompt::render(
            include_str!("../../../prompts/orchestrator.md"),
            &vars,
            false,
        );
        assert_eq!(
            text.matches(include_str!("../../../prompts/run-verbs.md"))
                .count(),
            1
        );
        assert!(!text.contains("{{"), "{text}");
    }
}

#[tokio::test]
async fn an_alias_is_refused_with_selected_exact_ids_in_the_reply() {
    use axum::response::IntoResponse;
    let (_dir, e, c, task) = fake_run();
    settings(&e, "model_pool", json!(["fake-next"])).await;
    let before = e.proj().runs.len();
    let body = serde_json::from_value::<TaskStartBody>(json!({
        "task": task,
        "role": "backend",
        "model": "sonnet",
        "reason": "Build"
    }))
    .unwrap();
    let response = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        c,
        Json(body),
    )
    .await
    .unwrap_err()
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reply["code"], "MODEL_NOT_IN_POOL");
    let detail = reply["detail"].as_str().unwrap();
    assert!(detail.contains("sonnet"));
    assert!(detail.contains("valid model ids: fake-next"));
    assert!(!detail.contains("fake-1"));
    assert_eq!(e.proj().runs.len(), before);
    assert_eq!(
        e.proj().tasks[&task].view.state,
        dvb_core::TaskState::Planned
    );
    assert!(support::events::all(&e).iter().any(|ev| matches!(
            &ev.kind, EventKind::CommandRefused(r) if r.refusal.current().unwrap().code == RefusalCode::ModelNotInPool && r.refusal.current().unwrap().detail == detail
        )));
    let (r, p) = {
        let proj = e.proj();
        (
            proj.runs[&RunId::new("orchestrator-run")].clone(),
            proj.projects[&ProjectId::new("p")].clone(),
        )
    };
    let text = dvb_engine::prompt::render(
        include_str!("../../../prompts/orchestrator.md"),
        &dvb_engine::prompt::context(&e, &r, &p).await.unwrap(),
        false,
    );
    assert!(text.contains("Selected model pool (exact ids): fake-next"));
    assert!(!text.contains("Selected model pool (exact ids): fake-1"));
}

#[tokio::test]
async fn removed_fake_model_refuses_wake_builder_and_reviewer_but_an_admitted_run_keeps_its_assignment()
 {
    let (_dir, e, _c, task) = fake_run();
    // A valid second model lets the owner remove the current Fake hire from selection.
    settings(&e, "model_pool", json!(["fake-next"])).await;
    let before = e.proj().runs.len();
    removed(wake::wake(e.clone(), ProjectId::new("p"), wake::Wake::Goal, 0, 0).await);
    removed(
        support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            caller(&e),
            Json(TaskStartBody {
                task: task.clone(),
                role: "backend".into(),
                model: Some("fake-1".into()),
                effort: None,
                reason: "Build".into(),
                routine: false,
            }),
        )
        .await
        .map(|_| ()),
    );
    assert_eq!(
        e.proj().tasks[&task].view.state,
        dvb_core::TaskState::Planned
    );
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::BranchPublished {
            task: task.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: "head".into(),
        }])
    })
    .unwrap();
    // In review, the dispatcher's reviewer start refuses the removed model.
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from: dvb_core::TaskState::Planned,
            to: dvb_core::TaskState::InReview,
        }])
    })
    .unwrap();
    removed(dvb_engine::dispatch::step_now(&e, &task).await);
    assert_eq!(e.proj().runs.len(), before);
    // The already-admitted Fake run can still ask; pool edits never rewrite or end it.
    let _ = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(AskBody {
            subject: "Still running".into(),
            options: vec![],
            recommended: 1,
            format: None,
            issues: vec![],
        }),
    )
    .await
    .unwrap();
    {
        let proj = e.proj();
        let r = &proj.runs[&RunId::new("orchestrator-run")];
        assert_eq!(r.assignment.model, "fake-1");
        assert!(r.ended.is_none());
        drop(proj);
    }
    assert!(support::events::all(&e).iter().any(|ev| matches!(&ev.kind, EventKind::CommandRefused(r) if r.refusal.current().unwrap().code == RefusalCode::ModelNotInPool)));
    settings(&e, "model_pool", json!(["fake-1"])).await;
    assert_eq!(e.vendor_of("fake-1").unwrap(), Vendor::Fake);
}

/// the orchestrator retries a failed task only after the owner chose an
/// option naming it, answered since the task last failed; one yes is
/// one retry, in the kept workspace's base.
#[tokio::test]
async fn the_orchestrator_retries_a_failed_task_only_with_the_owners_yes_to_it() {
    use dvb_core::TaskState;
    use dvb_core::event::EndReason;
    let (_dir, e, _c, task) = fake_run();
    let p = ProjectId::new("p");
    let fail = || {
        let from = e.proj().tasks[&task].view.state.clone();
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::TaskStateChanged {
                task: task.clone(),
                from,
                to: TaskState::Failed {
                    reason: "3 attempts; the last ended: crashed".into(),
                },
            }])
        })
        .unwrap();
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("b1"))
                    .task(Some(task.clone()))
                    .hire(HireId::new("h-backend"))
                    .reason("Build it".into())
                    .base(Some("kept-base".into()))
                    .build(),
            ),
            EventKind::RunEnded {
                run: RunId::new("b1"),
                reason: EndReason::Crashed,
                detail: None,
            },
        ])
    })
    .unwrap();
    fail();
    let retry = || {
        support::http::run::<TaskView, _>(
            "task/retry",
            State(e.clone()),
            caller(&e),
            Json(TaskRetryBody {
                task: task.clone(),
                text: Some("The disk has space now. Use the existing notes.".into()),
            }),
        )
    };
    let code = |r: Result<Json<TaskView>, Fail>| match r {
        Err(Fail::Refused(r)) => r.code,
        other => panic!("{other:?}"),
    };
    let ask_retry = async |names: &TaskId| {
        let body = serde_json::from_value::<AskBody>(json!({
                "subject": "Build it failed (the disk was full). Retry it?",
                "options": [
                    {"text": "Retry it", "effect": "One more run in its kept workspace.", "retries_task": names},
                    {"text": "Leave it", "effect": "It stays failed."}
                ],
                "recommended": 1
            }))
            .unwrap();
        support::http::run::<dvb_core::run_verbs::AskReply, _>(
            "ask",
            State(e.clone()),
            caller(&e),
            Json(body),
        )
        .await
    };
    let answer = |question: &QuestionId, n| {
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::QuestionAnswered {
                question: question.clone(),
                answer: Answer::Choice(n),
            }])
        })
        .unwrap();
    };

    // No question names it: refused, and recorded.
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    assert!(support::events::all(&e).iter().any(|ev| matches!(
        &ev.kind,
        EventKind::CommandRefused(r) if r.command == "task retry"
    )));
    // A question can only name a task of the project.
    assert!(matches!(
        ask_retry(&TaskId::new("elsewhere")).await,
        Err(Fail::NotFound(_))
    ));
    // The owner said no.
    let no = ask_retry(&task).await.unwrap().0.question;
    answer(&no, 2);
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    // The owner said yes: one more run on the kept base.
    let yes = ask_retry(&task).await.unwrap().0.question;
    answer(&yes, 1);
    let too_long = support::http::run::<TaskView, _>(
        "task/retry",
        State(e.clone()),
        caller(&e),
        Json(TaskRetryBody {
            task: task.clone(),
            text: Some("é".repeat(8193)),
        }),
    )
    .await;
    assert_eq!(code(too_long), RefusalCode::InvalidValue);
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::TaskRetryTextSet { .. }))
    );
    let view = retry().await.unwrap().0;
    assert_eq!(view.id, task);
    let events = support::events::all(&e);
    let started = events.iter().rev().find_map(|ev| {
        if let EventKind::RunStarted(a) = &ev.kind
            && a.task.as_ref() == Some(&task)
        {
            Some(a.clone())
        } else {
            None
        }
    });
    let started = started.expect("a new run");
    assert_ne!(started.run, RunId::new("b1"));
    assert_eq!(started.base.as_deref(), Some("kept-base"));
    assert!(
        events
            .iter()
            .any(|ev| ev.actor == Actor::Run(RunId::new("orchestrator-run"))
                && matches!(&ev.kind, EventKind::TaskResumed { task: t, reason }
                if *t == task && reason.as_deref() == Some("retried after it failed")))
    );
    // Exercise the real renderer.
    let (run, project) = {
        let proj = e.proj();
        (proj.runs[&started.run].clone(), proj.projects[&p].clone())
    };
    let mut vars = dvb_engine::prompt::context(&e, &run, &project)
        .await
        .unwrap();
    vars.push(("task_title", "Build it".into()));
    let rendered =
        dvb_engine::prompt::render(include_str!("../../../prompts/builder.md"), &vars, false);
    assert!(rendered.contains("The disk has space now. Use the existing notes."));
    assert!(rendered.find("One edit").unwrap() < rendered.find("## Retry context").unwrap());
    assert!(rendered.contains("does not replace the task"));
    assert!(
        events
            .iter()
            .any(|ev| ev.actor == Actor::Run(RunId::new("orchestrator-run"))
                && matches!(&ev.kind, EventKind::TaskRetryTextSet { text: Some(text), .. }
                if text == "The disk has space now. Use the existing notes."))
    );
    // The yes is used up by its retry.
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    // Failed again: the old yes no longer counts.
    fail();
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    assert_eq!(
        support::events::all(&e)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::TaskRetryTextSet { .. }))
            .count(),
        1,
        "refused retries record no replacement"
    );
}

/// The orchestrator sends a task with a PR back (in review, or
/// stopped) only with the owner's yes since it last stopped or was retried.
/// Without it the refusal comes first: the review goes on, unstopped.
#[tokio::test]
async fn the_orchestrator_sends_a_task_with_a_pr_back_only_with_a_yes_since_it_stopped() {
    use dvb_core::TaskState;
    let (_dir, e, _c, task) = fake_run();
    let p = ProjectId::new("p");
    let from = e.proj().tasks[&task].view.state.clone();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::BranchPublished {
                task: task.clone(),
                round: 1,
                branch: "dvb/t-1".into(),
                head: "h1".into(),
            },
            EventKind::PrOpened {
                task: task.clone(),
                pr: 1,
                url: "https://github.invalid/pr/1".into(),
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from,
                to: TaskState::InReview,
            },
        ])
    })
    .unwrap();
    let retry = || {
        support::http::run::<TaskView, _>(
            "task/retry",
            State(e.clone()),
            caller(&e),
            Json(TaskRetryBody {
                task: task.clone(),
                text: Some("Remove the PDF; generate it in the test.".into()),
            }),
        )
    };
    let code = |r: Result<Json<TaskView>, Fail>| match r {
        Err(Fail::Refused(r)) => r.code,
        other => panic!("{other:?}"),
    };
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::InReview);

    // A yes given before the task stopped does not count for the stopped task.
    let body = serde_json::from_value::<AskBody>(json!({
        "subject": "Send Build it back with the owner's note?",
        "options": [{"text": "Send it back", "effect": "One builder round on its head.", "retries_task": task}],
        "recommended": 1
    }))
    .unwrap();
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(body),
    )
    .await
    .unwrap()
    .0
    .question;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question,
            answer: Answer::Choice(1),
        }])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from: TaskState::InReview,
            to: TaskState::Stopped {
                was: Box::new(TaskState::InReview),
            },
        }])
    })
    .unwrap();
    assert_eq!(code(retry().await), RefusalCode::GoalNotApproved);
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::TaskRetryTextSet { .. }))
    );
}

/// A yes checked before the task's lock is checked again with the decision:
/// a retry that ran (and failed again) while this one waited uses it up.
#[tokio::test]
async fn a_retry_waiting_for_the_lock_is_refused_once_its_yes_is_stale() {
    use dvb_core::TaskState;
    let (_dir, e, _c, task) = fake_run();
    let p = ProjectId::new("p");
    let fail = || {
        let from = e.proj().tasks[&task].view.state.clone();
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::TaskStateChanged {
                task: task.clone(),
                from,
                to: TaskState::Failed {
                    reason: "crashed".into(),
                },
            }])
        })
        .unwrap();
    };
    // Its last builder's base: the retry needs no forge.
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("b1"))
                    .task(Some(task.clone()))
                    .hire(HireId::new("h-backend"))
                    .reason("Build it".into())
                    .base(Some("kept-base".into()))
                    .build(),
            ),
            EventKind::RunEnded {
                run: RunId::new("b1"),
                reason: dvb_core::event::EndReason::Crashed,
                detail: None,
            },
        ])
    })
    .unwrap();
    fail();
    let body = serde_json::from_value::<AskBody>(json!({
        "subject": "Build it failed. Retry it?",
        "options": [{"text": "Retry it", "effect": "One more run.", "retries_task": task}],
        "recommended": 1
    }))
    .unwrap();
    let question = support::http::run::<dvb_core::run_verbs::AskReply, _>(
        "ask",
        State(e.clone()),
        caller(&e),
        Json(body),
    )
    .await
    .unwrap()
    .0
    .question;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question,
            answer: Answer::Choice(1),
        }])
    })
    .unwrap();
    // Another retry holds the task's lock; this one passes the first check
    // and waits.
    let held = e.lock(&task).await;
    let waiting = tokio::spawn(support::http::run::<TaskView, _>(
        "task/retry",
        State(e.clone()),
        caller(&e),
        Json(TaskRetryBody {
            task: task.clone(),
            text: Some("refused stale retry text".into()),
        }),
    ));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!waiting.is_finished());
    // That retry ran and the task failed again.
    let from = e.proj().tasks[&task].view.state.clone();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from,
            to: TaskState::Preparing,
        }])
    })
    .unwrap();
    fail();
    drop(held);
    match waiting.await.unwrap() {
        Err(Fail::Refused(r)) => assert_eq!(r.code, RefusalCode::GoalNotApproved),
        other => panic!("{other:?}"),
    }
    let started = support::events::all(&e)
        .iter()
        .filter(|ev| matches!(&ev.kind, EventKind::TaskStarted { task: t, .. } if *t == task))
        .count();
    assert_eq!(started, 0);
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::TaskRetryTextSet { .. })),
        "authorization that lapses while waiting cannot persist text"
    );
}

/// when a run ends, the engine starts the task whose start the run cap
/// refused, as the orchestrator asked; never a task of a goal without the
/// owner's yes.
#[tokio::test]
async fn a_start_refused_for_the_run_cap_starts_when_a_run_ends() {
    let (_dir, e, [t1, _t2, t3, other]) = three_tasks_at_the_cap().await;
    dvb_engine::dispatch::stop(&e, &t1, Actor::Owner)
        .await
        .unwrap();
    let run = started(&e, &t3).await;
    let (a, by) = {
        let events = support::events::all(&e);
        let ev = events
            .into_iter()
            .find(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.run == run))
            .unwrap();
        let EventKind::RunStarted(a) = ev.kind else {
            unreachable!()
        };
        (a, ev.actor)
    };
    assert_eq!(by, Actor::Engine);
    assert_eq!((a.hire, a.effort), (HireId::new("h-backend"), Effort::High));
    assert_eq!(a.reason, "Start t3");
    assert!(e.proj().tasks[&t3].waiting.is_none());
    // The next run end has no slot to give the unapproved task's start.
    dvb_engine::dispatch::start_waiting(e.clone())
        .await
        .unwrap();
    assert!(e.proj().tasks[&other].facts.run.is_none());
    assert_eq!(
        e.proj().tasks[&other].view.state,
        dvb_core::TaskState::Planned
    );
}

/// a paused project's waiting task does not start when a run ends; it
/// starts when the project is unpaused, if a slot is free.
#[tokio::test]
async fn a_waiting_task_of_a_paused_project_starts_when_it_is_unpaused() {
    let (_dir, e, [t1, _t2, t3, other]) = three_tasks_at_the_cap().await;
    let p = ProjectId::new("p");
    dvb_engine::owner::pause(&e, &p, true).unwrap();
    dvb_engine::dispatch::stop(&e, &t1, Actor::Owner)
        .await
        .unwrap();
    dvb_engine::dispatch::start_waiting(e.clone())
        .await
        .unwrap();
    assert!(e.proj().tasks[&t3].facts.run.is_none());
    assert!(e.proj().tasks[&t3].waiting.is_some());
    dvb_engine::owner::pause(&e, &p, false).unwrap();
    started(&e, &t3).await;
    dvb_engine::dispatch::start_waiting(e.clone())
        .await
        .unwrap();
    assert!(e.proj().tasks[&other].facts.run.is_none());
}

/// a start still waiting when the engine stopped after the last run
/// ended starts when the engine starts again, by the engine, and does not
/// wake the orchestrator for it.
#[tokio::test]
async fn a_waiting_start_left_at_a_restart_starts_without_waking_the_orchestrator() {
    let (dir, e, [t1, t2, t3, _other]) = three_tasks_at_the_cap().await;
    let p = ProjectId::new("p");
    // Paused, the runs end without starting it; the unpause is recorded,
    // and the engine stops before acting on it.
    dvb_engine::owner::pause(&e, &p, true).unwrap();
    end_all_runs(&e, [&t1, &t2]).await;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    let before = support::events::all(&e).last().unwrap().id;
    drop(e);

    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                dvb_core::launcher::UnitState::Inactive,
            ))),
            ..Default::default()
        },
    );
    dvb_engine::reconcile::mark(&e).unwrap();
    dvb_engine::reconcile::reconcile(e.clone()).await.unwrap();
    let run = started(&e, &t3).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let since = e.store.after(before).unwrap();
    assert!(since.iter().any(|ev| ev.actor == Actor::Engine
        && matches!(&ev.kind, EventKind::RunStarted(a) if a.run == run)));
    assert!(
        !since
            .iter()
            .any(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.task.is_none())),
        "the orchestrator was woken"
    );
}

/// a run that ends the moment a start is refused for the run cap (as
/// soon as the refusal is seen) finds the start waiting, and its slot
/// starts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_ending_as_a_start_is_refused_for_the_cap_starts_it() {
    use tokio::sync::broadcast::error::RecvError;
    let (_dir, e, [_t1, _t2, t3, _other]) = two_tasks_at_the_cap().await;
    let mut feed = e.subscribe();
    let rt = tokio::runtime::Handle::current();
    let ender = {
        let (e, t3) = (e.clone(), t3.clone());
        std::thread::spawn(move || {
            let _rt = rt.enter();
            loop {
                match feed.blocking_recv() {
                    Ok(ev) if matches!(&ev.kind, EventKind::CommandRefused(r) if r.command == "task start") =>
                    {
                        let waiting = e.proj().tasks[&t3].waiting.is_some();
                        e.end_run(
                            &RunId::new("orchestrator-run"),
                            dvb_core::event::EndReason::Completed,
                            None,
                        )
                        .unwrap();
                        return waiting;
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return false,
                }
            }
        })
    };
    let refused = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(start_body(&t3)),
    )
    .await;
    assert!(
        matches!(refused, Err(Fail::Refused(r)) if r.code == RefusalCode::CapReached { which: dvb_core::Cap::ConcurrentRuns })
    );
    let waiting = tokio::task::spawn_blocking(move || ender.join().unwrap())
        .await
        .unwrap();
    assert!(waiting, "the refusal was seen before the waiting start");
    started(&e, &t3).await;
}
