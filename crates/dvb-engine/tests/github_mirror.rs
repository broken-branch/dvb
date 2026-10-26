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
async fn two_prs_share_one_milestone_cards_move_and_manual_edits_stay_until_the_task_changes() {
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
            // A pass sends only what changed since its receipt, so a manual
            // edit stays until the task's next change overwrites it.
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
                changed
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
    assert!(
        mirror_observations(&restarted)
            .last()
            .is_some_and(|(_, _, mirrored)| *mirrored)
    );
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
async fn finished_goal_snapshots_skip_unchanged_writes_also_after_a_restart() {
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

        // The receipts are durable: the first pass after a start sends nothing.
        drop(e);
        let e = support::engine::open_with(
            dir.path(),
            "dvb",
            dvb_engine::EngineOptions {
                clock: Some(clock.clone()),
                forge: Some(forge.clone()),
                ..Default::default()
            },
        );
        let events = support::events::all(&e).len();
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 1);
        assert_eq!(attempts(&forge, "task"), 2);
        assert_eq!(support::events::all(&e).len(), events);

        // Only the changed item is sent: the goal's text is on its milestone,
        // a task's title on its card and in the milestone's task lines.
        e.proj().goals.get_mut(&goal).unwrap().text = "Changed goal".into();
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 2);
        assert_eq!(attempts(&forge, "task"), 2);
        e.proj().tasks.get_mut(&tasks[0]).unwrap().view.title = "Changed task".into();
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 3);
        assert_eq!(attempts(&forge, "task"), 3);
        assert!(mirror::sync(&e).await.unwrap());
        assert_eq!(attempts(&forge, "goal"), 3);
        assert_eq!(attempts(&forge, "task"), 3);
    }
}

/// The forge calls of one pass, from the Fake forge's record.
async fn pass_calls(e: &AppState, forge: &dvb_forge::FakeForge) -> Vec<String> {
    let before = forge.calls().len();
    assert!(mirror::sync(e).await.unwrap());
    forge.calls()[before..].to_vec()
}

#[tokio::test]
async fn an_unchanged_second_pass_sends_nothing_to_github() {
    let (_dir, e, project, _goal, tasks, forge) = fixture();
    let first = pass_calls(&e, &forge).await;
    assert_eq!(
        first,
        [
            "mirror_project",
            "visibility",
            "mirror_goal",
            "mirror_task",
            "mirror_task"
        ]
    );
    let events = support::events::all(&e).len();
    for _ in 0..2 {
        let unchanged = pass_calls(&e, &forge).await;
        assert_eq!(unchanged, ["mirror_project", "visibility"]);
    }
    assert_eq!(
        support::events::all(&e).len(),
        events,
        "no receipt is resent"
    );

    // A task's new state is on its card and in the milestone's task lines.
    state(&e, &project, &tasks[1], TaskState::Running);
    let changed = pass_calls(&e, &forge).await;
    assert_eq!(
        changed,
        ["mirror_project", "visibility", "mirror_goal", "mirror_task"]
    );
    let card = forge
        .cards(&Repo("org/repo".into()))
        .into_iter()
        .find(|card| card.key == tasks[1].to_string())
        .unwrap();
    assert_eq!(card.state, BoardState::Working);
}

