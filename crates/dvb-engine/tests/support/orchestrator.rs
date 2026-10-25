use axum::{Json, extract::State};
use dvb_core::event::PlannedTask;
use dvb_core::git::Git;
use dvb_core::roles::ReviewKind;
use dvb_core::run_verbs::TaskStartBody;
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, ProjectId, RefusalCode, RoleId, RunId, TaskId,
};
use dvb_engine::serve::Caller;
use dvb_engine::{AppState, Fail};
use serde_json::json;
pub fn fake_run() -> (tempfile::TempDir, AppState, Caller, TaskId) {
    fake_run_on(None)
}

pub fn fake_run_on(
    launcher: Option<std::sync::Arc<dyn dvb_core::launcher::Launcher>>,
) -> (tempfile::TempDir, AppState, Caller, TaskId) {
    fake_run_with_options(dvb_engine::EngineOptions {
        launcher,
        ..Default::default()
    })
}

pub fn fake_run_with_options(
    options: dvb_engine::EngineOptions,
) -> (tempfile::TempDir, AppState, Caller, TaskId) {
    fake_run_in(tempfile::tempdir().unwrap(), options)
}

/// The same run, in an engine directory a test has prepared.
pub fn fake_run_in(
    dir: tempfile::TempDir,
    options: dvb_engine::EngineOptions,
) -> (tempfile::TempDir, AppState, Caller, TaskId) {
    let e = super::engine::open_with(dir.path(), "dvb", options);
    let project = ProjectId::new("p");
    let task = TaskId::new("t");
    let goal = GoalId::new("g");
    let mut kinds = vec![super::events::project_event(
        &project,
        "project",
        &dir.path().join("repo").to_string_lossy(),
    )];
    let (roles, hires) = super::events::default_role_hire_events();
    for (role, hire) in roles.into_iter().zip(hires) {
        kinds.push(hire);
        kinds.push(role);
    }
    kinds.extend([
        EventKind::GoalStated {
            goal: goal.clone(),
            text: "Ship it".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: goal.clone(),
            tasks: vec![PlannedTask {
                after: vec![],
                task: task.clone(),
                title: "Build it".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "One edit".into(),
                routine: false,
            }],
        },
        EventKind::GoalApproved { goal },
        EventKind::RunStarted(
            super::assignment::Assignment::new(RunId::new("orchestrator-run"))
                .role(RoleId::new("orchestrator"))
                .hire(HireId::new("h-orchestrator"))
                .reason("Plan".into())
                .base(Some("base".into()))
                .build(),
        ),
    ]);
    kinds.push(EventKind::SettingsChanged {
        key: "max_runs".into(),
        old: json!(null),
        new: json!(3),
        restored_from: None,
    });
    e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    let caller = Caller {
        run: e.proj().runs[&RunId::new("orchestrator-run")].clone(),
    };
    (dir, e, caller, task)
}

