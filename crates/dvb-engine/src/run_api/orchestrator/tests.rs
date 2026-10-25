use crate::Fail;
use crate::test_support as support;
use crate::test_support::orchestrator::fake_run;
use crate::wake;
use axum::{Json, extract::State};
use dvb_core::RefusalCode;
use dvb_core::event::PlannedTask;
use dvb_core::roles::{HireRecord, HireStatus, ReviewKind};
use dvb_core::run_verbs::TaskStartBody;
use dvb_core::{Actor, Effort, EventKind, GoalId, HireId, ProjectId, RoleId, RunId, TaskId};
use dvb_engine::AppState;
use serde_json::json;
use support::orchestrator::{
    caller, fake_repo, fake_run_on, settings, start_body, started, two_tasks_at_the_cap,
};
use support::orchestrator::{end_all_runs, three_tasks_at_the_cap};
#[test]
fn auto_admission_serializes_same_and_different_models_with_owner_hiring() {
    for (second_model, owner_hires) in
        [("fake-next", false), ("fake-1", false), ("fake-next", true)]
    {
        let (_dir, e, _, _) = fake_run();
        let project = ProjectId::new("p");
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::PlanProposed {
                    goal: GoalId::new("g"),
                    tasks: ["t", "t2"]
                        .into_iter()
                        .map(|id| PlannedTask {
                            after: vec![],
                            task: TaskId::new(id),
                            title: id.into(),
                            role: RoleId::new("backend"),
                            review: ReviewKind::Code,
                            summary: "edit".into(),
                            routine: false,
                        })
                        .collect(),
                },
                EventKind::GoalApproved {
                    goal: GoalId::new("g"),
                },
            ])
        })
        .unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(if owner_hires { 4 } else { 3 }));
        std::thread::scope(|scope| {
            for (id, model) in [("t", "fake-next"), ("t2", second_model)] {
                let e = e.clone();
                let gate = gate.clone();
                scope.spawn(move || {
                    let hire = HireRecord {
                        id: HireId::new(format!("new-{id}")),
                        role: RoleId::new("backend"),
                        model: model.into(),
                        effort: Effort::High,
                        status: HireStatus::Active,
                        routine: false,
                    };
                    let assignment = crate::test_support::assignment::Assignment::new(RunId::new(
                        format!("run-{id}"),
                    ))
                    .task(Some(TaskId::new(id)))
                    .role(hire.role.clone())
                    .hire(hire.id.clone())
                    .model(hire.model.clone())
                    .effort(hire.effort)
                    .base(Some("base".into()))
                    .reason("edit".into())
                    .build();
                    gate.wait();
                    let admitted = e
                        .admit_task_hiring(
                            &TaskId::new(id),
                            Actor::Engine,
                            assignment,
                            Some(hire),
                            None,
                        )
                        .unwrap();
                    assert_eq!(e.proj().runs[&admitted.run].assignment, admitted);
                });
            }
            if owner_hires {
                let e = e.clone();
                let gate = gate.clone();
                scope.spawn(move || {
                    gate.wait();
                    crate::proj::hires::create_hire(
                        &e,
                        dvb_core::api::NewHire {
                            role: RoleId::new("backend"),
                            model: "fake-1".into(),
                            effort: Effort::Medium,
                            routine: false,
                        },
                        &Actor::Owner,
                        None,
                    )
                    .unwrap();
                });
            }
            gate.wait();
        });
        let proj = e.proj();
        let hires = &proj.projects[&project].hires;
        assert_eq!(
            hires
                .iter()
                .filter(|h| h.role == RoleId::new("backend") && h.status == HireStatus::Active)
                .count(),
            1
        );
        if second_model == "fake-next" && !owner_hires {
            assert_eq!(
                proj.runs[&RunId::new("run-t")].assignment.hire,
                proj.runs[&RunId::new("run-t2")].assignment.hire
            );
        }
    }
}

