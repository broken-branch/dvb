//! J20 acceptance uses a local Fake forge and durable engine events, with no sockets.
mod support;
use dvb_core::event::PlannedTask;
use dvb_core::forge::{BoardState, Forge, Repo};
use dvb_core::roles::{ReviewKind, default_records};
use dvb_core::{
    Actor, Clock, EventKind, FakeClock, GoalId, ProjectId, TaskId, TaskState, Timestamp,
};
use dvb_engine::{AppState, publish::mirror};
use std::sync::Arc;
use support::InterruptedForge;
use tokio::sync::Barrier;

type Fixture = (
    tempfile::TempDir,
    AppState,
    ProjectId,
    GoalId,
    Vec<TaskId>,
    Arc<dvb_forge::FakeForge>,
);

fn fixture() -> Fixture {
    fixture_with(None)
}

fn fixture_with(forge: Option<Arc<dyn Forge>>) -> Fixture {
    fixture_with_clock(forge, Arc::new(dvb_core::SystemClock))
}

fn fixture_with_clock(forge: Option<Arc<dyn Forge>>, clock: Arc<dyn Clock>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let _fake = Arc::new(dvb_forge::FakeForge::local_git());
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            clock: Some(clock),
            forge: Some(forge.unwrap_or_else(|| _fake.clone())),
            ..Default::default()
        },
    );
    let project = ProjectId::new("project");
    let goal = GoalId::new("goal");
    let roles = default_records(|role| dvb_core::RoleId::new(role.slug));
    let role = roles
        .iter()
        .find(|r| r.slug == "backend")
        .unwrap()
        .id
        .clone();
    let tasks = vec![TaskId::new("one"), TaskId::new("two")];
    let mut events = vec![support::events::project_event(
        &project, "Project", "org/repo",
    )];
    events.extend(roles.into_iter().map(EventKind::RoleAdded));
    events.push(EventKind::GoalStated {
        goal: goal.clone(),
        text: "Ship the feature".into(),
        from_issue: None,
        issues: Vec::new(),
        after: None,
    });
    events.push(EventKind::PlanProposed {
        goal: goal.clone(),
        tasks: tasks
            .iter()
            .map(|id| PlannedTask {
                after: vec![],
                task: id.clone(),
                title: format!("Build {id}"),
                role: role.clone(),
                review: ReviewKind::Code,
                summary: "Implement the feature".into(),
                routine: false,
            })
            .collect(),
    });
    e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    (dir, e, project, goal, tasks, _fake)
}
fn state(e: &dvb_engine::AppState, project: &ProjectId, task: &TaskId, to: TaskState) {
    let from = e.proj().tasks[task].view.state.clone();
    e.append_checked(Some(project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from,
            to,
        }])
    })
    .unwrap();
}

#[tokio::test]
async fn milestone_completion_and_task_cleanup_use_their_own_snapshot_states() {
    let (_dir, e, project, goal, tasks, _fake) = fixture();
    let repo = Repo("org/repo".into());
    let forge = &_fake;
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalApproved { goal: goal.clone() }])
    })
    .unwrap();
    for task in &tasks {
        state(&e, &project, task, TaskState::Done);
    }
    // The dispatcher records task.done before it records goal.done. Capture
    // exactly that boundary rather than depending on a scheduler to hit it.
    mirror::sync(&e).await.unwrap();
    let milestones = forge.milestones(&repo);
    assert_eq!(milestones.len(), 1);
    assert!(!milestones[0].1.done);
    assert_eq!(
        milestones[0].1.description,
        "Ship the feature\n- Done: Build one\n- Done: Build two"
    );
    let id = milestones[0].0;
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone { goal: goal.clone() }])
    })
    .unwrap();
    mirror::sync(&e).await.unwrap();
    let milestones = forge.milestones(&repo);
    assert_eq!(milestones[0].0, id);
    assert!(milestones[0].1.done);
    assert_eq!(
        milestones[0].1.description,
        "Ship the feature\n- Done: Build one\n- Done: Build two"
    );
}

