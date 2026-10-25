//! Dependency admission uses the same Fake forge and launcher as normal starts.
mod support;
use axum::{Json, extract::State};
use dvb_core::event::PlannedTask;
use dvb_core::run_verbs::{TaskStartBody, TaskStartReply};
use dvb_core::{Actor, EventKind, GoalId, ProjectId, RoleId, RunId, TaskId, TaskState};
use dvb_engine::{dispatch, proj::Projections};
use std::sync::Arc;

#[tokio::test]
async fn dependent_waits_across_replay_and_starts_once_on_merged_main_with_selected_hire() {
    let launcher = Arc::new(support::launcher::FakeLauncher::default());
    let (dir, e, c, prerequisite) = support::orchestrator::fake_run_on(Some(launcher));
    e.manual_dispatch();
    support::orchestrator::fake_repo(dir.path(), &e).await;
    let dependent = TaskId::new("dependent");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![PlannedTask {
                task: dependent.clone(),
                title: "Use the API".into(),
                role: RoleId::new("backend"),
                summary: "Implement caller".into(),
                review: dvb_core::roles::ReviewKind::Code,
                after: vec![prerequisite.clone()],
                routine: false,
            }],
        }])
    })
    .unwrap();
    let start = TaskStartBody {
        task: dependent.clone(),
        role: "backend".into(),
        model: None,
        effort: None,
        reason: "approved".into(),
        routine: false,
    };
    let reply =
        support::http::run::<TaskStartReply, _>("task/start", State(e.clone()), c, Json(start))
            .await
            .unwrap()
            .0;
    assert_eq!(reply.run, None);
    assert_eq!(reply.waiting_for, vec![prerequisite.clone()]);
    for _ in 0..3 {
        dispatch::start_waiting(e.clone()).await.unwrap();
    }
    let replay = Projections::rebuild(&support::events::all(&e));
    assert!(replay.tasks[&dependent].facts.run.is_none());
    assert_eq!(replay.tasks[&dependent].view.state, TaskState::Planned);
    assert_eq!(
        replay.tasks[&dependent].view.reason.as_deref(),
        Some("Waiting for Build it")
    );
    assert!(dvb_cli::task_line(&replay.tasks[&dependent].view).contains("Waiting for Build it"));
    // Simulate the forge merging the prerequisite into default main.
    let repo = dir.path().join("repo");
    std::fs::write(repo.join("api"), "merged API\n").unwrap();
    support::git::git(&repo, &["add", "."]);
    support::git::git(&repo, &["commit", "--quiet", "-m", "API"]);
    let merged = support::git::git(&repo, &["rev-parse", "HEAD"]);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PrMerged {
            task: prerequisite.clone(),
            pr: 1,
            sha: merged.clone(),
        }])
    })
    .unwrap();
    for _ in 0..3 {
        dispatch::start_waiting(e.clone()).await.unwrap();
    }
    let records = support::events::all(&e);
    let starts: Vec<_> = records
        .iter()
        .filter_map(|ev| {
            if let EventKind::RunStarted(a) = &ev.kind {
                return (a.task.as_ref() == Some(&dependent)).then_some(a);
            }
            None
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].base.as_deref(), Some(merged.as_str()));
    assert_eq!(starts[0].hire.as_str(), "h-backend");
    assert!(e.proj().tasks[&dependent].view.waiting_for.is_empty());
}