#[test]
fn capped_auto_admission_records_waiting_without_retiring_or_hiring() {
    let (_dir, e, _, task) = fake_run();
    let project = ProjectId::new("p");
    let mut assignment = e.proj().runs[&RunId::new("orchestrator-run")]
        .assignment
        .clone();
    for id in ["slot-2", "slot-3"] {
        assignment.run = RunId::new(id);
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(assignment.clone())])
        })
        .unwrap();
    }
    assignment.run = RunId::new("capped");
    assignment.task = Some(task.clone());
    assignment.role = RoleId::new("backend");
    assignment.hire = HireId::new("new");
    assignment.model = "fake-next".into();
    let hire = HireRecord {
        id: assignment.hire.clone(),
        role: assignment.role.clone(),
        model: assignment.model.clone(),
        effort: assignment.effort,
        status: HireStatus::Active,
        routine: false,
    };
    let before = e.store.last_id().unwrap();
    let hires = e.proj().projects[&project].hires.clone();
    let waiting = EventKind::TaskWaiting {
        task: task.clone(),
        model: Some(assignment.model.clone()),
        effort: None,
        reason: "edit".into(),
        routine: false,
    };
    assert!(
        matches!(e.admit_task_hiring(&task, Actor::Engine, assignment, Some(hire), Some(waiting.clone())), Err(Fail::Refused(r)) if r.code == RefusalCode::CapReached { which: dvb_core::Cap::ConcurrentRuns })
    );
    assert_eq!(e.proj().projects[&project].hires, hires);
    let events = e.store.after(before).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].kind, EventKind::CommandRefused(_)));
    assert_eq!(events[1].kind, waiting);
}

/// Retain an automatic admission at the run cap, then free the synthetic
/// slots without waking dispatch so tests can race or guard its replay.
async fn defer_automatic(e: &AppState, task: &TaskId) {
    let p = ProjectId::new("p");
    let busy: Vec<_> = (1..3).map(|i| RunId::new(format!("busy-{i}"))).collect();
    let a = caller(e).run.assignment;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(busy
            .iter()
            .map(|run| {
                EventKind::RunStarted(
                    crate::test_support::assignment::Assignment::from_assignment(a.clone())
                        .review_stage(None)
                        .goal_version(None)
                        .run(run.clone())
                        .build(),
                )
            })
            .collect())
    })
    .unwrap();
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    assert!(e.proj().tasks[task].waiting.is_some());
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(busy
            .into_iter()
            .map(|run| EventKind::RunEnded {
                run,
                reason: dvb_core::event::EndReason::Completed,
                detail: None,
            })
            .collect())
    })
    .unwrap();
}

#[tokio::test]
async fn auto_admission_waits_for_the_orchestrators_model_effort_and_reason() {
    let (dir, e, c, task) = fake_run_on(Some(std::sync::Arc::new(
        support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
    )));
    fake_repo(dir.path(), &e).await;
    settings(&e, "model_mode", json!("Auto")).await;
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    assert!(e.proj().tasks[&task].facts.run.is_none());
    assert!(e.proj().tasks[&task].waiting.is_none());
    let body = TaskStartBody {
        model: Some("fake-next".into()),
        effort: Some(Effort::High),
        reason: "Use the second model for this task".into(),
        ..start_body(&task)
    };
    let run = support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
        "task/start",
        State(e.clone()),
        c,
        Json(body.clone()),
    )
    .await
    .unwrap()
    .0
    .run
    .unwrap();
    let a = e.proj().runs[&run].assignment.clone();
    assert_eq!(Some(a.model), body.model);
    assert_eq!(Some(a.effort), body.effort);
    assert_eq!(a.reason, body.reason);
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    assert_eq!(e.proj().tasks[&task].facts.run.as_ref(), Some(&run));
}

#[tokio::test]
async fn automatic_admission_at_the_cap_leaves_a_missing_hire_for_the_orchestrator() {
    let (_dir, e, [_t1, _t2, task, _other]) = two_tasks_at_the_cap().await;
    let p = ProjectId::new("p");
    let mut hire = e.proj().projects[&p]
        .hire(&RoleId::new("backend"), None)
        .unwrap()
        .clone();
    hire.status = HireStatus::Retired;
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::HireRetired(hire)])
    })
    .unwrap();
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    let proj = e.proj();
    assert_eq!(proj.live_runs().count(), 3);
    assert!(proj.tasks[&task].facts.run.is_none());
    assert!(proj.tasks[&task].waiting.is_none());
    assert!(proj.goal_needs_orchestrator(&proj.goals[&GoalId::new("g")], &e.pool));
    drop(proj);
}