#[tokio::test]
async fn a_public_repositorys_milestone_carries_only_the_task_lines() {
    let (_dir, e, project, goal, tasks, forge) = fixture();
    let repo = Repo("org/repo".into());
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalApproved { goal: goal.clone() }])
    })
    .unwrap();
    for task in &tasks {
        state(&e, &project, task, TaskState::Done);
    }
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone { goal: goal.clone() }])
    })
    .unwrap();
    assert!(mirror::sync(&e).await.unwrap());
    let private = forge.milestones(&repo);
    assert_eq!(
        private[0].1.description,
        "Ship the feature\n- Done: Build one\n- Done: Build two"
    );
    // Visibility is read on every pass, so a repository that turns public is
    // rewritten, even for a finished goal.
    forge.set_public(&repo);
    assert!(mirror::sync(&e).await.unwrap());
    let public = forge.milestones(&repo);
    assert_eq!(public[0].0, private[0].0);
    assert_eq!(public[0].1.title, "Ship the feature");
    assert_eq!(
        public[0].1.description,
        "- Done: Build one\n- Done: Build two"
    );
}

#[tokio::test]
async fn a_paused_project_starts_no_mirror_writes_and_resume_wakes_the_watcher() {
    let forge = Arc::new(InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::default(),
    )));
    let (_dir, e, project, _goal, tasks, _fake) = fixture_with(Some(forge.clone()));
    let repo = Repo("org/repo".into());
    dvb_engine::owner::pause(&e, &project, true).unwrap();
    mirror::sync(&e).await.unwrap();
    assert!(forge.mirror_writes.lock().unwrap().is_empty());

    let mut events = e.subscribe();
    let watcher = tokio::spawn(mirror::watch(e.clone()));
    // Let the watcher subscribe, perform its paused snapshot, and wait for events.
    tokio::task::yield_now().await;
    assert!(forge.mirror_writes.lock().unwrap().is_empty());
    dvb_engine::owner::pause(&e, &project, false).unwrap();
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::GithubMirrorObserved { task: Some(ref id), mirrored: true, .. } if id == &tasks[1]) {
                break;
            }
        }
    }).await;
    watcher.abort();
    let _ = watcher.await;
    resumed.expect("ProjectResumed must wake the mirror without waiting for its periodic timer");
    assert_eq!(
        *forge.mirror_writes.lock().unwrap(),
        ["project", "goal", "task", "task"]
    );
    assert_eq!(forge.inner.milestones(&repo).len(), 1);
    assert_eq!(forge.inner.cards(&repo).len(), 2);
}

#[tokio::test]
async fn pause_allows_the_inflight_mirror_write_to_finish_but_starts_no_next_write() {
    for (blocked, expected) in [
        ("project", vec!["project"]),
        ("goal", vec!["project", "goal"]),
        ("task", vec!["project", "goal", "task"]),
    ] {
        let barrier = Arc::new(Barrier::new(2));
        let mut forge = InterruptedForge::new(Arc::new(dvb_forge::FakeForge::default()));
        forge.mirror_barrier = Some((blocked, barrier.clone()));
        let forge = Arc::new(forge);
        let (_dir, e, project, _goal, _tasks, _fake) = fixture_with(Some(forge.clone()));
        let repo = Repo("org/repo".into());
        let worker = e.clone();
        let sync = tokio::spawn(async move { mirror::sync(&worker).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), barrier.wait())
            .await
            .unwrap();
        dvb_engine::owner::pause(&e, &project, true).unwrap();
        barrier.wait().await;
        sync.await.unwrap().unwrap();
        assert_eq!(
            *forge.mirror_writes.lock().unwrap(),
            expected,
            "paused during {blocked}"
        );
        assert_eq!(
            forge.inner.milestones(&repo).len(),
            usize::from(blocked != "project")
        );
        assert_eq!(
            forge.inner.cards(&repo).len(),
            usize::from(blocked == "task")
        );
        // A fresh snapshot while still paused must not touch even existing mirrors.
        mirror::sync(&e).await.unwrap();
        assert_eq!(*forge.mirror_writes.lock().unwrap(), expected);
        dvb_engine::owner::pause(&e, &project, false).unwrap();
        mirror::sync(&e).await.unwrap();
        assert_eq!(forge.inner.milestones(&repo).len(), 1);
        assert_eq!(forge.inner.cards(&repo).len(), 2);
    }
}

