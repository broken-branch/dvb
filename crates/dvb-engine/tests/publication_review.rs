//! Gateway integration using real local git and the Fake forge. No sockets,
//! provider launches, credentials or GitHub traffic.
use dvb_core::caps;
use dvb_core::event::{CiMode, PlannedTask};
use dvb_core::forge::{Forge, PrState, Repo};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, ProjectId, RoleId, RunAssignment, RunId, TaskId,
    TaskState, Vendor, VerdictId,
};
use dvb_engine::wake;
use dvb_engine::{AppState, dispatch, publish};
use dvb_forge::FakeForge;

use support::git::{collect, git, seed_repository};

/// The engine, on a Fake forge wrapped for interruptions; the Fake itself.
struct Fixture {
    readings: support::engine::Readings,
    clock: Arc<AheadClock>,
    _tmp: tempfile::TempDir,
    e: AppState,
    repo: Repo,
    task: TaskId,
    /// The default branch the first round was built on.
    base: String,
    first: String,
    second: String,
    fake: Arc<FakeForge>,
    interrupted: Arc<InterruptedForge>,
}

/// The real clock, moved ahead by the milliseconds in it.
#[derive(Debug, Default)]
struct AheadClock(std::sync::atomic::AtomicI64);

impl dvb_core::Clock for AheadClock {
    fn now(&self) -> dvb_core::Timestamp {
        dvb_core::SystemClock
            .now()
            .plus_millis(self.0.load(Ordering::SeqCst))
    }
}

async fn merging() -> Fixture {
    merging_with(None).await
}

async fn merging_with(barrier: Option<Arc<tokio::sync::Barrier>>) -> Fixture {
    fixture(barrier, None, None).await
}

async fn merging_on(
    barrier: Option<Arc<tokio::sync::Barrier>>,
    launcher: Option<Arc<dyn dvb_core::launcher::Launcher>>,
) -> Fixture {
    fixture(barrier, None, launcher).await
}

/// `owner`: an owner name the publication policy refuses in published text.
async fn fixture(
    barrier: Option<Arc<tokio::sync::Barrier>>,
    owner: Option<&str>,
    launcher: Option<Arc<dyn dvb_core::launcher::Launcher>>,
) -> Fixture {
    fixture_plan(barrier, owner, launcher, Vec::new()).await
}

async fn fixture_plan(
    barrier: Option<Arc<tokio::sync::Barrier>>,
    owner: Option<&str>,
    launcher: Option<Arc<dyn dvb_core::launcher::Launcher>>,
    more: Vec<PlannedTask>,
) -> Fixture {
    fixture_plan_review(barrier, owner, launcher, more, ReviewKind::Code, true).await
}

/// `reviewed`: the first round has a clean code verdict; else a builder
/// built it on `base` and it has no verdict yet.
async fn fixture_plan_review(
    barrier: Option<Arc<tokio::sync::Barrier>>,
    owner: Option<&str>,
    launcher: Option<Arc<dyn dvb_core::launcher::Launcher>>,
    more: Vec<PlannedTask>,
    review: ReviewKind,
    reviewed: bool,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    if let Some(owner) = owner {
        let state = tmp.path().join("engine").join("state");
        std::fs::create_dir_all(&state).unwrap();
        let settings = serde_json::json!({"owner_name": owner});
        std::fs::write(state.join("publication.json"), settings.to_string()).unwrap();
    }
    let fake = Arc::new(FakeForge::local_git());
    let mut interrupted = InterruptedForge::new(fake.clone());
    interrupted.cleanup_barrier = barrier;
    let interrupted = Arc::new(interrupted);
    let forge: Arc<dyn Forge> = interrupted.clone();
    let readings = support::engine::Readings::default();
    let clock = Arc::new(AheadClock::default());
    let e = support::engine::open_with(
        &tmp.path().join("engine"),
        "unused-dvb",
        readings.options(dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher,
            clock: Some(clock.clone()),
            ..Default::default()
        }),
    );
    // Publication and review scenarios need predictable disk admission.
    e.append_checked(None, Actor::Owner, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "max_runs".into(),
            old: serde_json::json!(null),
            new: serde_json::json!(3),
            restored_from: None,
        }])
    })
    .unwrap();
    let seed = tmp.path().join("seed");
    let origin = tmp.path().join("origin.git");
    seed_repository(&seed, "main", Some(("file", "base\n")), "base");
    let base = git(&seed, &["rev-parse", "HEAD"]);
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    std::fs::write(seed.join("file"), "changed\n").unwrap();
    git(&seed, &["commit", "--quiet", "-am", "change"]);
    let first = git(&seed, &["rev-parse", "HEAD"]);
    std::fs::write(seed.join("file"), "moved\n").unwrap();
    git(&seed, &["commit", "--quiet", "-am", "move"]);
    let second = git(&seed, &["rev-parse", "HEAD"]);
    // Fetch both heads into the engine mirror through the bare repo.
    git(
        &seed,
        &[
            "push",
            "--quiet",
            origin.to_str().unwrap(),
            "HEAD:refs/heads/seed",
        ],
    );
    let repo = Repo(origin.to_string_lossy().into());
    let forge = &fake;
    let mirror = e.mirror("project");
    forge.clone_repo(&repo, &mirror).await.unwrap();
    forge.push(&repo, &mirror, "dvb/t-1", &first).await.unwrap();
    let pr = forge
        .open_pr(&repo, "dvb/t-1", "Task", "Body")
        .await
        .unwrap();
    let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
    let built = RunId::new("builder1");
    let mut kinds = vec![
        support::events::project_event(&p, "project", &repo.0),
        support::events::goal_event(&g, "Goal", Some(4)),
        EventKind::PlanProposed {
            goal: g,
            tasks: std::iter::once(PlannedTask {
                after: vec![],
                task: t.clone(),
                title: "Task".into(),
                role: RoleId::new("builder"),
                review,
                summary: "Task".into(),
                routine: false,
            })
            .chain(more)
            .collect(),
        },
    ];
    if !reviewed {
        kinds.extend([
            EventKind::RunStarted(
                support::assignment::Assignment::new(built.clone())
                    .task(Some(t.clone()))
                    .role(RoleId::new("builder"))
                    .reason("Build".into())
                    .base(Some(base.clone()))
                    .build(),
            ),
            EventKind::RunEnded {
                run: built,
                reason: dvb_core::event::EndReason::Completed,
                detail: None,
            },
        ]);
    }
    kinds.extend([
        EventKind::BranchPublished {
            task: t.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: first.clone(),
        },
        EventKind::PrOpened {
            task: t.clone(),
            pr: pr.number,
            url: pr.url,
        },
        EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Planned,
            to: TaskState::Merging,
        },
    ]);
    if reviewed {
        kinds.extend(clean_review(&t, &first));
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    Fixture {
        readings,
        clock,
        _tmp: tmp,
        e,
        repo,
        task: t,
        base,
        first,
        second,
        fake,
        interrupted,
    }
}

/// The `reviewer` run's clean code verdict on `head`.
fn clean_review(t: &TaskId, head: &str) -> [EventKind; 2] {
    let run = RunId::new("reviewer");
    [
        EventKind::RunStarted(
            support::assignment::Assignment::new(run.clone())
                .task(Some(t.clone()))
                .role(RoleId::new("code-reviewer"))
                .reason("Review".into())
                .reviews(Some(head.into()))
                .base(Some(head.into()))
                .build(),
        ),
        EventKind::VerdictRecorded {
            review_stage: None,
            verdict: VerdictId::new("v"),
            task: t.clone(),
            head: head.into(),
            run,
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            findings: vec![],
            summary: "Local gates ran".into(),
        },
    ]
}

/// Dispatcher steps until the task is no longer waiting for CI or merging.
async fn settle(e: &AppState, task: &TaskId) -> Result<(), dvb_engine::Fail> {
    for _ in 0..20 {
        dispatch::step_now(e, task).await?;
        let state = e.proj().tasks[task].view.state.clone();
        if !matches!(state, TaskState::AwaitingChecks | TaskState::Merging) {
            return Ok(());
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_moved_head_invalidates_the_old_review_and_reenters_review() {
    let Fixture {
        _tmp,
        e,
        repo,
        task: t,
        first,
        second,
        fake,
        ..
    } = merging().await;
    let forge = &fake;
    forge
        .push(&repo, &e.mirror("project"), "dvb/t-1", &second)
        .await
        .unwrap();
    // The merge step adopts the moved head; review is the dispatcher's next
    // step (this fixture has no reviewer role, so it goes no further).
    dispatch::step_now(&e, &t).await.unwrap();
    assert_eq!(e.proj().tasks[&t].view.state, TaskState::InReview);
    assert_eq!(
        e.proj().tasks[&t].view.head.as_deref(),
        Some(second.as_str())
    );
    assert_ne!(
        e.proj().tasks[&t].view.head.as_deref(),
        Some(first.as_str())
    );
    assert_eq!(forge.pr(&repo, 1).await.unwrap().state, PrState::Open);
    assert!(forge.closed_issues(&repo).is_empty());
    assert!(
        support::events::all(&e)
            .iter()
            .any(|ev| matches!(&ev.kind,EventKind::MergeRefused { task, .. } if task == &t))
    );
}

#[tokio::test]
async fn merge_finishes_issue_and_branch_effects_and_a_retry_observes_done() {
    for landed_before_restart in [false, true] {
        let Fixture {
            _tmp,
            e,
            repo,
            task: t,
            fake,
            first,
            interrupted,
            ..
        } = merging().await;
        approved(&e);
        if landed_before_restart {
            fake.merge(&repo, 1, &first, "t").await.unwrap();
            dvb_engine::reconcile::mark(&e).unwrap();
            let state = e
                .decide(
                    e.proj(),
                    &t,
                    Actor::Engine,
                    dvb_engine::Decision::new(
                        "reconcile",
                        Command::Reconcile(dvb_core::state::Observed {
                            unit: dvb_core::launcher::UnitState::Inactive,
                            head: None,
                        }),
                    ),
                    |_| Ok(()),
                )
                .unwrap();
            // The fold returns to Merging; the dispatcher reads the landed merge.
            assert_eq!(state, TaskState::Merging);
        }
        // Completion is recorded; the orchestrator's wake is its own (this fixture
        // has no orchestrator).
        dispatch::step_now(&e, &t).await.unwrap();
        assert_eq!(e.proj().tasks[&t].view.state, TaskState::Done);
        let forge = &fake;
        assert_eq!(forge.pr(&repo, 1).await.unwrap().state, PrState::Merged);
        assert_eq!(forge.closed_issues(&repo), vec![4]);
        // Positive control for the completion recorder: a linked goal really sends an
        // issue operation, which the unlinked full-lifecycle test excludes.
        assert!(interrupted.calls.lock().unwrap().contains(&"close_issue"));
        // Done is idempotent: no second merge or second cleanup request.
        dispatch::step_now(&e, &t).await.unwrap();
        assert_eq!(forge.closed_issues(&repo), vec![4]);
    }
}

mod support;
use dvb_core::event::ChecksStatus;
use dvb_core::forge::CheckObservation;
use dvb_core::{Command, EventId};
use std::sync::{Arc, atomic::Ordering};
use support::InterruptedForge;

fn approved(e: &AppState) {
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GoalApproved {
            goal: GoalId::new("g"),
        }])
    })
    .unwrap();
}
#[tokio::test]
async fn polling_reenters_review_when_the_old_head_never_goes_green() {
    for (move_during_request, old_status) in [
        (false, ChecksStatus::Pending),
        (true, ChecksStatus::Pending),
        (true, ChecksStatus::Success),
        (true, ChecksStatus::Failed),
    ] {
        let Fixture {
            _tmp,
            e,
            repo,
            task,
            first,
            second,
            fake,
            interrupted,
            ..
        } = merging().await;
        let p = ProjectId::new("p");
        // Record the real expected-checks mode, leaving the old head pending forever.
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                support::events::project_event_with_ci(
                    &(p),
                    "project",
                    &(repo.0),
                    CiMode::Expected(vec!["rust".into()]),
                ),
                EventKind::TaskStateChanged {
                    task: task.clone(),
                    from: TaskState::Merging,
                    to: TaskState::AwaitingChecks,
                },
            ])
        })
        .unwrap();
        fake.set_checks(
            &repo,
            &first,
            vec![CheckObservation {
                conclusion: None,
                url: None,
                name: "rust".into(),
                status: old_status,
                run: None,
            }],
        );
        fake.set_checks(
            &repo,
            &second,
            vec![CheckObservation {
                conclusion: None,
                url: None,
                name: "rust".into(),
                status: ChecksStatus::Success,
                run: None,
            }],
        );
        if move_during_request {
            *interrupted.move_during_checks.lock().unwrap() =
                Some((e.mirror("project"), "dvb/t-1".into(), second.clone()));
        } else {
            fake.push(&repo, &e.mirror("project"), "dvb/t-1", &second)
                .await
                .unwrap();
        }
        // The sole PR observation follows CI, so this poll adopts a move
        // before recording or acting on the old head's result.
        dispatch::step_now(&e, &task).await.unwrap();
        assert_eq!(e.proj().tasks[&task].view.state, TaskState::InReview);
        assert_eq!(
            e.proj().tasks[&task].view.head.as_deref(),
            Some(second.as_str())
        );
        assert!(
            e.proj().tasks[&task].checks.is_none(),
            "adopting the new head invalidates old-head evidence"
        );
        assert!(
            e.decide(
                e.proj(),
                &task,
                Actor::Engine,
                dvb_engine::Decision::new(
                    "stale verdict",
                    Command::VerdictRecorded {
                        head: first.clone(),
                        open: 0
                    }
                ),
                |_| Ok(())
            )
            .is_err()
        );
        assert!(!support::events::all(&e).iter().any(|ev| matches!(&ev.kind,
            EventKind::ChecksObserved { head, status: ChecksStatus::Success, .. } if head == &second)));
        assert_eq!(fake.pr(&repo, 1).await.unwrap().state, PrState::Open);
        verdict_by(&e, &task, &second, "code-reviewer", false);
        e.decide(
            e.proj(),
            &task,
            Actor::Engine,
            dvb_engine::Decision::new(
                "new verdict",
                Command::VerdictRecorded {
                    head: second.clone(),
                    open: 0,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
        settle(&e, &task).await.unwrap();
        assert_eq!(e.proj().tasks[&task].view.state, TaskState::Done);
        assert_eq!(fake.pr(&repo, 1).await.unwrap().head, second);
        let old_success = support::events::all(&e).iter().any(|ev| {
            matches!(&ev.kind,
            EventKind::ChecksObserved { head, status: ChecksStatus::Success, .. } if head == &first)
        });
        assert!(!old_success);
    }
}

#[tokio::test]
async fn concurrent_last_merges_close_the_issue_once() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let Fixture {
        _tmp,
        e,
        repo,
        task,
        first: head,
        fake,
        ..
    } = merging_with(Some(barrier.clone())).await;
    let other = TaskId::new("other");
    fake.push(&repo, &e.mirror("project"), "dvb/other-1", &head)
        .await
        .unwrap();
    let pr = fake
        .open_pr(&repo, "dvb/other-1", "Other", "Body")
        .await
        .unwrap();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: other.clone(),
                    title: "Other".into(),
                    role: RoleId::new("builder"),
                    review: ReviewKind::Code,
                    summary: "Other".into(),
                    routine: false,
                }],
            },
            EventKind::BranchPublished {
                task: other.clone(),
                round: 1,
                branch: "dvb/other-1".into(),
                head: head.clone(),
            },
            EventKind::PrOpened {
                task: other.clone(),
                pr: pr.number,
                url: pr.url,
            },
            EventKind::TaskStateChanged {
                task: other.clone(),
                from: TaskState::Planned,
                to: TaskState::Merging,
            },
        ])
    })
    .unwrap();
    verdict_by(&e, &other, &head, "code-reviewer", false);
    approved(&e);
    // A branch behind main is brought up to date before it merges, so two
    // last merges meet when both landed (their replies lost) and are read back.
    fake.merge(&repo, 1, &head, "Task").await.unwrap();
    fake.merge(&repo, pr.number, &head, "Other").await.unwrap();
    let a = tokio::spawn({
        let (e, t) = (e.clone(), task.clone());
        async move { dispatch::step_now(&e, &t).await }
    });
    let b = tokio::spawn({
        let (e, t) = (e.clone(), other.clone());
        async move { dispatch::step_now(&e, &t).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), barrier.wait())
        .await
        .unwrap();
    // Neither task is done while both are held in cleanup.
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Merging);
    assert_eq!(e.proj().tasks[&other].view.state, TaskState::Merging);
    // Both tasks reach branch deletion before either can record TaskDone.
    barrier.wait().await;
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Done);
    assert_eq!(e.proj().tasks[&other].view.state, TaskState::Done);
    // Exactly one of the two last merges records the goal and closes its issue.
    assert_eq!(fake.closed_issues(&repo), vec![4]);
    let events = support::events::all(&e);
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::GoalDone { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::GoalIssueClosed { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_goal_issue_effect_recovers_after_restart_without_task_cleanup() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        fake,
        ..
    } = merging().await;
    approved(&e);
    // Crash after the recorded merge, before goal completion or task cleanup.
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PrMerged {
            task: task.clone(),
            pr: 1,
            sha: "merged".into(),
        }])
    })
    .unwrap();
    drop(e);
    let interrupted = Arc::new(InterruptedForge::new(fake.clone()));
    interrupted.close_issue_fails.store(true, Ordering::SeqCst);
    let forge: Arc<dyn Forge> = interrupted.clone();
    let e = support::engine::open_with(
        &tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher: None,
            ..Default::default()
        },
    );
    publish::reconcile::restore_goal_effects(&e).unwrap();
    assert!(
        publish::reconcile::close_goal_issue(&e, &GoalId::new("g"))
            .await
            .is_err()
    );
    assert!(fake.closed_issues(&repo).is_empty());
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Merging);
    interrupted.close_issue_fails.store(false, Ordering::SeqCst);
    publish::reconcile::close_goal_issue(&e, &GoalId::new("g"))
        .await
        .unwrap();
    publish::reconcile::close_goal_issue(&e, &GoalId::new("g"))
        .await
        .unwrap();
    assert_eq!(fake.closed_issues(&repo), vec![4]);
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Merging);
}

#[tokio::test]
async fn restoring_an_old_commit_hash_does_not_restore_its_invalidated_verdict() {
    let Fixture {
        _tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = merging().await;
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::BranchPublished {
            task: task.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: first,
        }])
    })
    .unwrap();
    assert!(matches!(
        dispatch::step_now(&e, &task).await,
        Err(dvb_engine::Fail::Refused(dvb_core::Refusal {
            code: dvb_core::RefusalCode::HeadMismatch,
            ..
        }))
    ));
    assert_eq!(fake.pr(&repo, 1).await.unwrap().state, PrState::Open);
}

// --- Behind its base ----------------------------------------------------

/// Another merge lands on the origin's main: `file` gets `contents`.
fn move_main(tmp: &std::path::Path, repo: &Repo, file: &str, contents: &str) -> String {
    let other = tmp.join(format!("main-{file}"));
    git(
        tmp,
        &[
            "clone",
            "--quiet",
            "-b",
            "main",
            &repo.0,
            other.to_str().unwrap(),
        ],
    );
    std::fs::write(other.join(file), contents).unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "--quiet", "-m", "another task"]);
    git(&other, &["push", "--quiet", "origin", "main"]);
    git(&other, &["rev-parse", "HEAD"])
}

fn ci(e: &AppState, repo: &Repo) {
    ci_named(e, repo, &["rust"]);
}

fn ci_named(e: &AppState, repo: &Repo, names: &[&str]) {
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![support::events::project_event_with_ci(
            &(ProjectId::new("p")),
            "project",
            &(repo.0),
            CiMode::Expected(names.iter().map(|n| (*n).into()).collect()),
        )])
    })
    .unwrap();
}

fn green(fake: &FakeForge, repo: &Repo, head: &str) {
    fake.set_checks(
        repo,
        head,
        vec![CheckObservation {
            conclusion: None,
            url: None,
            name: "rust".into(),
            status: ChecksStatus::Success,
            run: None,
        }],
    );
}

fn updates(e: &AppState) -> Vec<(String, String, bool)> {
    support::events::all(e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::BranchUpdated {
                from,
                head,
                same_change,
                ..
            } = ev.kind
            {
                Some((from, head, same_change))
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn a_behind_branch_is_brought_up_to_date_and_merges_with_its_carried_verdict() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = merging().await;
    fake.require_up_to_date(&repo);
    ci(&e, &repo);
    green(&fake, &repo, &first);
    let main = move_main(tmp.path(), &repo, "other", "other\n");

    // The merge is refused as behind; the engine brings the branch up to date.
    dispatch::step_now(&e, &task).await.unwrap();
    let [(from, updated, true)] = &updates(&e)[..] else {
        panic!("one update with the same change: {:?}", updates(&e));
    };
    assert_eq!(from, &first);
    let t = e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::Merging);
    assert_eq!(t.view.head.as_ref(), Some(updated));
    assert_eq!(fake.pr(&repo, 1).await.unwrap().head, *updated);
    // The update is the engine's own merge commit, with the neutral identity.
    let origin = std::path::Path::new(&repo.0);
    assert_eq!(
        git(
            origin,
            &["log", "-1", "--format=%an <%ae> %cn <%ce>", updated]
        ),
        format!(
            "{n} <{m}> {n} <{m}>",
            n = publish::NEUTRAL_NAME,
            m = publish::NEUTRAL_EMAIL
        )
    );
    assert_eq!(
        git(origin, &["log", "-1", "--format=%P", updated]),
        format!("{first} {main}")
    );
    // The clean verdict is carried over to the new head, and says why.
    assert!(
        t.facts
            .verdict
            .as_ref()
            .is_some_and(|v| v.open == 0 && v.head.as_bytes() == updated.as_bytes())
    );
    assert!(support::events::all(&e).iter().any(|ev| matches!(&ev.kind,
        EventKind::VerdictRecorded { head, summary, .. }
            if head == updated && summary.starts_with("Carried over from the review of"))));

    // CI runs again on the new head before the merge.
    dispatch::step_now(&e, &task).await.unwrap();
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::AwaitingChecks);
    green(&fake, &repo, updated);
    settle(&e, &task).await.unwrap();
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Done);
    assert_eq!(fake.pr(&repo, 1).await.unwrap().state, PrState::Merged);
    assert_eq!(git(origin, &["show", "main:file"]), "changed");
    assert_eq!(git(origin, &["show", "main:other"]), "other");
    assert!(
        !e.proj()
            .runs
            .values()
            .any(|r| r.assignment.reviews.as_ref() == Some(updated))
    );
}