pub async fn settings(e: &AppState, key: &str, value: serde_json::Value) {
    use tower::ServiceExt;
    let body = json!({"values": {key: value}});
    let response = dvb_engine::api::router(e.clone())
        .oneshot(
            axum::http::Request::put("/settings")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

pub fn caller(e: &AppState) -> Caller {
    Caller {
        run: e.proj().runs[&RunId::new("orchestrator-run")].clone(),
    }
}

pub async fn fake_repo(dir: &std::path::Path, e: &AppState) {
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    Git::new(&repo)
        .run(&["init", "--initial-branch=main"], &[])
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    Git::new(&repo)
        .run(
            &[
                "-c",
                "user.name=builder",
                "-c",
                "user.email=builder@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
            &[],
        )
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    dvb_engine::publish::create_mirror(&repo.to_string_lossy(), &e.mirror("project"))
        .await
        .unwrap();
}

pub fn start_body(task: &TaskId) -> TaskStartBody {
    TaskStartBody {
        task: task.clone(),
        role: "backend".into(),
        model: None,
        effort: Some(Effort::High),
        reason: format!("Start {task}"),
        routine: false,
    }
}

/// An approved plan of three tasks, with the orchestrator's run live: its
/// first two start, so the run cap is reached. Also a planned task of a goal
/// without the owner's yes, recorded as waiting. Returns the three and the
/// unapproved one.
pub async fn two_tasks_at_the_cap() -> (tempfile::TempDir, AppState, [TaskId; 4]) {
    let (dir, e, _c, t1) = fake_run_on(Some(std::sync::Arc::new(
        super::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
    )));
    fake_repo(dir.path(), &e).await;
    let (t2, t3, other) = (TaskId::new("t2"), TaskId::new("t3"), TaskId::new("u"));
    let planned = |task: &TaskId| PlannedTask {
        after: vec![],
        task: task.clone(),
        title: format!("Build {task}"),
        role: RoleId::new("backend"),
        review: ReviewKind::Code,
        summary: "One edit".into(),
        routine: false,
    };
    let (g, g2) = (GoalId::new("g"), GoalId::new("g2"));
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: g.clone(),
                tasks: vec![planned(&t2), planned(&t3)],
            },
            EventKind::GoalApproved { goal: g },
            EventKind::GoalStated {
                goal: g2.clone(),
                text: "Not yet".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: g2,
                tasks: vec![planned(&other)],
            },
            EventKind::TaskWaiting {
                task: other.clone(),
                model: None,
                effort: None,
                reason: "Not approved".into(),
                routine: false,
            },
        ])
    })
    .unwrap();
    for t in [&t1, &t2] {
        let _run = super::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
            "task/start",
            State(e.clone()),
            caller(&e),
            Json(start_body(t)),
        )
        .await
        .unwrap();
    }
    (dir, e, [t1, t2, t3, other])
}

/// `two_tasks_at_the_cap`, and the third is refused for the run cap and
/// waits (`task.waiting`), recorded with the refusal in one transaction.
pub async fn three_tasks_at_the_cap() -> (tempfile::TempDir, AppState, [TaskId; 4]) {
    use dvb_core::TaskState;
    let (dir, e, [t1, t2, t3, other]) = two_tasks_at_the_cap().await;
    let refused = super::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        caller(&e),
        Json(start_body(&t3)),
    )
    .await;
    assert!(
        matches!(refused, Err(Fail::Refused(r)) if r.code == RefusalCode::CapReached { which: dvb_core::Cap::ConcurrentRuns })
    );
    let events = super::events::all(&e);
    let waiting = events
            .iter()
            .position(|ev| {
                matches!(&ev.kind, EventKind::TaskWaiting { task, effort: Some(Effort::High), .. } if task == &t3)
                    && ev.actor == Actor::Run(RunId::new("orchestrator-run"))
            })
            .expect("task.waiting recorded for the refused start");
    assert!(matches!(
        &events[waiting - 1].kind,
        EventKind::CommandRefused(r) if r.command == "task start"
    ));
    assert_eq!(events[waiting - 1].id.0 + 1, events[waiting].id.0);
    assert_eq!(e.proj().tasks[&t3].view.state, TaskState::Planned);
    (dir, e, [t1, t2, t3, other])
}

pub async fn started(e: &AppState, task: &TaskId) -> RunId {
    for _ in 0..200 {
        if let Some(run) = e.proj().tasks[task].facts.run.clone() {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{task} did not start");
}

/// Ends every run of `three_tasks_at_the_cap`: its two tasks' and the
/// orchestrator's.
pub async fn end_all_runs(e: &AppState, tasks: [&TaskId; 2]) {
    for t in tasks {
        dvb_engine::dispatch::stop(e, t, Actor::Owner)
            .await
            .unwrap();
    }
    e.end_run(
        &RunId::new("orchestrator-run"),
        dvb_core::event::EndReason::Completed,
        None,
    )
    .unwrap();
    for _ in 0..200 {
        if e.proj().live_runs().count() == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("runs still live");
}