#[tokio::test]
async fn two_prs_share_one_milestone_cards_move_and_manual_edits_are_overwritten() {
    let (_dir, e, project, goal, tasks, _fake) = fixture();
    let repo = Repo("org/repo".into());
    let forge = &_fake;
    mirror::sync(&e).await.unwrap();
    assert_eq!(forge.milestones(&repo).len(), 1);
    assert_eq!(forge.cards(&repo).len(), 2);
    for (index, task) in tasks.iter().enumerate() {
        for (to, column) in [
            (TaskState::Running, BoardState::Working),
            (TaskState::InReview, BoardState::InReview),
            (TaskState::Merging, BoardState::ReadyToMerge),
            (TaskState::Done, BoardState::Done),
        ] {
            state(&e, &project, task, to.clone());
            if column == BoardState::InReview {
                e.append_checked(Some(&project), Actor::Engine, |_, _| {
                    Ok(vec![EventKind::PrOpened {
                        task: task.clone(),
                        pr: 20 + index as u64,
                        url: "https://github.invalid/pr".into(),
                    }])
                })
                .unwrap();
            }
            mirror::sync(&e).await.unwrap();
            let card = forge
                .cards(&repo)
                .into_iter()
                .find(|c| c.key == task.to_string())
                .unwrap();
            assert_eq!(card.state, column);
            assert_eq!(card.milestone, 1);
            assert_eq!(card.role, "Backend builder");
            let mut changed = card.clone();
            changed.state = BoardState::Failed;
            changed.title = "Manual edit".into();
            forge.mirror_task(&repo, &changed).await.unwrap();
            assert_eq!(e.proj().tasks[task].view.state, to);
            mirror::sync(&e).await.unwrap();
            assert_eq!(
                forge
                    .cards(&repo)
                    .into_iter()
                    .find(|c| c.key == task.to_string())
                    .unwrap(),
                card
            );
        }
    }
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone { goal: goal.clone() }])
    })
    .unwrap();
    mirror::sync(&e).await.unwrap();
    let milestones = forge.milestones(&repo);
    assert_eq!(milestones.len(), 1);
    assert!(milestones[0].1.done);
    assert!(milestones[0].1.description.contains("Done: Build one"));
    assert_eq!(
        forge
            .cards(&repo)
            .iter()
            .map(|c| c.pr.unwrap())
            .collect::<Vec<_>>(),
        vec![20, 21]
    );
    assert!(forge.closed_issues(&repo).is_empty());
    let n = support::events::all(&e).len();
    mirror::sync(&e).await.unwrap();
    assert_eq!(
        support::events::all(&e).len(),
        n,
        "identical mirror observations are not logged twice"
    );
}
#[tokio::test]
async fn scan_hits_mark_work_as_not_mirrored_and_recovery_clears_the_mark() {
    let (dir, e, project, goal, tasks, _fake) = fixture();
    let repo = Repo("org/repo".into());
    state(&e, &project, &tasks[0], TaskState::Running);
    e.proj().goals.get_mut(&goal).unwrap().text = format!("ghp_{}", "a".repeat(30));
    mirror::sync(&e).await.unwrap();
    assert!(_fake.milestones(&repo).is_empty());
    let failed = support::events::all(&e);
    assert!(failed.iter().any(|ev|matches!(&ev.kind,EventKind::GithubMirrorObserved {mirrored:false,detail:Some(detail),..} if detail.contains("github-token"))));
    assert!(
        failed
            .iter()
            .filter_map(
                |ev| if let EventKind::GithubMirrorObserved { detail, .. } = &ev.kind {
                    detail.as_ref()
                } else {
                    None
                }
            )
            .all(|d| !d.contains(&"a".repeat(30)))
    );
    drop(e); // Restart rebuilds only from authoritative engine events.
    let restarted_fake = Arc::new(dvb_forge::FakeForge::local_git());
    let restarted = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            forge: Some(restarted_fake.clone()),
            ..Default::default()
        },
    );
    mirror::sync(&restarted).await.unwrap();
    assert_eq!(
        restarted.proj().tasks[&tasks[0]].view.state,
        TaskState::Running
    );
    assert_eq!(restarted_fake.milestones(&repo).len(), 1);
    assert!(matches!(
        support::events::all(&restarted).last().unwrap().kind,
        EventKind::GithubMirrorObserved { mirrored: true, .. }
    ));
}