/// No ruleset refuses a branch behind main: two tasks green on the same base
/// must not both merge as they are.
#[tokio::test]
async fn without_a_ruleset_the_second_of_two_green_tasks_is_updated_and_checked_again() {
    let f = fixture_plan(
        None,
        None,
        None,
        vec![PlannedTask {
            after: vec![],
            task: TaskId::new("t2"),
            title: "Other".into(),
            role: RoleId::new("builder"),
            review: ReviewKind::Code,
            summary: "Other".into(),
            routine: false,
        }],
    )
    .await;
    let (e, repo, fake, task) = (&f.e, &f.repo, &f.fake, &f.task);
    let origin = std::path::Path::new(&repo.0);
    // The second task's change, built on the same base as the first's.
    let ws = f._tmp.path().join("t2");
    git(
        f._tmp.path(),
        &[
            "clone",
            "--quiet",
            "-b",
            "main",
            &repo.0,
            ws.to_str().unwrap(),
        ],
    );
    std::fs::write(ws.join("other"), "other\n").unwrap();
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "--quiet", "-m", "other"]);
    git(&ws, &["push", "--quiet", "origin", "HEAD:refs/heads/seed2"]);
    let mirror = e.mirror("project");
    let head = fake.fetch(repo, &mirror, "seed2").await.unwrap();
    fake.push(repo, &mirror, "dvb/t-2", &head).await.unwrap();
    let pr = fake
        .open_pr(repo, "dvb/t-2", "Other", "Body")
        .await
        .unwrap();
    let t2 = TaskId::new("t2");
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::BranchPublished {
                task: t2.clone(),
                round: 1,
                branch: "dvb/t-2".into(),
                head: head.clone(),
            },
            EventKind::PrOpened {
                task: t2.clone(),
                pr: pr.number,
                url: pr.url,
            },
            EventKind::TaskStateChanged {
                task: t2.clone(),
                from: TaskState::Planned,
                to: TaskState::Merging,
            },
        ])
    })
    .unwrap();
    verdict_by(e, &t2, &head, "code-reviewer", false);
    ci(e, repo);
    green(fake, repo, &f.first);
    green(fake, repo, &head);

    settle(e, task).await.unwrap();
    assert_eq!(e.proj().tasks[task].view.state, TaskState::Done);
    let main = git(origin, &["rev-parse", "main"]);

    // The second is behind main now: updated, not merged.
    dispatch::step_now(e, &t2).await.unwrap();
    let [(from, updated, true)] = &updates(e)[..] else {
        panic!("one update with the same change: {:?}", updates(e));
    };
    assert_eq!(from, &head);
    assert_eq!(
        git(origin, &["log", "-1", "--format=%P", updated]),
        format!("{head} {main}")
    );
    assert_eq!(fake.pr(repo, pr.number).await.unwrap().state, PrState::Open);
    assert_eq!(git(origin, &["rev-parse", "main"]), main);

    // Its checks run again on the new head before it merges.
    dispatch::step_now(e, &t2).await.unwrap();
    assert_eq!(e.proj().tasks[&t2].view.state, TaskState::AwaitingChecks);
    assert_eq!(fake.pr(repo, pr.number).await.unwrap().state, PrState::Open);
    green(fake, repo, updated);
    settle(e, &t2).await.unwrap();
    assert_eq!(e.proj().tasks[&t2].view.state, TaskState::Done);
    assert_eq!(
        fake.pr(repo, pr.number).await.unwrap().state,
        PrState::Merged
    );
    assert_eq!(git(origin, &["show", "main:file"]), "changed");
    assert_eq!(git(origin, &["show", "main:other"]), "other");
}

#[tokio::test]
async fn a_behind_branch_that_conflicts_with_main_fails_with_a_plain_reason() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = merging().await;
    fake.require_up_to_date(&repo);
    move_main(tmp.path(), &repo, "file", "conflict\n");
    dispatch::step_now(&e, &task).await.unwrap();
    assert_eq!(
        e.proj().tasks[&task].view.state,
        TaskState::Failed {
            reason: "merging failed: conflicts with main; needs a fix round".into()
        }
    );
    assert!(updates(&e).is_empty());
    assert_eq!(fake.pr(&repo, 1).await.unwrap().head, first);
}

#[tokio::test]
async fn a_behind_branch_whose_update_message_hits_the_scan_fails_the_task() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        fake,
        ..
    } = fixture(None, Some("up to date"), None).await;
    fake.require_up_to_date(&repo);
    move_main(tmp.path(), &repo, "other", "other\n");
    dispatch::step_now(&e, &task).await.unwrap();
    assert!(matches!(
        &e.proj().tasks[&task].view.state,
        TaskState::Failed { reason } if reason.contains("publish blocked")
    ));
    assert!(updates(&e).is_empty());
}

#[tokio::test]
async fn a_behind_branch_whose_change_differs_is_reviewed_again() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = merging().await;
    fake.require_up_to_date(&repo);
    // Main already has the task's change: the PR's own patch differs (empty).
    move_main(tmp.path(), &repo, "file", "changed\n");
    dispatch::step_now(&e, &task).await.unwrap();
    let [(from, updated, false)] = &updates(&e)[..] else {
        panic!("one update with a changed patch: {:?}", updates(&e));
    };
    assert_eq!(from, &first);
    let t = e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_eq!(t.view.head.as_ref(), Some(updated));
    assert!(t.facts.verdict.is_none());
    assert!(t.needs_reviewer());
    assert_eq!(fake.pr(&repo, 1).await.unwrap().state, PrState::Open);
}

#[tokio::test]
async fn earlier_updates_do_not_stop_another_update_at_merge() {
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = merging().await;
    let earlier = (0..=dvb_core::caps::BRANCH_REPAIRS_PER_TASK)
        .map(|_| EventKind::BranchUpdated {
            task: task.clone(),
            branch: "dvb/t-1".into(),
            base: first.clone(),
            from: first.clone(),
            head: first.clone(),
            same_change: true,
        })
        .collect();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(earlier)
    })
    .unwrap();
    verdict_by(&e, &task, &first, "code-reviewer", false);
    fake.require_up_to_date(&repo);
    move_main(tmp.path(), &repo, "other", "other\n");
    dispatch::step_now(&e, &task).await.unwrap();
    // Other tasks' merges are not the task's problem: it is updated again.
    let t = e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::Merging);
    assert_eq!(t.repairs, 0);
    assert_ne!(t.view.head.as_ref(), Some(&first));
    assert_eq!(updates(&e).len(), 5);
}

/// The fixture's task, behind main by another task's merge, NoCi, strict.
async fn behind_main() -> Fixture {
    let f = merging().await;
    f.fake.require_up_to_date(&f.repo);
    move_main(f._tmp.path(), &f.repo, "other", "other\n");
    f
}

/// The update was recorded once, with its verdict carried over, and merges.
async fn recorded_once_and_merges(f: &Fixture, updated: &str) {
    let [(from, head, true)] = &updates(&f.e)[..] else {
        panic!("one carried update: {:?}", updates(&f.e));
    };
    assert_eq!((from.as_str(), head.as_str()), (f.first.as_str(), updated));
    let t = f.e.proj().tasks[&f.task].clone();
    assert_eq!(t.view.state, TaskState::Merging);
    assert_eq!(t.view.head.as_deref(), Some(updated));
    assert_eq!(t.repairs, 0);
    assert!(t.updated_base.is_some());
    assert!(
        t.facts
            .verdict
            .as_ref()
            .is_some_and(|v| v.open == 0 && v.head.as_bytes() == updated.as_bytes())
    );
    let published = |e: &AppState| {
        support::events::all(e)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::BranchPublished { .. }))
            .count()
    };
    assert_eq!(published(&f.e), 1, "only the fixture's publication");
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().head, updated);
}

#[tokio::test]
async fn a_pr_read_catching_up_with_the_update_keeps_the_carried_verdict() {
    let f = behind_main().await;
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let updated = f.e.proj().tasks[&f.task].view.head.clone().unwrap();
    assert_ne!(updated, f.first);
    // The first PR read still shows the head the update replaced.
    *f.interrupted.stale_pr_once.lock().unwrap() = Some(f.first.clone());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    recorded_once_and_merges(&f, &updated).await;
}

#[tokio::test]
async fn a_lost_push_reply_records_the_landed_update_once() {
    let f = behind_main().await;
    f.interrupted.push_reply_lost.store(true, Ordering::SeqCst);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(matches!(
        f.e.proj().tasks[&f.task].view.state,
        TaskState::RetryPending { .. }
    ));
    assert!(updates(&f.e).is_empty());
    let updated = f.fake.pr(&f.repo, 1).await.unwrap().head;
    assert_ne!(updated, f.first);
    f.e.decide(
        f.e.proj(),
        &f.task,
        Actor::Engine,
        dvb_engine::Decision::new("retry", Command::RetryDue),
        |_| Ok(()),
    )
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    recorded_once_and_merges(&f, &updated).await;
}

#[tokio::test]
async fn a_crash_after_the_update_push_records_it_once_on_restart() {
    let Fixture {
        _tmp,
        e,
        repo,
        task,
        base,
        first,
        second,
        fake,
        interrupted,
        readings,
        clock,
    } = behind_main().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *interrupted.push_barrier.lock().unwrap() = Some(barrier.clone());
    // The push lands; the engine dies before it hears back.
    let mut step = Box::pin(dispatch::step_now(&e, &task));
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::select! {
            result = &mut step => panic!("push reply was not held: {result:?}"),
            _ = barrier.wait() => {}
        }
    })
    .await
    .expect("the push did not land before the crash");
    drop(step);
    assert!(updates(&e).is_empty());
    let updated = fake.pr(&repo, 1).await.unwrap().head;
    assert_ne!(updated, first);
    drop(e);
    let forge: Arc<dyn Forge> = interrupted.clone();
    let e = support::engine::open_with(
        &_tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher: None,
            ..Default::default()
        },
    );
    dvb_engine::reconcile::mark(&e).unwrap();
    let observed = dvb_core::state::Observed {
        unit: dvb_core::launcher::UnitState::Inactive,
        head: None,
    };
    let state = e
        .decide(
            e.proj(),
            &task,
            Actor::Engine,
            dvb_engine::Decision::new("reconcile", Command::Reconcile(observed)),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(state, TaskState::Merging);
    dispatch::step_now(&e, &task).await.unwrap();
    let f = Fixture {
        readings,
        clock,
        _tmp,
        e,
        repo,
        task,
        base,
        first,
        second,
        fake,
        interrupted,
    };
    recorded_once_and_merges(&f, &updated).await;
}

/// the push landed but opening the PR was refused, so the task failed with
/// a head and no PR. Retry goes back to publishing on the same branch and head,
/// opens the PR before any review, and the task merges.
#[tokio::test]
async fn a_retry_after_a_failed_pr_opening_opens_it_and_merges() {
    let Fixture {
        _tmp,
        e,
        repo,
        first,
        fake,
        interrupted,
        ..
    } = merging().await;
    let t = TaskId::new("t2");
    let builder = RunId::new("builder-t2");
    let mirror = e.mirror("project");
    fake.push(&repo, &mirror, "dvb/t2-1", &first).await.unwrap();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: t.clone(),
                    title: "Second task".into(),
                    role: RoleId::new("builder"),
                    review: ReviewKind::Code,
                    summary: "Task".into(),
                    routine: false,
                }],
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(builder.clone())
                    .task(Some(t.clone()))
                    .role(RoleId::new("builder"))
                    .reason("Build".into())
                    .base(Some(first.clone()))
                    .build(),
            ),
            EventKind::RunEnded {
                run: builder,
                reason: dvb_core::event::EndReason::Completed,
                detail: None,
            },
            EventKind::BranchPublished {
                task: t.clone(),
                round: 1,
                branch: "dvb/t2-1".into(),
                head: first.clone(),
            },
            EventKind::TaskStateChanged {
                task: t.clone(),
                from: TaskState::Planned,
                to: TaskState::Publishing,
            },
        ])
    })
    .unwrap();
    approved(&e);
    interrupted.open_pr_fails.store(true, Ordering::SeqCst);
    dispatch::step_now(&e, &t).await.unwrap();
    let failed = e.proj().tasks[&t].view.clone();
    assert!(
        matches!(failed.state, TaskState::Failed { .. }),
        "{:?}",
        failed.state
    );
    assert_eq!(
        (failed.head.as_deref(), failed.pr),
        (Some(first.as_str()), None)
    );

    // The forge recovers; the owner retries.
    interrupted.open_pr_fails.store(false, Ordering::SeqCst);
    let state = dispatch::retry_failed(&e, &t, Actor::Owner, None, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(state, TaskState::Publishing);
    dispatch::step_now(&e, &t).await.unwrap();
    let opened = e.proj().tasks[&t].view.clone();
    assert_eq!(opened.state, TaskState::InReview);
    assert_eq!(opened.head.as_deref(), Some(first.as_str()));
    let pr = opened.pr.expect("the PR is opened before review");
    assert_eq!(fake.pr(&repo, pr).await.unwrap().state, PrState::Open);
    // Reviewed clean, it merges.
    verdict_by(&e, &t, &first, "code-reviewer", false);
    let state = e
        .decide(
            e.proj(),
            &t,
            Actor::Engine,
            dvb_engine::Decision::new(
                "verdict",
                Command::VerdictRecorded {
                    head: first.clone(),
                    open: 0,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(state, TaskState::Merging);
    settle(&e, &t).await.unwrap();
    assert_eq!(e.proj().tasks[&t].view.state, TaskState::Done);
    assert_eq!(fake.pr(&repo, pr).await.unwrap().state, PrState::Merged);
}

/// One approval covers both tasks. The first has a clean Fake-forge PR;
/// the second has never had a start request or run.
async fn approved_two_task_plan() -> Fixture {
    let f = fixture_plan(
        None,
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
        vec![PlannedTask {
            after: vec![],
            task: TaskId::new("next"),
            title: "Follow up".into(),
            role: RoleId::new("backend"),
            review: ReviewKind::Code,
            summary: "Build on the first merge".into(),
            routine: false,
        }],
    )
    .await;
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    approved(&f.e);
    *f.readings.disk.lock().unwrap() = Some(u64::MAX);
    f
}

async fn next_builder(e: &AppState) -> RunAssignment {
    let next = TaskId::new("next");
    support::wait::FAST
        .until("the second task's builder", || {
            e.proj().tasks[&next].view.state == TaskState::Running
        })
        .await;
    let proj = e.proj();
    let a = proj.runs[proj.tasks[&next].facts.run.as_ref().unwrap()]
        .assignment
        .clone();
    assert_eq!(a.role, RoleId::new("backend"));
    assert_eq!(a.hire, HireId::new("h-backend"));
    assert_eq!(a.effort, Effort::Medium);
    assert!(proj.tasks[&next].waiting.is_none());
    assert!(
        proj.runs.values().all(|r| r.assignment.task.is_some()),
        "no orchestrator run was needed"
    );
    drop(proj);
    a
}

#[tokio::test]
async fn merging_the_first_of_two_approved_tasks_starts_the_second_without_an_orchestrator() {
    let f = approved_two_task_plan().await;
    // Record the reviewer's already completed unit without triggering admission:
    // only the merge below can start the second task.
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    assert!(f.e.proj().tasks[&TaskId::new("next")].facts.run.is_none());
    settle(&f.e, &f.task).await.unwrap();
    let a = next_builder(&f.e).await;
    let base = f
        .fake
        .fetch(&f.repo, &f.e.mirror("project"), "main")
        .await
        .unwrap();
    assert_eq!(a.base, Some(base));
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    assert_eq!(f.e.proj().goals[&GoalId::new("g")].tasks.len(), 2);
}

#[tokio::test]
async fn a_freed_run_slot_starts_an_approved_task_that_was_never_asked_to_start() {
    let f = approved_two_task_plan().await;
    // Occupy every slot with reviewer units while the first task merges.
    let reviewer = f.e.proj().runs[&RunId::new("reviewer")].assignment.clone();
    let extra = (1..3)
        .map(|i| {
            EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(reviewer.clone())
                    .review_stage(None)
                    .goal_version(None)
                    .run(RunId::new(format!("busy-{i}")))
                    .build(),
            )
        })
        .collect();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(extra))
        .unwrap();
    settle(&f.e, &f.task).await.unwrap();
    support::wait::FAST
        .until("the automatic start retained at the cap", || {
            f.e.proj().tasks[&TaskId::new("next")].waiting.is_some()
        })
        .await;
    assert!(f.e.proj().tasks[&TaskId::new("next")].facts.run.is_none());
    f.e.end_run(
        &RunId::new("reviewer"),
        dvb_core::event::EndReason::Completed,
        None,
    )
    .unwrap();
    next_builder(&f.e).await;
    assert_eq!(f.e.proj().live_runs().count(), 3);
}

#[tokio::test]
async fn an_automatic_start_at_the_cap_is_admitted_when_another_goals_run_ends() {
    let f = approved_two_task_plan().await;
    let p = ProjectId::new("p");
    // Goal A has no live runs left; all slots belong to other goals.
    f.e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    for i in 0..3 {
        let goal = GoalId::new(format!("foreign-{i}"));
        let task = TaskId::new(format!("foreign-{i}"));
        let run = RunId::new(format!("foreign-{i}"));
        let mut a = f.e.proj().runs[&RunId::new("reviewer")].assignment.clone();
        a.run = run.clone();
        a.task = Some(task.clone());
        f.e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::GoalStated {
                    goal: goal.clone(),
                    text: "Other work".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal: goal.clone(),
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: task.clone(),
                        title: "Other work".into(),
                        role: RoleId::new("backend"),
                        review: ReviewKind::Code,
                        summary: "Other work".into(),
                        routine: false,
                    }],
                },
                EventKind::GoalApproved { goal },
                EventKind::RunStarted(a),
                EventKind::VerdictRecorded {
                    review_stage: None,
                    verdict: VerdictId::new(format!("foreign-{i}")),
                    task,
                    run,
                    head: f.first.clone(),
                    role: RoleId::new("code-reviewer"),
                    model: "fake-1".into(),
                    vendor: Vendor::Fake,
                    findings: Vec::new(),
                    summary: "Ready".into(),
                },
            ])
        })
        .unwrap();
    }
    settle(&f.e, &f.task).await.unwrap();
    support::wait::FAST
        .until("goal A's retained admission", || {
            f.e.proj().tasks[&TaskId::new("next")].waiting.is_some()
        })
        .await;
    assert!(f.e.proj().tasks[&TaskId::new("next")].facts.run.is_none());
    f.e.end_run(
        &RunId::new("foreign-0"),
        dvb_core::event::EndReason::Completed,
        None,
    )
    .unwrap();
    next_builder(&f.e).await;
    assert_eq!(f.e.proj().live_runs().count(), 3);
}

#[tokio::test]
async fn a_failed_automatic_preparation_retries_without_another_run_ending() {
    use std::sync::atomic::Ordering;
    let f = approved_two_task_plan().await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    // The merge reads main first: the failure is for the preparation after it.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *f.interrupted.merge_barrier.lock().unwrap() = Some(barrier.clone());
    let merged = tokio::spawn({
        let (e, t) = (f.e.clone(), f.task.clone());
        async move { settle(&e, &t).await }
    });
    barrier.wait().await;
    f.interrupted.fetch_fails_once.store(true, Ordering::SeqCst);
    barrier.wait().await;
    merged.await.unwrap().unwrap();
    support::wait::FAST
        .until(
            "the failed automatic preparation retained for retry",
            || f.e.proj().tasks[&TaskId::new("next")].waiting.is_some(),
        )
        .await;
    assert!(!f.interrupted.fetch_fails_once.load(Ordering::SeqCst));
    assert_eq!(f.e.proj().live_runs().count(), 0);
    assert!(f.e.proj().tasks[&TaskId::new("next")].facts.run.is_none());
    let before_retry = f.e.store.last_id().unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    tokio::time::resume();
    next_builder(&f.e).await;
    let events = f.e.store.after(before_retry).unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::RunEnded { .. }))
    );
    assert_eq!(events.iter().filter(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.task == Some(TaskId::new("next")))).count(), 1);
}

/// The project's roles and hires (a hire for the task's builder role too) and
/// the goal approved; the task then fails in its merge (`fail`), ready to retry.
async fn failed_merge(f: &Fixture, fail: Option<&str>) {
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<EventKind> = roles.into_iter().chain(hires).collect();
    kinds.push(EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-builder"),
        role: RoleId::new("builder"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: false,
    }));
    // The fixture's reviewer has finished.
    kinds.push(EventKind::RunEnded {
        run: RunId::new("reviewer"),
        reason: dvb_core::event::EndReason::Completed,
        detail: None,
    });
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(&f.e);
    match fail {
        None => dispatch::step_now(&f.e, &f.task).await.unwrap(),
        Some(reason) => {
            let to = TaskState::Failed {
                reason: reason.into(),
            };
            let kind = EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::Merging,
                to,
            };
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![kind])
            })
            .unwrap();
        }
    }
    assert!(
        matches!(
            f.e.proj().tasks[&f.task].view.state,
            TaskState::Failed { .. }
        ),
        "{:?}",
        f.e.proj().tasks[&f.task].view.state
    );
}