#[tokio::test]
async fn failed_or_stopped_prerequisite_never_starts_its_dependent_on_recovery_or_retry() {
    for terminal in [
        TaskState::Failed {
            reason: "CI failed".into(),
        },
        TaskState::Stopped {
            was: Box::new(TaskState::Running),
        },
    ] {
        let launcher = Arc::new(support::launcher::FakeLauncher {
            forbid_start: Some("dependent must stay unstarted"),
            ..Default::default()
        });
        let (dir, e, c, prerequisite) = support::orchestrator::fake_run_on(Some(launcher));
        e.manual_dispatch();
        support::orchestrator::fake_repo(dir.path(), &e).await;
        let dependent = TaskId::new("dependent");
        e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::PlanProposed {
                    goal: GoalId::new("g"),
                    tasks: vec![PlannedTask {
                        task: dependent.clone(),
                        title: "Caller".into(),
                        role: RoleId::new("backend"),
                        summary: "Caller".into(),
                        review: dvb_core::roles::ReviewKind::Code,
                        after: vec![prerequisite.clone()],
                        routine: false,
                    }],
                },
                EventKind::TaskStateChanged {
                    task: prerequisite.clone(),
                    from: TaskState::Planned,
                    to: terminal,
                },
            ])
        })
        .unwrap();
        let reply = support::http::run::<TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            c,
            Json(TaskStartBody {
                task: dependent.clone(),
                role: "backend".into(),
                model: None,
                effort: None,
                reason: "approved".into(),
                routine: false,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(reply.run.is_none());
        for _ in 0..3 {
            dispatch::start_waiting(e.clone()).await.unwrap();
        }
        let replay = Projections::rebuild(&support::events::all(&e));
        assert!(replay.tasks[&dependent].facts.run.is_none());
        assert_eq!(
            replay.tasks[&dependent].view.waiting_for,
            vec![prerequisite]
        );
        // Even direct worker admission cannot bypass the dependency.
        let a = support::assignment::Assignment::new(RunId::new("bypass"))
            .task(Some(dependent.clone()))
            .build();
        assert!(
            matches!(dispatch::start_task(&e, a, Actor::Owner).await, Err(dvb_engine::Fail::Refused(r)) if matches!(r.code, dvb_core::RefusalCode::DependencyNotMerged { .. }))
        );
    }
}

#[tokio::test]
async fn planning_recovery_validates_dependencies_and_carries_roles_and_order_into_approval() {
    use dvb_engine::serve::Caller;
    use serde_json::json;
    let (dir, e, c, task) = support::orchestrator::fake_run();
    e.manual_dispatch();
    support::orchestrator::fake_repo(dir.path(), &e).await;
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("scope"))
                    .task(Some(task.clone()))
                    .build(),
            ),
            EventKind::RunCollected {
                run: RunId::new("scope"),
                head: None,
                summary: None,
                outcome: Some(dvb_core::run_verbs::BuilderOutcome::Partial {
                    reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                    detail: "Needs engine API".into(),
                }),
            },
            EventKind::RunEnded {
                run: RunId::new("scope"),
                reason: dvb_core::event::EndReason::Completed,
                detail: None,
                stderr: None,
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Stopped {
                    was: Box::new(TaskState::Publishing),
                },
            },
        ])
    })
    .unwrap();
    let new_task =
        |after| json!({"title":"API", "role":"backend", "summary":"API only", "after":after});
    for tasks in [
        json!([new_task(json!([0]))]),
        json!([new_task(json!([2]))]),
        json!([new_task(json!([1]))]),
        json!([new_task(json!([2])), new_task(json!([1]))]),
        json!([new_task(json!(["missing"]))]),
    ] {
        let result = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
            "plan",
            State(e.clone()),
            Caller { run: c.run.clone() },
            Json(json!({"tasks":tasks})),
        )
        .await;
        assert!(
            matches!(result, Err(dvb_engine::Fail::Refused(r)) if r.code == dvb_core::RefusalCode::InvalidValue)
        );
    }
    let reply = support::http::run::<dvb_core::run_verbs::PlanReply,_>("plan",State(e.clone()),Caller {run:c.run.clone()},Json(json!({"tasks":[new_task(json!([])),{"title":"Caller","role":"frontend","summary":"UI only","after":[1]}]}))).await.unwrap().0;
    assert_eq!(reply.tasks.len(), 2);
    assert!(e.proj().tasks[&reply.tasks[0]].approved);
    assert_eq!(
        e.proj().tasks[&reply.tasks[1]].view.after,
        vec![reply.tasks[0].clone()]
    );
    let ask = support::http::run::<dvb_core::run_verbs::AskReply,_>("ask",State(e.clone()),Caller {run:c.run.clone()},Json(json!({"subject":"Continue?","options":[{"text":"Yes","effect":"Continue","approves_plan":true}],"recommended":1}))).await.unwrap().0;
    let q = e.proj().questions[&ask.question].view.clone();
    assert_eq!(q.plan.len(), 2);
    assert!(
        q.subject.contains("backend")
            && q.subject.contains("frontend")
            && q.subject.contains("2 starts after 1 merges")
    );
    assert_eq!(q.plan[1].after, vec![reply.tasks[0].clone()]);
    // The goal itself was never rewritten or reapproved by recovery.
    assert_eq!(e.proj().goals[&GoalId::new("g")].text, "Ship it");
    let result = support::http::run::<dvb_core::run_verbs::PlanReply, _>(
        "plan",
        State(e.clone()),
        Caller { run: c.run.clone() },
        Json(json!({"goal":"larger goal","tasks":[new_task(json!([]))]})),
    )
    .await;
    assert!(result.is_err());
    let _retried = support::http::run::<dvb_core::api::TaskView, _>("task/retry", State(e.clone()), Caller {run:c.run}, Json(json!({"task":task,"text":"Finish only the approved backend work; keep the retained changes."}))).await.unwrap();
    assert!(support::events::all(&e).iter().any(|event| matches!(&event.kind,
        EventKind::TaskRetryTextSet {task:retried, text:Some(text)} if retried == &task && text.contains("approved backend work"))));
}