#[tokio::test]
async fn an_update_holds_mirror_effects_until_admission_reopens() {
    let (_dir, e, _project, _goal, _tasks, _fake) = fixture();
    let repo = Repo("org/repo".into());
    e.update.close_admission();
    let worker = e.clone();
    let sync = tokio::spawn(async move { mirror::sync(&worker).await });
    tokio::task::yield_now().await;
    assert!(_fake.milestones(&repo).is_empty());
    assert_eq!(e.update.effects_inflight(), 0);
    e.update.reopen_admission();
    sync.await.unwrap().unwrap();
    assert_eq!(_fake.milestones(&repo).len(), 1);
    assert_eq!(e.update.effects_inflight(), 0);
}

#[tokio::test]
async fn a_reviewer_run_does_not_replace_the_builders_model_on_the_card() {
    let (_dir, e, project, _goal, tasks, _fake) = fixture();
    let repo = Repo("org/repo".into());
    let builder_role = e.proj().tasks[&tasks[0]].view.role.clone();
    let reviewer_role = e.proj().projects[&project]
        .role_by_slug("code-reviewer")
        .unwrap()
        .id
        .clone();
    let builder = support::assignment::Assignment::new(dvb_core::RunId::new("builder"))
        .task(Some(tasks[0].clone()))
        .role(builder_role)
        .hire(dvb_core::HireId::new("builder-hire"))
        .model("builder-model".into())
        .vendor(dvb_core::Vendor::Codex)
        .effort(dvb_core::Effort::High)
        .reason("Build the task".into())
        .base(Some("base".into()))
        .build();
    let reviewer = support::assignment::Assignment::from_assignment(builder.clone())
        .review_stage(None)
        .goal_version(None)
        .run(dvb_core::RunId::new("reviewer"))
        .role(reviewer_role)
        .model("reviewer-model".into())
        .reviews(Some("head".into()))
        .build();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(builder),
            EventKind::RunStarted(reviewer),
        ])
    })
    .unwrap();
    state(&e, &project, &tasks[0], TaskState::InReview);
    mirror::sync(&e).await.unwrap();
    let card = _fake
        .cards(&repo)
        .into_iter()
        .find(|c| c.key == tasks[0].to_string())
        .unwrap();
    assert_eq!(card.role, "Backend builder");
    assert_eq!(card.model, "builder-model");
    assert_eq!(card.state, BoardState::InReview);
}

fn attempts(forge: &InterruptedForge, operation: &str) -> usize {
    forge
        .mirror_writes
        .lock()
        .unwrap()
        .iter()
        .filter(|write| **write == operation)
        .count()
}

#[test]
fn unfinished_goal_issue_recovery_starts_no_background_tasks() {
    let (_dir, e, project, goal, _tasks, _fake) =
        fixture_with_clock(None, Arc::new(FakeClock::at(Timestamp(0))));
    // With no async runtime, an unnecessary spawn would panic.
    dvb_engine::publish::reconcile::restore_goal_effects(&e).unwrap();
    for event in [
        EventKind::GoalApproved { goal: goal.clone() },
        EventKind::GoalDropped { goal },
    ] {
        e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(vec![event]))
            .unwrap();
        dvb_engine::publish::reconcile::restore_goal_effects(&e).unwrap();
    }
}