async fn until_state(e: &AppState, task: &TaskId, state: TaskState) {
    for _ in 0..200 {
        if e.proj().tasks[task].view.state == state {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "{task} never {state:?}: {:?}",
        e.proj().tasks[task].view.state
    );
}

/// A task that failed because its branch conflicts with main retries as a
/// fix round: a builder on the kept branch, told to merge main and resolve the
/// conflict; its tree is published as a merge with main, reviewed and merged.
#[tokio::test]
async fn a_conflict_failed_task_retries_into_a_fix_round_and_merges() {
    let live = Arc::new(support::launcher::FakeLauncher::new(
        dvb_core::launcher::UnitState::Active,
    ));
    let f = merging_on(None, Some(live.clone())).await;
    f.fake.require_up_to_date(&f.repo);
    let main = move_main(f._tmp.path(), &f.repo, "file", "conflict\n");
    failed_merge(&f, None).await;
    let Fixture {
        _tmp: tmp,
        e,
        repo,
        task,
        first,
        fake,
        ..
    } = f;

    let state = dispatch::retry_failed(&e, &task, Actor::Owner, None, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(state, TaskState::Preparing);
    let mirror = e.mirror("project");
    assert_eq!(
        publish::merges(&mirror, task.as_str()).await,
        Some(main.clone())
    );
    // Each builder of the round starts on the head and is told to merge main.
    let round_builder = async |after: Option<&RunId>| {
        for _ in 0..400 {
            let t = e.proj().tasks[&task].clone();
            if t.view.state == TaskState::Running && t.facts.builder.as_ref() != after {
                let run = t.facts.builder.unwrap();
                let a = e.proj().runs[&run].assignment.clone();
                assert_eq!(a.reviews, None);
                assert_eq!(a.base.as_deref(), Some(first.as_str()));
                let dir = e.cfg.paths.work.join(run.as_str());
                let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap();
                assert!(
                    prompt.contains(&format!("git merge --no-edit {main}")),
                    "{prompt}"
                );
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!(
            "no builder of the round: {:?}",
            e.proj().tasks[&task].view.state
        );
    };
    let crashed = round_builder(None).await;
    // It crashes before collecting: the automatic retry continues the round.
    e.end_run(&crashed, dvb_core::event::EndReason::Crashed, None)
        .unwrap();
    live.end(&crashed);
    let stopped = round_builder(Some(&crashed)).await;
    // The owner stops and resumes it: the resumed builder continues the round.
    dispatch::stop(&e, &task, Actor::Owner).await.unwrap();
    dispatch::resume(&e, &task, Actor::Owner).await.unwrap();
    let run = round_builder(Some(&stopped)).await;
    let dir = e.cfg.paths.work.join(run.as_str());

    // The builder merges main in its checkout and resolves the conflict.
    let ws = dir.join("ws");
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            mirror.to_str().unwrap(),
            ws.to_str().unwrap(),
        ],
    );
    git(&ws, &["checkout", "--quiet", "--detach", &first]);
    let merged = std::process::Command::new("git")
        .current_dir(&ws)
        .args(["merge", "--no-edit", &main])
        .output()
        .unwrap();
    assert!(!merged.status.success(), "the merge conflicts");
    std::fs::write(ws.join("file"), "changed and conflict\n").unwrap();
    git(&ws, &["add", "file"]);
    let tree = git(&ws, &["write-tree"]);
    let collected = git(&ws, &["commit-tree", &tree, "-p", &first, "-m", "snapshot"]);
    collect(&ws, &collected);
    e.append_checked(
        Some(&ProjectId::new("p")),
        Actor::Run(run.clone()),
        |_, _| {
            Ok(vec![EventKind::RunCollected {
                outcome: None,
                run: run.clone(),
                head: Some(collected),
                summary: None,
            }])
        },
    )
    .unwrap();
    e.run_command(
        &run,
        dvb_engine::Decision::new("collected", Command::HeadWritten),
    )
    .unwrap();
    e.end_run(&run, dvb_core::event::EndReason::Completed, None)
        .unwrap();

    // Published on the same branch and PR as a merge with main, then reviewed.
    until_state(&e, &task, TaskState::InReview).await;
    let head = e.proj().tasks[&task].view.head.clone().unwrap();
    let origin = std::path::Path::new(&repo.0);
    assert_eq!(
        git(origin, &["log", "-1", "--format=%P", &head]),
        format!("{first} {main}")
    );
    assert_eq!(fake.pr(&repo, 1).await.unwrap().head, head);
    assert_eq!(
        publish::merges(&mirror, task.as_str()).await,
        None,
        "the round is over"
    );
    verdict_by(&e, &task, &head, "code-reviewer", false);
    let state = e
        .decide(
            e.proj(),
            &task,
            Actor::Engine,
            dvb_engine::Decision::new(
                "verdict",
                Command::VerdictRecorded {
                    head: head.clone(),
                    open: 0,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(state, TaskState::Merging);
    settle(&e, &task).await.unwrap();
    assert_eq!(e.proj().tasks[&task].view.state, TaskState::Done);
    assert_eq!(fake.pr(&repo, 1).await.unwrap().state, PrState::Merged);
    assert_eq!(git(origin, &["show", "main:file"]), "changed and conflict");
}

/// A task that failed in its merge for another reason (main has not moved
/// into a conflict), its head approved, retries into its merge: no builder
/// and no review of the unchanged head.
#[tokio::test]
async fn a_failed_merge_without_a_conflict_retries_into_its_merge_without_a_run() {
    let f = merging_on(
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
    )
    .await;
    failed_merge(&f, Some("the merge failed: GitHub refused the merge")).await;
    let before = f.e.store.last_id().unwrap();
    let state = dispatch::retry_failed(&f.e, &f.task, Actor::Owner, None, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(state, TaskState::Merging);
    assert_eq!(
        publish::merges(&f.e.mirror("project"), f.task.as_str()).await,
        None
    );
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    assert_eq!(runs_since(&f.e, before), vec![]);
}

/// A review fix round that changes nothing (its builder closed the
/// finding outside the code, or rejected it) does not fail the task: the head
/// it started on is the next round, nothing is pushed, and it is reviewed again.
#[tokio::test]
async fn a_fix_round_that_changes_nothing_is_reviewed_again() {
    unchanged_fix_round(None).await;
}

#[tokio::test]
async fn a_summary_only_fix_replaces_the_body_and_is_reviewed_again() {
    unchanged_fix_round(Some(
        "Corrected the description. Checked the unchanged head.",
    ))
    .await;
}

async fn unchanged_fix_round(summary: Option<&str>) {
    let f = merging_on(
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
    )
    .await;
    let p = ProjectId::new("p");
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<EventKind> = roles.into_iter().chain(hires).collect();
    kinds.push(EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-builder"),
        role: RoleId::new("builder"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: false,
    }));
    kinds.push(EventKind::RunEnded {
        run: RunId::new("reviewer"),
        reason: dvb_core::event::EndReason::Completed,
        detail: None,
    });
    kinds.push(EventKind::TaskStateChanged {
        task: f.task.clone(),
        from: TaskState::Merging,
        to: TaskState::InReview,
    });
    f.e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(&f.e);
    verdict_by(&f.e, &f.task, &f.first, "code-reviewer", true);
    let reviewed = RunId::new(format!("{}-code-reviewer-{}-true", f.task, f.first));
    let ended = EventKind::RunEnded {
        run: reviewed,
        reason: dvb_core::event::EndReason::Completed,
        detail: None,
    };
    f.e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(vec![ended]))
        .unwrap();
    let Fixture {
        _tmp: tmp,
        e,
        task,
        first,
        interrupted,
        fake,
        repo,
        ..
    } = f;

    // The fix round's builder starts on the head and commits nothing.
    dispatch::step_now(&e, &task).await.unwrap();
    until_state(&e, &task, TaskState::Running).await;
    let run = e.proj().tasks[&task].facts.builder.clone().unwrap();
    assert_eq!(
        e.proj().runs[&run].assignment.base.as_deref(),
        Some(first.as_str())
    );
    let ws = e.cfg.paths.work.join(run.as_str()).join("ws");
    let mirror = e.mirror("project");
    let (mirror, ws_path) = (mirror.to_str().unwrap(), ws.to_str().unwrap());
    git(tmp.path(), &["clone", "--quiet", mirror, ws_path]);
    let tree = format!("{first}^{{tree}}");
    let collected = git(&ws, &["commit-tree", &tree, "-p", &first, "-m", "snapshot"]);
    collect(&ws, &collected);
    let kind = EventKind::RunCollected {
        outcome: None,
        run: run.clone(),
        head: Some(collected),
        summary: summary.map(str::to_owned),
    };
    e.append_checked(Some(&p), Actor::Run(run.clone()), |_, _| Ok(vec![kind]))
        .unwrap();
    e.run_command(
        &run,
        dvb_engine::Decision::new("collected", Command::HeadWritten),
    )
    .unwrap();
    e.end_run(&run, dvb_core::event::EndReason::Completed, None)
        .unwrap();

    // Round 2 is the same head, published by that builder, and reviewed again.
    support::wait::FAST
        .until("the unchanged round's reviewer", || {
            e.proj().runs.values().any(|r| {
                r.assignment.reviews.as_deref() == Some(first.as_str()) && r.ended.is_none()
            })
        })
        .await;
    let t = e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_eq!(
        (t.facts.round, t.view.head.as_deref()),
        (2, Some(first.as_str()))
    );
    assert_eq!(t.facts.published_by, Some(run.clone()));
    let ids = format!("Task {task}, round 2, built by run {run}.");
    let body = match summary {
        Some(summary) => format!("{summary}\n\n{ids}"),
        None => ids,
    };
    assert_eq!(fake.pr_body(&repo, t.view.pr.unwrap()), Some(body));
    assert!(t.needs_reviewer());
    // The re-review is told to recheck the finding the round left in place.
    let reviewer = e
        .proj()
        .runs
        .values()
        .find(|r| r.assignment.reviews.as_deref() == Some(first.as_str()) && r.ended.is_none())
        .map(|r| r.assignment.run.clone())
        .unwrap();
    let prompt = e.cfg.paths.work.join(reviewer.as_str()).join("prompt.md");
    support::wait::FAST
        .until("the reviewer's prompt", || prompt.is_file())
        .await;
    let prompt = std::fs::read_to_string(prompt).unwrap();
    assert!(
        prompt.contains("## Earlier findings to recheck"),
        "{prompt}"
    );
    assert!(prompt.contains("- [High] file: Broken. Broken"), "{prompt}");
    assert!(!interrupted.calls.lock().unwrap().contains(&"push"));
    let events = support::events::all(&e);
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::TaskFailed { .. }))
    );
}

// --- The merge rules ----------------------------------------------

/// A verdict on `head` by a run of `role`, with `open` open findings.
fn verdict_by(e: &AppState, task: &TaskId, head: &str, role: &str, open: bool) {
    let run = RunId::new(format!("{task}-{role}-{head}-{open}"));
    let finding = dvb_core::run_verbs::Finding {
        severity: dvb_core::run_verbs::Severity::High,
        file: "file".into(),
        line: None,
        title: "Broken".into(),
        detail: "Broken".into(),
        open,
    };
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(run.clone())
                    .task(Some(task.clone()))
                    .role(RoleId::new(role))
                    .reason("Review".into())
                    .reviews(Some(head.into()))
                    .base(Some(head.into()))
                    .build(),
            ),
            EventKind::VerdictRecorded {
                review_stage: None,
                verdict: VerdictId::new(format!("v-{run}")),
                task: task.clone(),
                head: head.into(),
                run,
                role: RoleId::new(role),
                model: "fake-1".into(),
                vendor: Vendor::Fake,
                findings: if open { vec![finding] } else { vec![] },
                summary: "Reviewed".into(),
            },
        ])
    })
    .unwrap();
}

/// One table over the merge rules: a mergeable task merges, and each row that
/// breaks one condition never reaches the forge's merge.
#[tokio::test]
async fn no_merge_unless_every_rule_holds() {
    type Break = fn(&Fixture);
    // None closes the PR through the async test control.
    let rows: [(&str, Option<Break>, bool); 8] = [
        ("all good", Some(|_| {}), true),
        (
            "wrong reviewer",
            Some(|f| verdict_by(&f.e, &f.task, &f.first, "backend", false)),
            false,
        ),
        (
            "open findings",
            Some(|f| verdict_by(&f.e, &f.task, &f.first, "code-reviewer", true)),
            false,
        ),
        (
            "verdict on an older head",
            Some(|f| {
                let kind = EventKind::BranchPublished {
                    task: f.task.clone(),
                    round: 2,
                    branch: "dvb/t-1".into(),
                    head: f.second.clone(),
                };
                f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                    Ok(vec![kind])
                })
                .unwrap();
                green(&f.fake, &f.repo, &f.second);
                verdict_by(&f.e, &f.task, &f.first, "code-reviewer", false);
            }),
            false,
        ),
        ("PR closed", None, false),
        (
            "check failed",
            Some(|f| {
                f.fake.set_checks(
                    &f.repo,
                    &f.first,
                    vec![CheckObservation {
                        conclusion: None,
                        url: None,
                        name: "rust".into(),
                        status: ChecksStatus::Failed,
                        run: None,
                    }],
                )
            }),
            false,
        ),
        (
            "check pending",
            Some(|f| {
                f.fake.set_checks(
                    &f.repo,
                    &f.first,
                    vec![CheckObservation {
                        conclusion: None,
                        url: None,
                        name: "rust".into(),
                        status: ChecksStatus::Pending,
                        run: None,
                    }],
                )
            }),
            false,
        ),
        (
            "check missing",
            Some(|f| f.fake.set_checks(&f.repo, &f.first, vec![])),
            false,
        ),
    ];
    for (name, break_it, merges) in rows {
        let f = merging().await;
        ci(&f.e, &f.repo);
        let (roles, hires) = support::events::default_role_hire_events();
        let kinds = roles.into_iter().chain(hires).collect();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
            .unwrap();
        green(&f.fake, &f.repo, &f.first);
        approved(&f.e);
        match break_it {
            Some(break_it) => break_it(&f),
            None => f.fake.close_pr(&f.repo, 1).await,
        }
        let _ = dispatch::step_now(&f.e, &f.task).await;
        let sent = f.interrupted.merges.load(Ordering::SeqCst);
        let state = f.e.proj().tasks[&f.task].view.state.clone();
        if merges {
            assert_eq!((sent, state), (1, TaskState::Done), "{name}");
        } else {
            assert_eq!(sent, 0, "{name}: the forge was asked to merge");
            assert_ne!(state, TaskState::Done, "{name}");
        }
    }
}

/// The owner reassigns the builder's reviewer between the verdict and the
/// merge: the old verdict stops counting and the task returns to review.
#[tokio::test]
async fn a_role_chart_change_after_review_sends_the_task_back_to_review() {
    let f = merging().await;
    ci(&f.e, &f.repo);
    let (roles, hires) = support::events::default_role_hire_events();
    let kinds = roles.into_iter().chain(hires).collect();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    green(&f.fake, &f.repo, &f.first);
    approved(&f.e);
    let mut backend = dvb_core::roles::default_records(|d| RoleId::new(d.slug))
        .into_iter()
        .find(|r| r.slug == "backend")
        .unwrap();
    // The fixture's builder role joins the chart, reviewed by someone else.
    backend.id = RoleId::new("builder");
    backend.slug = "builder".into();
    backend.reviewed_by = Some(RoleId::new("design-reviewer"));
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RoleAdded(backend)])
    })
    .unwrap();
    let _ = dispatch::step_now(&f.e, &f.task).await;
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
    // The old reviewer's unit then exits: it gave its verdict, so that is not
    // a review ending without one.
    let proj = f.e.proj();
    let run = proj
        .runs
        .iter()
        .find(|(_, r)| r.verdict_given)
        .unwrap()
        .0
        .clone();
    drop(proj);
    let before = f.e.proj().tasks[&f.task].view.state.clone();
    f.e.end_run(&run, dvb_core::event::EndReason::Completed, None)
        .unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, before);
}

fn runs_started(e: &AppState) -> Vec<(RoleId, String)> {
    support::events::all(e)
        .into_iter()
        .filter_map(|ev| {
            let EventKind::RunStarted(a) = ev.kind else {
                return None;
            };
            Some((a.role, a.reason))
        })
        .collect()
}

/// Lets any spawned wake or dispatcher finish.
async fn quiesce() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

#[tokio::test]
async fn merge_close_scan_and_status_start_no_run() {
    let counting = Arc::new(support::launcher::FakeLauncher {
        start_state: Some(dvb_core::launcher::UnitState::Inactive),
        stop_changes_state: false,
        ..Default::default()
    });
    let f = merging_on(None, Some(counting.clone())).await;
    let e = &f.e;
    // Roles and hires exist, so any wake or start the engine wanted would run.
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<EventKind> = roles.into_iter().chain(hires).collect();
    kinds.push(EventKind::RunEnded {
        run: RunId::new("reviewer"),
        reason: dvb_core::event::EndReason::Completed,
        detail: None,
    });
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(e);
    let before = runs_started(e);

    // Merge, then close the goal. The one run the engine may start is the
    // orchestrator's, asking the owner what is next.
    settle(e, &f.task).await.unwrap();
    assert_eq!(e.proj().tasks[&f.task].view.state, TaskState::Done);
    publish::reconcile::close_goal_issue(e, &GoalId::new("g"))
        .await
        .unwrap();
    assert_eq!(f.fake.closed_issues(&f.repo), vec![4]);
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        counting.started.notified(),
    )
    .await
    .expect("the goal-done wake did not start");
    let after_close = runs_started(e);
    assert_eq!(
        after_close[before.len()..],
        [(RoleId::new("orchestrator"), "The goal is done.".to_string())],
        "merge and close start only the goal-done wake"
    );
    let units = counting.starts.load(Ordering::SeqCst);
    assert_eq!(units, 1, "only the goal-done wake unit started");

    // Scan: the mirror, reconcile and every dispatcher and wake.
    publish::mirror::sync(e).await.unwrap();
    dvb_engine::reconcile::reconcile(e.clone()).await.unwrap();
    dispatch::kick_all(e, None);
    wake::wake_if_missed(e, &ProjectId::new("p"));
    dispatch::step_now(e, &f.task).await.unwrap();

    // Status reads.
    for path in ["/status", "/status?project=p"] {
        let req = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let res = tower::ServiceExt::oneshot(dvb_engine::api::router(e.clone()), req)
            .await
            .unwrap();
        assert!(res.status().is_success(), "{path}: {}", res.status());
    }
    quiesce().await;

    assert_eq!(runs_started(e), after_close, "an extra run was recorded");
    assert_eq!(
        counting.starts.load(Ordering::SeqCst),
        units,
        "a unit was started"
    );
}

/// A task waiting for CI whose builder role can run a fix round.
async fn awaiting_ci() -> Fixture {
    awaiting_ci_with(true, 0).await
}

/// `awaiting_ci`, the reviewer's run perhaps still running and `others` other
/// runs live.
async fn awaiting_ci_with(reviewer_ended: bool, others: u32) -> Fixture {
    awaiting_ci_on(reviewer_ended, others, &["rust"]).await
}

/// `awaiting_ci_with`, CI expecting `names`.
async fn awaiting_ci_on(reviewer_ended: bool, others: u32, names: &[&str]) -> Fixture {
    let f = merging_on(
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
    )
    .await;
    ci_named(&f.e, &f.repo, names);
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<EventKind> = roles.into_iter().chain(hires).collect();
    kinds.push(EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-builder"),
        role: RoleId::new("builder"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: false,
    }));
    if reviewer_ended {
        kinds.push(EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        });
    }
    for i in 0..others {
        kinds.push(EventKind::RunStarted(
            support::assignment::Assignment::new(RunId::new(format!("other{i}")))
                .role(RoleId::new("orchestrator"))
                .reason("Other".into())
                .build(),
        ));
    }
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(&f.e);
    f
}

/// The check ended cancelled, as run `id`.
fn cancelled(f: &Fixture, id: u64) {
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![CheckObservation {
            conclusion: None,
            url: None,
            name: "rust".into(),
            status: ChecksStatus::Failed,
            run: Some(id),
        }],
    );
}

fn state(f: &Fixture) -> TaskState {
    f.e.proj().tasks[&f.task].view.state.clone()
}

fn ordinary_failure(f: &Fixture, id: u64) {
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![CheckObservation {
            name: "rust".into(),
            status: ChecksStatus::Failed,
            run: Some(id),
            conclusion: Some("failure".into()),
            url: Some(format!("https://ci.invalid/pr/{id}")),
        }],
    );
}

#[tokio::test]
async fn an_ordinary_failure_gets_one_rerun_before_a_round() {
    for succeeds in [true, false] {
        let f = awaiting_ci().await;
        f.e.manual_dispatch();
        ordinary_failure(&f, 1);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        assert_eq!(f.e.proj().tasks[&f.task].facts.round, 1);
        assert_eq!(f.fake.reruns(&f.repo), vec![1]);
        let f = reopen(f);
        f.e.manual_dispatch();
        for _ in 0..3 {
            dispatch::step_now(&f.e, &f.task).await.unwrap();
        }
        assert_eq!(f.fake.reruns(&f.repo), vec![1]);
        if succeeds {
            green(&f.fake, &f.repo, &f.first);
            settle(&f.e, &f.task).await.unwrap();
            assert_eq!(state(&f), TaskState::Done);
        } else {
            ordinary_failure(&f, 2);
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            assert_eq!(state(&f), TaskState::Preparing);
            assert_eq!(f.fake.reruns(&f.repo), vec![1]);
            assert!(
                builder_prompt(&f)
                    .await
                    .contains("required check `rust` failed")
            );
        }
    }
}

#[tokio::test]
async fn unknown_mergeability_checks_local_conflicts_immediately() {
    for conflicting in [true, false] {
        let f = ordered_review_fixture(ReviewKind::Code, false).await;
        f.e.manual_dispatch();
        *f.readings.disk.lock().unwrap() = Some(0);
        f.fake.set_conflicting(&f.repo, 1, None);
        let runs = f.e.proj().runs.len();
        if conflicting {
            move_main(f._tmp.path(), &f.repo, "file", "conflict\n");
        }
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        let task = f.e.proj().tasks[&f.task].clone();
        assert_eq!(f.e.proj().runs.len(), runs + 1);
        assert_eq!(task.repairs, u8::from(conflicting));
        let assignment = f.e.proj().runs[task.facts.run.as_ref().unwrap()]
            .assignment
            .clone();
        assert_eq!(assignment.reviews.is_none(), conflicting);
    }
}

#[tokio::test]
async fn mergeability_fallback_does_not_merge_a_stopped_task() {
    interrupted_mergeability_fallback(false).await;
}

#[tokio::test]
async fn mergeability_fallback_does_not_merge_a_paused_project() {
    interrupted_mergeability_fallback(true).await;
}