#[tokio::test]
async fn recorded_model_or_effort_and_reason_survive_deferred_restart_in_both_modes() {
    for mode in ["Custom", "Auto"] {
        for model in [None, Some("fake-next".to_owned())] {
            let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
                support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
            )));
            fake_repo(dir.path(), &e).await;
            defer_automatic(&e, &task).await;
            settings(&e, "model_mode", json!(mode)).await;
            let p = ProjectId::new("p");
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::HireAdded(HireRecord {
                        id: HireId::new("second"),
                        role: RoleId::new("backend"),
                        model: "fake-next".into(),
                        effort: Effort::Medium,
                        status: HireStatus::Active,
                        routine: false,
                    }),
                    EventKind::TaskWaiting {
                        task: task.clone(),
                        model: model.clone(),
                        effort: Some(Effort::High),
                        reason: "Keep this recorded choice".into(),
                        routine: false,
                    },
                ])
            })
            .unwrap();
            let restored = support::engine::open_config(
                e.cfg.clone(),
                dvb_engine::EngineOptions {
                    pool: Some(e.pool.clone()),
                    forge: None,
                    launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                        dvb_core::launcher::UnitState::Inactive,
                    ))),
                    ..Default::default()
                },
            );
            dvb_engine::dispatch::start_waiting(restored.clone())
                .await
                .unwrap();
            let run = started(&restored, &task).await;
            let a = restored.proj().runs[&run].assignment.clone();
            assert_eq!(a.model, model.unwrap_or_else(|| "fake-1".into()));
            assert_eq!(a.effort, Effort::High);
            assert_eq!(a.reason, "Keep this recorded choice");
        }
    }
}

#[tokio::test]
async fn an_unchosen_automatic_request_replayed_in_auto_mode_returns_to_the_orchestrator() {
    let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
        support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
    )));
    fake_repo(dir.path(), &e).await;
    defer_automatic(&e, &task).await;
    settings(&e, "model_mode", json!("Auto")).await;
    let restored = support::engine::open_config(
        e.cfg.clone(),
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                dvb_core::launcher::UnitState::Inactive,
            ))),
            ..Default::default()
        },
    );
    dvb_engine::dispatch::start_waiting(restored.clone())
        .await
        .unwrap();
    let proj = restored.proj();
    assert!(proj.tasks[&task].facts.run.is_none());
    assert!(proj.has_waiting_tasks(&proj.goals[&GoalId::new("g")], &e.pool));
    assert_eq!(
        proj.goal_for_wake(&ProjectId::new("p"), Some(wake::Wake::Approved), &e.pool)
            .unwrap()
            .id,
        GoalId::new("g")
    );
}

#[tokio::test]
async fn deferred_automatic_admission_keeps_its_guards_and_goal_version_after_replay() {
    for guard in [
        "paused",
        "stopped",
        "dropped",
        "unapproved",
        "reapproved",
        "no-hire",
    ] {
        let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
            support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
        )));
        fake_repo(dir.path(), &e).await;
        defer_automatic(&e, &task).await;
        let (p, g) = (ProjectId::new("p"), GoalId::new("g"));
        match guard {
            "paused" => dvb_engine::owner::pause(&e, &p, true).unwrap(),
            "stopped" => {
                e.decide(
                    e.proj(),
                    &task,
                    Actor::Owner,
                    dvb_engine::Decision::new("stop", dvb_core::Command::Stop(None)),
                    |_| Ok(()),
                )
                .unwrap();
            }
            "dropped" => {
                dvb_engine::owner::drop_goal(&e, &g).unwrap();
            }
            "unapproved" | "reapproved" => {
                let mut changed = vec![EventKind::GoalStated {
                    goal: g.clone(),
                    text: "A different version".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                }];
                if guard == "reapproved" {
                    changed.push(EventKind::GoalApproved { goal: g });
                }
                e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(changed))
                    .unwrap();
            }
            "no-hire" => {
                let mut h = e.proj().projects[&p]
                    .hire(&RoleId::new("backend"), None)
                    .unwrap()
                    .clone();
                h.status = HireStatus::Retired;
                e.append_checked(Some(&p), Actor::Owner, |_, _| {
                    Ok(vec![EventKind::HireRetired(h)])
                })
                .unwrap();
            }
            _ => unreachable!(),
        }
        let replay = dvb_engine::proj::Projections::rebuild(&crate::test_support::events::all(&e));
        assert_eq!(
            replay.tasks[&task]
                .waiting
                .as_ref()
                .unwrap()
                .approved_version,
            e.proj().tasks[&task]
                .waiting
                .as_ref()
                .unwrap()
                .approved_version
        );
        dvb_engine::dispatch::start_waiting(e.clone())
            .await
            .unwrap();
        assert!(
            e.proj().tasks[&task].facts.run.is_none(),
            "{guard} allowed a run"
        );
        if guard == "paused" {
            dvb_engine::owner::pause(&e, &p, false).unwrap();
            dvb_engine::dispatch::start_waiting(e.clone())
                .await
                .unwrap();
            started(&e, &task).await;
        }
    }
}