#[tokio::test]
async fn finished_goal_snapshots_skip_unchanged_writes_and_repair_after_restart() {
    for dropped in [false, true] {
        let clock = Arc::new(FakeClock::at(Timestamp(0)));
        let forge = Arc::new(InterruptedForge::new(Arc::new(
            dvb_forge::FakeForge::default(),
        )));
        let (dir, e, project, goal, tasks, _fake) =
            fixture_with_clock(Some(forge.clone()), clock.clone());
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![EventKind::PrOpened {
                task: tasks[0].clone(),
                pr: 23,
                url: "https://github.invalid/pr/23".into(),
            }])
        })
        .unwrap();
        for task in &tasks {
            state(&e, &project, task, TaskState::Done);
        }
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![if dropped {
                EventKind::GoalDropped { goal: goal.clone() }
            } else {
                EventKind::GoalDone { goal: goal.clone() }
            }])
        })
        .unwrap();
        assert!(mirror::sync(&e).await.unwrap());
        let card = forge
            .inner
            .cards(&Repo("org/repo".into()))
            .into_iter()
            .find(|card| card.key == tasks[0].to_string())
            .unwrap();
        assert_eq!(card.pr, Some(23));
        assert_eq!(card.state, BoardState::Done);
        assert_eq!(card.milestone, 1);
        let events = support::events::all(&e).len();
        for _ in 0..3 {
            clock.advance_millis(300_000);
            assert!(mirror::sync(&e).await.unwrap());
        }
        assert_eq!(attempts(&forge, "goal"), 1);
        assert_eq!(attempts(&forge, "task"), 2);
        assert_eq!(support::events::all(&e).len(), events);

        e.proj().goals.get_mut(&goal).unwrap().text = "Changed goal".into();
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 2);
        assert_eq!(attempts(&forge, "task"), 4);
        e.proj().tasks.get_mut(&tasks[0]).unwrap().view.title = "Changed task".into();
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 3);
        assert_eq!(attempts(&forge, "task"), 6);
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 3);
        assert_eq!(attempts(&forge, "task"), 6);

        drop(e);
        let restarted = support::engine::open_with(
            dir.path(),
            "dvb",
            dvb_engine::EngineOptions {
                clock: Some(clock.clone()),
                forge: Some(forge.clone()),
                ..Default::default()
            },
        );
        let events = support::events::all(&restarted).len();
        assert!(mirror::sync(&restarted).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 4);
        assert_eq!(attempts(&forge, "task"), 8);
        assert_eq!(support::events::all(&restarted).len(), events);
        assert!(mirror::sync(&restarted).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 4);
        assert_eq!(attempts(&forge, "task"), 8);
    }
}

#[tokio::test]
async fn a_finished_goal_is_cached_only_after_all_tasks_are_mirrored() {
    let ((_dir, e, project, goal, _tasks, _fake), forge, clock) =
        failing_fixture("task", Timestamp(0));
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalDone { goal }])
    })
    .unwrap();
    assert!(!mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "task"), 2);
    *forge.mirror_failure.lock().unwrap() = None;
    assert!(!mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "task"), 2);
    clock.advance_millis(60_000);
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "task"), 4);
    let goals = attempts(&forge, "goal");
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "goal"), goals);
    assert_eq!(attempts(&forge, "task"), 4);
}

#[tokio::test]
async fn slow_mirror_operations_start_backoff_when_the_attempt_finishes() {
    for (blocked, failed, writes_per_attempt) in [
        ("project", "project", 1),
        ("goal", "goal", 1),
        ("task", "task", 2),
        ("project", "task", 2),
    ] {
        let clock = Arc::new(FakeClock::at(Timestamp(0)));
        let barrier = Arc::new(Barrier::new(2));
        let mut forge = InterruptedForge::new(Arc::new(dvb_forge::FakeForge::default()));
        forge.mirror_barrier = Some((blocked, barrier.clone()));
        let forge = Arc::new(forge);
        *forge.mirror_failure.lock().unwrap() = Some(failed);
        let (_dir, e, _project, _goal, _tasks, _fake) =
            fixture_with_clock(Some(forge.clone()), clock.clone());
        let worker = e.clone();
        let sync = tokio::spawn(async move { mirror::sync(&worker).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), barrier.wait())
            .await
            .unwrap();
        clock.advance_millis(60_000);
        barrier.wait().await;
        assert!(!sync.await.unwrap().unwrap());
        assert_eq!(attempts(&forge, failed), writes_per_attempt);

        // Even a minute-long operation must leave a full minute before retrying.
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, failed), writes_per_attempt, "{blocked}");
        clock.advance_millis(59_999);
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, failed), writes_per_attempt, "{blocked}");
        clock.advance_millis(1);
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(
            attempts(&forge, failed),
            2 * writes_per_attempt,
            "{blocked}"
        );
    }
}