async fn interrupted_mergeability_fallback(pause: bool) {
    let f = merging().await;
    f.e.manual_dispatch();
    let project = ProjectId::new("p");
    assert_eq!(f.e.proj().projects[&project].view.ci, CiMode::NoCi);
    f.fake.set_conflicting(&f.repo, 1, None);

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *f.interrupted.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let (step, ()) = tokio::join!(dispatch::step_now(&f.e, &f.task), async {
        barrier.wait().await;
        if pause {
            dvb_engine::owner::pause(&f.e, &project, true).unwrap();
            barrier.wait().await;
        } else {
            let stop = dispatch::stop(&f.e, &f.task, Actor::Owner);
            tokio::pin!(stop);
            // Stop records its decision before waiting for dispatch's task lock.
            tokio::select! {
                biased;
                result = &mut stop => panic!("stop passed the held task lock: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
            assert!(matches!(state(&f), TaskState::Stopped { .. }));
            barrier.wait().await;
            stop.await.unwrap();
        }
    });
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
    step.unwrap();
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
    if pause {
        assert!(f.e.proj().projects[&project].view.paused);
        assert_eq!(state(&f), TaskState::Merging);
    } else {
        assert!(matches!(state(&f), TaskState::Stopped { .. }));
    }
}

#[tokio::test]
async fn conflicts_before_review_and_at_the_cap_do_not_charge_a_round() {
    for waiting in [false, true] {
        let f = awaiting_ci_at_cap().await;
        f.e.manual_dispatch();
        *f.readings.disk.lock().unwrap() = Some(0);
        if !waiting {
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
                let kinds = vec![EventKind::TaskStateChanged {
                    task: f.task.clone(),
                    from: TaskState::AwaitingChecks,
                    to: TaskState::InReview,
                }];
                move |_, _| Ok(kinds)
            })
            .unwrap();
        }
        move_main(f._tmp.path(), &f.repo, "file", "conflict\n");
        f.fake.set_conflicting(&f.repo, 1, Some(true));
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::Preparing);
        let run = {
            let proj = f.e.proj();
            assert_eq!(proj.tasks[&f.task].facts.round, caps::REVIEW_ROUNDS_PER_PR);
            assert!(proj.questions.is_empty());
            let run = &proj.runs[proj.tasks[&f.task].facts.run.as_ref().unwrap()];
            let id = run.assignment.run.clone();
            let builder = run.assignment.reviews.is_none();
            drop(proj);
            assert!(builder);
            id
        };
        let f = reopen(f);
        f.e.manual_dispatch();
        *f.readings.disk.lock().unwrap() = Some(0);
        assert!(f.e.proj().tasks[&f.task].facts.branch_repair);
        let mirror = f.e.mirror("project");
        let ws = f.e.cfg.paths.work.join(run.as_str()).join("ws");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        git(
            f._tmp.path(),
            &[
                "clone",
                "--quiet",
                mirror.to_str().unwrap(),
                ws.to_str().unwrap(),
            ],
        );
        let tree = git(&mirror, &["rev-parse", &format!("{}^{{tree}}", f.second)]);
        let collected = git(
            &ws,
            &["commit-tree", &tree, "-p", &f.first, "-m", "Resolved tree"],
        );
        collect(&ws, &collected);
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![
                EventKind::RunCollected {
                    outcome: None,
                    run: run.clone(),
                    head: Some(collected),
                    summary: None,
                },
                EventKind::RunEnded {
                    run,
                    reason: dvb_core::event::EndReason::Completed,
                    detail: None,
                },
                EventKind::TaskStateChanged {
                    task: f.task.clone(),
                    from: state(&f),
                    to: TaskState::Publishing,
                },
            ];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::InReview);
        let head = f.e.proj().tasks[&f.task].view.head.clone().unwrap();
        assert_ne!(head, f.first);
        assert_eq!(
            f.e.proj().tasks[&f.task].facts.round,
            caps::REVIEW_ROUNDS_PER_PR
        );
        assert!(!f.e.proj().tasks[&f.task].reviews_approved());
        assert!(f.e.proj().tasks[&f.task].checks.is_none());
        f.fake.set_conflicting(&f.repo, 1, Some(false));
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        let review = f.e.proj().tasks[&f.task].facts.run.clone().unwrap();
        assert_eq!(
            f.e.proj().runs[&review].assignment.reviews.as_deref(),
            Some(head.as_str())
        );
        assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
        verdict_by(&f.e, &f.task, &head, "code-reviewer", false);
        f.e.decide(
            f.e.proj(),
            &f.task,
            Actor::Engine,
            dvb_engine::Decision::new(
                "verdict",
                Command::VerdictRecorded {
                    head: head.clone(),
                    open: 0,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
        green(&f.fake, &f.repo, &head);
        settle(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::Done);
        assert_eq!(
            f.e.proj().tasks[&f.task].facts.round,
            caps::REVIEW_ROUNDS_PER_PR
        );
    }
}

#[tokio::test]
async fn main_failure_holds_the_task_with_one_project_question() {
    for answer in [None, Some(1), Some(2), Some(0)] {
        let f = awaiting_ci().await;
        f.e.manual_dispatch();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![EventKind::SettingsChanged {
                key: "project_checks".into(),
                old: serde_json::json!("no_ci"),
                new: serde_json::json!({"expected":["rust", "identity"]}),
                restored_from: None,
            }];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        let next = TaskId::new("next");
        f.fake
            .push(&f.repo, &f.e.mirror("project"), "dvb/next-1", &f.first)
            .await
            .unwrap();
        let pr = f
            .fake
            .open_pr(&f.repo, "dvb/next-1", "Other task", "Body")
            .await
            .unwrap();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![
                EventKind::PlanProposed {
                    goal: GoalId::new("g"),
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: next.clone(),
                        title: "Other task".into(),
                        role: RoleId::new("builder"),
                        review: ReviewKind::Code,
                        summary: "Other task".into(),
                        routine: false,
                    }],
                },
                EventKind::GoalApproved {
                    goal: GoalId::new("g"),
                },
                EventKind::BranchPublished {
                    task: next.clone(),
                    round: 1,
                    branch: "dvb/next-1".into(),
                    head: f.first.clone(),
                },
                EventKind::PrOpened {
                    task: next.clone(),
                    pr: pr.number,
                    url: pr.url,
                },
                EventKind::TaskStateChanged {
                    task: next.clone(),
                    from: TaskState::Planned,
                    to: TaskState::AwaitingChecks,
                },
            ];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        f.fake.set_checks(
            &f.repo,
            "main",
            vec![
                CheckObservation {
                    name: "rust".into(),
                    status: ChecksStatus::Failed,
                    run: None,
                    conclusion: Some("failure".into()),
                    url: Some("https://ci.invalid/main/41".into()),
                },
                CheckObservation {
                    name: "identity".into(),
                    status: ChecksStatus::Missing,
                    run: None,
                    conclusion: Some("skipped".into()),
                    url: Some("https://ci.invalid/main/41".into()),
                },
            ],
        );
        ordinary_failure(&f, 1);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        ordinary_failure(&f, 2);
        let (a, b) = tokio::join!(
            dispatch::step_now(&f.e, &f.task),
            dispatch::step_now(&f.e, &next)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        assert_eq!(
            f.e.proj().tasks[&next].view.state,
            TaskState::AwaitingChecks
        );
        let question = {
            let proj = f.e.proj();
            assert_eq!(proj.questions.len(), 1);
            let q = proj.questions.values().next().unwrap().view.clone();
            drop(proj);
            assert!(q.open);
            assert!(q.subject.contains("`rust`"));
            assert!(
                !q.subject.contains("`identity`"),
                "PR-only skipped jobs are not failures on main"
            );
            assert!(q.subject.contains("https://ci.invalid/main/41"));
            assert_eq!(q.recommended, 1);
            assert_eq!(q.options[1].text, "Retry now");
            q.id
        };
        let f = reopen(f);
        f.e.manual_dispatch();
        for _ in 0..3 {
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            dispatch::step_now(&f.e, &next).await.unwrap();
        }
        assert_eq!(f.e.proj().questions.len(), 1);
        assert_eq!(f.fake.reruns(&f.repo), vec![1]);
        f.fake.set_checks(&f.repo, "main", vec![]);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(
            state(&f),
            TaskState::AwaitingChecks,
            "missing main evidence must not release the hold"
        );
        assert!(f.e.proj().questions[&question].view.open);
        f.fake.set_checks(
            &f.repo,
            "main",
            vec![CheckObservation {
                name: "rust".into(),
                status: ChecksStatus::Failed,
                run: None,
                conclusion: Some("failure".into()),
                url: Some("https://ci.invalid/main/43".into()),
            }],
        );
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &next).await.unwrap();
        assert_eq!(f.e.proj().questions.len(), 1);
        assert!(
            f.e.proj().questions[&question]
                .view
                .subject
                .contains("https://ci.invalid/main/43")
        );
        let started = runs_started(&f.e);
        if answer == Some(0) {
            let (status, _) = support::http::call(
                &f.e,
                axum::http::Method::PUT,
                "/projects/p/checks",
                Some(serde_json::json!("no_ci")),
            )
            .await;
            assert!(status.is_success());
            assert!(!f.e.proj().questions[&question].view.open);
            settle(&f.e, &f.task).await.unwrap();
            assert_eq!(state(&f), TaskState::Done);
            assert_eq!(f.fake.reruns(&f.repo), vec![1]);
            assert_eq!(runs_started(&f.e), started);
            continue;
        }
        if let Some(choice) = answer {
            for pick in if choice == 2 {
                vec![1, 1, 2]
            } else {
                vec![1, 1]
            } {
                let req = axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/questions/{question}/answer"))
                    .header("content-type", "application/json")
                    .header(dvb_core::api::VERSION_HEADER, dvb_core::VERSION)
                    .body(axum::body::Body::from(
                        serde_json::json!({"choice": pick}).to_string(),
                    ))
                    .unwrap();
                let res = tower::ServiceExt::oneshot(dvb_engine::api::router(f.e.clone()), req)
                    .await
                    .unwrap();
                assert!(res.status().is_success(), "{}", res.status());
                if pick == 1 {
                    assert!(f.e.proj().questions[&question].view.open);
                    assert!(f.e.proj().questions[&question].view.answer.is_none());
                }
            }
            if choice == 1 {
                assert!(f.e.proj().questions[&question].view.open);
                dispatch::step_now(&f.e, &f.task).await.unwrap();
                assert_eq!(f.fake.reruns(&f.repo), vec![1]);
                assert_eq!(runs_started(&f.e), started);
            }
        }
        let f = reopen(f);
        f.e.manual_dispatch();
        assert_eq!(f.e.proj().questions[&question].view.open, answer != Some(2));
        if answer != Some(2) {
            f.fake.set_checks(
                &f.repo,
                "main",
                vec![CheckObservation {
                    name: "rust".into(),
                    status: ChecksStatus::Success,
                    run: None,
                    conclusion: Some("success".into()),
                    url: Some("https://ci.invalid/main/44".into()),
                }],
            );
        }
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &next).await.unwrap();
        assert!(!f.e.proj().questions[&question].view.open);
        assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
        assert_eq!(runs_started(&f.e), started);
        let f = reopen(f);
        f.e.manual_dispatch();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &next).await.unwrap();
        assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
        assert_eq!(f.e.proj().questions.len(), 1);
        ordinary_failure(&f, 3);
        let (a, b) = tokio::join!(
            dispatch::step_now(&f.e, &f.task),
            dispatch::step_now(&f.e, &next)
        );
        a.unwrap();
        b.unwrap();
        if answer == Some(2) {
            assert_eq!(state(&f), TaskState::AwaitingChecks);
            assert!(f.e.proj().questions[&question].view.open);
            assert_eq!(f.e.proj().questions.len(), 1);
            f.fake.set_checks(
                &f.repo,
                "main",
                vec![CheckObservation {
                    name: "rust".into(),
                    status: ChecksStatus::Success,
                    run: None,
                    conclusion: Some("success".into()),
                    url: Some("https://ci.invalid/main/44".into()),
                }],
            );
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            dispatch::step_now(&f.e, &next).await.unwrap();
            assert_eq!(f.fake.reruns(&f.repo), vec![1, 2, 3]);
            ordinary_failure(&f, 4);
            let (a, b) = tokio::join!(
                dispatch::step_now(&f.e, &f.task),
                dispatch::step_now(&f.e, &next)
            );
            a.unwrap();
            b.unwrap();
        }
        assert!(matches!(
            state(&f),
            TaskState::Preparing | TaskState::Running
        ));
        assert!(matches!(
            f.e.proj().tasks[&next].view.state,
            TaskState::Preparing | TaskState::Running
        ));
        let started = runs_started(&f.e);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &next).await.unwrap();
        assert_eq!(runs_started(&f.e), started);
        assert_eq!(
            f.fake.reruns(&f.repo),
            if answer == Some(2) {
                vec![1, 2, 3]
            } else {
                vec![1, 2]
            }
        );
    }
}

#[tokio::test]
async fn stale_main_recovery_cannot_close_another_tasks_changed_question() {
    for rust_recovered in [true, false] {
        let f = awaiting_ci_on(true, 0, &["rust", "e2e"]).await;
        f.e.manual_dispatch();
        let next = TaskId::new("next");
        f.fake
            .push(&f.repo, &f.e.mirror("project"), "dvb/next-1", &f.first)
            .await
            .unwrap();
        let pr = f
            .fake
            .open_pr(&f.repo, "dvb/next-1", "Other task", "Body")
            .await
            .unwrap();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![
                EventKind::PlanProposed {
                    goal: GoalId::new("g"),
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: next.clone(),
                        title: "Other task".into(),
                        role: RoleId::new("builder"),
                        review: ReviewKind::Code,
                        summary: "Other task".into(),
                        routine: false,
                    }],
                },
                EventKind::GoalApproved {
                    goal: GoalId::new("g"),
                },
                EventKind::BranchPublished {
                    task: next.clone(),
                    round: 1,
                    branch: "dvb/next-1".into(),
                    head: f.first.clone(),
                },
                EventKind::PrOpened {
                    task: next.clone(),
                    pr: pr.number,
                    url: pr.url,
                },
                EventKind::TaskStateChanged {
                    task: next.clone(),
                    from: TaskState::Planned,
                    to: TaskState::AwaitingChecks,
                },
            ];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        let failure = |name: &str, run| CheckObservation {
            name: name.into(),
            status: ChecksStatus::Failed,
            run,
            conclusion: Some("failure".into()),
            url: Some(format!("https://ci.invalid/{name}")),
        };
        f.fake.set_checks(
            &f.repo,
            &f.first,
            vec![failure("rust", Some(1)), failure("e2e", Some(2))],
        );
        f.fake
            .set_checks(&f.repo, "main", vec![failure("rust", None)]);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        f.fake.set_checks(
            &f.repo,
            &f.first,
            vec![failure("rust", Some(3)), failure("e2e", Some(4))],
        );
        for task in [&f.task, &next] {
            dispatch::step_now(&f.e, task).await.unwrap();
        }
        let old = f.e.proj().questions.values().next().unwrap().clone();
        assert!(old.view.open);
        assert_eq!(old.main_ci.as_ref().unwrap()[0].name, "rust");
        assert_eq!(
            f.e.proj().tasks[&next].ci_question.as_ref(),
            Some(&old.view.id)
        );
        let reruns = f.fake.reruns(&f.repo);
        let started = runs_started(&f.e);

        f.fake.set_checks(
            &f.repo,
            "main",
            ["rust", "e2e"]
                .into_iter()
                .map(|name| CheckObservation {
                    name: name.into(),
                    status: if name == "rust" && !rust_recovered {
                        ChecksStatus::Failed
                    } else {
                        ChecksStatus::Success
                    },
                    run: None,
                    conclusion: Some(
                        if name == "rust" && !rust_recovered {
                            "failure"
                        } else {
                            "success"
                        }
                        .into(),
                    ),
                    url: None,
                })
                .collect(),
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *f.interrupted.main_checks_barrier.lock().unwrap() = Some(barrier.clone());
        let engine = f.e.clone();
        let task = f.task.clone();
        let pending = tokio::spawn(async move { dispatch::step_now(&engine, &task).await });
        barrier.wait().await;

        // Commit the other task's observation while the first task's main read is in flight.
        // The project poll lock serializes dispatch; exercise the append boundary directly.
        f.fake
            .set_checks(&f.repo, "main", vec![failure("e2e", None)]);
        let broken = f.fake.main_checks(&f.repo, "main").await.unwrap();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![
                EventKind::QuestionOpened {
                    kind: Some(dvb_core::event::QuestionKind::Ordinary),
                    task: None,
                    question: old.view.id.clone(),
                    subject: "main also fails required CI: e2e".into(),
                    format: old.view.format,
                    options: old.view.options.clone(),
                    recommended: old.view.recommended,
                    lapses_on: old.lapses_on,
                    main_ci: Some(broken.clone()),
                },
                EventKind::ChecksObserved {
                    task: next.clone(),
                    head: f.first.clone(),
                    status: ChecksStatus::Failed,
                    names: vec!["rust".into(), "e2e".into()],
                    retry_question: Some(old.view.id.clone()),
                    retry_runs: vec![],
                },
            ];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        // Only e2e still fails on the held heads, so stale partial recovery could release them.
        f.fake
            .set_checks(&f.repo, &f.first, vec![failure("e2e", Some(4))]);
        let changed = f.e.proj().questions[&old.view.id].opened_at;
        assert_ne!(changed, old.opened_at);
        let boundary = f.e.store.last_id().unwrap();
        barrier.wait().await;
        pending.await.unwrap().unwrap();
        dispatch::step_now(&f.e, &next).await.unwrap();

        let proj = f.e.proj();
        assert!(
            proj.questions[&old.view.id].view.open,
            "stale rust recovery closed the newer e2e hold"
        );
        assert_eq!(proj.questions[&old.view.id].main_ci.as_ref(), Some(&broken));
        assert_eq!(proj.questions[&old.view.id].opened_at, changed);
        for task in [&f.task, &next] {
            assert_eq!(proj.tasks[task].view.state, TaskState::AwaitingChecks);
            assert!(
                proj.tasks[task].ci_retry_runs.is_empty(),
                "stale recovery authorized a rerun for {task} (rust_recovered={rust_recovered})"
            );
            assert_eq!(proj.tasks[task].facts.round, 1);
        }
        drop(proj);
        assert_eq!(f.fake.reruns(&f.repo), reruns);
        assert_eq!(runs_started(&f.e), started);
        assert!(!f.e.store.after(boundary).unwrap().iter().any(|event|
        matches!(&event.kind, EventKind::QuestionClosed { question, .. } if question == &old.view.id)
    ));
    }
}

#[tokio::test]
async fn rendered_code_and_design_prompts_include_required_ci_results() {
    let f = awaiting_ci().await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
        let kinds = vec![EventKind::SettingsChanged {
            key: "project_checks".into(),
            old: serde_json::json!("no_ci"),
            new: serde_json::json!({"expected":["rust", "e2e"]}),
            restored_from: None,
        }];
        move |_, _| Ok(kinds)
    })
    .unwrap();
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![
            CheckObservation {
                name: "rust".into(),
                status: ChecksStatus::Success,
                run: None,
                conclusion: Some("success".into()),
                url: Some("https://ci.invalid/pr/17".into()),
            },
            CheckObservation {
                name: "e2e".into(),
                status: ChecksStatus::Failed,
                run: Some(18),
                conclusion: Some("timed_out".into()),
                url: Some("https://ci.invalid/pr/18".into()),
            },
            CheckObservation {
                name: "optional".into(),
                status: ChecksStatus::Failed,
                run: None,
                conclusion: Some("failure".into()),
                url: None,
            },
        ],
    );
    let (run, project) = {
        let proj = f.e.proj();
        (
            proj.runs[&RunId::new("reviewer")].clone(),
            proj.projects[&ProjectId::new("p")].clone(),
        )
    };
    let vars = dvb_engine::prompt::context(&f.e, &run, &project)
        .await
        .unwrap();
    for template in [
        include_str!("../../../prompts/code-reviewer.md"),
        include_str!("../../../prompts/design-reviewer.md"),
    ] {
        let prompt = dvb_engine::prompt::render(template, &vars, false);
        assert!(prompt.contains("rust: success"), "{prompt}");
        assert!(prompt.contains("e2e: timed_out"), "{prompt}");
        assert!(prompt.contains("https://ci.invalid/pr/17"), "{prompt}");
        assert!(prompt.contains("https://ci.invalid/pr/18"), "{prompt}");
        assert!(!prompt.contains("optional: failure"), "{prompt}");
        assert!(prompt.contains("CI is the engine's gate"), "{prompt}");
        assert!(
            prompt.contains("must not leave findings about CI they cannot see"),
            "{prompt}"
        );
    }
    for (role, stage) in [
        ("architect", ReviewKind::Code),
        ("design-reviewer", ReviewKind::Design),
    ] {
        let mut a = run.assignment.clone();
        a.run = RunId::new(format!("ci-{role}"));
        a.role = RoleId::new(role);
        a.review_stage = Some(stage);
        a.hire = project.hire(&a.role, None).unwrap().id.clone();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, {
            let kinds = vec![EventKind::RunStarted(a.clone())];
            move |_, _| Ok(kinds)
        })
        .unwrap();
        dvb_engine::launch::launch(f.e.clone(), a.run.clone(), "[]".into(), vec![], None)
            .await
            .unwrap();
        let prompt =
            std::fs::read_to_string(f.e.cfg.paths.work.join(a.run.as_str()).join("prompt.md"))
                .unwrap();
        assert!(prompt.contains("rust: success"), "{prompt}");
        assert!(prompt.contains("e2e: timed_out"), "{prompt}");
        assert!(prompt.contains("https://ci.invalid/pr/17"), "{prompt}");
        assert!(prompt.contains("https://ci.invalid/pr/18"), "{prompt}");
        assert!(prompt.contains("CI is the engine's gate"), "{prompt}");
    }
}