#[tokio::test]
async fn a_failed_send_is_sent_again_even_when_its_content_returns_to_its_receipt() {
    use dvb_core::forge::ForgeError;
    let clock = Arc::new(FakeClock::at(Timestamp(0)));
    let forge = Arc::new(dvb_forge::FakeForge::default().with_clock(clock.clone()));
    let (dir, e, _project, _goal, tasks, _fake) =
        fixture_with_clock(Some(forge.clone()), clock.clone());
    let repo = Repo("org/repo".into());
    let card = |forge: &dvb_forge::FakeForge| {
        forge
            .cards(&repo)
            .into_iter()
            .find(|card| card.key == tasks[0].to_string())
            .unwrap()
    };
    let sends = |calls: &[String]| calls.iter().filter(|c| *c == "mirror_task").count();
    pass_calls(&e, &forge).await;
    let confirmed = card(&forge);

    // GitHub may have applied part of a failed send: its receipt no longer holds.
    let fail = || ForgeError {
        rate_limit: None,
        retryable: true,
        unreachable: false,
        detail: "GitHub HTTP 502".into(),
    };
    for restart in [false, true] {
        e.proj().tasks.get_mut(&tasks[0]).unwrap().view.title = "Changed".into();
        forge.fail_next("mirror_task", fail());
        let before = forge.calls().len();
        assert!(!mirror::sync(&e).await.unwrap());
        assert_eq!(sends(&forge.calls()[before..]), 1);
        e.proj().tasks.get_mut(&tasks[0]).unwrap().view.title = confirmed.title.clone();
        let e = if restart {
            // The restarted engine reads the title back from its log.
            support::engine::open_with(
                dir.path(),
                "dvb",
                dvb_engine::EngineOptions {
                    clock: Some(clock.clone()),
                    forge: Some(forge.clone()),
                    ..Default::default()
                },
            )
        } else {
            e.clone()
        };
        assert_eq!(
            sends(&pass_calls(&e, &forge).await),
            1,
            "restart: {restart}"
        );
        assert_eq!(card(&forge), confirmed);
        assert_eq!(
            sends(&pass_calls(&e, &forge).await),
            0,
            "restart: {restart}"
        );
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
                unreachable: false,
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
        let ((_dir, e, _project, _goal, tasks, _fake), forge, clock) =
            failing_fixture(operation, Timestamp(0));
        mirror::sync(&e).await.unwrap();
        clock.advance_millis(60_000);
        mirror::sync(&e).await.unwrap();
        *forge.mirror_failure.lock().unwrap() = None;
        clock.advance_millis(120_000);
        assert!(mirror::sync(&e).await.unwrap());
        *forge.mirror_failure.lock().unwrap() = Some(operation);
        // An unchanged goal or card is not sent again; a change is.
        for task in &tasks {
            e.proj().tasks.get_mut(task).unwrap().view.title = "Changed".into();
        }
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
        // Every card and, through its task lines, every milestone changes.
        let ids: Vec<TaskId> = e.proj().tasks.keys().cloned().collect();
        for id in ids {
            e.proj().tasks.get_mut(&id).unwrap().view.title = format!("Rebuild {id}");
        }
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

#[tokio::test]
async fn a_renamed_repository_is_reported_once_and_a_repoint_keeps_its_mirror() {
    use axum::http::{Method, StatusCode};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let forge = Arc::new(dvb_forge::FakeForge::default().with_clock(clock.clone()));
    let (_dir, e, project, _, _, _) = fixture_with_clock(Some(forge.clone()), clock.clone());
    let (old, new) = (Repo("org/repo".into()), Repo("org/renamed".into()));
    let mirror_dir = e.mirror("Project");
    std::fs::create_dir_all(&mirror_dir).unwrap();
    support::git::git(&mirror_dir, &["init", "--quiet", "--bare"]);
    support::git::git(
        &mirror_dir,
        &["remote", "add", "origin", "https://github.com/org/repo.git"],
    );
    assert!(mirror::sync(&e).await.unwrap());
    let (milestones, cards) = (forge.milestones(&old), forge.cards(&old));
    assert_eq!((milestones.len(), cards.len()), (1, 2));
    let observed = mirror_observations(&e).len();
    let unreachable = |e: &AppState| {
        support::events::all(e)
            .into_iter()
            .filter_map(|event| {
                if let EventKind::ProjectRepoUnreachable { repo } = event.kind {
                    Some(repo)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };

    forge.rename_repo(&old, &new);
    for _ in 0..3 {
        assert!(!mirror::sync(&e).await.unwrap());
        clock.advance_millis(3_600_000);
    }
    assert_eq!(
        unreachable(&e),
        ["org/repo"],
        "one notice for every refused call"
    );
    assert_eq!(
        mirror_observations(&e).len(),
        observed,
        "no observation per item"
    );

    for (name, detail) in [
        ("org/missing", "reaches no repository named org/missing"),
        ("org/other", "org/other is not the repository org/repo was"),
    ] {
        if name == "org/other" {
            forge.mirror_project(&Repo(name.into())).await.unwrap();
        }
        let (status, body) = support::http::call(
            &e,
            Method::PUT,
            "/projects/project/repo",
            Some(serde_json::json!({"repo": name})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "REPOSITORY_MISMATCH");
        assert!(body["detail"].as_str().unwrap().contains(detail), "{body}");
    }
    assert_eq!(e.proj().projects[&project].view.repo, "org/repo");

    let (status, body) = support::http::call(
        &e,
        Method::PUT,
        "/projects/project/repo",
        Some(serde_json::json!({"repo": "org/renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repo"], "org/renamed");
    assert_eq!(
        support::git::git(&mirror_dir, &["config", "--get", "remote.origin.url"]),
        "https://github.com/org/renamed.git"
    );
    // The receipts moved with the repository: nothing changed, nothing is sent.
    let writes = forge.calls().len();
    assert!(mirror::sync(&e).await.unwrap());
    let sent = |from: usize| {
        forge.calls()[from..]
            .iter()
            .filter(|call| *call == "mirror_goal" || *call == "mirror_task")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(sent(writes), Vec::<String>::new());
    let task = e.proj().tasks.keys().next().cloned().unwrap();
    e.proj().tasks.get_mut(&task).unwrap().view.title = "Renamed card".into();
    let writes = forge.calls().len();
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(sent(writes), ["mirror_goal", "mirror_task"]);
    assert_eq!(
        forge.milestones(&new).len(),
        1,
        "same milestone, no new one"
    );
    assert_eq!(forge.milestones(&new)[0].0, milestones[0].0);
    assert_eq!(forge.cards(&new).len(), cards.len());
    assert_eq!(unreachable(&e).len(), 1);
    let events = support::events::all(&e);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ProjectRepoChanged { from, to } if from == "org/repo" && to == "org/renamed"
    )));
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ProjectAdded { repo, .. } if repo == "org/repo"
        )),
        "earlier events keep the old name"
    );
}

#[tokio::test]
async fn a_missing_old_repository_name_cannot_confirm_a_repoint() {
    use axum::http::{Method, StatusCode};
    let (_dir, e, project, _, _, forge) = fixture();
    forge
        .mirror_project(&Repo("org/renamed".into()))
        .await
        .unwrap();
    let (status, body) = support::http::call(
        &e,
        Method::PUT,
        "/projects/project/repo",
        Some(serde_json::json!({"repo": "org/renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "REPOSITORY_MISMATCH");
    assert_eq!(
        body["detail"],
        "GitHub no longer resolves org/repo, so dvb cannot confirm org/renamed is the same repository; remove the project and add org/renamed"
    );
    assert_eq!(e.proj().projects[&project].view.repo, "org/repo");
}

#[tokio::test]
async fn removing_and_readding_the_same_repository_reuses_one_board_and_clones_a_fresh_mirror() {
    use axum::http::{Method, StatusCode};
    use support::git::{git, seed_repository};

    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    seed_repository(&origin, "main", None, "Seed");
    let forge = Arc::new(dvb_forge::FakeForge::local_git());
    let e = support::engine::open_with(
        &dir.path().join("engine"),
        "dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge.clone()),
            ..Default::default()
        },
    );
    e.manual_dispatch();
    let repo = Repo(origin.to_str().unwrap().into());
    let body = serde_json::json!({"name": "Project", "repo": repo.0, "ci": "no_ci"});
    let (status, first) =
        support::http::call(&e, Method::POST, "/projects", Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(forge.boards().as_slice(), std::slice::from_ref(&repo));
    let mirror_dir = e.mirror("Project");
    let old_head = git(&mirror_dir, &["rev-parse", "HEAD"]);
    std::fs::write(mirror_dir.join("old-mirror"), "local receipt").unwrap();

    let (status, removed) = support::http::call(
        &e,
        Method::POST,
        &format!("/projects/{}/remove", first["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["not_freed"], serde_json::json!([]));
    assert!(!mirror_dir.exists());
    git(
        &origin,
        &["commit", "--quiet", "--allow-empty", "-m", "Advance main"],
    );
    let head = git(&origin, &["rev-parse", "HEAD"]);
    assert_ne!(head, old_head);

    let (status, second) = support::http::call(&e, Method::POST, "/projects", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_ne!(first["id"], second["id"]);
    assert_eq!(e.proj().projects.len(), 1);
    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(forge.boards(), [repo]);
    assert_eq!(git(&mirror_dir, &["rev-parse", "HEAD"]), head);
    assert!(!mirror_dir.join("old-mirror").exists());
}

#[tokio::test]
async fn a_paused_project_with_stopped_tasks_is_removed_locally_and_github_gets_no_write() {
    use axum::http::{Method, StatusCode};
    let forge = Arc::new(dvb_forge::FakeForge::local_git());
    let traced = Arc::new(InterruptedForge::new(forge.clone()));
    let (_dir, e, project, goal, tasks, _) = fixture_with(Some(traced.clone()));
    let repo = Repo("org/repo".into());
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalApproved { goal: goal.clone() }])
    })
    .unwrap();
    let run = dvb_core::RunId::new("builder");
    let builder = support::assignment::Assignment::new(run.clone())
        .task(Some(tasks[0].clone()))
        .build();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(builder)])
    })
    .unwrap();
    state(&e, &project, &tasks[0], TaskState::Running);
    assert!(mirror::sync(&e).await.unwrap());
    let (milestones, cards) = (forge.milestones(&repo), forge.cards(&repo));
    assert_eq!((milestones.len(), cards.len()), (1, 2));
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: run.clone(),
            reason: dvb_core::event::EndReason::Stopped,
            detail: None,
            stderr: None,
        }])
    })
    .unwrap();
    for task in &tasks {
        let was = Box::new(e.proj().tasks[task].view.state.clone());
        state(&e, &project, task, TaskState::Stopped { was });
    }
    let (status, _) = support::http::call(&e, Method::POST, "/projects/project/pause", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, before) = support::http::call(&e, Method::GET, "/status", None).await;
    assert_eq!(before["tasks"].as_array().unwrap().len(), 2, "{before}");

    let paths = &e.cfg.paths;
    let files = [
        paths.work.join(run.as_str()).join("ws"),
        e.mirror("Project"),
        paths
            .mirrors
            .join(".cache")
            .join(project.as_str())
            .join("cargo"),
    ];
    for dir in &files {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("file"), "kept").unwrap();
    }
    let other = paths.mirrors.join(".cache").join("other");
    std::fs::create_dir_all(&other).unwrap();
    let calls = forge.calls().len();
    // The receipts go right after the removal event, before any file is freed.
    let forgotten = Arc::new(std::sync::Mutex::new(None));
    *traced.on_forget_mirror.lock().unwrap() = Some(Box::new({
        let (e, forgotten) = (e.clone(), forgotten.clone());
        move || {
            *forgotten.lock().unwrap() = Some((
                e.proj().projects.contains_key(&ProjectId::new("project")),
                files.iter().all(|dir| dir.exists()),
            ));
        }
    }));

    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project"]["name"], "Project");
    assert_eq!(body["not_freed"], serde_json::json!([]));
    assert_eq!(
        *forgotten.lock().unwrap(),
        Some((false, true)),
        "receipts forgotten after the removal event and before the files are freed"
    );
    for dir in [
        paths.work.join(run.as_str()),
        e.mirror("Project"),
        paths.mirrors.join(".cache").join(project.as_str()),
    ] {
        assert!(!dir.exists(), "{} is freed", dir.display());
    }
    assert!(other.exists(), "another project's cache stays");

    assert!(mirror::sync(&e).await.unwrap());
    assert_eq!(
        forge.calls()[calls..],
        ["forget_mirror"],
        "only the local receipts are dropped; nothing is sent to GitHub"
    );
    assert_eq!(forge.milestones(&repo), milestones, "milestones untouched");
    assert_eq!(forge.cards(&repo), cards, "board untouched");

    let events = support::events::all(&e);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ProjectRemoved { name, repo } if name == "Project" && repo == "org/repo"
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ProjectAdded { .. })),
        "earlier events stay"
    );
    let (_, projects) = support::http::call(&e, Method::GET, "/projects", None).await;
    assert_eq!(projects, serde_json::json!([]));
    let (status, _) = support::http::call(&e, Method::GET, "/projects/project", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, after) = support::http::call(&e, Method::GET, "/status", None).await;
    assert_eq!(after["needs_you"], 0, "{after}");
    assert!(after.get("tasks").is_none(), "{after}");
    let (_, goals) = support::http::call(&e, Method::GET, "/goals", None).await;
    assert_eq!(goals, serde_json::json!([]));

    let (status, body) = support::http::call(
        &e,
        Method::POST,
        "/projects/project/goals",
        Some(serde_json::json!({"text": "Ship more"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "PROJECT_UNKNOWN");
    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "PROJECT_UNKNOWN");
}

#[tokio::test]
async fn remove_frees_a_toolchain_cache_a_run_left_but_not_what_its_links_point_to() {
    use axum::http::{Method, StatusCode};
    use std::os::unix::fs::PermissionsExt;
    let (dir, e, project, _goal, tasks, _) = fixture();
    for task in &tasks {
        state(&e, &project, task, TaskState::Cancelled);
    }
    let mode = |path: &std::path::Path| {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    let set = |path: &std::path::Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    // Made as a run makes it: the engine's 2770 cache and the run's link to it,
    // cargo's directories under umask 0002, sccache's at 2750 with 0600 entries,
    // and a link the run planted to somewhere else.
    let cache = e.cfg.paths.mirrors.join(".cache").join(project.as_str());
    let run = e.cfg.paths.work.join("builder");
    std::fs::create_dir_all(&cache).unwrap();
    set(&cache, 0o2770);
    std::fs::create_dir_all(&run).unwrap();
    std::os::unix::fs::symlink(&cache, run.join("cache")).unwrap();
    std::fs::create_dir_all(cache.join("cargo/registry")).unwrap();
    for sub in ["cargo/registry", "cargo"] {
        set(&cache.join(sub), 0o2775);
    }
    let entry = cache.join("sccache/9/2/927dffda");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, "object").unwrap();
    set(&entry, 0o600);
    for sub in ["sccache/9/2", "sccache/9", "sccache"] {
        set(&cache.join(sub), 0o2750);
    }
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("kept"), "kept").unwrap();
    set(&outside, 0o700);
    std::os::unix::fs::symlink(&outside, cache.join("sccache/9/escape")).unwrap();

    dvb_run::share_run(&run);
    // The engine is in the run's group, not the owner: it can remove an entry
    // only from a directory the group may write.
    let mut todo = vec![cache.clone()];
    while let Some(dir) = todo.pop() {
        assert_eq!(mode(&dir) & 0o070, 0o070, "{}", dir.display());
        for child in std::fs::read_dir(&dir).unwrap() {
            let child = child.unwrap();
            if child.file_type().unwrap().is_dir() {
                todo.push(child.path());
            }
        }
    }
    assert_eq!(mode(&outside), 0o700);
    std::fs::remove_dir_all(&run).unwrap();

    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["not_freed"], serde_json::json!([]));
    assert!(!cache.exists());
    assert_eq!(mode(&outside), 0o700);
    assert_eq!(
        std::fs::read_to_string(outside.join("kept")).unwrap(),
        "kept"
    );
}