#[tokio::test]
async fn automatic_admission_respects_pause_stop_goal_approval_and_hire_guards() {
    use dvb_core::{Command, TaskState};
    for guard in [
        "paused",
        "stopped",
        "dropped",
        "redirected",
        "no-hire",
        "removed-model",
        "already-started",
    ] {
        let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
            support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
        )));
        fake_repo(dir.path(), &e).await;
        let (p, g) = (ProjectId::new("p"), GoalId::new("g"));
        match guard {
            "paused" => {
                dvb_engine::owner::pause(&e, &p, true).unwrap();
            }
            "stopped" => {
                e.decide(
                    e.proj(),
                    &task,
                    Actor::Owner,
                    dvb_engine::Decision::new("stop", Command::Stop(None)),
                    |_| Ok(()),
                )
                .unwrap();
            }
            "dropped" => {
                dvb_engine::owner::drop_goal(&e, &g).unwrap();
            }
            "redirected" => {
                // A newer goal version has no approval, even though the
                // older task retains its own historic yes.
                e.append_checked(Some(&p), Actor::Owner, |_, _| {
                    Ok(vec![EventKind::GoalStated {
                        goal: g.clone(),
                        text: "A new direction".into(),
                        from_issue: None,
                        issues: Vec::new(),
                        after: None,
                    }])
                })
                .unwrap();
            }
            "no-hire" => {
                let mut hire = e.proj().projects[&p]
                    .hire(&RoleId::new("backend"), None)
                    .unwrap()
                    .clone();
                hire.status = HireStatus::Retired;
                e.append_checked(Some(&p), Actor::Owner, |_, _| {
                    Ok(vec![EventKind::HireRetired(hire)])
                })
                .unwrap();
            }
            "removed-model" => {
                settings(&e, "model_pool", json!(["fake-next"])).await;
            }
            "already-started" => {
                let a = crate::test_support::assignment::Assignment::from_assignment(
                    caller(&e).run.assignment,
                )
                .review_stage(None)
                .goal_version(None)
                .run(RunId::new("prior"))
                .task(Some(task.clone()))
                .build();
                e.append_checked(Some(&p), Actor::Engine, |_, _| {
                    Ok(vec![EventKind::RunStarted(a)])
                })
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = e.store.last_id().unwrap();
        crate::dispatch::start_approved(e.clone(), g).await.unwrap();
        assert!(
            !e.store
                .after(before)
                .unwrap()
                .iter()
                .any(|ev| matches!(ev.kind, EventKind::RunStarted(_))),
            "{guard} allowed a run"
        );
        if guard == "stopped" {
            assert_eq!(
                e.proj().tasks[&task].view.state,
                TaskState::Stopped {
                    was: Box::new(TaskState::Planned)
                }
            );
        }
    }
}

#[tokio::test]
async fn automatic_admission_is_scoped_to_the_approved_goal() {
    let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
        support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
    )));
    fake_repo(dir.path(), &e).await;
    let (p, g) = (ProjectId::new("p"), GoalId::new("g"));
    let mut others = Vec::new();
    for (id, approve) in [("other", true), ("unapproved", false)] {
        let goal = GoalId::new(id);
        let task = TaskId::new(id);
        others.extend([
            EventKind::GoalStated {
                goal: goal.clone(),
                text: id.into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: goal.clone(),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task,
                    title: id.into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: id.into(),
                    routine: false,
                }],
            },
        ]);
        if approve {
            others.push(EventKind::GoalApproved { goal });
        }
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(others))
        .unwrap();
    crate::dispatch::start_approved(e.clone(), g).await.unwrap();
    assert!(e.proj().tasks[&task].facts.run.is_some());
    for id in ["other", "unapproved"] {
        assert!(e.proj().tasks[&TaskId::new(id)].facts.run.is_none());
    }
    crate::dispatch::start_approved(e.clone(), GoalId::new("unapproved"))
        .await
        .unwrap();
    assert!(
        e.proj().tasks[&TaskId::new("unapproved")]
            .facts
            .run
            .is_none()
    );
}