#[tokio::test]
async fn a_skipped_required_check_reported_missing_keeps_waiting_without_merging() {
    let f = awaiting_ci().await;
    f.e.manual_dispatch();
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![CheckObservation {
            name: "rust".into(),
            status: ChecksStatus::Missing,
            run: None,
            conclusion: Some("skipped".into()),
            url: Some("https://ci.invalid/pr/17".into()),
        }],
    );
    for _ in 0..3 {
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        assert_eq!(
            f.e.proj().tasks[&f.task].checks,
            Some((f.first.clone(), ChecksStatus::Missing))
        );
        assert!(f.fake.reruns(&f.repo).is_empty());
        assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
        assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
    }
    assert!(
        !support::events::all(&f.e)
            .iter()
            .any(|event| matches!(event.kind, EventKind::TaskFailed { .. }))
    );
    green(&f.fake, &f.repo, &f.first);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
}

#[tokio::test]
async fn a_cancelled_check_is_rerun_once_then_a_fix_round_starts() {
    let f = awaiting_ci().await;
    cancelled(&f, 1);
    // The first look only enters the wait for CI.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(f.fake.reruns(&f.repo).is_empty());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    assert_eq!(f.fake.reruns(&f.repo), vec![1]);
    // The forge still shows the cancelled run: the rerun has not started yet.
    for _ in 0..3 {
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
    }
    // The rerun ends cancelled too: the fix round starts, with no second rerun.
    cancelled(&f, 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert_eq!(f.fake.reruns(&f.repo), vec![1]);
    let prompt = builder_prompt(&f).await;
    assert!(prompt.contains("required check `rust` failed"), "{prompt}");
}

#[tokio::test]
async fn a_rerun_check_that_passes_merges() {
    let f = awaiting_ci().await;
    cancelled(&f, 1);
    // The first look only enters the wait for CI.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(f.fake.reruns(&f.repo).is_empty());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.reruns(&f.repo).len(), 1);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    green(&f.fake, &f.repo, &f.first);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
}

#[tokio::test]
async fn a_fix_round_waits_for_the_reviewer_to_end() {
    let f = awaiting_ci_with(false, 0).await;
    cancelled(&f, 1);
    // The first look only enters the wait for CI.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(f.fake.reruns(&f.repo).is_empty());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    cancelled(&f, 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
}

#[tokio::test]
async fn a_fix_round_waits_for_a_free_run_slot() {
    let f = awaiting_ci_with(true, 3).await;
    cancelled(&f, 1);
    // The first look only enters the wait for CI.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(f.fake.reruns(&f.repo).is_empty());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    cancelled(&f, 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("other0"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
}

#[tokio::test]
async fn a_conflicting_head_reaches_the_conflict_fix_round_while_waiting_for_ci() {
    let f = awaiting_ci().await;
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![CheckObservation {
            conclusion: None,
            url: None,
            name: "rust".into(),
            status: ChecksStatus::Pending,
            run: None,
        }],
    );
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(
        f.e.proj().tasks[&f.task].view.state,
        TaskState::AwaitingChecks
    );
    let main = move_main(f._tmp.path(), &f.repo, "file", "conflict\n");
    f.fake.set_conflicting(&f.repo, 1, Some(true));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Preparing);
    assert_eq!(
        publish::merges(&f.e.mirror("project"), f.task.as_str()).await,
        Some(main)
    );
}

// --- Main moves while the task awaits checks --------------------------------

/// `rust` on `head`, as given.
fn rust_on(f: &Fixture, head: &str, status: ChecksStatus, run: Option<u64>) {
    f.fake.set_checks(
        &f.repo,
        head,
        vec![CheckObservation {
            name: "rust".into(),
            status,
            run,
            conclusion: (status == ChecksStatus::Failed).then(|| "failure".into()),
            url: None,
        }],
    );
}

/// `awaiting_ci`, its head's CI running.
async fn running_ci() -> Fixture {
    let f = awaiting_ci().await;
    f.e.manual_dispatch();
    rust_on(&f, &f.first, ChecksStatus::Pending, None);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    f
}

/// `rust` on the head, Pending, with its link.
fn rust_pending(f: &Fixture) {
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![CheckObservation {
            name: "rust".into(),
            status: ChecksStatus::Pending,
            run: None,
            conclusion: None,
            url: Some("https://ci.example/rust/1".into()),
        }],
    );
}

/// Every `checks.stuck` recorded.
fn stuck_notices(f: &Fixture) -> Vec<EventKind> {
    support::events::all(&f.e)
        .into_iter()
        .map(|ev| ev.kind)
        .filter(|kind| matches!(kind, EventKind::ChecksStuck { .. }))
        .collect()
}

fn advance(f: &Fixture, by: std::time::Duration) {
    f.clock
        .0
        .fetch_add(i64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
}

#[tokio::test]
async fn a_required_check_pending_past_the_limit_is_told_once() {
    let f = awaiting_ci().await;
    f.e.manual_dispatch();
    rust_pending(&f);
    // The first step enters AwaitingChecks; the next reads the forge.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(
        &f,
        caps::CHECK_PENDING_LIMIT - std::time::Duration::from_secs(1),
    );
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(stuck_notices(&f), vec![], "not yet past the limit");

    advance(&f, std::time::Duration::from_secs(1));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(&f, caps::CHECK_PENDING_LIMIT);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let told = vec![EventKind::ChecksStuck {
        task: f.task.clone(),
        head: f.first.clone(),
        check: "rust".into(),
        url: Some("https://ci.example/rust/1".into()),
        limit_minutes: 60,
    }];
    assert_eq!(stuck_notices(&f), told);
    assert_eq!(
        state(&f),
        TaskState::AwaitingChecks,
        "the task keeps waiting"
    );
    assert!(f.fake.reruns(&f.repo).is_empty());
    assert!(f.fake.cancels(&f.repo).is_empty());

    let f = reopen(f);
    f.e.manual_dispatch();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(&f, caps::CHECK_PENDING_LIMIT * 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(stuck_notices(&f), told, "not again after a restart");
    assert_eq!(state(&f), TaskState::AwaitingChecks);
}

#[tokio::test]
async fn pending_elapsed_time_survives_a_restart_before_the_limit() {
    let f = awaiting_ci().await;
    f.e.manual_dispatch();
    rust_pending(&f);
    // The first step enters AwaitingChecks; the next reads the forge.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(&f, std::time::Duration::from_secs(55 * 60));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(stuck_notices(&f).is_empty());
    assert_eq!(
        support::events::all(&f.e)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::ChecksPending { .. }))
            .count(),
        1,
        "unchanged polls do not record another Pending snapshot"
    );

    let f = reopen(f);
    f.e.manual_dispatch();
    advance(&f, std::time::Duration::from_secs(10 * 60));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let told = vec![EventKind::ChecksStuck {
        task: f.task.clone(),
        head: f.first.clone(),
        check: "rust".into(),
        url: Some("https://ci.example/rust/1".into()),
        limit_minutes: 60,
    }];
    assert_eq!(stuck_notices(&f), told);
    advance(&f, caps::CHECK_PENDING_LIMIT);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let f = reopen(f);
    f.e.manual_dispatch();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(stuck_notices(&f), told);
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    assert!(f.fake.reruns(&f.repo).is_empty());
    assert!(f.fake.cancels(&f.repo).is_empty());
}

#[tokio::test]
async fn a_required_check_finishing_before_the_limit_is_not_told() {
    let f = awaiting_ci().await;
    f.e.manual_dispatch();
    rust_pending(&f);
    // The first step enters AwaitingChecks; the next reads the forge.
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(&f, caps::CHECK_PENDING_LIMIT / 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    green(&f.fake, &f.repo, &f.first);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    advance(&f, caps::CHECK_PENDING_LIMIT);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
    assert_eq!(stuck_notices(&f), vec![]);
}

/// Events recorded since `before` that would act on a CI result.
fn ci_decisions(f: &Fixture, before: EventId) -> Vec<EventKind> {
    f.e.store
        .after(before)
        .unwrap()
        .into_iter()
        .map(|ev| ev.kind)
        .filter(|kind| {
            matches!(
                kind,
                EventKind::ChecksObserved { .. }
                    | EventKind::TaskFailed { .. }
                    | EventKind::RunStarted(_)
                    | EventKind::PrMerged { .. }
                    | EventKind::QuestionOpened { .. }
            )
        })
        .collect()
}

#[tokio::test]
async fn main_moving_while_awaiting_checks_updates_the_branch_at_once() {
    for stale in [
        ChecksStatus::Pending,
        ChecksStatus::Success,
        ChecksStatus::Failed,
    ] {
        let f = running_ci().await;
        let main = move_main(f._tmp.path(), &f.repo, "other", "other\n");
        // The old head's run ends (or not) in the same look that sees main moved.
        rust_on(&f, &f.first, stale, Some(1));
        let before = f.e.store.last_id().unwrap();

        dispatch::step_now(&f.e, &f.task).await.unwrap();

        let [(from, updated, true)] = &updates(&f.e)[..] else {
            panic!("{stale:?}: one carried update: {:?}", updates(&f.e));
        };
        assert_eq!(from, &f.first);
        let t = f.e.proj().tasks[&f.task].clone();
        assert_eq!(t.view.state, TaskState::AwaitingChecks, "{stale:?}");
        assert_eq!(t.view.head.as_ref(), Some(updated));
        assert!(t.reviews_approved(), "{stale:?}: the review is kept");
        let pr = f.fake.pr(&f.repo, 1).await.unwrap();
        assert_eq!((pr.state, &pr.head), (PrState::Open, updated));
        let origin = std::path::Path::new(&f.repo.0);
        assert_eq!(
            git(origin, &["log", "-1", "--format=%P", updated]),
            format!("{} {main}", f.first)
        );
        // The stale run is cancelled; its result decides nothing.
        assert_eq!(f.fake.cancels(&f.repo), vec![f.first.clone()]);
        assert!(f.fake.reruns(&f.repo).is_empty(), "{stale:?}");
        assert!(ci_decisions(&f, before).is_empty(), "{stale:?}");

        // Only the new head's checks decide: a stale pass does not merge, a
        // stale failure starts no rerun or fix round.
        for _ in 0..2 {
            dispatch::step_now(&f.e, &f.task).await.unwrap();
        }
        assert_eq!(state(&f), TaskState::AwaitingChecks, "{stale:?}");
        assert!(f.fake.reruns(&f.repo).is_empty(), "{stale:?}");
        assert_eq!(f.e.proj().tasks[&f.task].facts.round, 1);
        rust_on(&f, updated, ChecksStatus::Success, None);
        settle(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::Done, "{stale:?}");
        assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
        assert_eq!(git(origin, &["show", "main:other"]), "other");
        assert!(
            !f.e.proj()
                .runs
                .values()
                .any(|r| r.assignment.reviews.as_ref() == Some(updated)),
            "{stale:?}: no review of the update"
        );
    }
}

#[tokio::test]
async fn other_merges_during_ci_never_fail_the_task() {
    let f = running_ci().await;
    // A refused cancel is logged; the update stands.
    f.fake.fail_next(
        "cancel_checks",
        dvb_core::forge::ForgeError {
            rate_limit: None,
            retryable: false,
            detail: "cancel refused".into(),
        },
    );
    let moves = usize::from(caps::BRANCH_REPAIRS_PER_TASK) + 2;
    for i in 0..moves {
        move_main(f._tmp.path(), &f.repo, &format!("other{i}"), "other\n");
        let head = f.e.proj().tasks[&f.task].view.head.clone().unwrap();
        rust_on(&f, &head, ChecksStatus::Pending, None);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks, "move {i}");
        assert_eq!(updates(&f.e).len(), i + 1, "move {i}");
        // The first cancel is refused.
        assert_eq!(f.fake.cancels(&f.repo).len(), i, "move {i}");
    }
    assert!(updates(&f.e).iter().all(|(_, _, same)| *same));
    let head = f.e.proj().tasks[&f.task].view.head.clone().unwrap();
    rust_on(&f, &head, ChecksStatus::Success, None);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().head, head);
}

#[tokio::test]
async fn an_update_while_awaiting_checks_whose_change_differs_is_reviewed_again() {
    let f = running_ci().await;
    // Main already has the task's change: the PR's own patch differs (empty).
    move_main(f._tmp.path(), &f.repo, "file", "changed\n");
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let [(from, updated, false)] = &updates(&f.e)[..] else {
        panic!("one update with a changed patch: {:?}", updates(&f.e));
    };
    assert_eq!(from, &f.first);
    let t = f.e.proj().tasks[&f.task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_eq!(t.view.head.as_ref(), Some(updated));
    assert!(t.needs_reviewer());
    assert_eq!(f.fake.cancels(&f.repo), vec![f.first.clone()]);
}

#[tokio::test]
async fn main_moving_into_a_conflict_while_awaiting_checks_starts_a_repair_round() {
    let f = running_ci().await;
    // The forge has not flagged the conflict yet; the update finds it.
    let main = move_main(f._tmp.path(), &f.repo, "file", "conflict\n");
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert!(updates(&f.e).is_empty());
    assert_eq!(
        publish::merges(&f.e.mirror("project"), f.task.as_str()).await,
        Some(main)
    );
    assert_eq!(f.e.proj().tasks[&f.task].repairs, 1);
}

#[tokio::test]
async fn main_moving_while_the_tasks_run_is_live_waits_for_its_end() {
    let f = awaiting_ci_with(false, 0).await;
    f.e.manual_dispatch();
    rust_on(&f, &f.first, ChecksStatus::Pending, None);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    move_main(f._tmp.path(), &f.repo, "other", "other\n");
    rust_on(&f, &f.first, ChecksStatus::Success, None);
    let before = f.e.store.last_id().unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    // Not updated, and the stale pass does not merge.
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    assert!(updates(&f.e).is_empty());
    assert!(ci_decisions(&f, before).is_empty());
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    assert_eq!(updates(&f.e).len(), 1);
}

/// The required check `rust` and an optional `lint`, each as given.
fn checks_seen(f: &Fixture, rust: (ChecksStatus, Option<u64>), lint: (ChecksStatus, Option<u64>)) {
    let check = |name: &str, (status, interrupted): (ChecksStatus, Option<u64>)| CheckObservation {
        conclusion: None,
        url: None,
        name: name.into(),
        status,
        run: interrupted,
    };
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![check("rust", rust), check("lint", lint)],
    );
}

/// The fix round's builder prompt, rendered.
async fn builder_prompt(f: &Fixture) -> String {
    let (run, project) = {
        let proj = f.e.proj();
        let builder = proj.tasks[&f.task].facts.builder.clone().unwrap();
        (
            proj.runs[&builder].clone(),
            proj.projects[&ProjectId::new("p")].clone(),
        )
    };
    let vars = dvb_engine::prompt::context(&f.e, &run, &project)
        .await
        .unwrap();
    dvb_engine::prompt::render(include_str!("../../../prompts/builder.md"), &vars, false)
}

#[tokio::test]
async fn only_the_required_cancelled_check_is_rerun() {
    let f = awaiting_ci().await;
    checks_seen(
        &f,
        (ChecksStatus::Failed, Some(1)),
        (ChecksStatus::Failed, Some(9)),
    );
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    assert_eq!(f.fake.reruns(&f.repo), vec![1]);
}

#[tokio::test]
async fn a_failed_required_check_starts_a_fix_round_told_what_failed() {
    let f = awaiting_ci().await;
    // An optional check's failure, and a pending required check, start nothing.
    checks_seen(
        &f,
        (ChecksStatus::Pending, None),
        (ChecksStatus::Failed, None),
    );
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    checks_seen(
        &f,
        (ChecksStatus::Failed, None),
        (ChecksStatus::Success, None),
    );
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert!(f.fake.reruns(&f.repo).is_empty());
    let prompt = builder_prompt(&f).await;
    assert!(prompt.contains("## Fix what failed"), "{prompt}");
    assert!(prompt.contains("required check `rust` failed"), "{prompt}");
    assert!(!prompt.contains("`lint`"), "{prompt}");
    assert!(f.e.proj().questions.is_empty());
}

/// The last allowed publication round, reviewed clean, awaiting its checks.
async fn awaiting_ci_at_cap() -> Fixture {
    let f = awaiting_ci().await;
    // The head is the cap's last round, reviewed clean.
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::BranchPublished {
                task: f.task.clone(),
                round: caps::REVIEW_ROUNDS_PER_PR,
                branch: "dvb/t-1".into(),
                head: f.first.clone(),
            },
            // Its PR body was edited for the round.
            EventKind::PrOpened {
                task: f.task.clone(),
                pr: 1,
                url: "https://github.invalid/pr/1".into(),
            },
            EventKind::VerdictRecorded {
                review_stage: None,
                verdict: VerdictId::new("v-cap"),
                task: f.task.clone(),
                head: f.first.clone(),
                run: RunId::new("reviewer"),
                role: RoleId::new("code-reviewer"),
                model: "fake-1".into(),
                vendor: Vendor::Fake,
                findings: vec![],
                summary: "Local gates ran".into(),
            },
        ])
    })
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    f
}

#[tokio::test]
async fn failed_ci_on_a_moved_head_neither_starts_a_fix_nor_fails_at_the_cap() {
    for at_cap in [true, false] {
        for interrupted in [None, Some(7)] {
            let f = if at_cap {
                awaiting_ci_at_cap().await
            } else {
                let f = awaiting_ci().await;
                dispatch::step_now(&f.e, &f.task).await.unwrap();
                assert_eq!(state(&f), TaskState::AwaitingChecks);
                f
            };
            f.fake.set_checks(
                &f.repo,
                &f.first,
                vec![CheckObservation {
                    name: "rust".into(),
                    status: ChecksStatus::Failed,
                    run: interrupted,
                    conclusion: None,
                    url: None,
                }],
            );
            f.fake.set_checks(
                &f.repo,
                &f.second,
                vec![CheckObservation {
                    name: "rust".into(),
                    status: ChecksStatus::Success,
                    run: None,
                    conclusion: None,
                    url: None,
                }],
            );
            *f.interrupted.move_during_checks.lock().unwrap() =
                Some((f.e.mirror("project"), "dvb/t-1".into(), f.second.clone()));
            let before = f.e.store.last_id().unwrap();
            f.interrupted.calls.lock().unwrap().clear();

            dispatch::step_now(&f.e, &f.task).await.unwrap();

            assert_eq!(
                state(&f),
                TaskState::InReview,
                "at_cap={at_cap}, interrupted={interrupted:?}"
            );
            assert_eq!(
                f.e.proj().tasks[&f.task].view.head.as_deref(),
                Some(f.second.as_str()),
                "at_cap={at_cap}, interrupted={interrupted:?}"
            );
            assert!(f.e.proj().tasks[&f.task].checks.is_none());
            assert!(f.e.proj().questions.is_empty());
            assert!(f.fake.reruns(&f.repo).is_empty());
            assert_eq!(
                *f.interrupted.calls.lock().unwrap(),
                vec!["pr", "checks", "pr", "fetch"]
            );
            assert!(
                !f.e.store
                    .after(before)
                    .unwrap()
                    .iter()
                    .any(|event| matches!(
                        event.kind,
                        EventKind::ChecksObserved { .. }
                            | EventKind::TaskFailed { .. }
                            | EventKind::RunStarted(_)
                    ))
            );
        }
    }
}

/// A terminal cap retains one question, status attention and all task evidence
/// after repeated dispatcher looks and a real store reopen.
async fn assert_durable_cap_question(mut f: Fixture, why: &str) {
    let task = f.e.proj().tasks[&f.task].view.clone();
    assert!(matches!(&task.state, TaskState::Failed { reason } if reason.contains(why)));
    assert_eq!(task.head.as_deref(), Some(f.first.as_str()));
    assert_eq!(task.pr, Some(1));
    let questions: Vec<_> =
        f.e.proj()
            .questions
            .values()
            .map(|q| q.view.clone())
            .collect();
    assert_eq!(questions.len(), 1);
    let question = &questions[0];
    assert!(question.open);
    assert_eq!(question.project, task.project);
    assert!(question.subject.contains(why), "{}", question.subject);
    assert!(question.subject.contains("3 review rounds used up"));
    assert!(!question.subject.contains("findings are still open"));
    assert!(!question.subject.contains("needs a fix round"));
    assert_eq!(question.recommended, 1);
    assert_eq!(question.options[0].text, "Leave it failed");
    assert_eq!(question.options[1].text, "Change course");
    assert!(
        question
            .options
            .iter()
            .all(|o| !o.approves_plan && o.retries_task.is_none())
    );
    let events = support::events::all(&f.e);
    let failure = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::TaskFailed { task: id, reason: Some(reason) } if id == &f.task && reason.contains(why)))
        .unwrap()
        .clone();
    let opened = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::QuestionOpened { kind: Some(dvb_core::event::QuestionKind::Ordinary),
question: id, .. } if id == &question.id))
        .unwrap();
    assert_eq!(opened.actor, Actor::Engine);
    assert_eq!(opened.project, Some(task.project.clone()));
    {
        let proj = f.e.proj();
        assert_eq!(
            proj.questions[&question.id].lapses_on,
            dvb_core::event::LapsesOn::GoalVersion(proj.goals[&task.goal].version)
        );
    }
    let starts = runs_started(&f.e);
    for restart in [false, true] {
        if restart {
            f = reopen(f);
            dvb_engine::reconcile::mark(&f.e).unwrap();
            dvb_engine::reconcile::reconcile(f.e.clone()).await.unwrap();
        }
        for _ in 0..3 {
            dispatch::step_now(&f.e, &f.task).await.unwrap();
        }
        assert_eq!(f.e.proj().tasks[&f.task].view, task);
        assert_eq!(
            f.e.proj().tasks[&f.task].facts.round,
            caps::REVIEW_ROUNDS_PER_PR
        );
        assert!(f.e.proj().tasks[&f.task].facts.builder.is_none());
        assert_eq!(runs_started(&f.e), starts, "the cap admitted another run");
        let events = support::events::all(&f.e);
        assert!(
            events
                .iter()
                .any(|ev| ev.id == failure.id && ev.kind == failure.kind)
        );
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::QuestionOpened { .. }))
                .count(),
            1
        );
        assert_eq!(f.e.proj().questions[&question.id].view, *question);
        assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
        for path in ["/status", "/status?project=p"] {
            let req = axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            let res = tower::ServiceExt::oneshot(dvb_engine::api::router(f.e.clone()), req)
                .await
                .unwrap();
            assert!(res.status().is_success());
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            let status: dvb_core::api::Status = serde_json::from_slice(&body).unwrap();
            // The owner question and the failed task, as the app's Needs you lists them.
            assert_eq!(status.needs_you, 2);
            assert_eq!(status.running, 0);
        }
    }
}

