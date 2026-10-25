mod support;

use dvb_core::event::{Answer, LapsesOn, OutputItem, QuestionFormat, QuestionKind, ResourceWait};
use dvb_core::{Actor, EventKind, GoalId, ProjectId, QuestionId, RunId, TaskId, TaskState};
use dvb_engine::proj::Projections;
use support::assignment::Assignment;

#[tokio::test]
async fn resource_metadata_controls_status_and_literal_notes_replay_without_metadata() {
    let (_dir, e) = support::engine::engine();
    let project = ProjectId::new("p");
    let task = TaskId::new("t");
    let run = RunId::new("r");
    let mut events = support::events::approved_task_events();
    events.extend([
        EventKind::TaskStarted {
            task: task.clone(),
            run: run.clone(),
            resume: false,
            branch_repair: false,
            retried: false,
        },
        EventKind::RunStarted(
            Assignment::new(run.clone())
                .task(Some(task.clone()))
                .build(),
        ),
        EventKind::TaskStateChanged {
            task: task.clone(),
            from: TaskState::Planned,
            to: TaskState::Preparing,
        },
    ]);
    e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    for (record, expected) in [
        (
            EventKind::RunOutput {
                run: run.clone(),
                resource: Some(ResourceWait::HostLoad),
                item: OutputItem::Note("deliberately unrelated text".into()),
            },
            Some("Waiting for host load"),
        ),
        (serde_json::from_str(r#"{"kind":"run.output","payload":{"run":"r","item":{"note":"waiting for disk space: 1 GB free, needs 25 GB"}}}"#).unwrap(), Some("Waiting for disk")),
        (serde_json::from_str(r#"{"kind":"run.output","payload":{"run":"r","item":{"note":"Resources available."}}}"#).unwrap(), None),
        (
            EventKind::RunOutput {
                run: run.clone(),
                resource: Some(ResourceWait::Memory),
                item: OutputItem::Note("localized notice".into()),
            },
            Some("Waiting for memory"),
        ),
        (
            EventKind::TaskReasonChanged {
                run,
                task: task.clone(),
                resource: Some(ResourceWait::Available),
                reason: Some("ignored owner prose".into()),
            },
            None,
        ),
    ] {
        let saved = e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(vec![record])).unwrap();
        if matches!(saved[0].kind, EventKind::RunOutput { resource: Some(ResourceWait::HostLoad), .. }) {
            let notice = "Waiting for host load: deliberately unrelated text";
            assert_eq!(dvb_core::wording::event_text(&saved[0].kind, |_| String::new()).as_deref(), Some(notice));
            let (status, body) = support::http::request(&e, axum::http::Method::GET, "/runs/r/output", None).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            let output: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(output.as_array().unwrap().last().unwrap()["item"]["note"], notice);
        }
        assert_eq!(e.proj().tasks[&task].view.reason.as_deref(), expected);
        let replay = Projections::rebuild(&support::events::all(&e));
        assert_eq!(replay.tasks[&task].view.reason.as_deref(), expected);
    }
}

#[test]
fn question_metadata_overrides_prefixes_and_recovers_fetch_attempts_without_a_run() {
    for historical in [false, true] {
        let (_dir, e) = support::engine::engine();
        let project = ProjectId::new("p");
        let goal = GoalId::new("g");
        e.append_checked(Some(&project), Actor::Owner, |_, _| {
            Ok(vec![
                support::events::project_event(&project, "p", "/offline"),
                support::events::goal_event(&goal, "Plan", None),
            ])
        })
        .unwrap();
        let version = e.proj().goals[&goal].version;
        let ordinary = EventKind::QuestionOpened {
            kind: Some(QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("planning-arbitrary"),
            subject: "The orchestrator stopped: owner supplied prose".into(),
            format: QuestionFormat::Simple,
            options: vec![],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(version),
        };
        e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(vec![ordinary]))
            .unwrap();
        assert_eq!(
            e.proj().questions[&QuestionId::new("planning-arbitrary")].kind,
            QuestionKind::Ordinary
        );
        assert!(!e.proj().goals[&goal].planning_held);
        for (id, kind, subject) in [
            (
                "typed-failure",
                Some(QuestionKind::OrchestratorFailed),
                "localized recovery",
            ),
            ("historical-failure", None, "The orchestrator stopped twice"),
        ] {
            let record = serde_json::json!({"kind":"question.opened", "payload":{
                "kind":kind, "question":id, "subject":subject, "format":"simple", "options":[],
                "recommended":1, "lapses_on":{"goal_version":version}
            }});
            e.append_checked(Some(&project), Actor::Engine, |_, _| {
                Ok(vec![serde_json::from_value(record).unwrap()])
            })
            .unwrap();
            let replay = Projections::rebuild(&support::events::all(&e));
            assert_eq!(
                replay.questions[&QuestionId::new(id)].kind,
                QuestionKind::OrchestratorFailed
            );
        }
        let attempt = if historical {
            "fetch-old"
        } else {
            "opaque-attempt"
        };
        let id = if historical {
            format!("planning-{version}-{attempt}")
        } else {
            "opaque-question".into()
        };
        let mut record = serde_json::json!({"kind":"question.opened", "payload":{
            "question":id, "subject":"Unrelated localized subject", "format":"simple", "options":[],
            "recommended":1, "lapses_on":{"goal_version":version}
        }});
        if !historical {
            record["payload"]["kind"] = serde_json::to_value(QuestionKind::PlanningHold {
                attempt: RunId::new(attempt),
                fetch: true,
            })
            .unwrap();
        }
        let record: EventKind = serde_json::from_value(record).unwrap();
        e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(vec![record]))
            .unwrap();
        let replay = Projections::rebuild(&support::events::all(&e));
        assert!(replay.goals[&goal].planning_held);
        assert_eq!(replay.goals[&goal].planning_run, Some(RunId::new(attempt)));
        assert!(replay.runs.is_empty());
        assert!(replay.questions[&QuestionId::new(&id)].view.open);
        e.append_checked(Some(&project), Actor::Owner, |_, _| {
            Ok(vec![EventKind::QuestionAnswered {
                question: QuestionId::new(&id),
                answer: Answer::Choice(1),
            }])
        })
        .unwrap();
        let replay = Projections::rebuild(&support::events::all(&e));
        assert!(!replay.goals[&goal].planning_held);
        assert!(!replay.questions[&QuestionId::new(&id)].view.open);
    }
}

#[tokio::test]
async fn publication_and_update_failures_retain_their_sources() {
    use std::error::Error;
    let dir = tempfile::tempdir().unwrap();
    let error = dvb_engine::publish::create_mirror(
        "/missing-offline-repository",
        &dir.path().join("mirror"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, dvb_engine::InternalError::Git(_)));
    assert!(error.source().is_some());
    let failure = dvb_engine::Fail::context("publication", error);
    assert!(
        failure
            .source()
            .unwrap()
            .source()
            .unwrap()
            .source()
            .is_some()
    );
    let error =
        dvb_engine::update::fetch::verify_signature("invalid", b"sum", "invalid").unwrap_err();
    assert!(matches!(error, dvb_engine::InternalError::Message(_)));
    let error =
        dvb_engine::update::fetch::verify_signature("key\ninvalid", b"sum", "invalid").unwrap_err();
    assert!(error.source().is_some());
}
