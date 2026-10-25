//! The walking skeleton in dev mode: one fake task from goal to merge through the
//! real binary, engine, store, runs, Fake provider and Fake forge (a local bare
//! repo). `scripts/run-e2e.sh` checks the same on an install.

pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{QuestionView, Settings, TaskView};
use dvb_core::event::QuestionFormat;
use dvb_core::roles::ReviewKind;
use dvb_core::{Event, EventKind, RefusalCode, TaskState};
use support::{
    Engine, add_project, approve, engine_cmd, engine_up, git, run_prompts, seed_origin, state_goal,
    until,
};

#[tokio::test(flavor = "multi_thread")]
async fn socket_one_fake_task_goes_from_goal_to_merge() {
    skeleton("main", ReviewKind::Code).await;
}

/// A repo whose default branch is not `main`: the PR targets and merges into it.
#[tokio::test(flavor = "multi_thread")]
async fn socket_a_trunk_default_branch_gets_the_merge() {
    skeleton("trunk", ReviewKind::Code).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_design_task_runs_both_fake_reviewers_before_merge() {
    skeleton("main", ReviewKind::Design).await;
}

async fn skeleton(default: &str, review: ReviewKind) {
    // Short: a socket path must fit in 108 bytes.
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, default);
    let seeded = git(&origin, &["rev-parse", default]);

    let dir = d.join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    let mut script: serde_json::Value = serde_json::from_str(include_str!(
        "../../dvb-engine/src/fake_scripts/orchestrator-goal.json"
    ))
    .unwrap();
    if review == ReviewKind::Design {
        script[0]["run"][1]["tasks"][0]["review"] = serde_json::json!("design");
    }
    std::fs::write(
        dir.join("fake/orchestrator-goal.json"),
        serde_json::to_string(&script).unwrap(),
    )
    .unwrap();
    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;

    let p = add_project(&c, &origin).await;
    assert_eq!(p.default_branch, default);
    // The running engine publishes as a changed identity: no restart.
    let publisher = if default == "trunk" {
        let changed: dvb_core::api::PublishingIdentity = c
            .put(
                "/settings/publication",
                &dvb_core::api::CommitIdentity {
                    name: "publisher".into(),
                    email: "publisher@example.invalid".into(),
                },
            )
            .await
            .unwrap();
        assert!(!changed.is_default);
        "publisher <publisher@example.invalid>"
    } else {
        "dvb <dvb@dvb.invalid>"
    };
    let format = if default == "main" {
        QuestionFormat::Simple
    } else {
        QuestionFormat::Swot
    };
    let settings: Settings =
        serde_json::from_value(serde_json::json!({ "values": { "question_format": format } }))
            .unwrap();
    let _: Settings = c.put("/settings", &settings).await.unwrap();
    let prompts = run_prompts(&c, &dir.join("work"));
    state_goal(&c, &p, "Add the build version to dvb --version").await;
    let q = until("the plan question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().next()
    })
    .await;
    assert_eq!(
        q.format, format,
        "the Fake orchestrator uses the owner default"
    );
    let value = serde_json::to_value(format).unwrap();
    until("the planner's captured prompt", async || {
        prompts
            .lock()
            .unwrap()
            .values()
            .any(|prompt| {
                prompt.contains(&format!(
                    "default question format is `{}`",
                    value.as_str().unwrap()
                ))
            })
            .then_some(())
    })
    .await;
    approve(&c, &q).await;

    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    let task = tasks[0].id.clone();
    until("the task to be done", async || {
        let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    until("the next question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        (open.len() == 1 && open[0].id != q.id).then_some(())
    })
    .await;

    let mut kinds = Vec::new();
    let mut refused = Vec::new();
    let mut reviews = Vec::new();
    let mut verdicts = Vec::new();
    c.read_events(0, true, |ev: Event| {
        if let EventKind::CommandRefused(r) = &ev.kind {
            refused.push((r.command.clone(), r.refusal.current().unwrap().code.clone()));
        }
        if let EventKind::RunStarted(a) = &ev.kind
            && a.task.as_ref() == Some(&task)
            && let Some(stage) = a.review_stage
        {
            reviews.push(stage);
        }
        if let EventKind::VerdictRecorded {
            review_stage: Some(stage),
            task: reviewed,
            findings,
            ..
        } = &ev.kind
            && reviewed == &task
        {
            assert!(!findings.iter().any(|finding| finding.open));
            verdicts.push(*stage);
        }
        if let EventKind::PrMerged { task: merged, .. } = &ev.kind
            && merged == &task
        {
            assert_eq!(reviews, verdicts);
            assert_eq!(
                verdicts,
                if review == ReviewKind::Design {
                    vec![ReviewKind::Code, ReviewKind::Design]
                } else {
                    vec![ReviewKind::Code]
                }
            );
        }
        kinds.push(ev.kind.kind());
        true
    })
    .await
    .unwrap();
    assert_eq!(
        refused.first(),
        Some(&("task start".to_owned(), RefusalCode::GoalNotApproved))
    );
    // Automatic admission can win before the approved orchestrator wake, or
    // refuse its racing start. Alias validation is exercised when it wakes.
    assert!(
        refused
            .iter()
            .skip(1)
            .all(|(command, code)| command == "task start"
                && matches!(
                    code,
                    RefusalCode::ModelNotInPool | RefusalCode::ValueChanged
                )),
        "{refused:?}"
    );
    for k in [
        "task.started",
        "branch.published",
        "pr.opened",
        "verdict.recorded",
        "pr.merged",
        "task.done",
        "goal.done",
    ] {
        assert_eq!(
            kinds.iter().filter(|x| **x == k).count(),
            if k == "verdict.recorded" && review == ReviewKind::Design {
                2
            } else {
                1
            },
            "{k} in {kinds:?}"
        );
    }

    assert_eq!(
        git(
            &origin,
            &["rev-list", "--count", &format!("{seeded}..{default}")]
        ),
        "1"
    );
    assert_eq!(
        git(
            &origin,
            &["log", "-1", "--format=%an <%ae> %cn <%ce>", default]
        ),
        format!("{publisher} {publisher}")
    );
    git(
        &origin,
        &["cat-file", "-e", &format!("{default}:docs/skeleton.md")],
    );
    // Only the task retains a branch alongside the default; setup pushes none.
    let refs = git(
        &origin,
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(refs.lines().count(), 2, "{refs}");
    assert!(
        refs.lines()
            .any(|reference| reference == format!("refs/heads/{default}")),
        "{refs}"
    );
    assert_eq!(
        refs.lines()
            .filter(|reference| reference.starts_with("refs/heads/dvb/"))
            .count(),
        1,
        "{refs}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_structured_incomplete_results_survive_driver_collection_and_restart_without_publication()
 {
    use dvb_core::api::Status;
    use dvb_core::event::EndReason;
    use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
    use serde_json::json;
    for outcome in [
        BuilderOutcome::Blocked {
            prerequisite: "Backend API task has not merged".into(),
        },
        BuilderOutcome::Partial {
            reason: PartialReason::CompletionUnmet,
            detail: "Required verify passes remain unmet".into(),
        },
        BuilderOutcome::Partial {
            reason: PartialReason::OutOfRole,
            detail: "Needs another role's API work".into(),
        },
    ] {
        let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
        let origin = seed_origin(tmp.path(), "main");
        let dir = tmp.path().join("e");
        std::fs::create_dir_all(dir.join("fake")).unwrap();
        let mut steps = vec![];
        if matches!(
            outcome,
            BuilderOutcome::Partial {
                reason: PartialReason::OutOfRole,
                ..
            }
        ) {
            steps.push(json!({"write":{"path":"retained.txt","text":"Preserved role work\n"}}));
        }
        steps.push(json!({"run":["collected",{"summary":"Refs #323, #283, #324. Incomplete work kept.","outcome":outcome}]}));
        std::fs::write(dir.join("fake/builder.json"), json!(steps).to_string()).unwrap();
        let engine = Engine::spawn(engine_cmd(&dir));
        let c = Client::new(dir.join("engine.sock"));
        engine_up(&c).await;
        let project = add_project(&c, &origin).await;
        let base = git(&origin, &["rev-parse", "main"]);
        state_goal(&c, &project, "Build the assigned API").await;
        let question = until("plan approval", async || {
            c.get::<Vec<QuestionView>>("/questions?open=true")
                .await
                .ok()?
                .into_iter()
                .find(|q| q.options.iter().any(|o| o.approves_plan))
        })
        .await;
        approve(&c, &question).await;
        let task = until("incomplete builder to stop", async || {
            c.get::<Vec<TaskView>>(&format!("/tasks?project={}", project.id))
                .await
                .ok()?
                .into_iter()
                .find(|t| {
                    matches!(
                        t.state,
                        TaskState::Stopped { .. } | TaskState::Failed { .. } | TaskState::Done
                    )
                })
        })
        .await;
        until("Fake workers to finish", async || {
            let status: Status = c.get("/status").await.ok()?;
            (status.running == 0).then_some(())
        })
        .await;
        assert!(matches!(task.state, TaskState::Stopped { .. }));
        assert_eq!(task.outcome, outcome);
        assert!(task.pr.is_none());
        assert_eq!(
            task.current_run.as_ref().unwrap().ended,
            Some(EndReason::Completed)
        );
        assert_eq!(task.current_run.as_ref().unwrap().outcome, outcome);
        drop(engine);
        let _restarted = Engine::spawn(engine_cmd(&dir));
        engine_up(&c).await;
        let replayed: TaskView = c.get(&format!("/tasks/{}", task.id)).await.unwrap();
        assert!(matches!(replayed.state, TaskState::Stopped { .. }));
        assert_eq!(replayed.outcome, outcome);
        assert!(replayed.pr.is_none());
        let run_id = task.current_run.as_ref().unwrap().id.clone();
        let mut collected = false;
        c.read_events(0, true, |event: Event| {
            if let EventKind::RunCollected {
                run, head: Some(_), ..
            } = &event.kind
                && *run == run_id
            {
                collected = true;
            }
            let task_id = &task.id;
            assert!(!matches!(&event.kind, EventKind::TaskFailed { task, .. } if task == task_id));
            assert!(!matches!(&event.kind, EventKind::PrOpened { task, .. } if task == task_id));
            true
        })
        .await
        .unwrap();
        assert!(
            collected,
            "the real driver must collect even an empty blocked tree"
        );
        assert_eq!(git(&origin, &["rev-parse", "main"]), base);
        let questions: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
        let completion_questions = questions
            .iter()
            .filter(|q| {
                matches!(
                    q.kind,
                    Some(dvb_core::event::QuestionKind::Completion { .. })
                )
            })
            .count();
        assert_eq!(
            completion_questions,
            usize::from(matches!(
                outcome,
                BuilderOutcome::Partial {
                    reason: PartialReason::CompletionUnmet,
                    ..
                }
            ))
        );
    }
}

/// A builder that saves its summary but no outcome keeps the historical
/// complete behavior end to end: published, reviewed and merged.
#[tokio::test(flavor = "multi_thread")]
async fn socket_a_builder_reporting_no_outcome_completes_and_merges() {
    use dvb_core::run_verbs::BuilderOutcome;
    use serde_json::json;
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let origin = seed_origin(tmp.path(), "main");
    let seeded = git(&origin, &["rev-parse", "main"]);
    let dir = tmp.path().join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    let summary = "Wrote the notes. Checked them by reading.";
    std::fs::write(
        dir.join("fake/builder.json"),
        json!([
            {"write":{"path":"docs/notes.md","text":"Notes\n"}},
            {"run":["collected",{"summary":summary}]}
        ])
        .to_string(),
    )
    .unwrap();
    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let project = add_project(&c, &origin).await;
    state_goal(&c, &project, "Write the notes").await;
    let question = until("plan approval", async || {
        c.get::<Vec<QuestionView>>("/questions?open=true")
            .await
            .ok()?
            .into_iter()
            .find(|q| q.options.iter().any(|o| o.approves_plan))
    })
    .await;
    approve(&c, &question).await;
    let task = until("the task to be done", async || {
        c.get::<Vec<TaskView>>(&format!("/tasks?project={}", project.id))
            .await
            .ok()?
            .into_iter()
            .find(|t| {
                matches!(
                    t.state,
                    TaskState::Done | TaskState::Failed { .. } | TaskState::Stopped { .. }
                )
            })
    })
    .await;
    assert_eq!(task.state, TaskState::Done);
    assert_eq!(task.outcome, BuilderOutcome::Complete);
    let mut saved = Vec::new();
    let mut merged = 0;
    c.read_events(0, true, |event: Event| {
        if let EventKind::RunCollected {
            summary: Some(s),
            outcome,
            ..
        } = &event.kind
        {
            saved.push((s.clone(), outcome.clone()));
        }
        if matches!(&event.kind, EventKind::PrMerged { task: t, .. } if *t == task.id) {
            merged += 1;
        }
        true
    })
    .await
    .unwrap();
    assert_eq!(saved, vec![(summary.to_owned(), None)]);
    assert_eq!(merged, 1);
    assert_eq!(
        git(
            &origin,
            &["rev-list", "--count", &format!("{seeded}..main")]
        ),
        "1"
    );
    git(&origin, &["cat-file", "-e", "main:docs/notes.md"]);
}