#[tokio::test]
async fn a_project_with_running_or_open_tasks_is_not_removed() {
    use axum::http::{Method, StatusCode};
    let (_dir, e, project, goal, tasks, forge) = fixture();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalApproved { goal: goal.clone() }])
    })
    .unwrap();
    state(&e, &project, &tasks[0], TaskState::Running);
    let mirror_dir = e.mirror("Project");
    std::fs::create_dir_all(&mirror_dir).unwrap();
    let calls = forge.calls().len();

    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "PROJECT_BUSY");
    assert_eq!(body["tasks"], serde_json::json!(["one", "two"]));
    let detail = body["detail"].as_str().unwrap();
    for named in [
        "one \"Build one\" (Working): stop it with dvb stop one",
        "two \"Build two\" (Planned): stop it with dvb stop two",
    ] {
        assert!(detail.contains(named), "{detail}");
    }

    // A stopped task whose run has not ended yet still holds the project.
    for task in &tasks {
        let was = Box::new(e.proj().tasks[task].view.state.clone());
        state(&e, &project, task, TaskState::Stopped { was });
    }
    let builder = support::assignment::Assignment::new(dvb_core::RunId::new("builder"))
        .task(Some(tasks[0].clone()))
        .build();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(builder)])
    })
    .unwrap();
    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["tasks"], serde_json::json!(["one"]));
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("one \"Build one\" has a run still ending"),
        "{body}"
    );

    assert!(e.proj().projects.contains_key(&project));
    assert!(mirror_dir.exists());
    assert_eq!(forge.calls().len(), calls, "a refusal touches no receipt");
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|event| matches!(event.kind, EventKind::ProjectRemoved { .. }))
    );
}