#[tokio::test]
async fn an_automatic_start_waits_for_disk_and_update_admission() {
    let units = std::sync::Arc::new(support::launcher::FakeLauncher::new(
        dvb_core::launcher::UnitState::Inactive,
    ));
    let readings = support::engine::Readings::default();
    let (dir, e, _c, task) =
        support::orchestrator::fake_run_with_options(readings.options(crate::EngineOptions {
            launcher: Some(units.clone()),
            ..Default::default()
        }));
    fake_repo(dir.path(), &e).await;
    *readings.disk.lock().unwrap() = Some(0);
    e.update.close_admission();
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    let run = started(&e, &task).await;
    let mut waiting_for_disk = false;
    for _ in 0..200 {
        if crate::test_support::events::all(&e).iter().any(|ev| {
            matches!(&ev.kind,
                EventKind::RunOutput { resource: Some(dvb_core::event::ResourceWait::Disk), run: r, .. } if *r == run)
        }) {
            waiting_for_disk = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        waiting_for_disk,
        "automatic run did not wait for disk space"
    );
    assert!(!units.is_active(&run));
    *readings.disk.lock().unwrap() = Some(u64::MAX);
    e.disk_freed.notify_waiters();
    // A negative check needs a window: let the freed launch reach update admission.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!units.is_active(&run));
    e.update.reopen_admission();
    for _ in 0..200 {
        if units.is_active(&run) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("automatic run did not launch after disk and update gates opened");
}

#[tokio::test]
async fn an_orchestrator_start_racing_automatic_admission_starts_only_one_run() {
    for (automatic_first, deferred) in [(true, false), (false, false), (true, true), (false, true)]
    {
        let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
            support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
        )));
        fake_repo(dir.path(), &e).await;
        if deferred {
            defer_automatic(&e, &task).await;
        }
        let held = e.lock(&task).await;
        let mut automatic = Box::pin(crate::dispatch::start_approved(e.clone(), GoalId::new("g")));
        let mut explicit = Box::pin(
            support::http::run::<dvb_core::run_verbs::TaskStartReply, _>(
                "task/start",
                State(e.clone()),
                caller(&e),
                Json(start_body(&task)),
            ),
        );
        // Poll both starts through base selection and into the held task
        // lock, in either order. Neither can record its decision yet.
        let wait = std::time::Duration::from_millis(100);
        if automatic_first {
            assert!(tokio::time::timeout(wait, &mut automatic).await.is_err());
            assert!(tokio::time::timeout(wait, &mut explicit).await.is_err());
        } else {
            assert!(tokio::time::timeout(wait, &mut explicit).await.is_err());
            assert!(tokio::time::timeout(wait, &mut automatic).await.is_err());
        }
        drop(held);
        let (automatic, explicit) = tokio::join!(automatic, explicit);
        automatic.unwrap();
        // Whichever wins, the other start changes nothing and nothing is refused.
        let reply = explicit.unwrap();
        let run = started(&e, &task).await;
        assert_eq!(reply.run, Some(run));
        let events = crate::test_support::events::all(&e);
        assert!(
            !events
                .iter()
                .any(|ev| matches!(&ev.kind, EventKind::CommandRefused(_)))
        );
        assert_eq!(events.iter().filter(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.task.as_ref() == Some(&task))).count(), 1);
        assert_eq!(
            events
                .iter()
                .filter(
                    |ev| matches!(&ev.kind, EventKind::TaskStarted { task: t, .. } if t == &task)
                )
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn automatic_admission_rechecks_goal_version_with_the_start_decision() {
    for (reapproved, deferred) in [(false, false), (true, false), (false, true), (true, true)] {
        let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
            support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
        )));
        fake_repo(dir.path(), &e).await;
        if deferred {
            defer_automatic(&e, &task).await;
        }
        let held = e.lock(&task).await;
        let mut start = Box::pin(crate::dispatch::start_approved(e.clone(), GoalId::new("g")));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut start)
                .await
                .is_err()
        );
        let mut changed = vec![EventKind::GoalStated {
            goal: GoalId::new("g"),
            text: "New version".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }];
        if reapproved {
            changed.push(EventKind::GoalApproved {
                goal: GoalId::new("g"),
            });
        }
        e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(changed))
            .unwrap();
        let before = e.store.last_id().unwrap();
        drop(held);
        start.await.unwrap();
        assert!(e.proj().tasks[&task].facts.run.is_none());
        assert!(e.store.after(before).unwrap().iter().any(|ev| matches!(&ev.kind, EventKind::CommandRefused(r) if r.refusal.current().unwrap().code == RefusalCode::GoalNotApproved)));
    }
}