fn failing_fixture(
    operation: &'static str,
    at: Timestamp,
) -> (Fixture, Arc<InterruptedForge>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::at(at));
    let forge = Arc::new(InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::default(),
    )));
    *forge.mirror_failure.lock().unwrap() = Some(operation);
    let fixture = fixture_with_clock(Some(forge.clone()), clock.clone());
    (fixture, forge, clock)
}

#[tokio::test]
async fn mirror_failures_back_off_from_one_minute_to_one_hour() {
    for (operation, writes_per_attempt) in [("project", 1), ("goal", 1), ("task", 2)] {
        let ((_dir, e, _project, _goal, _tasks, _fake), forge, clock) =
            failing_fixture(operation, Timestamp(1_000));
        assert!(!mirror::sync(&e).await.unwrap());
        let mut expected = writes_per_attempt;
        let initial_events = support::events::all(&e).len();
        for delay in [
            60_000, 120_000, 240_000, 480_000, 960_000, 1_920_000, 3_600_000, 3_600_000,
        ] {
            // Repeated event wakes do not bypass the retry deadline.
            for _ in 0..3 {
                assert!(!mirror::sync(&e).await.unwrap());
                assert_eq!(attempts(&forge, operation), expected, "{operation}");
            }
            clock.advance_millis(delay - 1);
            assert!(!mirror::sync(&e).await.unwrap());
            assert_eq!(attempts(&forge, operation), expected, "{operation}");
            clock.advance_millis(1);
            assert!(!mirror::sync(&e).await.unwrap());
            expected += writes_per_attempt;
            assert_eq!(attempts(&forge, operation), expected, "{operation}");
        }
        assert_eq!(
            support::events::all(&e).len(),
            initial_events,
            "failure observations stay deduplicated"
        );
    }
}

#[tokio::test]
async fn untimed_mirror_limits_double_backoff_and_cap_at_one_hour() {
    use dvb_core::forge::{ForgeError, RateLimit};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let forge = Arc::new(dvb_forge::FakeForge::default().with_clock(clock.clone()));
    let (_dir, e, _, _, _, _fake) = fixture_with_clock(Some(forge.clone()), clock.clone());
    let calls = || {
        forge
            .calls()
            .iter()
            .filter(|op| op.as_str() == "mirror_project")
            .count()
    };
    for (attempt, delay) in [
        60_000, 120_000, 240_000, 480_000, 960_000, 1_920_000, 3_600_000, 3_600_000,
    ]
    .into_iter()
    .enumerate()
    {
        forge.fail_next(
            "mirror_project",
            ForgeError {
                retryable: true,
                detail: "GitHub HTTP 429 (rate limit)".into(),
                rate_limit: Some(RateLimit { reset: None }),
            },
        );
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(calls(), attempt + 1);
        clock.advance_millis(delay - 1);
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(calls(), attempt + 1);
        clock.advance_millis(1);
    }
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(calls(), 9);
}

#[tokio::test]
async fn mirror_success_resets_the_next_failure_to_one_minute() {
    for (operation, writes_per_attempt) in [("project", 1), ("goal", 1), ("task", 2)] {
        let ((_dir, e, _project, _goal, _tasks, _fake), forge, clock) =
            failing_fixture(operation, Timestamp(0));
        mirror::sync(&e).await.unwrap();
        clock.advance_millis(60_000);
        mirror::sync(&e).await.unwrap();
        *forge.mirror_failure.lock().unwrap() = None;
        clock.advance_millis(120_000);
        assert!(mirror::sync(&e).await.unwrap());
        *forge.mirror_failure.lock().unwrap() = Some(operation);
        assert!(!mirror::sync(&e).await.unwrap());
        let before = attempts(&forge, operation);
        clock.advance_millis(59_999);
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, operation), before);
        clock.advance_millis(1);
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, operation), before + writes_per_attempt);
    }
}