#[tokio::test]
async fn a_remove_during_a_pass_waits_for_one_job_and_the_rest_of_the_pass_skips_the_project() {
    use axum::http::{Method, StatusCode};
    let barrier = Arc::new(Barrier::new(2));
    let mut forge = InterruptedForge::new(Arc::new(dvb_forge::FakeForge::default()));
    forge.mirror_barrier = Some(("goal", barrier.clone()));
    let forge = Arc::new(forge);
    let (_dir, e, project, _goal, tasks, _fake) = fixture_with(Some(forge.clone()));
    let repo = Repo("org/repo".into());
    for task in &tasks {
        state(&e, &project, task, TaskState::Cancelled);
    }
    e.append_checked(Some(&project), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("later"),
            text: "Ship more".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let worker = e.clone();
    let sync = tokio::spawn(async move { mirror::sync(&worker).await });
    // The first goal's job holds its forge call open.
    tokio::time::timeout(std::time::Duration::from_secs(2), barrier.wait())
        .await
        .unwrap();
    let remover = e.clone();
    let remove = tokio::spawn(async move {
        support::http::call(&remover, Method::POST, "/projects/project/remove", None).await
    });
    // Let the remove queue on the job's lock.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !remove.is_finished(),
        "the remove waits for the running job"
    );
    barrier.wait().await;
    let (status, body) = remove.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(sync.await.unwrap().unwrap());
    assert_eq!(
        *forge.mirror_writes.lock().unwrap(),
        ["project", "goal", "task", "task"],
        "the running job finished; the later goal's job did not start"
    );
    assert_eq!(forge.inner.milestones(&repo).len(), 1);
    let events = support::events::all(&e);
    let removed = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::ProjectRemoved { .. }))
        .unwrap();
    assert_eq!(
        events[removed + 1..]
            .iter()
            .filter(|event| event.project.as_ref() == Some(&project))
            .count(),
        0,
        "no mirror event for the removed project"
    );
}

