//! A builder that changed nothing is never published or merged, also when it
//! collected and exited while the engine was down (the task is then Unknown and
//! reconciliation, not `run_ended`, moves it on).

mod support;
use dvb_core::forge::{Forge, Repo};
use dvb_core::launcher::Launcher;
use dvb_core::{Actor, EventKind, GoalId, ProjectId, QuestionId, RunId, TaskId, TaskState};
use dvb_engine::{dispatch, reconcile};
use dvb_forge::FakeForge;
use std::sync::Arc;
use support::git::{collect, git, seed_repository};
use support::wait::SLOW as wait;

async fn unchanged_collection(reported: bool, restart: bool, existing_question: bool) {
    unchanged_outcome(reported, restart, existing_question, None).await;
}

async fn unchanged_outcome(
    reported: bool,
    restart: bool,
    existing_question: bool,
    outcome: Option<dvb_core::run_verbs::BuilderOutcome>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeForge::local_git());
    let recorded = Arc::new(support::InterruptedForge::new(fake.clone()));
    let forge: Arc<dyn Forge> = recorded.clone();
    let launcher: Arc<dyn Launcher> = Arc::new(support::launcher::FakeLauncher {
        forbid_start: Some("no-change dispatch must not launch another builder"),
        ..Default::default()
    });
    let e = support::engine::open_with(
        &tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher: Some(launcher),
            ..Default::default()
        },
    );
    let (seed, origin) = (tmp.path().join("seed"), tmp.path().join("origin.git"));
    seed_repository(&seed, "main", Some(("file", "base\n")), "base");
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
    let repo = Repo(origin.to_string_lossy().into());
    let mirror = e.mirror("project");
    fake.clone_repo(&repo, &mirror).await.unwrap();
    let base = git(&mirror, &["rev-parse", "main"]);

    // The builder's workspace: the unchanged tree, snapshotted on the base.
    let run_dir = e.cfg.paths.work.join("b1");
    let ws = run_dir.join("ws");
    std::fs::create_dir_all(&run_dir).unwrap();
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            mirror.to_str().unwrap(),
            ws.to_str().unwrap(),
        ],
    );
    let tree = git(&ws, &["rev-parse", &format!("{base}^{{tree}}")]);
    let head = git(&ws, &["commit-tree", &tree, "-p", &base, "-m", "collected"]);
    collect(&ws, &head);
    std::fs::write(run_dir.join("head"), format!("{head}\n")).unwrap();

    let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
    let mut events = vec![
        support::events::project_event(&p, "project", &repo.0),
        support::events::goal_event(&g, "x", None),
        support::events::backend_plan_event(&g, &t),
        EventKind::GoalApproved { goal: g.clone() },
        EventKind::RunStarted(
            support::assignment::Assignment::new(RunId::new("b1"))
                .task(Some(t.clone()))
                .reason("x".into())
                .base(Some(base.clone()))
                .build(),
        ),
        EventKind::TaskStarted {
            branch_repair: false,
            retried: false,
            task: t.clone(),
            run: RunId::new("b1"),
            resume: false,
        },
        EventKind::TaskStateChanged {
            task: t.clone(),
            from: TaskState::Planned,
            to: TaskState::Running,
        },
    ];
    if reported {
        events.push(EventKind::RunCollected {
            outcome: None,
            run: RunId::new("b1"),
            head: Some(head.clone()),
            summary: None,
        });
    }
    if let Some(outcome) = &outcome {
        events.push(EventKind::RunCollected {
            run: RunId::new("b1"),
            head: None,
            summary: None,
            outcome: Some(outcome.clone()),
        });
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();

    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::ProjectPaused {}])
    })
    .unwrap();
    if existing_question {
        let version = e.proj().goals[&g].version;
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: QuestionId::new("existing"),
                subject: "Retry the failed task?".into(),
                format: dvb_core::event::QuestionFormat::QuickCompare,
                options: vec![dvb_core::event::QuestionOption {
                    id: 1,
                    text: "Retry".into(),
                    effect: "One run".into(),
                    approves_plan: false,
                    no_effect: false,
                    retries_task: Some(t.clone()),
                }],
                recommended: 1,
                lapses_on: dvb_core::event::LapsesOn::GoalVersion(version),
            }])
        })
        .unwrap();
    }
    // Paused projects keep orchestration quiet while the builder's real end is settled.
    e.remember();
    if restart {
        reconcile::mark(&e).unwrap();
        reconcile::reconcile(e.clone()).await.unwrap();
    } else {
        e.run_command(
            &RunId::new("b1"),
            dvb_engine::Decision::new("collected", dvb_core::Command::HeadWritten),
        )
        .unwrap();
        dispatch::run_ended(&e, &RunId::new("b1"), dvb_core::event::EndReason::Completed)
            .await
            .unwrap();
    }
    wait.until("the run to end", || {
        e.proj().runs[&RunId::new("b1")].ended.is_some()
    })
    .await;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::ProjectResumed {}])
    })
    .unwrap();
    for _ in 0..5 {
        dispatch::step_now(&e, &t).await.unwrap();
    }
    let state = e.proj().tasks[&t].view.state.clone();
    if let Some(outcome) = outcome {
        assert!(matches!(state, TaskState::Stopped { .. }));
        assert_eq!(e.proj().tasks[&t].view.outcome, outcome);
        assert!(
            !support::events::all(&e)
                .iter()
                .any(|ev| matches!(ev.kind, EventKind::TaskFailed { .. }))
        );
        assert!(fake.find_pr(&repo, "dvb/t-1").await.unwrap().is_none());
        return;
    }
    assert_eq!(
        state,
        TaskState::Failed {
            reason: "no changes: the run changed nothing".into()
        }
    );
    let records = support::events::all(&e);
    assert_eq!(
        records
            .iter()
            .filter(
                |ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.task.as_ref() == Some(&t))
            )
            .count(),
        1
    );
    assert!(
        !records
            .iter()
            .any(|ev| matches!(&ev.kind, EventKind::EffectRetry { .. }))
    );
    assert_eq!(records.iter().filter(|ev| matches!(&ev.kind, EventKind::TaskFailed { reason, .. } if reason.as_deref() == Some("no changes: the run changed nothing"))).count(), 1);
    let question = {
        let proj = e.proj();
        assert!(proj.tasks[&t].facts.retries.is_empty());
        assert_eq!(proj.questions.len(), 1);
        let q = proj.questions.values().next().unwrap().view.clone();
        assert!(q.open && q.subject.contains("no changes"));
        assert_eq!(
            q.options
                .iter()
                .filter(|o| o.retries_task.as_ref() == Some(&t))
                .count(),
            1
        );
        if existing_question {
            assert_eq!(q.id, QuestionId::new("existing"));
            assert_eq!(q.options[0].id, 1);
            assert_eq!(q.options[0].text, "Retry");
        }
        drop(proj);
        q
    };
    // Restart and repeated dispatch recover the same terminal result and question.
    reconcile::mark(&e).unwrap();
    reconcile::reconcile(e.clone()).await.unwrap();
    dispatch::step_now(&e, &t).await.unwrap();
    let rebuilt = dvb_engine::proj::Projections::rebuild(&support::events::all(&e));
    assert_eq!(rebuilt.tasks[&t].view.state, state);
    assert_eq!(rebuilt.questions[&question.id].view, question);
    assert_eq!(rebuilt.runs.len(), 1);
    assert!(ws.is_dir());
    assert!(!recorded.calls.lock().unwrap().contains(&"push"));
    assert_eq!(
        support::events::all(&e)
            .iter()
            .filter(|e| matches!(e.kind, EventKind::QuestionOpened { .. }))
            .count(),
        records
            .iter()
            .filter(|e| matches!(e.kind, EventKind::QuestionOpened { .. }))
            .count()
    );

    assert_eq!(
        git(std::path::Path::new(&repo.0), &["rev-parse", "main"]),
        base
    );
    let published = support::events::all(&e)
        .iter()
        .filter(|ev| ev.kind.kind() == "branch.published")
        .count();
    assert_eq!(published, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_reported_collection_is_not_published_after_a_restart() {
    unchanged_collection(true, true, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_collection_recovered_from_its_receipt_is_not_published_after_a_restart() {
    unchanged_collection(false, true, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_unchanged_collection_ends_with_one_retry_question() {
    unchanged_collection(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_failed_task_question_is_reused_for_no_changes() {
    unchanged_collection(true, false, true).await;
}

#[tokio::test]
async fn no_change_blocked_and_partial_reports_survive_lost_head_report_and_restart() {
    use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
    for outcome in [
        BuilderOutcome::Blocked {
            prerequisite: "API task has not merged".into(),
        },
        BuilderOutcome::Partial {
            reason: PartialReason::CompletionUnmet,
            detail: "verification remains unmet".into(),
        },
    ] {
        unchanged_outcome(true, false, false, Some(outcome.clone())).await;
        unchanged_outcome(false, true, false, Some(outcome)).await;
    }
}