#[tokio::test]
async fn at_the_round_cap_a_failed_check_opens_one_durable_owner_question() {
    let f = awaiting_ci_at_cap().await;
    checks_seen(
        &f,
        (ChecksStatus::Failed, None),
        (ChecksStatus::Success, None),
    );
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(
        f.e.proj().tasks[&f.task].checks,
        Some((f.first.clone(), ChecksStatus::Failed))
    );
    assert_durable_cap_question(f, "required check `rust` failed").await;
}

const RETRY_TEXT: &str = "The e2e check needs the fixture regenerated; regenerate it.";

/// The runs started after `before`.
fn runs_since(e: &AppState, before: EventId) -> Vec<RunAssignment> {
    e.store
        .after(before)
        .unwrap()
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::RunStarted(a) = ev.kind {
                Some(a)
            } else {
                None
            }
        })
        .collect()
}

/// The owner's retry with `RETRY_TEXT` of the failed task starts its builder
/// on the head, told the text, before any review; returns that builder.
async fn retried_into_a_builder(f: &Fixture) -> RunAssignment {
    let before = f.e.store.last_id().unwrap();
    let state =
        dispatch::retry_failed(&f.e, &f.task, Actor::Owner, Some(RETRY_TEXT.into()), |_| {
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(state, TaskState::Preparing);
    let started = runs_since(&f.e, before);
    let [builder] = &started[..] else {
        panic!("one run, the builder: {started:?}");
    };
    assert_eq!(builder.reviews, None);
    assert_eq!(builder.base.as_deref(), Some(f.first.as_str()));
    let prompt = builder_prompt(f).await;
    assert!(prompt.contains("## Retry context"), "{prompt}");
    assert!(prompt.contains(RETRY_TEXT), "{prompt}");
    builder.clone()
}

/// The builder fixes the head it started on in its workspace, collects it and ends.
fn collect_fix(f: &Fixture, builder: &RunAssignment) {
    let on = builder.base.as_deref().unwrap();
    let mirror = f.e.mirror("project");
    let ws = f.e.cfg.paths.work.join(builder.run.as_str()).join("ws");
    git(
        f._tmp.path(),
        &[
            "clone",
            "--quiet",
            mirror.to_str().unwrap(),
            ws.to_str().unwrap(),
        ],
    );
    git(&ws, &["checkout", "--quiet", "--detach", on]);
    std::fs::write(ws.join("file"), format!("fixed by {}\n", builder.run)).unwrap();
    git(&ws, &["add", "file"]);
    let tree = git(&ws, &["write-tree"]);
    let collected = git(&ws, &["commit-tree", &tree, "-p", on, "-m", "fix"]);
    collect(&ws, &collected);
    let run = builder.run.clone();
    f.e.append_checked(
        Some(&ProjectId::new("p")),
        Actor::Run(run.clone()),
        |_, _| {
            Ok(vec![EventKind::RunCollected {
                outcome: None,
                run: run.clone(),
                head: Some(collected),
                summary: None,
            }])
        },
    )
    .unwrap();
    f.e.run_command(
        &run,
        dvb_engine::Decision::new("collected", Command::HeadWritten),
    )
    .unwrap();
    f.e.end_run(&run, dvb_core::event::EndReason::Completed, None)
        .unwrap();
}

/// A task that failed on a required check retries as a builder on its
/// head with the retry text, not as a review of the unchanged head.
#[tokio::test]
async fn a_task_failed_on_a_required_check_retries_into_a_builder_with_its_text() {
    let f = awaiting_ci().await;
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::ChecksObserved {
                task: f.task.clone(),
                head: f.first.clone(),
                status: ChecksStatus::Failed,
                names: vec!["rust".into()],
                retry_question: None,
                retry_runs: vec![],
            },
            EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::AwaitingChecks,
                to: TaskState::Failed {
                    reason: "CI failed: required check `rust` failed".into(),
                },
            },
        ])
    })
    .unwrap();
    retried_into_a_builder(&f).await;
}

#[tokio::test]
async fn owner_retry_closes_failure_and_orchestrator_stop_questions() {
    use axum::Json;
    use axum::extract::State;
    use axum::http::{Method, StatusCode};
    use dvb_core::run_verbs::{AskBody, AskReply};

    for failed in [true, false] {
        let f = awaiting_ci_at_cap().await;
        let mut closed = vec![];
        if failed {
            checks_seen(
                &f,
                (ChecksStatus::Failed, None),
                (ChecksStatus::Success, None),
            );
            settle(&f.e, &f.task).await.unwrap();
            assert!(matches!(state(&f), TaskState::Failed { .. }));
            let proj = f.e.proj();
            let questions: Vec<_> = proj.questions.values().filter(|q| q.view.open).collect();
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].failure_task.as_ref(), Some(&f.task));
            closed.push(questions[0].view.id.clone());
            drop(proj);
        } else {
            dispatch::stop(&f.e, &f.task, Actor::Owner).await.unwrap();
            assert!(matches!(state(&f), TaskState::Stopped { .. }));
        }
        let orchestrator = RunId::new("orchestrator-run");
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::new(orchestrator.clone())
                    .role(RoleId::new("orchestrator"))
                    .reason("Plan".into())
                    .build(),
            )])
        })
        .unwrap();
        let mut unrelated = None;
        for about_task in [true, false] {
            let body: AskBody = serde_json::from_value(serde_json::json!({
                "subject": if about_task { "Retry this task?" } else { "Another decision?" },
                "options": [{
                    "text": "Continue", "effect": "Continue the work.",
                    "retries_task": about_task.then_some(&f.task)
                }],
                "recommended": 1
            }))
            .unwrap();
            let caller = dvb_engine::serve::Caller {
                run: f.e.proj().runs[&orchestrator].clone(),
            };
            let question =
                support::http::run::<AskReply, _>("ask", State(f.e.clone()), caller, Json(body))
                    .await
                    .unwrap()
                    .0
                    .question;
            assert!(f.e.proj().questions[&question].view.open);
            if about_task {
                closed.push(question);
            } else {
                unrelated = Some(question);
            }
        }
        let before = f.e.store.last_id().unwrap();
        let (status, _) = support::http::call(
            &f.e,
            Method::POST,
            &format!("/tasks/{}/retry", f.task),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let events = f.e.store.after(before).unwrap();
        for question in &closed {
            assert!(!f.e.proj().questions[question].view.open);
            assert!(events.iter().any(|ev| matches!(&ev.kind,
                EventKind::QuestionClosed { question: id, task }
                    if id == question && task == &f.task)));
        }
        let f = reopen(f);
        for question in &closed {
            assert!(!f.e.proj().questions[question].view.open);
        }
        assert!(f.e.proj().questions[&unrelated.unwrap()].view.open);
    }
}

/// At the review-round cap, a task whose required check failed retries
/// as one more builder round with the retry text; that round is reviewed and
/// checked as usual, and merges.
#[tokio::test]
async fn after_the_review_round_cap_a_retry_builds_one_more_round_that_merges() {
    let f = awaiting_ci_at_cap().await;
    checks_seen(
        &f,
        (ChecksStatus::Failed, None),
        (ChecksStatus::Success, None),
    );
    settle(&f.e, &f.task).await.unwrap();
    assert!(matches!(state(&f), TaskState::Failed { .. }));
    let builder = retried_into_a_builder(&f).await;
    until_state(&f.e, &f.task, TaskState::Running).await;

    collect_fix(&f, &builder);

    // Published as the next round, then reviewed and checked.
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let t = f.e.proj().tasks[&f.task].clone();
    let head = t.view.head.clone().unwrap();
    assert_ne!(head, f.first);
    assert_eq!(t.facts.round, caps::REVIEW_ROUNDS_PER_PR + 1);
    assert!(!t.facts.retried);
    assert!(t.needs_reviewer());
    verdict_by(&f.e, &f.task, &head, "code-reviewer", false);
    let reviewed =
        f.e.decide(
            f.e.proj(),
            &f.task,
            Actor::Engine,
            dvb_engine::Decision::new(
                "verdict",
                Command::VerdictRecorded {
                    head: head.clone(),
                    open: 0,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(reviewed, TaskState::AwaitingChecks);
    green(&f.fake, &f.repo, &head);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
}

/// The prompt of the task's current run, a reviewer.
async fn reviewer_prompt(f: &Fixture) -> String {
    let (run, project) = {
        let proj = f.e.proj();
        let run = proj.tasks[&f.task].facts.run.clone().unwrap();
        (
            proj.runs[&run].clone(),
            proj.projects[&ProjectId::new("p")].clone(),
        )
    };
    assert!(run.assignment.reviews.is_some(), "{run:?}");
    let vars = dvb_engine::prompt::context(&f.e, &run, &project)
        .await
        .unwrap();
    dvb_engine::prompt::render(
        include_str!("../../../prompts/code-reviewer.md"),
        &vars,
        false,
    )
}

/// A task the owner stopped with its PR (its head approved and green,
/// at the review-round cap), retried with text, gets one builder round on
/// that branch's head with the text; the old head is not merged as it was.
/// The new round is reviewed against the text.
#[tokio::test]
async fn a_stopped_task_with_a_pr_retried_with_text_builds_on_its_head_and_never_merges_it() {
    let f = awaiting_ci_at_cap().await;
    green(&f.fake, &f.repo, &f.first);
    dispatch::stop(&f.e, &f.task, Actor::Owner).await.unwrap();
    assert!(matches!(state(&f), TaskState::Stopped { .. }));
    let builder = retried_into_a_builder(&f).await;
    let t = f.e.proj().tasks[&f.task].clone();
    assert!(t.facts.retried);
    assert_eq!(t.view.pr, Some(1));
    until_state(&f.e, &f.task, TaskState::Running).await;
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);

    collect_fix(&f, &builder);
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let t = f.e.proj().tasks[&f.task].clone();
    assert_ne!(t.view.head.as_ref(), Some(&f.first));
    assert_eq!(t.branch.as_deref(), Some("dvb/t-1"));
    assert_eq!(t.facts.round, caps::REVIEW_ROUNDS_PER_PR + 1);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
    let prompt = reviewer_prompt(&f).await;
    assert!(
        prompt.contains("## Instructions this task was sent back with"),
        "{prompt}"
    );
    assert!(prompt.contains(&format!("1. {RETRY_TEXT}")), "{prompt}");
}

/// A task in review with a PR, retried with text: its review of the old
/// head is stopped first, then a builder builds on that head. A late clean
/// verdict on the old head moves nothing, and it is not merged.
#[tokio::test]
async fn an_in_review_task_retried_with_text_stops_its_review_and_builds_on_its_head() {
    let f = awaiting_ci().await;
    let reviewer = RunId::new("reviewer2");
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::Merging,
                to: TaskState::InReview,
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(reviewer.clone())
                    .task(Some(f.task.clone()))
                    .role(RoleId::new("code-reviewer"))
                    .reason("Review".into())
                    .reviews(Some(f.first.clone()))
                    .base(Some(f.first.clone()))
                    .build(),
            ),
        ])
    })
    .unwrap();
    let before = f.e.store.last_id().unwrap();
    retried_into_a_builder(&f).await;
    assert_eq!(
        f.e.proj().runs[&reviewer].ended,
        Some(dvb_core::event::EndReason::Stopped)
    );
    let stopped = f.e.store.after(before).unwrap().into_iter().any(|ev| {
        matches!(
            &ev.kind,
            EventKind::TaskStateChanged {
                from: TaskState::InReview,
                to: TaskState::Stopped { .. },
                ..
            }
        )
    });
    assert!(stopped, "its review was stopped first");
    let late = f.e.decide(
        f.e.proj(),
        &f.task,
        Actor::Engine,
        dvb_engine::Decision::new(
            "verdict",
            Command::VerdictRecorded {
                head: f.first.clone(),
                open: 0,
            },
        ),
        |_| Ok(()),
    );
    assert!(
        !matches!(
            late,
            Ok(TaskState::AwaitingChecks | TaskState::Merging | TaskState::Done)
        ),
        "{late:?}"
    );
    green(&f.fake, &f.repo, &f.first);
    settle(&f.e, &f.task).await.unwrap();
    assert!(!matches!(state(&f), TaskState::Done | TaskState::Merging));
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
}

/// The orchestrator sends an in-review task back with the owner's yes,
/// given while it was in review. Its review is stopped and its builder builds
/// on the head: that stop is the retry's own and does not use up the yes, but
/// the retry does.
#[tokio::test]
async fn the_orchestrator_sends_an_in_review_task_back_with_a_yes_given_in_review() {
    use axum::Json;
    use axum::extract::State;
    use dvb_core::api::TaskView;
    use dvb_core::event::Answer;
    use dvb_core::run_verbs::{AskBody, AskReply, TaskRetryBody};
    let f = awaiting_ci().await;
    let p = ProjectId::new("p");
    let orchestrator = RunId::new("orchestrator-run");
    f.e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::Merging,
                to: TaskState::InReview,
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(orchestrator.clone())
                    .role(RoleId::new("orchestrator"))
                    .reason("Plan".into())
                    .build(),
            ),
        ])
    })
    .unwrap();
    let caller = || dvb_engine::serve::Caller {
        run: f.e.proj().runs[&orchestrator].clone(),
    };
    let body = serde_json::from_value::<AskBody>(serde_json::json!({
        "subject": "Send it back with the owner's note?",
        "options": [{"text": "Send it back", "effect": "One builder round on its head.", "retries_task": f.task}],
        "recommended": 1
    }))
    .unwrap();
    let question =
        support::http::run::<AskReply, _>("ask", State(f.e.clone()), caller(), Json(body))
            .await
            .unwrap()
            .0
            .question;
    f.e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question,
            answer: Answer::Choice(1),
        }])
    })
    .unwrap();
    let retry = || {
        support::http::run::<TaskView, _>(
            "task/retry",
            State(f.e.clone()),
            caller(),
            Json(TaskRetryBody {
                task: f.task.clone(),
                text: Some(RETRY_TEXT.into()),
            }),
        )
    };
    let before = f.e.store.last_id().unwrap();
    let view = retry().await.unwrap().0;
    assert_eq!(view.state, TaskState::Preparing);
    let started = runs_since(&f.e, before);
    let [builder] = &started[..] else {
        panic!("one run, the builder: {started:?}");
    };
    assert_eq!(builder.reviews, None);
    assert_eq!(builder.base.as_deref(), Some(f.first.as_str()));
    let prompt = builder_prompt(&f).await;
    assert!(prompt.contains(RETRY_TEXT), "{prompt}");
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
    // The yes is used up by its retry.
    match retry().await {
        Err(dvb_engine::Fail::Refused(r)) => {
            assert_eq!(r.code, dvb_core::RefusalCode::GoalNotApproved);
        }
        other => panic!("{other:?}"),
    }
}

/// A task stopped before it had a PR head is still refused as stopped.
#[tokio::test]
async fn a_stopped_task_without_a_pr_is_still_refused_as_stopped() {
    let f = awaiting_ci().await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: f.task.clone(),
            from: TaskState::Merging,
            to: TaskState::Stopped {
                was: Box::new(TaskState::Merging),
            },
        }])
    })
    .unwrap();
    // Its PR is not opened yet: the branch was pushed again and not yet reviewed.
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::BranchPublished {
            task: f.task.clone(),
            round: 2,
            branch: "dvb/t-1".into(),
            head: f.second.clone(),
        }])
    })
    .unwrap();
    assert!(f.e.proj().tasks[&f.task].publication_pending);
    let refused =
        dispatch::retry_failed(&f.e, &f.task, Actor::Owner, Some(RETRY_TEXT.into()), |_| {
            Ok(())
        })
        .await;
    match refused {
        Err(dvb_engine::Fail::Refused(r)) => {
            assert_eq!(r.code, dvb_core::RefusalCode::AlreadyStopped);
        }
        other => panic!("{other:?}"),
    }
}

/// A task that failed on the branch repair limit, reviewed clean and
/// green, retries with its repair count reset and merges with no run.
#[tokio::test]
async fn a_task_failed_on_the_repair_limit_retries_into_its_merge_without_a_run() {
    let f = awaiting_ci().await;
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::AwaitingChecks);
    // Its repair rounds are used up.
    let starts = (0..caps::BRANCH_REPAIRS_PER_TASK)
        .flat_map(|i| {
            [true, false].map(|branch_repair| EventKind::TaskStarted {
                task: f.task.clone(),
                run: RunId::new(format!("repair{i}-{branch_repair}")),
                resume: false,
                branch_repair,
                retried: false,
            })
        })
        .collect();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(starts))
        .unwrap();
    assert_eq!(
        f.e.proj().tasks[&f.task].repairs,
        caps::BRANCH_REPAIRS_PER_TASK
    );
    f.fake.set_conflicting(&f.repo, 1, Some(true));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert!(
        matches!(&state(&f), TaskState::Failed { reason } if reason.contains("repair limit")),
        "{:?}",
        state(&f)
    );

    // The owner resolved it on main; CI is green on the unchanged head.
    f.fake.set_conflicting(&f.repo, 1, Some(false));
    green(&f.fake, &f.repo, &f.first);
    let before = f.e.store.last_id().unwrap();
    let retried = dispatch::retry_failed(&f.e, &f.task, Actor::Owner, None, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(retried, TaskState::AwaitingChecks);
    assert_eq!(f.e.proj().tasks[&f.task].repairs, 0);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
    assert_eq!(runs_since(&f.e, before), vec![], "no builder or reviewer");
    let f = reopen(f);
    assert_eq!(f.e.proj().tasks[&f.task].repairs, 0, "after replay");
}

/// The engine restarted over the same store, with the same forge.
fn reopen(f: Fixture) -> Fixture {
    let Fixture {
        readings,
        clock,
        _tmp,
        e,
        repo,
        task,
        base,
        first,
        second,
        fake,
        interrupted,
    } = f;
    drop(e);
    let forge: Arc<dyn Forge> = interrupted.clone();
    let e = support::engine::open_with(
        &_tmp.path().join("engine"),
        "unused-dvb",
        readings.options(dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher: Some(Arc::new(support::launcher::FakeLauncher::new(
                dvb_core::launcher::UnitState::Active,
            ))),
            clock: Some(clock.clone()),
            ..Default::default()
        }),
    );
    Fixture {
        readings,
        clock,
        _tmp,
        e,
        repo,
        task,
        base,
        first,
        second,
        fake,
        interrupted,
    }
}

/// `rust` and `e2e`: failed as the run given, else passed.
fn both(f: &Fixture, rust: Option<u64>, e2e: Option<u64>) {
    let check = |name: &str, interrupted: Option<u64>| CheckObservation {
        conclusion: None,
        url: None,
        name: name.into(),
        status: if interrupted.is_some() {
            ChecksStatus::Failed
        } else {
            ChecksStatus::Success
        },
        run: interrupted,
    };
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![check("rust", rust), check("e2e", e2e)],
    );
}

#[tokio::test]
async fn two_cancelled_jobs_of_one_workflow_are_rerun_once_across_a_restart() {
    let f = awaiting_ci_on(true, 0, &["rust", "e2e"]).await;
    both(&f, Some(1), Some(2));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
    let f = reopen(f);
    // The forge still shows the cancelled runs: their rerun is not reported yet.
    for _ in 0..3 {
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
    }
    assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
    // `rust`'s rerun passes, `e2e`'s ends cancelled too: the fix round.
    both(&f, None, Some(3));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
    let prompt = builder_prompt(&f).await;
    assert!(prompt.contains("required check `e2e` failed"), "{prompt}");
}

/// `rust` and `e2e`, each with its status and, when it failed, its run.
fn rust_and_e2e(f: &Fixture, rust: (ChecksStatus, Option<u64>), e2e: (ChecksStatus, Option<u64>)) {
    let check = |name: &str, (status, run): (ChecksStatus, Option<u64>)| CheckObservation {
        conclusion: None,
        url: run.map(|id| format!("https://ci.invalid/runs/{id}")),
        name: name.into(),
        status,
        run,
    };
    f.fake.set_checks(
        &f.repo,
        &f.first,
        vec![check("rust", rust), check("e2e", e2e)],
    );
}