fn mirror_observations(e: &AppState) -> Vec<(GoalId, Option<TaskId>, bool)> {
    support::events::all(e)
        .into_iter()
        .filter_map(|event| {
            if let EventKind::GithubMirrorObserved {
                goal,
                task,
                mirrored,
                ..
            } = event.kind
            {
                Some((goal, task, mirrored))
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn an_outage_records_one_failure_and_one_recovery_for_the_whole_project() {
    for operation in ["all", "task"] {
        let forge = Arc::new(InterruptedForge::new(Arc::new(
            dvb_forge::FakeForge::default(),
        )));
        let clock = Arc::new(FakeClock::at(Timestamp(0)));
        let (_dir, e, project, _goal, tasks, _fake) =
            fixture_with_clock(Some(forge.clone()), clock.clone());
        let role = e.proj().tasks[&tasks[0]].view.role.clone();
        for (goal, ids) in [("goal-b", ["three", "four"]), ("goal-c", ["five", "six"])] {
            let goal = GoalId::new(goal);
            e.append_checked(Some(&project), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::GoalStated {
                        goal: goal.clone(),
                        text: "Another goal".into(),
                        from_issue: None,
                        issues: Vec::new(),
                        after: None,
                    },
                    EventKind::PlanProposed {
                        goal: goal.clone(),
                        tasks: ids
                            .iter()
                            .map(|id| PlannedTask {
                                after: vec![],
                                task: TaskId::new(*id),
                                title: format!("Build {id}"),
                                role: role.clone(),
                                review: ReviewKind::Code,
                                summary: "Implement".into(),
                                routine: false,
                            })
                            .collect(),
                    },
                ])
            })
            .unwrap();
        }
        // Normal mirroring: every goal and task is mirrored and observed once.
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(forge.inner.milestones(&Repo("org/repo".into())).len(), 3);
        assert_eq!(forge.inner.cards(&Repo("org/repo".into())).len(), 6);
        let before = mirror_observations(&e);
        assert_eq!(before.len(), 9);
        assert!(before.iter().all(|(_, _, mirrored)| *mirrored));

        *forge.mirror_failure.lock().unwrap() = Some(operation);
        forge.mirror_writes.lock().unwrap().clear();
        for delay in [0, 0, 60_000, 0, 120_000, 240_000] {
            clock.advance_millis(delay);
            assert!(!mirror::sync(&e).await.unwrap());
        }
        let failed = mirror_observations(&e)[before.len()..].to_vec();
        assert_eq!(failed.len(), 1, "{operation}: {failed:?}");
        assert!(!failed[0].2);
        // Every item keeps its own backoff: four attempts in seven minutes.
        let (failing, items) = if operation == "all" {
            ("goal", 3)
        } else {
            ("task", 6)
        };
        assert_eq!(attempts(&forge, failing), 4 * items, "{operation}");

        *forge.mirror_failure.lock().unwrap() = None;
        clock.advance_millis(480_000);
        assert!(mirror::sync(&e).await.unwrap());
        let after = mirror_observations(&e)[before.len()..].to_vec();
        assert_eq!(
            after,
            [
                failed[0].clone(),
                (failed[0].0.clone(), failed[0].1.clone(), true)
            ],
            "{operation}"
        );
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(mirror_observations(&e).len(), before.len() + 2);
    }
}

#[tokio::test]
async fn one_failing_task_does_not_hold_back_the_others() {
    let ((_dir, e, _project, _goal, tasks, _fake), forge, clock) =
        failing_fixture("one", Timestamp(0));
    for _ in 0..3 {
        assert!(!mirror::sync(&e).await.unwrap());
        clock.advance_millis(3_600_000);
    }
    let cards = forge.inner.cards(&Repo("org/repo".into()));
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].key, tasks[1].to_string());
    let observed = mirror_observations(&e);
    assert_eq!(observed.len(), 3, "{observed:?}");
    assert!(observed.contains(&(GoalId::new("goal"), Some(tasks[0].clone()), false)));
    assert!(observed.contains(&(GoalId::new("goal"), Some(tasks[1].clone()), true)));
}

