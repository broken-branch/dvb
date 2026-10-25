mod support;

use dvb_core::event::{LapsesOn, QuestionFormat, QuestionOption};
use dvb_core::{Actor, Command, EventKind, ProjectId, QuestionId, TaskId, TaskState};
use dvb_engine::{Config, Engine};

#[tokio::test]
async fn retry_completion_closes_every_failure_question_in_the_done_transaction_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::dev(dir.path(), "unused-dvb".into());
    let e = Engine::open(config.clone(), Default::default()).unwrap();
    let project = ProjectId::new("p");
    let task = TaskId::new("t");
    e.append_checked(Some(&project), Actor::Owner, |_, _| {
        Ok(support::events::approved_task_events())
    })
    .unwrap();
    let version = e.proj().goals[&dvb_core::GoalId::new("g")].version;
    let question = |id: &str, retry: Option<TaskId>| EventKind::QuestionOpened {
        kind: Some(dvb_core::event::QuestionKind::Ordinary),
        main_ci: None,
        task: None,
        question: QuestionId::new(id),
        subject: id.into(),
        format: QuestionFormat::Simple,
        options: vec![QuestionOption {
            id: 1,
            text: "Retry".into(),
            effect: "Retry".into(),
            approves_plan: false,
            no_effect: false,
            retries_task: retry,
        }],
        recommended: 1,
        lapses_on: LapsesOn::GoalVersion(version),
    };
    let failed = TaskState::Failed {
        reason: "offline failure".into(),
    };
    // Historical cap questions have no retry option; the durable failure just
    // before their opening supplies the task binding on replay.
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: failed.clone(),
            },
            EventKind::TaskFailed {
                task: task.clone(),
                reason: Some("offline failure".into()),
            },
            question("cap", None),
        ])
    })
    .unwrap();
    e.append_checked(Some(&project), Actor::Owner, |_, _| {
        Ok(vec![
            question("retry-one", Some(task.clone())),
            question("retry-two", Some(task.clone())),
            question("other-task", Some(TaskId::new("other"))),
            question("unrelated", None),
        ])
    })
    .unwrap();
    let mut approval = question("approval", None);
    if let EventKind::QuestionOpened { options, .. } = &mut approval {
        options[0].approves_plan = true;
    }
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::TaskFailed {
                task: task.clone(),
                reason: Some("offline failure".into()),
            },
            approval,
        ])
    })
    .unwrap();
    // Restore before completion to prove the historical association is replayed.
    drop(e);
    let e = Engine::open(config.clone(), Default::default()).unwrap();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskStateChanged {
            task: task.clone(),
            from: failed,
            to: TaskState::Merging,
        }])
    })
    .unwrap();
    let before = e.store.last_id().unwrap();
    assert_eq!(
        e.decide(
            e.proj(),
            &task,
            Actor::Engine,
            dvb_engine::Decision::new("merge", Command::MergeDone),
            |_| Ok(())
        )
        .unwrap(),
        TaskState::Done
    );
    let events = e.store.after(before).unwrap();
    let closed: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::QuestionClosed {
                question,
                task: closed_task,
            } = &e.kind
            {
                assert_eq!(closed_task, &task);
                Some(question.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(closed, ["cap", "retry-one", "retry-two"]);
    assert!(events.iter().all(|event| event.at == events[0].at));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::QuestionAnswered { .. }))
    );
    let end = e.store.last_id().unwrap();
    e.decide(
        e.proj(),
        &task,
        Actor::Engine,
        dvb_engine::Decision::new("merge", Command::MergeDone),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(e.store.last_id().unwrap(), end);
    drop(e);
    let replay = Engine::open(config, Default::default()).unwrap();
    let proj = replay.proj();
    for id in ["cap", "retry-one", "retry-two"] {
        let q = &proj.questions[&QuestionId::new(id)];
        assert!(!q.view.open);
        assert!(q.view.answer.is_none());
    }
    for id in ["unrelated", "other-task", "approval"] {
        assert!(proj.questions[&QuestionId::new(id)].view.open);
    }
    assert_eq!(proj.tasks[&task].view.state, TaskState::Done);
    assert!(support::events::all(&replay).len() > events.len());
}

#[test]
fn explicit_failure_task_survives_nonadjacent_events_and_historical_replay() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), "unused-dvb".into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    let p = ProjectId::new("p");
    let t = TaskId::new("t");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(support::events::approved_task_events())
    })
    .unwrap();
    let version = e.proj().goals[&dvb_core::GoalId::new("g")].version;
    let question: EventKind = serde_json::from_value(serde_json::json!({
        "kind": "question.opened", "payload": {"question": "explicit", "task": "t", "subject": "cap", "format": "simple", "options": [], "recommended": 1, "lapses_on": {"goal_version": version}}
    })).unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::TaskFailed {
                task: t.clone(),
                reason: None,
            },
            EventKind::ProjectSeen {},
            question,
        ])
    })
    .unwrap();
    assert_eq!(
        e.proj().questions[&QuestionId::new("explicit")].failure_task,
        Some(t.clone())
    );
    drop(e);
    let replay = Engine::open(cfg, Default::default()).unwrap();
    assert_eq!(
        replay.proj().questions[&QuestionId::new("explicit")].failure_task,
        Some(t)
    );
}