/// The task's failure reasons, oldest first.
fn failure_reasons(f: &Fixture) -> Vec<String> {
    support::events::all(&f.e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::TaskFailed { task, reason } = ev.kind
                && task == f.task
            {
                reason
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn a_fix_round_waits_for_every_rerun_and_names_each_failure() {
    use ChecksStatus::{Failed, Pending};
    let f = awaiting_ci_on(true, 0, &["rust", "e2e"]).await;
    f.fake
        .set_check_excerpt(&f.repo, 3, "error: this `if` statement can be collapsed");
    f.fake
        .set_check_excerpt(&f.repo, 4, "FAIL: 2: dvb watch differs from the stream");
    rust_and_e2e(&f, (Failed, Some(1)), (Failed, Some(2)));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
    // `rust` fails again while `e2e`'s rerun still runs: no round yet.
    rust_and_e2e(&f, (Failed, Some(3)), (Pending, None));
    for _ in 0..3 {
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
    }
    assert!(failure_reasons(&f).is_empty());
    rust_and_e2e(&f, (Failed, Some(3)), (Failed, Some(4)));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert_eq!(f.fake.reruns(&f.repo), vec![1, 2]);
    assert_eq!(
        failure_reasons(&f),
        vec![
            "reading CI failed: CI failed: required checks `e2e`, `rust` failed (rust, e2e failed again after its \
             rerun); `e2e`: FAIL: 2: dvb watch differs from the stream; `rust`: error: this \
             `if` statement can be collapsed; needs a fix round"
                .to_owned()
        ]
    );
    let prompt = builder_prompt(&f).await;
    assert!(
        prompt.contains("`e2e`: FAIL: 2: dvb watch differs"),
        "{prompt}"
    );
    assert!(prompt.contains("`rust`: error: this `if`"), "{prompt}");
}

#[tokio::test]
async fn a_fix_round_waits_for_a_running_check_that_then_passes() {
    use ChecksStatus::{Failed, Pending, Success};
    let f = awaiting_ci_on(true, 0, &["rust", "e2e"]).await;
    f.fake
        .set_check_excerpt(&f.repo, 3, "error: could not compile");
    rust_and_e2e(&f, (Failed, Some(1)), (Pending, None));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.reruns(&f.repo), vec![1]);
    rust_and_e2e(&f, (Failed, Some(3)), (Pending, None));
    for _ in 0..3 {
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
    }
    assert!(failure_reasons(&f).is_empty());
    rust_and_e2e(&f, (Failed, Some(3)), (Success, None));
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert_eq!(
        failure_reasons(&f),
        vec![
            "reading CI failed: CI failed: required check `rust` failed (rust failed again after its rerun); \
             `rust`: error: could not compile; needs a fix round"
                .to_owned()
        ]
    );
}

#[tokio::test]
async fn a_failed_check_whose_log_is_unreadable_is_named_with_its_link() {
    let f = awaiting_ci().await;
    ordinary_failure(&f, 1);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    ordinary_failure(&f, 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    assert_eq!(
        failure_reasons(&f),
        vec![
            "reading CI failed: CI failed: required check `rust` failed (rust failed again after its rerun); \
             `rust` (https://ci.invalid/pr/2): its log could not be read; needs a fix round"
                .to_owned()
        ]
    );
}

#[tokio::test]
async fn a_failed_preparation_leaves_the_fix_round_to_the_next_look() {
    for restart in [false, true] {
        let f = awaiting_ci().await;
        checks_seen(
            &f,
            (ChecksStatus::Failed, None),
            (ChecksStatus::Success, None),
        );
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        // The fix round's fetch of the default branch fails (as does the
        // look for a moved default branch before it).
        *f.interrupted.fetch_error.lock().unwrap() = Some(dvb_core::forge::ForgeError {
            rate_limit: None,
            retryable: true,
            detail: "simulated interruption".into(),
        });
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        *f.interrupted.fetch_error.lock().unwrap() = None;
        assert_eq!(state(&f), TaskState::AwaitingChecks, "restart {restart}");
        let failures = support::events::all(&f.e);
        assert!(
            !failures
                .iter()
                .any(|ev| matches!(ev.kind, EventKind::TaskFailed { .. })),
            "restart {restart}"
        );
        let f = if restart { reopen(f) } else { f };
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::Preparing, "restart {restart}");
    }
}

#[tokio::test]
async fn a_waiting_pr_requires_the_newly_saved_upgrade_check_before_merge() {
    use axum::http::{Method, StatusCode};
    let f = awaiting_ci_on(false, 0, &["contracts", "e2e", "identity", "rust", "ui"]).await;
    let observation = |name: &str, status| CheckObservation {
        conclusion: None,
        url: None,
        name: name.into(),
        status,
        run: None,
    };
    let mut seen: Vec<_> = ["contracts", "e2e", "identity", "rust", "ui"]
        .iter()
        .map(|name| observation(name, ChecksStatus::Pending))
        .collect();
    f.fake.set_checks(&f.repo, &f.first, seen.clone());
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(
        f.e.proj().tasks[&f.task].view.state,
        TaskState::AwaitingChecks
    );
    let (s, _) = support::http::call(&f.e, Method::PUT, "/projects/p/checks",
        Some(serde_json::json!({"expected":["contracts", "e2e", "identity", "rust", "ui", "upgrade"]}))).await;
    assert_eq!(s, StatusCode::OK);
    for check in &mut seen {
        check.status = ChecksStatus::Success;
    }
    for status in [
        None,
        Some(ChecksStatus::Pending),
        Some(ChecksStatus::Failed),
    ] {
        let mut current = seen.clone();
        if let Some(status) = status {
            current.push(observation("upgrade", status));
        }
        f.fake.set_checks(&f.repo, &f.first, current);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(
            f.e.proj().tasks[&f.task].view.state,
            TaskState::AwaitingChecks
        );
        assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
    }
    seen.push(observation("upgrade", ChecksStatus::Success));
    f.fake.set_checks(&f.repo, &f.first, seen);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
}

#[tokio::test]
async fn a_waiting_pr_switches_to_no_ci_and_keeps_the_review_and_head_guards() {
    use axum::http::{Method, StatusCode};
    for guard in ["clean", "moved", "unreviewed"] {
        let f = awaiting_ci_with(false, 0).await;
        f.e.manual_dispatch();
        f.fake.set_checks(&f.repo, &f.first, vec![]);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(
            f.e.proj().tasks[&f.task].view.state,
            TaskState::AwaitingChecks
        );
        let (s, _) = support::http::call(
            &f.e,
            Method::PUT,
            "/projects/p/checks",
            Some(serde_json::json!("no_ci")),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        if guard == "moved" {
            f.fake
                .push(&f.repo, &f.e.mirror("project"), "dvb/t-1", &f.second)
                .await
                .unwrap();
        } else if guard == "unreviewed" {
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![EventKind::VerdictRecorded {
                    review_stage: None,
                    verdict: VerdictId::new("open"),
                    task: f.task.clone(),
                    head: f.first.clone(),
                    run: RunId::new("reviewer"),
                    role: RoleId::new("code-reviewer"),
                    model: "fake-1".into(),
                    vendor: Vendor::Fake,
                    findings: vec![dvb_core::run_verbs::Finding {
                        severity: dvb_core::run_verbs::Severity::High,
                        file: "file".into(),
                        line: None,
                        title: "Broken".into(),
                        detail: "Broken".into(),
                        open: true,
                    }],
                    summary: "Needs a fix".into(),
                }])
            })
            .unwrap();
        }
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        if guard == "moved" {
            assert_eq!(state(&f), TaskState::InReview);
            assert_eq!(
                f.e.proj().tasks[&f.task].view.head.as_ref(),
                Some(&f.second)
            );
            assert!(!f.e.proj().tasks[&f.task].reviews_approved());
            assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
            continue;
        }
        assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Merging);
        assert_eq!(
            f.e.proj().tasks[&f.task].checks,
            Some((f.first.clone(), ChecksStatus::Missing)),
            "NoCi invents no successful CI observation"
        );
        let result = dispatch::step_now(&f.e, &f.task).await;
        if guard == "clean" {
            result.unwrap();
            assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
        } else {
            assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
            if guard == "moved" {
                result.unwrap();
                assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
            } else {
                assert!(result.is_err());
            }
        }
    }
}

#[tokio::test]
async fn a_check_choice_accepted_during_the_final_pr_read_controls_the_merge() {
    use axum::http::{Method, StatusCode};
    for (initial_no_ci, choice) in [
        (false, serde_json::json!({"expected":["rust", "upgrade"]})),
        (false, serde_json::json!("no_ci")),
        (true, serde_json::json!({"expected":["rust", "upgrade"]})),
    ] {
        let f = if initial_no_ci {
            merging().await
        } else {
            let f = awaiting_ci_with(false, 0).await;
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            assert_eq!(state(&f), TaskState::AwaitingChecks);
            green(&f.fake, &f.repo, &f.first);
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            f
        };
        assert_eq!(state(&f), TaskState::Merging);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *f.interrupted.pr_barrier.lock().unwrap() = Some(barrier.clone());
        let step = tokio::spawn({
            let e = f.e.clone();
            let task = f.task.clone();
            async move { dispatch::step_now(&e, &task).await }
        });
        barrier.wait().await;
        let checks_before = f
            .interrupted
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|&&c| c == "checks")
            .count();
        let (status, _) = support::http::call(
            &f.e,
            Method::PUT,
            "/projects/p/checks",
            Some(choice.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // No CI must also skip the final check read even if the old checks disappear.
        f.fake.set_checks(
            &f.repo,
            &f.first,
            vec![CheckObservation {
                conclusion: None,
                url: None,
                name: "rust".into(),
                status: if choice.is_string() {
                    ChecksStatus::Pending
                } else {
                    ChecksStatus::Success
                },
                run: None,
            }],
        );
        barrier.wait().await;
        step.await.unwrap().unwrap();
        let checks_after = f
            .interrupted
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|&&c| c == "checks")
            .count();
        if choice.is_string() {
            assert_eq!(checks_after, checks_before);
            assert_eq!(state(&f), TaskState::Done);
            assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Merged);
        } else {
            assert!(checks_after > checks_before);
            assert_eq!(state(&f), TaskState::AwaitingChecks);
            assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
            assert_eq!(f.fake.pr(&f.repo, 1).await.unwrap().state, PrState::Open);
            f.fake.set_checks(
                &f.repo,
                &f.first,
                ["rust", "upgrade"]
                    .map(|name| CheckObservation {
                        conclusion: None,
                        url: None,
                        name: name.into(),
                        status: ChecksStatus::Success,
                        run: None,
                    })
                    .to_vec(),
            );
            settle(&f.e, &f.task).await.unwrap();
            assert_eq!(state(&f), TaskState::Done);
        }
    }
}

#[tokio::test]
async fn a_check_choice_write_waits_for_the_final_checks_and_merge_effect() {
    use axum::http::{Method, StatusCode};
    use std::task::Poll;
    for boundary in ["checks", "merge"] {
        let f = awaiting_ci_with(false, 0).await;
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        green(&f.fake, &f.repo, &f.first);
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::Merging);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let slot = if boundary == "checks" {
            &f.interrupted.checks_barrier
        } else {
            &f.interrupted.merge_barrier
        };
        *slot.lock().unwrap() = Some(barrier.clone());
        let step = tokio::spawn({
            let e = f.e.clone();
            let task = f.task.clone();
            async move { dispatch::step_now(&e, &task).await }
        });
        barrier.wait().await;
        let choice = serde_json::json!({"expected":["rust", "upgrade"]});
        let mut write = std::pin::pin!(support::http::call(
            &f.e,
            Method::PUT,
            "/projects/p/checks",
            Some(choice.clone())
        ));
        // Poll the route to its first suspension, proving it cannot accept a
        // changed choice while the old choice's merge effect is in flight.
        std::future::poll_fn(|cx| {
            assert!(
                write.as_mut().poll(cx).is_pending(),
                "write accepted during {boundary}"
            );
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            f.e.proj().projects[&ProjectId::new("p")].view.ci,
            CiMode::Expected(vec!["rust".into()])
        );
        barrier.wait().await;
        step.await.unwrap().unwrap();
        assert_eq!(write.await.0, StatusCode::OK);
        assert_eq!(state(&f), TaskState::Done);
        assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::json!(f.e.proj().projects[&ProjectId::new("p")].view.ci),
            choice
        );
    }
}

// Ordered review workers use the engine's live verdict validation and the Fake
// forge. The launcher keeps units alive until the test reports completion.
async fn ordered_review_fixture(review: ReviewKind, design_reviewer: bool) -> Fixture {
    review_fixture(review, design_reviewer, true).await
}

/// A task in review of its first round; `reviewed` as in `fixture_plan_review`.
async fn review_fixture(review: ReviewKind, design_reviewer: bool, reviewed: bool) -> Fixture {
    let f = fixture_plan_review(
        None,
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
        vec![],
        review,
        reviewed,
    )
    .await;
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<_> = roles
        .into_iter()
        .chain(hires)
        .filter(|k| {
            design_reviewer || !matches!(k, EventKind::RoleAdded(r) if r.slug == "design-reviewer")
        })
        .collect();
    kinds.extend([EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-builder"),
        role: RoleId::new("builder"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: false,
    })]);
    if reviewed {
        kinds.push(EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        });
    }
    kinds.extend([
        EventKind::BranchPublished {
            task: f.task.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: f.first.clone(),
        },
        EventKind::TaskStateChanged {
            task: f.task.clone(),
            from: TaskState::Merging,
            to: TaskState::InReview,
        },
    ]);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(&f.e);
    f
}

async fn review_worker(f: &Fixture, stage: ReviewKind) -> RunAssignment {
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    support::wait::FAST
        .until("ordered review worker", || {
            let proj = f.e.proj();
            proj.tasks[&f.task]
                .facts
                .run
                .as_ref()
                .and_then(|id| proj.runs.get(id))
                .is_some_and(|r| r.ended.is_none() && r.assignment.review_stage == Some(stage))
        })
        .await;
    let a = {
        let proj = f.e.proj();
        let a = proj.runs[proj.tasks[&f.task].facts.run.as_ref().unwrap()]
            .assignment
            .clone();
        assert_eq!(
            proj.tasks[&f.task].view.reason.as_deref(),
            Some(match stage {
                ReviewKind::Code => "code review",
                ReviewKind::Design => "design review",
            })
        );
        a
    };
    let prompt = f.e.cfg.paths.work.join(a.run.as_str()).join("prompt.md");
    support::wait::FAST
        .until("ordered review prompt", || prompt.is_file())
        .await;
    assert!(
        std::fs::read_to_string(prompt)
            .unwrap()
            .starts_with(match stage {
                ReviewKind::Code => "# Code reviewer",
                ReviewKind::Design => "# Design reviewer",
            })
    );
    a
}

fn worker_verdict(f: &Fixture, a: &RunAssignment, open: bool) -> EventKind {
    EventKind::VerdictRecorded {
        review_stage: a.review_stage,
        verdict: VerdictId::new(format!("v-{}", a.run)),
        task: f.task.clone(),
        head: a.reviews.clone().unwrap(),
        run: a.run.clone(),
        role: a.role.clone(),
        model: a.model.clone(),
        vendor: a.vendor,
        findings: if open {
            vec![dvb_core::run_verbs::Finding {
                severity: dvb_core::run_verbs::Severity::Medium,
                file: "file".into(),
                line: Some(std::num::NonZeroU32::new(1).unwrap()),
                title: "Fix this".into(),
                detail: "Needs a fix".into(),
                open: true,
            }]
        } else {
            vec![]
        },
        summary: "Fake worker review; local gates ran".into(),
    }
}

fn give_verdict(f: &Fixture, a: &RunAssignment, open: bool) {
    assert!(
        f.e.run_command(
            &a.run,
            dvb_engine::Decision {
                pre: vec![worker_verdict(f, a, open)],
                ..dvb_engine::Decision::new(
                    "verdict",
                    Command::VerdictRecorded {
                        head: a.reviews.clone().unwrap(),
                        open: u32::from(open),
                    }
                )
            }
        )
        .unwrap()
        .is_some()
    );
}

fn end_worker(f: &Fixture, a: &RunAssignment) {
    f.e.end_run(&a.run, dvb_core::event::EndReason::Completed, None)
        .unwrap();
}

fn assert_replayed_reviews(f: &Fixture, code: bool, design: bool) {
    let events = support::events::all(&f.e);
    let replay = dvb_engine::proj::Projections::rebuild(&events);
    let t = &replay.tasks[&f.task];
    assert_eq!(
        t.facts.code_verdict.as_ref().is_some_and(|v| v.open == 0),
        code
    );
    assert_eq!(
        t.facts.design_verdict.as_ref().is_some_and(|v| v.open == 0),
        design
    );
    assert_eq!(t.next_review(), f.e.proj().tasks[&f.task].next_review());
    assert_eq!(
        t.reviews_approved(),
        f.e.proj().tasks[&f.task].reviews_approved()
    );
}

#[tokio::test]
async fn code_has_one_review_and_design_requires_ordered_approvals() {
    for kind in [ReviewKind::Code, ReviewKind::Design] {
        let f = ordered_review_fixture(kind, true).await;
        let code = review_worker(&f, ReviewKind::Code).await;
        assert_eq!(code.role, RoleId::new("code-reviewer"));
        assert!(
            !f.e.proj()
                .runs
                .values()
                .any(|r| r.assignment.review_stage == Some(ReviewKind::Design))
        );
        give_verdict(&f, &code, false);
        if kind == ReviewKind::Design {
            // A duplicate clean or finding verdict cannot alter the next stage.
            assert!(
                f.e.run_command(
                    &code.run,
                    dvb_engine::Decision {
                        pre: vec![worker_verdict(&f, &code, true)],
                        ..dvb_engine::Decision::new(
                            "verdict",
                            Command::VerdictRecorded {
                                head: f.first.clone(),
                                open: 1,
                            }
                        )
                    }
                )
                .unwrap()
                .is_none()
            );
            assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
            assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
            assert_replayed_reviews(&f, true, false);
            // The code unit must finish before dispatch admits design review.
            dispatch::step_now(&f.e, &f.task).await.unwrap();
            assert!(
                !f.e.proj()
                    .runs
                    .values()
                    .any(|r| r.assignment.review_stage == Some(ReviewKind::Design))
            );
            end_worker(&f, &code);
            let design = review_worker(&f, ReviewKind::Design).await;
            assert_eq!(design.role, RoleId::new("design-reviewer"));
            assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
            give_verdict(&f, &design, false);
            assert_replayed_reviews(&f, true, true);
            end_worker(&f, &design);
        } else {
            end_worker(&f, &code);
        }
        settle(&f.e, &f.task).await.unwrap();
        assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
        let stages: Vec<_> =
            f.e.proj()
                .runs
                .values()
                .filter_map(|r| r.assignment.review_stage)
                .collect();
        assert_eq!(stages.iter().filter(|s| **s == ReviewKind::Code).count(), 1);
        assert_eq!(
            stages.iter().filter(|s| **s == ReviewKind::Design).count(),
            usize::from(kind == ReviewKind::Design)
        );
    }
}

#[tokio::test]
async fn either_design_task_review_finding_fixes_and_same_head_repeats_both_reviews() {
    for finding_stage in [ReviewKind::Code, ReviewKind::Design] {
        let f = ordered_review_fixture(ReviewKind::Design, true).await;
        let code = review_worker(&f, ReviewKind::Code).await;
        give_verdict(&f, &code, finding_stage == ReviewKind::Code);
        end_worker(&f, &code);
        let prior_design = if finding_stage == ReviewKind::Design {
            let design = review_worker(&f, ReviewKind::Design).await;
            give_verdict(&f, &design, true);
            end_worker(&f, &design);
            Some(design)
        } else {
            assert!(
                !f.e.proj()
                    .runs
                    .values()
                    .any(|r| r.assignment.review_stage == Some(ReviewKind::Design))
            );
            None
        };
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        until_state(&f.e, &f.task, TaskState::Running).await;
        let builder = f.e.proj().tasks[&f.task].facts.builder.clone().unwrap();
        assert_eq!(
            f.e.proj().runs[&builder].assignment.base.as_deref(),
            Some(f.first.as_str())
        );
        // Exercise the actual no-change publication path with a collected
        // snapshot whose tree equals the original head.
        let ws = f.e.cfg.paths.work.join(builder.as_str()).join("ws");
        git(
            f._tmp.path(),
            &[
                "clone",
                "--quiet",
                f.e.mirror("project").to_str().unwrap(),
                ws.to_str().unwrap(),
            ],
        );
        let collected = git(
            &ws,
            &[
                "commit-tree",
                &format!("{}^{{tree}}", f.first),
                "-p",
                &f.first,
                "-m",
                "snapshot",
            ],
        );
        collect(&ws, &collected);
        f.e.append_checked(
            Some(&ProjectId::new("p")),
            Actor::Run(builder.clone()),
            |_, _| {
                Ok(vec![EventKind::RunCollected {
                    outcome: None,
                    run: builder.clone(),
                    head: Some(collected),
                    summary: Some("Rechecked finding; local gates ran".into()),
                }])
            },
        )
        .unwrap();
        f.e.run_command(
            &builder,
            dvb_engine::Decision::new("collected", Command::HeadWritten),
        )
        .unwrap();
        f.e.end_run(&builder, dvb_core::event::EndReason::Completed, None)
            .unwrap();
        until_state(&f.e, &f.task, TaskState::InReview).await;
        let published = f.e.proj().tasks[&f.task].clone();
        assert_eq!(
            (published.facts.round, published.view.head),
            (2, Some(f.first.clone()))
        );
        assert_replayed_reviews(&f, false, false);
        // Old run evidence cannot return an approval on the identical hash.
        let stale = prior_design.as_ref().unwrap_or(&code);
        assert!(
            f.e.run_command(
                &stale.run,
                dvb_engine::Decision {
                    pre: vec![worker_verdict(&f, stale, false)],
                    ..dvb_engine::Decision::new(
                        "verdict",
                        Command::VerdictRecorded {
                            head: f.first.clone(),
                            open: 0
                        }
                    )
                }
            )
            .unwrap()
            .is_none()
        );
        let code2 = review_worker(&f, ReviewKind::Code).await;
        assert_ne!(code.run, code2.run);
        let code_prompt = std::fs::read_to_string(
            f.e.cfg
                .paths
                .work
                .join(code2.run.as_str())
                .join("prompt.md"),
        )
        .unwrap();
        assert_eq!(
            code_prompt.contains("## Earlier findings to recheck"),
            finding_stage == ReviewKind::Code
        );
        give_verdict(&f, &code2, false);
        end_worker(&f, &code2);
        assert_replayed_reviews(&f, true, false);
        let design2 = review_worker(&f, ReviewKind::Design).await;
        let design_prompt = std::fs::read_to_string(
            f.e.cfg
                .paths
                .work
                .join(design2.run.as_str())
                .join("prompt.md"),
        )
        .unwrap();
        if finding_stage == ReviewKind::Design {
            assert!(design_prompt.contains("## Earlier findings to recheck"));
            assert!(design_prompt.contains("file:1: Fix this. Needs a fix"));
            assert!(design_prompt.contains("keep it open unless it is resolved"));
        } else {
            assert!(!design_prompt.contains("## Earlier findings to recheck"));
        }
        give_verdict(&f, &design2, false);
        end_worker(&f, &design2);
        settle(&f.e, &f.task).await.unwrap();
        assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
        assert_replayed_reviews(&f, true, true);
    }
}

#[tokio::test]
async fn missing_design_reviewer_keeps_the_existing_missing_reviewer_path() {
    let f = ordered_review_fixture(ReviewKind::Design, false).await;
    let code = review_worker(&f, ReviewKind::Code).await;
    give_verdict(&f, &code, false);
    end_worker(&f, &code);
    let err = dispatch::step_now(&f.e, &f.task).await.unwrap_err();
    assert!(
        matches!(err, dvb_engine::Fail::Internal(ref why) if why.to_string() == "the project has no reviewer role")
    );
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
    assert_eq!(
        f.e.proj().tasks[&f.task].view.reason.as_deref(),
        Some("design review")
    );
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
    assert_replayed_reviews(&f, true, false);
}