#[tokio::test(start_paused = true)]
async fn a_remove_or_repoint_that_waits_too_long_is_refused_with_the_running_pass() {
    use axum::http::{Method, StatusCode};
    let clock = Arc::new(FakeClock::at(Timestamp(1_791_990_000_000)));
    let barrier = Arc::new(Barrier::new(2));
    let fake = Arc::new(dvb_forge::FakeForge::default());
    let mut forge = InterruptedForge::new(fake.clone());
    forge.mirror_barrier = Some(("project", barrier.clone()));
    let forge = Arc::new(forge);
    let (_dir, e, project, _goal, tasks, _) =
        fixture_with_clock(Some(forge.clone()), clock.clone());
    for task in &tasks {
        state(&e, &project, task, TaskState::Cancelled);
    }
    let (_, idle) = support::http::call(&e, Method::GET, "/status", None).await;
    assert!(idle.get("mirror_pass").is_none(), "{idle}");
    let (old, new) = (Repo("org/repo".into()), Repo("org/renamed".into()));
    fake.mirror_project(&old).await.unwrap();
    let worker = e.clone();
    let sync = tokio::spawn(async move { mirror::sync(&worker).await });
    barrier.wait().await;

    let (_, running) = support::http::call(&e, Method::GET, "/status", None).await;
    assert_eq!(
        running["mirror_pass"],
        serde_json::json!({"started": 1_791_990_000_000_i64, "project": "project"})
    );
    let started = "waiting for the GitHub mirror pass, started 15:00 UTC, now on project Project; nothing was changed, try again";
    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "MIRROR_PASS_RUNNING");
    assert_eq!(body["detail"], started);
    fake.rename_repo(&old, &new);
    let (status, body) = support::http::call(
        &e,
        Method::PUT,
        "/projects/project/repo",
        Some(serde_json::json!({"repo": "org/renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "MIRROR_PASS_RUNNING");
    assert_eq!(body["detail"], started);
    assert_eq!(e.proj().projects[&project].view.repo, "org/repo");
    assert!(
        !support::events::all(&e).iter().any(|event| matches!(
            event.kind,
            EventKind::ProjectRemoved { .. } | EventKind::ProjectRepoChanged { .. }
        )),
        "a refused remove or repoint changes nothing"
    );

    barrier.wait().await;
    sync.await.unwrap().unwrap();
    let (_, done) = support::http::call(&e, Method::GET, "/status", None).await;
    assert!(done.get("mirror_pass").is_none(), "{done}");
    let (status, body) = support::http::call(
        &e,
        Method::PUT,
        "/projects/project/repo",
        Some(serde_json::json!({"repo": "org/renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) =
        support::http::call(&e, Method::POST, "/projects/project/remove", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