fn stopped_planned() -> TaskState {
    TaskState::Stopped {
        was: Box::new(TaskState::Planned),
    }
}

async fn wait_for(e: &dvb_engine::AppState, what: &str, found: impl Fn(&EventKind) -> bool) {
    support::wait::FAST
        .until(what, || {
            support::events::all(e).iter().any(|ev| found(&ev.kind))
        })
        .await;
}

#[tokio::test]
async fn resuming_a_task_stopped_while_planned_starts_it() {
    let launcher = Arc::new(support::launcher::FakeLauncher::default());
    let (dir, e, _c, task) = support::orchestrator::fake_run_on(Some(launcher));
    support::orchestrator::fake_repo(dir.path(), &e).await;
    assert_eq!(
        dispatch::stop(&e, &task, Actor::Owner).await.unwrap(),
        stopped_planned()
    );
    assert_eq!(
        dispatch::resume(&e, &task, Actor::Owner).await.unwrap(),
        TaskState::Planned
    );
    wait_for(
        &e,
        "task.started",
        |k| matches!(k, EventKind::TaskStarted { task: t, .. } if *t == task),
    )
    .await;
    assert!(e.proj().tasks[&task].facts.run.is_some());
}

#[tokio::test]
async fn a_resumed_planned_task_waits_for_its_prerequisite_to_merge() {
    let launcher = Arc::new(support::launcher::FakeLauncher::default());
    let (dir, e, _c, prerequisite) = support::orchestrator::fake_run_on(Some(launcher));
    e.manual_dispatch();
    support::orchestrator::fake_repo(dir.path(), &e).await;
    let dependent = TaskId::new("dependent");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    task: dependent.clone(),
                    title: "Use the API".into(),
                    role: RoleId::new("backend"),
                    summary: "Implement caller".into(),
                    review: dvb_core::roles::ReviewKind::Code,
                    after: vec![prerequisite.clone()],
                    routine: false,
                }],
            },
            // The prerequisite is in review, so the resume starts nothing else.
            EventKind::TaskStateChanged {
                task: prerequisite.clone(),
                from: TaskState::Planned,
                to: TaskState::InReview,
            },
        ])
    })
    .unwrap();
    assert_eq!(
        dispatch::stop(&e, &dependent, Actor::Owner).await.unwrap(),
        stopped_planned()
    );
    assert_eq!(
        dispatch::resume(&e, &dependent, Actor::Owner)
            .await
            .unwrap(),
        TaskState::Planned
    );
    wait_for(
        &e,
        "task.waiting",
        |k| matches!(k, EventKind::TaskWaiting { task: t, .. } if *t == dependent),
    )
    .await;
    let t = e.proj().tasks[&dependent].clone();
    assert!(t.facts.run.is_none());
    assert_eq!(t.view.state, TaskState::Planned);
    assert_eq!(t.view.waiting_for, vec![prerequisite.clone()]);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PrMerged {
            task: prerequisite.clone(),
            pr: 1,
            sha: support::git::git(&dir.path().join("repo"), &["rev-parse", "HEAD"]),
        }])
    })
    .unwrap();
    dispatch::start_waiting(e.clone()).await.unwrap();
    let starts = support::events::all(&e)
        .iter()
        .filter(|ev| matches!(&ev.kind, EventKind::TaskStarted { task: t, .. } if *t == dependent))
        .count();
    assert_eq!(starts, 1);
}