#[tokio::test]
async fn design_code_stage_uses_lead_then_assignment_even_when_both_stages_share_a_role() {
    for routing in ["lead", "assigned", "same-role"] {
        let f = ordered_review_fixture(ReviewKind::Design, true).await;
        let mut roles = dvb_core::roles::default_records(|r| RoleId::new(r.slug));
        let mut builder = roles.iter().find(|r| r.slug == "backend").unwrap().clone();
        builder.id = RoleId::new("builder");
        builder.reviewed_by = Some(RoleId::new(if routing == "same-role" {
            "design-reviewer"
        } else {
            "architect"
        }));
        if routing == "lead" {
            builder.reports_to = Some(RoleId::new("code-reviewer"));
            let lead = roles
                .iter_mut()
                .find(|r| r.slug == "code-reviewer")
                .unwrap();
            lead.lead = true;
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RoleChanged {
                    restored_from: None,
                    role: lead.clone(),
                }])
            })
            .unwrap();
        }
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RoleAdded(builder)])
        })
        .unwrap();
        let code = review_worker(&f, ReviewKind::Code).await;
        assert_eq!(
            code.role.as_str(),
            match routing {
                "lead" => "code-reviewer",
                "assigned" => "architect",
                "same-role" => "design-reviewer",
                _ => unreachable!(),
            }
        );
        give_verdict(&f, &code, false);
        end_worker(&f, &code);
        let design = review_worker(&f, ReviewKind::Design).await;
        assert_eq!(design.role, RoleId::new("design-reviewer"));
        assert_ne!(code.run, design.run);
        give_verdict(&f, &design, false);
        end_worker(&f, &design);
        settle(&f.e, &f.task).await.unwrap();
        assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    }
}

#[tokio::test]
async fn design_moved_head_and_replayed_previous_round_verdicts_require_both_again() {
    let f = ordered_review_fixture(ReviewKind::Design, true).await;
    let code = review_worker(&f, ReviewKind::Code).await;
    give_verdict(&f, &code, false);
    end_worker(&f, &code);
    let design = review_worker(&f, ReviewKind::Design).await;
    // Hold dispatch while both approvals are recorded and the PR moves.
    let lock = f.e.lock(&f.task).await;
    give_verdict(&f, &design, false);
    end_worker(&f, &design);
    f.fake
        .push(&f.repo, &f.e.mirror("project"), "dvb/t-1", &f.second)
        .await
        .unwrap();
    drop(lock);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
    assert_eq!(
        f.e.proj().tasks[&f.task].view.head.as_deref(),
        Some(f.second.as_str())
    );
    assert_replayed_reviews(&f, false, false);
    // Restore the original hash as a new publication and replay old approvals.
    let lock = f.e.lock(&f.task).await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::BranchPublished {
                task: f.task.clone(),
                round: 2,
                branch: "dvb/t-1".into(),
                head: f.first.clone(),
            },
            worker_verdict(&f, &code, false),
            worker_verdict(&f, &design, false),
        ])
    })
    .unwrap();
    assert_replayed_reviews(&f, false, false);
    assert!(!f.e.proj().tasks[&f.task].reviews_approved());
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
    drop(lock);
}

#[tokio::test]
async fn a_historical_design_single_verdict_cannot_merge_after_replay() {
    let f = fixture_plan_review(None, None, None, vec![], ReviewKind::Design, true).await;
    assert!(f.e.proj().tasks[&f.task].facts.verdict.is_some());
    assert_replayed_reviews(&f, false, false);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::InReview);
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn design_neutral_branch_update_carries_both_reviews_and_rechecks_ci() {
    let f = ordered_review_fixture(ReviewKind::Design, true).await;
    let code = review_worker(&f, ReviewKind::Code).await;
    give_verdict(&f, &code, false);
    end_worker(&f, &code);
    let design = review_worker(&f, ReviewKind::Design).await;
    let lock = f.e.lock(&f.task).await;
    give_verdict(&f, &design, false);
    end_worker(&f, &design);
    f.fake.require_up_to_date(&f.repo);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "project_checks".into(),
            old: serde_json::json!("no_ci"),
            new: serde_json::json!({"expected": ["rust"]}),
            restored_from: None,
        }])
    })
    .unwrap();
    green(&f.fake, &f.repo, &f.first);
    move_main(f._tmp.path(), &f.repo, "other", "other\n");
    drop(lock);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let [(from, updated, true)] = &updates(&f.e)[..] else {
        panic!("both approvals must carry together");
    };
    assert_eq!(from, &f.first);
    assert_replayed_reviews(&f, true, true);
    let carried: Vec<_> = support::events::all(&f.e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::VerdictRecorded {
                head, review_stage, ..
            } = ev.kind
                && &head == updated
            {
                review_stage
            } else {
                None
            }
        })
        .collect();
    assert_eq!(carried, vec![ReviewKind::Code, ReviewKind::Design]);
    settle(&f.e, &f.task).await.unwrap();
    assert_ne!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
    // Behind main, the branch was brought up to date without a merge attempt.
    assert_eq!(f.interrupted.merges.load(Ordering::SeqCst), 0);
    green(&f.fake, &f.repo, updated);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
}

#[tokio::test]
async fn recovery_after_code_approval_admits_design_without_retrying_code() {
    let f = ordered_review_fixture(ReviewKind::Design, true).await;
    let code = review_worker(&f, ReviewKind::Code).await;
    let lock = f.e.lock(&f.task).await;
    give_verdict(&f, &code, false);
    dvb_engine::reconcile::mark(&f.e).unwrap();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunEnded {
            run: code.run.clone(),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        }])
    })
    .unwrap();
    assert_replayed_reviews(&f, true, false);
    drop(lock);
    dvb_engine::reconcile::reconcile(f.e.clone()).await.unwrap();
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let design = review_worker(&f, ReviewKind::Design).await;
    let events = support::events::all(&f.e);
    assert!(!events.iter().any(|ev| matches!(
        ev.kind,
        EventKind::TaskStateChanged {
            to: TaskState::RetryPending { .. },
            ..
        }
    )));
    assert_eq!(events.iter().filter(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.review_stage == Some(ReviewKind::Code))).count(), 1);
    give_verdict(&f, &design, false);
    end_worker(&f, &design);
    settle(&f.e, &f.task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&f.task].view.state, TaskState::Done);
}

#[tokio::test]
async fn ci_poll_reads_the_pr_before_and_after_checks() {
    let f = merging().await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::project_event_with_ci(
                &(ProjectId::new("p")),
                "project",
                &(f.repo.0),
                CiMode::Expected(vec!["rust".into()]),
            ),
            EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::Merging,
                to: TaskState::AwaitingChecks,
            },
        ])
    })
    .unwrap();
    let before = f.fake.calls().len();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    let calls = f.fake.calls();
    assert_eq!(
        calls[before..]
            .iter()
            .filter(|c| c.as_str() == "pr")
            .count(),
        2
    );
    assert_eq!(
        calls[before..]
            .iter()
            .filter(|c| c.as_str() == "checks")
            .count(),
        1
    );
    assert_eq!(
        f.e.proj().tasks[&f.task].view.state,
        TaskState::AwaitingChecks
    );
}

#[tokio::test(start_paused = true)]
async fn run_end_advances_its_task_without_polling_an_unrelated_ci_wait() {
    let other = TaskId::new("other");
    let f = fixture_plan(
        None,
        None,
        None,
        vec![PlannedTask {
            after: vec![],
            task: other.clone(),
            title: "Other".into(),
            role: RoleId::new("builder"),
            review: ReviewKind::Code,
            summary: "Other".into(),
            routine: false,
        }],
    )
    .await;
    let project = ProjectId::new("p");
    f.e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::project_event_with_ci(
                &(project),
                "project",
                &(f.repo.0),
                CiMode::Expected(vec!["rust".into()]),
            ),
            EventKind::BranchPublished {
                task: other.clone(),
                round: 1,
                branch: "dvb/t-1".into(),
                head: f.first.clone(),
            },
            EventKind::PrOpened {
                task: other.clone(),
                pr: 1,
                url: "https://example.invalid/1".into(),
            },
            EventKind::TaskStateChanged {
                task: other.clone(),
                from: TaskState::Planned,
                to: TaskState::AwaitingChecks,
            },
            EventKind::TaskStateChanged {
                task: f.task.clone(),
                from: TaskState::Merging,
                to: TaskState::AwaitingChecks,
            },
        ])
    })
    .unwrap();
    let before = f.fake.calls().len();
    dispatch::run_ended(
        &f.e,
        &RunId::new("reviewer"),
        dvb_core::event::EndReason::Completed,
    )
    .await
    .unwrap();
    // Its look fetches the default branch: real I/O.
    support::wait::FAST
        .until("the task's CI look", || {
            f.e.proj().tasks[&f.task].checks.is_some()
        })
        .await;
    let calls = f.fake.calls();
    assert_eq!(
        calls[before..]
            .iter()
            .filter(|c| c.as_str() == "checks")
            .count(),
        1
    );
    assert_eq!(
        f.e.proj().tasks[&other].view.state,
        TaskState::AwaitingChecks
    );
    assert!(f.e.proj().tasks[&f.task].checks.is_some());
}

#[tokio::test(start_paused = true)]
async fn run_end_wakes_a_slot_blocked_ci_fix_round_before_its_next_poll() {
    support::wait::io(async {
        let f = awaiting_ci_with(true, 3).await;
        f.fake.set_checks(
            &f.repo,
            &f.first,
            vec![CheckObservation {
                name: "rust".into(),
                status: ChecksStatus::Failed,
                run: None,
                conclusion: None,
                url: None,
            }],
        );
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        dispatch::step_now(&f.e, &f.task).await.unwrap();
        assert_eq!(state(&f), TaskState::AwaitingChecks);
        // Let its dispatcher reach the CI timer, with every slot still held.
        // Its look, after the second step's, ends reading main's CI after
        // fetching the default branch (real I/O).
        support::wait::FAST
            .until("the dispatcher's look", || {
                f.fake
                    .calls()
                    .iter()
                    .filter(|call| *call == "main_checks")
                    .count()
                    == 2
            })
            .await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let before = f.fake.calls().len();
        dispatch::run_ended(
            &f.e,
            &RunId::new("other0"),
            dvb_core::event::EndReason::Completed,
        )
        .await
        .unwrap();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let calls = f.fake.calls();
        assert!(
            calls[before..].iter().any(|call| call == "checks"),
            "released slot did not wake the CI fix round"
        );
        assert!(f.e.proj().live_runs().count() <= 3);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn slot_release_admits_review_while_the_builder_is_still_exiting() {
    let waiting = TaskId::new("waiting-review");
    let f = fixture_plan(
        None,
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
        vec![PlannedTask {
            after: vec![],
            task: waiting.clone(),
            title: "Review next".into(),
            role: RoleId::new("backend"),
            review: ReviewKind::Code,
            summary: "Review next".into(),
            routine: false,
        }],
    )
    .await;
    f.fake
        .push(&f.repo, &f.e.mirror("project"), "dvb/next", &f.first)
        .await
        .unwrap();
    let pr = f
        .fake
        .open_pr(&f.repo, "dvb/next", "Review next", "Body")
        .await
        .unwrap();
    let (roles, hires) = support::events::default_role_hire_events();
    let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
    events.push(EventKind::GoalApproved {
        goal: GoalId::new("g"),
    });
    for (run, task, reviews) in [
        ("exiting-builder", Some(waiting.clone()), None),
        ("slot-release", None, Some("unrelated".to_owned())),
    ] {
        events.push(EventKind::RunStarted(
            support::assignment::Assignment::new(RunId::new(run))
                .task(task)
                .hire(HireId::new("h-backend"))
                .reason("Fixture".into())
                .reviews(reviews)
                .base(Some(f.first.clone()))
                .build(),
        ));
    }
    events.push(EventKind::BranchPublished {
        task: waiting.clone(),
        round: 1,
        branch: "dvb/next".into(),
        head: f.first.clone(),
    });
    events.push(EventKind::PrOpened {
        task: waiting.clone(),
        pr: pr.number,
        url: pr.url,
    });
    events.push(EventKind::TaskStateChanged {
        task: waiting.clone(),
        from: TaskState::Planned,
        to: TaskState::InReview,
    });
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    assert_eq!(f.e.proj().live_runs().count(), 3);
    dispatch::step_now(&f.e, &waiting).await.unwrap();
    dispatch::run_ended(
        &f.e,
        &RunId::new("slot-release"),
        dvb_core::event::EndReason::Completed,
    )
    .await
    .unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        f.e.proj()
            .runs
            .values()
            .any(|run| run.assignment.task.as_ref() == Some(&waiting)
                && run.assignment.reviews.as_deref() == Some(f.first.as_str())
                && run.ended.is_none())
    );
    assert!(
        f.e.proj().runs[&RunId::new("exiting-builder")]
            .ended
            .is_none()
    );
    assert_eq!(f.e.proj().live_runs().count(), 3);
}

// --- Routine hires ------------------------------------------------

/// The builder role's routine hire, on another Fake model than its default.
fn routine_hire() -> EventKind {
    EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-routine"),
        role: RoleId::new("builder"),
        model: "fake-next".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: true,
    })
}

/// In review on `first`, published by the builder run `b1` on `built_by`:
/// the builder role's default hire `h-builder` and, when `routine`, its
/// routine hire.
async fn routine_review(routine: bool, built_by: &str) -> Fixture {
    let f = merging_on(
        None,
        Some(Arc::new(support::launcher::FakeLauncher::new(
            dvb_core::launcher::UnitState::Active,
        ))),
    )
    .await;
    f.e.manual_dispatch();
    let (roles, hires) = support::events::default_role_hire_events();
    let mut kinds: Vec<EventKind> = roles.into_iter().chain(hires).collect();
    kinds.push(EventKind::HireAdded(dvb_core::roles::HireRecord {
        id: HireId::new("h-builder"),
        role: RoleId::new("builder"),
        model: "fake-1".into(),
        effort: Effort::Medium,
        status: dvb_core::roles::HireStatus::Active,
        routine: false,
    }));
    if routine {
        kinds.push(routine_hire());
    }
    let b1 = RunId::new("b1");
    kinds.extend([
        EventKind::RunStarted(
            support::assignment::Assignment::new(b1.clone())
                .task(Some(f.task.clone()))
                .role(RoleId::new("builder"))
                .hire(HireId::new(built_by))
                .base(Some(f.first.clone()))
                .build(),
        ),
        EventKind::RunEnded {
            run: b1,
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        },
        EventKind::BranchPublished {
            task: f.task.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: f.first.clone(),
        },
        EventKind::RunEnded {
            run: RunId::new("reviewer"),
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        },
        EventKind::TaskStateChanged {
            task: f.task.clone(),
            from: TaskState::Merging,
            to: TaskState::InReview,
        },
    ]);
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    approved(&f.e);
    f
}

/// The reviewer sends `head` back with an open finding; the dispatcher
/// starts the fix round. Returns its run and hire.
async fn sent_back(f: &Fixture, head: &str) -> (RunId, HireId) {
    verdict_by(&f.e, &f.task, head, "code-reviewer", true);
    let ended = EventKind::RunEnded {
        run: RunId::new(format!("{}-code-reviewer-{head}-true", f.task)),
        reason: dvb_core::event::EndReason::Completed,
        detail: None,
    };
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![ended])
    })
    .unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    until_state(&f.e, &f.task, TaskState::Running).await;
    let t = f.e.proj().tasks[&f.task].clone();
    let run = t.facts.builder.clone().unwrap();
    let a = f.e.proj().runs[&run].assignment.clone();
    assert_eq!(a.base.as_deref(), Some(head), "a fix round on the head");
    assert_eq!(t.view.round_model, Some(a.model));
    (run, a.hire)
}

/// The fix round `run` publishes `head` as `round`, back in review.
async fn fix_published(f: &Fixture, run: RunId, round: u8, head: &str) {
    f.fake
        .push(&f.repo, &f.e.mirror("project"), "dvb/t-1", head)
        .await
        .unwrap();
    let kinds = vec![
        EventKind::RunEnded {
            run,
            reason: dvb_core::event::EndReason::Completed,
            detail: None,
        },
        EventKind::BranchPublished {
            task: f.task.clone(),
            round,
            branch: "dvb/t-1".into(),
            head: head.into(),
        },
        EventKind::TaskStateChanged {
            task: f.task.clone(),
            from: TaskState::Running,
            to: TaskState::InReview,
        },
    ];
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
}

/// A review fix round on the PR of a task that is not routine, built on the
/// default hire, runs on the role's routine hire.
#[tokio::test]
async fn a_review_fix_round_runs_on_the_routine_hire() {
    let f = routine_review(true, "h-builder").await;
    assert!(!f.e.proj().tasks[&f.task].view.routine);
    let (_, hire) = sent_back(&f, &f.first).await;
    assert_eq!(hire, HireId::new("h-routine"));
}

/// A routine round's PR sent back twice: the next round goes to the default
/// hire. The default hire's own send-backs are not counted.
#[tokio::test]
async fn two_send_backs_of_routine_rounds_move_the_next_round_to_the_default_hire() {
    let f = routine_review(true, "h-routine").await;
    let (run, hire) = sent_back(&f, &f.first).await;
    assert_eq!(hire, HireId::new("h-routine"), "one send-back");
    assert_eq!(f.e.proj().tasks[&f.task].routine_sent_back, 1);
    fix_published(&f, run, 2, &f.second).await;
    let (_, hire) = sent_back(&f, &f.second).await;
    assert_eq!(hire, HireId::new("h-builder"), "two send-backs");
    assert_eq!(f.e.proj().tasks[&f.task].routine_sent_back, 2);
    assert_eq!(
        f.e.proj().tasks[&f.task].view.round_model.as_deref(),
        Some("fake-1")
    );
    let f = reopen(f);
    assert_eq!(
        f.e.proj().tasks[&f.task].routine_sent_back,
        2,
        "after replay"
    );
}

/// With no routine hire a fix round runs on the last builder's hire, as before.
#[tokio::test]
async fn without_a_routine_hire_a_fix_round_keeps_the_last_builders_hire() {
    let f = routine_review(false, "h-builder").await;
    let (_, hire) = sent_back(&f, &f.first).await;
    assert_eq!(hire, HireId::new("h-builder"));
}

/// A CI fix round on the PR of a task that is not routine runs on the role's
/// routine hire.
#[tokio::test]
async fn a_ci_fix_round_runs_on_the_routine_hire() {
    let f = awaiting_ci().await;
    let kinds = vec![routine_hire()];
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(kinds))
        .unwrap();
    f.e.manual_dispatch();
    ordinary_failure(&f, 1);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(f.fake.reruns(&f.repo), vec![1]);
    ordinary_failure(&f, 2);
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    assert_eq!(state(&f), TaskState::Preparing);
    let t = f.e.proj().tasks[&f.task].clone();
    let a = f.e.proj().runs[t.facts.builder.as_ref().unwrap()]
        .assignment
        .clone();
    assert_eq!(a.hire, HireId::new("h-routine"));
    assert_eq!(t.view.round_model.as_deref(), Some("fake-next"));
}

/// The base a started review reads against, as its written prompt and the
/// task's diff give it; both cover the change from `base`.
async fn review_base_of(f: &Fixture, a: &RunAssignment, base: &str) {
    let head = a.reviews.as_deref().unwrap();
    let prompt =
        std::fs::read_to_string(f.e.cfg.paths.work.join(a.run.as_str()).join("prompt.md")).unwrap();
    assert!(
        prompt.contains(&format!("The change is `git diff {base} {head}`")),
        "{prompt}"
    );
    let (status, diff) = support::http::call(
        &f.e,
        axum::http::Method::GET,
        &format!("/tasks/{}/diff", f.task),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(diff["base"], base);
    assert_eq!(diff["head"], head);
}

/// The fix round the dispatcher starts after a review sent the head back.
async fn fix_round(f: &Fixture) -> RunAssignment {
    dispatch::step_now(&f.e, &f.task).await.unwrap();
    until_state(&f.e, &f.task, TaskState::Running).await;
    let proj = f.e.proj();
    proj.runs[proj.tasks[&f.task].facts.builder.as_ref().unwrap()]
        .assignment
        .clone()
}

/// A retry stops the review of round 1 before its verdict, so the
/// review of round 2 covers round 1 too: the whole change against the task's
/// base. A fix round after a finished review is reviewed against the head
/// that review gave its verdict on.
#[tokio::test]
async fn a_retry_during_review_has_the_next_review_cover_the_unreviewed_round() {
    let f = review_fixture(ReviewKind::Code, true, false).await;
    let first = review_worker(&f, ReviewKind::Code).await;
    review_base_of(&f, &first, &f.base).await;
    let builder = retried_into_a_builder(&f).await;
    assert_eq!(
        f.e.proj().runs[&first.run].ended,
        Some(dvb_core::event::EndReason::Stopped)
    );
    until_state(&f.e, &f.task, TaskState::Running).await;
    collect_fix(&f, &builder);
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let second = review_worker(&f, ReviewKind::Code).await;
    let round2 = second.reviews.clone().unwrap();
    assert_ne!(round2, f.first);
    review_base_of(&f, &second, &f.base).await;

    give_verdict(&f, &second, true);
    end_worker(&f, &second);
    let fix = fix_round(&f).await;
    assert_eq!(fix.base.as_deref(), Some(round2.as_str()));
    collect_fix(&f, &fix);
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let third = review_worker(&f, ReviewKind::Code).await;
    review_base_of(&f, &third, &round2).await;
}

/// A Design task's code review sends round 1 back; the code review of
/// the fix reads against round 1, but the design review, with no design
/// verdict yet, reads the whole change against the task's base.
#[tokio::test]
async fn a_design_review_after_a_code_fix_round_covers_the_whole_change() {
    let f = review_fixture(ReviewKind::Design, true, false).await;
    let code = review_worker(&f, ReviewKind::Code).await;
    review_base_of(&f, &code, &f.base).await;
    give_verdict(&f, &code, true);
    end_worker(&f, &code);
    let fix = fix_round(&f).await;
    collect_fix(&f, &fix);
    until_state(&f.e, &f.task, TaskState::InReview).await;
    let code2 = review_worker(&f, ReviewKind::Code).await;
    review_base_of(&f, &code2, &f.first).await;
    give_verdict(&f, &code2, false);
    end_worker(&f, &code2);
    let design = review_worker(&f, ReviewKind::Design).await;
    review_base_of(&f, &design, &f.base).await;
}