#[tokio::test]
async fn changed_mirror_content_retries_immediately_and_resets_only_that_mirror() {
    for operation in ["goal", "task"] {
        let ((_dir, e, project, goal, tasks, _fake), forge, clock) =
            failing_fixture(operation, Timestamp(0));
        mirror::sync(&e).await.unwrap();
        clock.advance_millis(60_000);
        mirror::sync(&e).await.unwrap();
        let before = attempts(&forge, operation);
        if operation == "goal" {
            e.proj().goals.get_mut(&goal).unwrap().text = "Changed goal".into();
        } else {
            state(&e, &project, &tasks[0], TaskState::Running);
        }
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, operation), before + 1);
        clock.advance_millis(60_000);
        assert!(!mirror::sync(&e).await.unwrap());
        // The changed task retries at one minute; the unchanged task is still
        // waiting for its two-minute deadline.
        assert_eq!(attempts(&forge, operation), before + 2);
    }
}

#[tokio::test]
async fn a_changed_project_repository_resets_its_mirror_retry() {
    let ((_dir, e, project, _goal, _tasks, _fake), forge, clock) =
        failing_fixture("project", Timestamp(0));
    mirror::sync(&e).await.unwrap();
    clock.advance_millis(60_000);
    mirror::sync(&e).await.unwrap();
    e.proj().projects.get_mut(&project).unwrap().view.repo = "org/changed".into();
    assert!(!mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "project"), 3);
    clock.advance_millis(60_000);
    assert!(!mirror::sync(&e).await.unwrap());
    assert_eq!(attempts(&forge, "project"), 4);
}

#[tokio::test(start_paused = true)]
async fn a_task_event_syncs_only_its_project_and_periodic_refresh_still_syncs_both() {
    let forge = Arc::new(InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::default(),
    )));
    let (_dir, e, a, _goal, tasks, _fake) = fixture_with(Some(forge.clone()));
    let b = ProjectId::new("b");
    e.append_checked(Some(&b), Actor::Engine, |_, _| {
        Ok(vec![support::events::project_event(&b, "B", "org/b")])
    })
    .unwrap();
    let watcher = tokio::spawn(mirror::watch(e.clone()));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    forge.requests.lock().unwrap().clear();
    state(&e, &a, &tasks[0], TaskState::Running);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let trace = forge.requests.lock().unwrap().clone();
    assert!(
        trace
            .iter()
            .any(|(op, args)| *op == "mirror_project" && args[0] == "org/repo")
    );
    assert!(
        trace
            .iter()
            .filter(|(op, _)| op.starts_with("mirror_"))
            .all(|(_, args)| args[0] == "org/repo")
    );
    forge.requests.lock().unwrap().clear();
    tokio::time::advance(std::time::Duration::from_secs(300)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        forge
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(op, args)| *op == "mirror_project" && args[0] == "org/b")
    );
    watcher.abort();
}

#[tokio::test(start_paused = true)]
async fn broadcast_lag_repairs_every_project() {
    let forge = Arc::new(InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::default(),
    )));
    let (_dir, e, a, _, _, _fake) = fixture_with(Some(forge.clone()));
    let b = ProjectId::new("b");
    e.append_checked(Some(&b), Actor::Engine, |_, _| {
        Ok(vec![support::events::project_event(&b, "B", "org/b")])
    })
    .unwrap();
    let watcher = tokio::spawn(mirror::watch(e.clone()));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    forge.requests.lock().unwrap().clear();
    e.append_checked(Some(&a), Actor::Engine, |_, _| {
        Ok(vec![EventKind::ProjectSeen {}; 1100])
    })
    .unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        forge
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(op, args)| *op == "mirror_project" && args[0] == "org/b")
    );
    watcher.abort();
}
