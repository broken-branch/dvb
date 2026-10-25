//! through the real binary with the Fake provider: a builder that writes its
//! first file and then crashes fails the task after its three attempts; the
//! goal is Failed. Three retries through the owner-approved run verb, owner API
//! and `dvb retry` carry text into the kept workspace; the last merges the task
//! with both files and completes the goal. A retry of a task that is not failed
//! is refused.

pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::{GoalState, GoalView, QuestionView, TaskView};
use dvb_core::event::{Answer, EndReason};
use dvb_core::{Actor, Event, EventId, EventKind, RefusalCode, RunId, TaskId, TaskState};
use serde_json::json;
use std::process::Command;
use support::{Engine, add_project, approve, engine_cmd, engine_up, git, seed_origin, until};

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_failed_task_is_retried_in_its_kept_workspace_and_merges() {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");
    let dir = d.join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    // Every run crashes (the disk-full case); the first leaves its edit.
    std::fs::write(
        dir.join("fake/builder.json"),
        r#"[{"write": {"path": "docs/first.md", "text": "first\n"}}, {"exit": 1}]"#,
    )
    .unwrap();
    std::fs::write(dir.join("fake/builder-resume.json"), r#"[{"exit": 1}]"#).unwrap();

    let _engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let p = add_project(&c, &origin).await;
    let goal = support::state_goal(&c, &p, "Write two notes").await;
    let q = until("the plan question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter()
            .find(|q| q.project == p.id && q.options.iter().any(|o| o.approves_plan))
    })
    .await;
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    let task = tasks[0].id.clone();
    // Approval delivery and automatic builder admission can occur in either
    // order. {{task}} selects a Planned task at wake admission, so it can be
    // empty here. Bind the ask to the task the planner actually persisted.
    // The owner still answers only after failure, authorizing one attributed retry.
    std::fs::write(dir.join("fake/orchestrator-approved.json"), serde_json::to_string(&json!([
        {"run": ["ask", {
            "subject": "Supply the missing notes format and retry?",
            "options": [{"text":"Retry", "effect":"Retry with the missing format.", "retries_task":task}],
            "recommended":1
        }]}
    ])).unwrap()).unwrap();
    approve(&c, &q).await;
    let retry = async || {
        c.post::<_, TaskView>(&format!("/tasks/{task}/retry"), &())
            .await
    };
    let goal_state = async || {
        let goals: Vec<GoalView> = c.get("/goals").await.unwrap();
        goals.into_iter().find(|g| g.id == goal.id).unwrap().state
    };

    // Not failed yet: refused.
    match retry().await {
        Err(CliError::Refused(r)) => assert_eq!(r.code, RefusalCode::ValueChanged),
        other => panic!("a retry before the failure: {other:?}"),
    }
    let (first_failure, _) = until("the task to fail", async || {
        failed_builder(&c, &task, EventId(0), EndReason::Crashed).await
    })
    .await;
    let initial = records(&c, EventId(0)).await;
    let attempts: Vec<_> = initial
        .iter()
        .filter_map(|ev| {
            if let EventKind::RunStarted(a) = &ev.kind
                && a.task.as_ref() == Some(&task)
                && a.reviews.is_none()
            {
                Some(&a.run)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(attempts.len(), 3, "three crash attempts before owner retry");
    for run in attempts {
        assert!(initial.iter().any(|ev| matches!(&ev.kind,
            EventKind::RunEnded { run: r, reason: EndReason::Crashed, .. } if r == run)));
    }
    let failed: TaskView = c.get(&format!("/tasks/{task}")).await.unwrap();
    assert!(matches!(failed.state, TaskState::Failed { .. }));
    assert_eq!(goal_state().await, GoalState::Failed);

    // Consume this approval before any direct owner retry closes its question.
    // One owner yes names this failed task; the real run verb carries its text.
    let q = until("the retry question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().find(|q| {
            q.project == p.id
                && q.options
                    .iter()
                    .any(|o| o.retries_task.as_ref() == Some(&task))
        })
    })
    .await;
    let attributed_text = "Run verb: retain the first note and add the missing second note.";
    std::fs::write(
        dir.join("fake/orchestrator-answer.json"),
        serde_json::to_string(&json!([
            {"run":["task retry", {"task":task,"text":attributed_text}]}
        ]))
        .unwrap(),
    )
    .unwrap();
    let _: QuestionView = c
        .post(&format!("/questions/{}/answer", q.id), &Answer::Choice(1))
        .await
        .unwrap();
    let (attributed_failure, attributed_run) = until("the attributed retry to fail", async || {
        failed_builder(&c, &task, first_failure, EndReason::Crashed).await
    })
    .await;
    let run_prompt = std::fs::read_to_string(
        dir.join("work")
            .join(attributed_run.as_str())
            .join("prompt.md"),
    )
    .unwrap();
    assert_retry_prompt(&run_prompt, attributed_text);

    // Owner API supplies context to a real Fake builder, which still crashes.
    let api_text = "API: the missing notes format is plain text.";
    let _: TaskView = c
        .post(&format!("/tasks/{task}/retry"), &json!({"text":api_text}))
        .await
        .unwrap();
    let (api_failure, api_run) = until("the API retry to fail", async || {
        failed_builder(&c, &task, attributed_failure, EndReason::Crashed).await
    })
    .await;
    let api_prompt =
        std::fs::read_to_string(dir.join("work").join(api_run.as_str()).join("prompt.md")).unwrap();
    assert_retry_prompt(&api_prompt, api_text);
    assert_ne!(api_run, attributed_run);
    assert!(!api_prompt.contains(attributed_text));

    // The disk is fixed: the next run succeeds.
    std::fs::write(
        dir.join("fake/builder-resume.json"),
        r#"[{"write": {"path": "docs/second.md", "text": "second\n"}}]"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(&dir)
        .args([
            "retry",
            task.as_str(),
            "CLI: use two plain text notes.",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // --json prints the retried task as status lists it: no longer failed.
    let retried: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(retried["task"]["id"], task.as_str(), "{retried}");
    assert_eq!(retried["task"]["goal"], goal.id.as_str());
    assert_eq!(retried["task"]["role_name"], "backend");
    assert_ne!(retried["task"]["state"], "failed");
    // Read while the task runs: a done task's run directories are removed.
    let (cli_started, cli_prompt) = until("the CLI retry's builder", async || {
        builder_prompt(&c, &dir, &task, api_failure).await
    })
    .await;
    until("the task to be done", async || {
        let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    // Task completion is durable; bind the prompt to this retry's own ended
    // builder, rather than a later run (the task's latest run is its reviewer).
    let cli_run = completed_builder(&records(&c, api_failure).await, &task).unwrap();
    assert_eq!(cli_run, cli_started);
    let t: TaskView = c.get(&format!("/tasks/{task}")).await.unwrap();
    assert_eq!(t.title, "Write two notes");
    assert_eq!(t.goal, failed.goal);
    assert_eq!(t.role, failed.role);
    assert_eq!(t.review, failed.review);
    // The first run's file merged with the last's: the workspace was kept.
    for f in ["docs/first.md", "docs/second.md"] {
        git(&origin, &["cat-file", "-e", &format!("main:{f}")]);
    }
    let run_dir = dir.join("work").join(cli_run.as_str());
    until("the done task's run directory to go", async || {
        (!run_dir.exists()).then_some(())
    })
    .await;
    until("the goal to be done", async || {
        records(&c, EventId(0))
            .await
            .into_iter()
            .find(|ev| matches!(&ev.kind, EventKind::GoalDone { goal: g } if g == &goal.id))
    })
    .await;
    assert_eq!(goal_state().await, GoalState::Done);

    let mut kinds = Vec::new();
    let mut retried = 0;
    let mut retry_contexts = Vec::new();
    let mut retry_questions = Vec::new();
    c.read_events(0, true, |ev: Event| {
        if let EventKind::TaskResumed { reason, .. } = &ev.kind {
            assert_eq!(reason.as_deref(), Some("retried after it failed"));
            retried += 1;
        }
        if let EventKind::TaskRetryTextSet { text, .. } = &ev.kind {
            retry_contexts.push((ev.actor.clone(), text.clone()));
        }
        if let EventKind::QuestionOpened {
            question, options, ..
        } = &ev.kind
            && options
                .iter()
                .any(|o| o.retries_task.as_ref() == Some(&task))
        {
            retry_questions.push(question.clone());
        }
        kinds.push(ev.kind.kind());
        true
    })
    .await
    .unwrap();
    assert_eq!(retried, 3, "{kinds:?}");
    assert_eq!(
        retry_questions.as_slice(),
        std::slice::from_ref(&q.id),
        "one persisted retry question"
    );
    assert_eq!(retry_contexts.len(), 3);
    assert!(matches!(retry_contexts[0].0, Actor::Run(_)));
    assert_eq!(retry_contexts[0].1.as_deref(), Some(attributed_text));
    assert_eq!(retry_contexts[1], (Actor::Owner, Some(api_text.into())));
    assert_eq!(
        retry_contexts[2],
        (Actor::Owner, Some("CLI: use two plain text notes.".into()))
    );
    assert_retry_prompt(&cli_prompt, "CLI: use two plain text notes.");
    assert!(!cli_prompt.contains(api_text));
    assert!(!cli_prompt.contains(attributed_text));
    assert_eq!(kinds.iter().filter(|k| **k == "task.failed").count(), 3);
    for k in ["pr.merged", "task.done", "goal.done"] {
        assert_eq!(
            kinds.iter().filter(|x| **x == k).count(),
            1,
            "{k} in {kinds:?}"
        );
    }
    // Done: nothing to retry.
    match retry().await {
        Err(CliError::Refused(r)) => assert_eq!(r.code, RefusalCode::AlreadyTerminal),
        other => panic!("a retry of a done task: {other:?}"),
    }
}

fn assert_retry_prompt(prompt: &str, text: &str) {
    assert!(prompt.contains(text), "{prompt}");
    assert!(prompt.contains("accompanies this retry and supplies missing information"));
    assert!(prompt.contains("does not replace the task"));
    assert!(prompt.find("## Your task").unwrap() < prompt.find("## Retry context").unwrap());
    assert!(
        prompt
            .find("A one-file edit, made by the Fake provider.")
            .unwrap()
            < prompt.find("## Retry context").unwrap()
    );
    assert!(prompt.find("## Retry context").unwrap() < prompt.find("## Fix what failed").unwrap());
}

async fn records(c: &Client, after: EventId) -> Vec<Event> {
    let mut events = Vec::new();
    c.read_events(after.0, true, |ev: Event| {
        events.push(ev);
        true
    })
    .await
    .unwrap();
    events
}

/// Select a failure after the preceding failure's immutable event id. The
/// assignment must precede this failure, and its own end must be recorded.
/// A newer retry cannot change the run selected from this one event backlog.
async fn failed_builder(
    c: &Client,
    task: &TaskId,
    after: EventId,
    expected: EndReason,
) -> Option<(EventId, RunId)> {
    let events = records(c, after).await;
    let failure = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::TaskFailed { task: t, .. } if t == task))?;
    let run = events.iter().rev().find_map(|ev| {
        if ev.id < failure.id
            && let EventKind::RunStarted(a) = &ev.kind
            && a.task.as_ref() == Some(task)
            && a.reviews.is_none()
        {
            Some(&a.run)
        } else {
            None
        }
    })?;
    let reason = events.iter().find_map(|ev| {
        if let EventKind::RunEnded { run: r, reason, .. } = &ev.kind
            && r == run
            && ev.id < failure.id
        {
            Some(reason)
        } else {
            None
        }
    })?;
    assert_eq!(*reason, expected);
    Some((failure.id, run.clone()))
}

/// The latest builder of the task started after `after`, and its prompt once
/// the launch wrote it.
async fn builder_prompt(
    c: &Client,
    dir: &std::path::Path,
    task: &TaskId,
    after: EventId,
) -> Option<(RunId, String)> {
    let run = records(c, after).await.into_iter().rev().find_map(|ev| {
        if let EventKind::RunStarted(a) = ev.kind
            && a.task.as_ref() == Some(task)
            && a.reviews.is_none()
        {
            Some(a.run)
        } else {
            None
        }
    })?;
    let run_dir = dir.join("work").join(run.as_str());
    // The Fake script is written after the whole prompt.
    if !run_dir.join("fake-script.json").exists() {
        return None;
    }
    let prompt = std::fs::read_to_string(run_dir.join("prompt.md")).ok()?;
    Some((run, prompt))
}

/// The first builder assigned after the known failure must itself complete.
/// RunStarted alone is too early: launch has not necessarily written its prompt.
fn completed_builder(events: &[Event], task: &TaskId) -> Option<RunId> {
    let run = events.iter().find_map(|ev| {
        if let EventKind::RunStarted(a) = &ev.kind
            && a.task.as_ref() == Some(task)
            && a.reviews.is_none()
        {
            Some(&a.run)
        } else {
            None
        }
    })?;
    let reason = events.iter().find_map(|ev| {
        if let EventKind::RunEnded { run: r, reason, .. } = &ev.kind
            && r == run
        {
            Some(reason)
        } else {
            None
        }
    })?;
    assert_eq!(*reason, EndReason::Completed);
    Some(run.clone())
}

/// The no-change question authorizes the existing attributed retry-text path.
/// A real Fake builder exits successfully without modifying its recorded tree.
#[tokio::test(flavor = "multi_thread")]
async fn socket_no_changes_asks_once_and_retries_only_after_the_owner_answers_with_text() {
    let tmp = tempfile::Builder::new()
        .prefix("dvbempty")
        .tempdir()
        .unwrap();
    let origin = seed_origin(tmp.path(), "main");
    let dir = tmp.path().join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    std::fs::write(dir.join("fake/builder.json"), r#"[{"exit":0}]"#).unwrap();
    std::fs::write(
        dir.join("fake/orchestrator-approved.json"),
        r#"[{"exit":0}]"#,
    )
    .unwrap();
    let engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let p = add_project(&c, &origin).await;
    let goal = support::state_goal(&c, &p, "Make a note").await;
    let plan = until("plan question", async || {
        let qs: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        qs.into_iter()
            .find(|q| q.project == p.id && q.options.iter().any(|o| o.approves_plan))
    })
    .await;
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    let planned = tasks.into_iter().find(|t| t.goal == goal.id).unwrap();
    approve(&c, &plan).await;
    let (failure, first) = until("empty builder to end", async || {
        failed_builder(&c, &planned.id, EventId(0), EndReason::Completed).await
    })
    .await;
    let task: TaskView = c.get(&format!("/tasks/{}", planned.id)).await.unwrap();
    let no_changes = TaskState::Failed {
        reason: "no changes: the run changed nothing".into(),
    };
    assert_eq!(task.state, no_changes);
    let q = until("the no-changes retry question", async || {
        let qs: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        qs.into_iter().find(|q| {
            q.project == p.id
                && q.options
                    .iter()
                    .any(|o| o.retries_task.as_ref() == Some(&task.id))
        })
    })
    .await;
    let questions: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0], q);
    assert!(q.subject.contains("no changes"));
    let retry = q
        .options
        .iter()
        .find(|o| o.retries_task.as_ref() == Some(&task.id))
        .unwrap()
        .id;
    let assert_empty_run = |events: &[dvb_core::Event]| {
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::RunStarted(a)
                        if a.task.as_ref() == Some(&task.id) && a.reviews.is_none()
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::QuestionOpened { subject, .. }
                        if subject.contains("no changes")
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|ev| matches!(
            &ev.kind,
            EventKind::EffectRetry { .. } | EventKind::BranchPublished { .. }
        )));
        let failures: Vec<_> = events
            .iter()
            .filter_map(|ev| {
                if let EventKind::TaskFailed { reason, .. } = &ev.kind {
                    Some(reason.as_deref())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(failures, [Some("no changes: the run changed nothing")]);
    };
    assert_empty_run(&records(&c, EventId(0)).await);
    // Repeat actual recovery twice; the failed task and its question stay put.
    drop(engine);
    let engine = Engine::spawn(engine_cmd(&dir));
    engine_up(&c).await;
    let recovered: TaskView = c.get(&format!("/tasks/{}", task.id)).await.unwrap();
    assert_eq!(recovered.state, no_changes);
    assert_empty_run(&records(&c, EventId(0)).await);
    let qs: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert_eq!(qs, questions);
    drop(engine);
    let _engine = Engine::spawn(engine_cmd(&dir));
    engine_up(&c).await;
    let recovered: TaskView = c.get(&format!("/tasks/{}", task.id)).await.unwrap();
    assert_eq!(recovered.state, no_changes);
    assert_empty_run(&records(&c, EventId(0)).await);
    let qs: Vec<QuestionView> = c.get("/questions?open=true").await.unwrap();
    assert_eq!(qs, questions);
    let text = "Retry context: write the note in plain text.";
    std::fs::write(
        dir.join("fake/builder-resume.json"),
        r#"[{"write":{"path":"docs/retried.md","text":"retried\n"}}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("fake/orchestrator-answer.json"),
        serde_json::to_string(&json!([
            {"run":["task retry", {"task":task.id,"text":text}]}
        ]))
        .unwrap(),
    )
    .unwrap();
    let _: QuestionView = c
        .post(
            &format!("/questions/{}/answer", q.id),
            &Answer::Choice(retry),
        )
        .await
        .unwrap();
    let (started, prompt) = until("the authorized retry's builder", async || {
        builder_prompt(&c, &dir, &task.id, failure).await
    })
    .await;
    let next = until("authorized retry builder", async || {
        completed_builder(&records(&c, failure).await, &task.id)
    })
    .await;
    assert_eq!(next, started);
    assert_ne!(next, first);
    until("retried task merges", async || {
        let t: TaskView = c.get(&format!("/tasks/{}", task.id)).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    until("the retried goal to be done", async || {
        records(&c, EventId(0))
            .await
            .into_iter()
            .find(|ev| matches!(&ev.kind, EventKind::GoalDone { goal: g } if g == &goal.id))
    })
    .await;
    let goals: Vec<GoalView> = c.get("/goals").await.unwrap();
    assert_eq!(
        goals.into_iter().find(|g| g.id == goal.id).unwrap().state,
        GoalState::Done
    );
    assert_retry_prompt(&prompt, text);
    let events = records(&c, EventId(0)).await;
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(
                &ev.kind,
                EventKind::RunStarted(a)
                    if a.task.as_ref() == Some(&task.id) && a.reviews.is_none()
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(&ev.kind, EventKind::TaskFailed { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(
                &ev.kind,
                EventKind::TaskRetryTextSet { text: Some(t), .. }
                    if t == text
            ))
            .count(),
        1
    );
    assert!(events.iter().any(|ev| matches!(&ev.kind,
        EventKind::TaskRetryTextSet { task: t, text: Some(context) }
            if t == &task.id && context == text)
        && matches!(&ev.actor, Actor::Run(_))));
    assert_eq!(events.iter().filter(|ev| matches!(&ev.kind,
        EventKind::QuestionOpened { question, options, .. }
            if question == &q.id && options.iter().any(|o| o.retries_task.as_ref() == Some(&task.id)))).count(), 1);
    let done: TaskView = c.get(&format!("/tasks/{}", task.id)).await.unwrap();
    assert_eq!(done.title, planned.title);
    assert_eq!(done.goal, planned.goal);
    assert_eq!(done.role, planned.role);
    assert_eq!(done.review, planned.review);
    git(&origin, &["cat-file", "-e", "main:docs/retried.md"]);
}