#[tokio::test]
async fn automatic_admission_rechecks_model_mode_and_hire_with_the_start_decision() {
    for deferred in [false, true] {
        for auto in [false, true] {
            let (dir, e, _c, task) = fake_run_on(Some(std::sync::Arc::new(
                support::launcher::FakeLauncher::new(dvb_core::launcher::UnitState::Inactive),
            )));
            fake_repo(dir.path(), &e).await;
            if deferred {
                defer_automatic(&e, &task).await;
            }
            let held = e.lock(&task).await;
            let mut start = Box::pin(crate::dispatch::start_approved(e.clone(), GoalId::new("g")));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut start)
                    .await
                    .is_err()
            );
            if auto {
                settings(&e, "model_mode", json!("Auto")).await;
            } else {
                let p = ProjectId::new("p");
                let mut hire = e.proj().projects[&p]
                    .hire(&RoleId::new("backend"), None)
                    .unwrap()
                    .clone();
                hire.status = HireStatus::Retired;
                e.append_checked(Some(&p), Actor::Owner, |_, _| {
                    Ok(vec![EventKind::HireRetired(hire)])
                })
                .unwrap();
            }
            drop(held);
            start.await.unwrap();
            let proj = e.proj();
            assert!(proj.tasks[&task].facts.run.is_none());
            assert!(proj.goal_needs_orchestrator(&proj.goals[&GoalId::new("g")], &e.pool));
            drop(proj);
        }
    }
}

/// a waiting start that could not read the forge is tried again on its
/// own, with no run left whose end would try it.
#[tokio::test]
async fn a_waiting_start_that_failed_is_tried_again_with_no_runs_left() {
    let (dir, e, [t1, t2, t3, _other]) = three_tasks_at_the_cap().await;
    let (repo, gone) = (dir.path().join("repo"), dir.path().join("gone"));
    std::fs::rename(&repo, &gone).unwrap();
    end_all_runs(&e, [&t1, &t2]).await;
    support::wait::FAST
        .until("scheduled waiting retry", || {
            e.waiting_again.load(std::sync::atomic::Ordering::SeqCst)
        })
        .await;
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert!(e.proj().tasks[&t3].facts.run.is_none());
    assert!(e.proj().tasks[&t3].waiting.is_some());
    assert_eq!(e.proj().live_runs().count(), 0);
    std::fs::rename(&gone, &repo).unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    started(&e, &t3).await;
}

#[tokio::test]
async fn automatic_approval_retains_the_dependent_hire_and_never_starts_it_before_a_merge() {
    let launcher = std::sync::Arc::new(support::launcher::FakeLauncher::default());
    let (dir, e, _c, prerequisite) = fake_run_on(Some(launcher));
    e.manual_dispatch();
    fake_repo(dir.path(), &e).await;
    let dependent = TaskId::new("dependent");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![PlannedTask {
                task: dependent.clone(),
                title: "Caller".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "Use the merged API".into(),
                after: vec![prerequisite.clone()],
                routine: false,
            }],
        }])
    })
    .unwrap();
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    assert!(e.proj().tasks[&prerequisite].facts.run.is_some());
    let replay = crate::proj::Projections::rebuild(&crate::test_support::events::all(&e));
    let waiting = &replay.tasks[&dependent];
    assert!(waiting.facts.run.is_none());
    assert_eq!(waiting.view.waiting_for, vec![prerequisite.clone()]);
    assert_eq!(
        waiting.waiting.as_ref().unwrap().hire,
        Some(HireId::new("h-backend"))
    );
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: prerequisite,
            from: dvb_core::TaskState::Preparing,
            to: dvb_core::TaskState::Failed {
                reason: "CI failed".into(),
            },
        }])
    })
    .unwrap();
    crate::dispatch::start_approved(e.clone(), GoalId::new("g"))
        .await
        .unwrap();
    assert!(e.proj().tasks[&dependent].facts.run.is_none());
}
