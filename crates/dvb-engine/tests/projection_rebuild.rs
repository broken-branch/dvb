//! Rebuild run attribution and verdict evidence from the real event store.
mod support;

use dvb_core::event::{EndReason, PlannedTask, open_findings};
use dvb_core::ids::{GoalId, HireId, RoleId, TaskId, VerdictId};
use dvb_core::model::{Effort, Vendor};
use dvb_core::roles::ReviewKind;
use dvb_core::run_verbs::{Finding, Severity, VerdictBody};
use dvb_core::{Actor, Event, EventId, EventKind, ProjectId, RunAssignment, RunId, Timestamp};
use dvb_engine::proj::Projections;
use dvb_store::{NewEvent, Store};
use std::num::NonZeroU32;

// Reopen SQLite so the assertions cover the persisted event representation.
fn log(kinds: Vec<(Actor, EventKind)>) -> Vec<Event> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let store = Store::open(&path).unwrap();
    for (i, (actor, kind)) in kinds.into_iter().enumerate() {
        store
            .append(
                Timestamp(1_000 + i as i64),
                vec![NewEvent {
                    actor,
                    project: Some(ProjectId::new("p")),
                    kind,
                    evidence: None,
                }],
            )
            .unwrap();
    }
    drop(store);
    Store::open(&path).unwrap().after(EventId(0)).unwrap()
}

fn assignment(run: &str, task: Option<&str>, role: &str, reviews: Option<&str>) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .task(task.map(TaskId::new))
        .role(RoleId::new(role))
        .hire(HireId::new(format!("h-{role}")))
        .reason("skeleton".into())
        .reviews(reviews.map(str::to_owned))
        .build()
}

#[test]
fn an_orchestrator_run_before_planning_is_rebuilt() {
    let events = log(vec![
        (
            Actor::Owner,
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "x".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        (
            Actor::Engine,
            EventKind::RunStarted(assignment("r1", None, "orchestrator", None)),
        ),
        (
            Actor::Run(RunId::new("r1")),
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: TaskId::new("t"),
                    title: "t".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "s".into(),
                    routine: false,
                }],
            },
        ),
        (
            Actor::Engine,
            EventKind::RunEnded {
                run: RunId::new("r1"),
                reason: EndReason::Completed,
                detail: None,
            },
        ),
    ]);
    let proj = Projections::rebuild(&events);
    let run = &proj.runs[&RunId::new("r1")];
    assert_eq!(run.project, ProjectId::new("p"));
    assert_eq!(run.goal_version, Some(events[0].id));
    let assignment = &run.assignment;
    assert_eq!(assignment.task, None);
    assert_eq!(assignment.role, RoleId::new("orchestrator"));
    assert_eq!(assignment.hire, HireId::new("h-orchestrator"));
    assert_eq!(
        (
            assignment.model.as_str(),
            assignment.vendor,
            assignment.effort
        ),
        ("fake-1", Vendor::Fake, Effort::Medium)
    );
    assert_eq!(events[1].at, Timestamp(1_001));
    assert_eq!(events[2].actor, Actor::Run(RunId::new("r1")));
    assert_eq!(run.ended, Some(EndReason::Completed));
}

#[test]
fn a_reviewer_run_and_its_blocking_verdict_are_rebuilt_whole() {
    let body: VerdictBody = serde_json::from_str(
        r#"{"findings": [
            {"severity": "high", "file": "src/a.rs", "line": 42, "title": "Push before stop check",
             "detail": "Check stopped in the same transaction.", "open": true},
            {"severity": "low", "file": "src/b.rs", "line": null, "title": "Naming",
             "detail": "Fine as is.", "open": false}
        ], "summary": "One blocking finding."}"#,
    )
    .unwrap();
    let events = log(vec![
        (
            Actor::Owner,
            support::events::goal_event(&GoalId::new("g"), "Review", None),
        ),
        (
            Actor::Engine,
            support::events::backend_plan_event(&GoalId::new("g"), &TaskId::new("t")),
        ),
        (
            Actor::Engine,
            EventKind::RunStarted(assignment("r9", Some("t"), "code-reviewer", Some("abc123"))),
        ),
        (
            Actor::Run(RunId::new("r9")),
            EventKind::VerdictRecorded {
                review_stage: None,
                verdict: VerdictId::new("v1"),
                task: TaskId::new("t"),
                head: "abc123".into(),
                run: RunId::new("r9"),
                role: RoleId::new("code-reviewer"),
                model: "fake-1".into(),
                vendor: Vendor::Fake,
                findings: body.findings.clone(),
                summary: body.summary.clone(),
            },
        ),
    ]);

    let proj = Projections::rebuild(&events);
    let run = &proj.runs[&RunId::new("r9")];
    assert_eq!(
        proj.tasks[&TaskId::new("t")].facts.verdict,
        Some(dvb_core::state::Verdict {
            head: "abc123".into(),
            open: 1,
            run: RunId::new("r9")
        })
    );
    assert!(run.verdict_given);
    assert_eq!(run.project, ProjectId::new("p"));
    assert_eq!(run.assignment.reviews.as_deref(), Some("abc123"));
    assert_eq!(run.assignment.task, Some(TaskId::new("t")));
    assert_eq!(run.assignment.role, RoleId::new("code-reviewer"));
    assert_eq!(run.assignment.hire, HireId::new("h-code-reviewer"));

    assert_eq!(events[3].actor, Actor::Run(RunId::new("r9")));
    let EventKind::VerdictRecorded {
        review_stage: _,
        verdict,
        task,
        head,
        run,
        role,
        model,
        vendor,
        findings,
        summary,
    } = &events[3].kind
    else {
        panic!("expected the recorded verdict")
    };
    assert_eq!(verdict, &VerdictId::new("v1"));
    assert_eq!(task, &TaskId::new("t"));
    assert_eq!(findings, &body.findings);
    assert_eq!(summary, &body.summary);
    assert_eq!(open_findings(findings), 1);
    assert_eq!(
        (run.as_str(), role.as_str(), head.as_str()),
        ("r9", "code-reviewer", "abc123")
    );
    assert_eq!((model.as_str(), *vendor), ("fake-1", Vendor::Fake));
    assert_eq!(
        findings[0],
        Finding {
            severity: Severity::High,
            file: "src/a.rs".into(),
            line: NonZeroU32::new(42),
            title: "Push before stop check".into(),
            detail: "Check stopped in the same transaction.".into(),
            open: true,
        }
    );
}

/// Captured pre-priority event shapes: two approved goals, a usable backend
/// hire for the first and no frontend hire for the second. The base attributed
/// the surviving orchestrator to EventId(3); new admission selects EventId(6).
#[test]
fn historical_orchestrator_binding_survives_cross_version_replay() {
    let historical: Vec<Event> =
        serde_json::from_str(include_str!("fixtures/legacy_orchestrator_binding.json")).unwrap();
    let events = log(historical
        .into_iter()
        .map(|ev| (ev.actor, ev.kind))
        .collect());
    let proj = Projections::rebuild(&events);
    assert_eq!(
        proj.goal_for_wake(
            &ProjectId::new("p"),
            Some(dvb_engine::wake::Wake::Approved),
            &dvb_core::ModelPool {
                models: vec![dvb_core::PoolModel {
                    id: "fake-1".into(),
                    vendor: dvb_core::Vendor::Fake,
                    efforts: vec![dvb_core::Effort::Medium]
                }]
            }
        )
        .unwrap()
        .version,
        EventId(6),
        "new wakes prioritize the missing hire"
    );
    let run = &proj.runs[&RunId::new("historical")];
    assert_eq!(run.goal_version, Some(EventId(3)));
    assert_eq!(run.assignment.goal_version, None);
    // Historical payloads remain unchanged when read and serialized again.
    assert!(
        serde_json::to_value(&run.assignment)
            .unwrap()
            .get("goal_version")
            .is_none()
    );

    let mut current = run.assignment.clone();
    current.run = RunId::new("current");
    current.goal_version = Some(EventId(6));
    let mut kinds: Vec<_> = events.into_iter().map(|ev| (ev.actor, ev.kind)).collect();
    kinds.push((Actor::Engine, EventKind::RunStarted(current)));
    let replayed = Projections::rebuild(&log(kinds));
    assert_eq!(
        replayed.runs[&RunId::new("historical")].goal_version,
        Some(EventId(3))
    );
    assert_eq!(
        replayed.runs[&RunId::new("current")].goal_version,
        Some(EventId(6))
    );
}

/// Earlier releases left failed tasks failed after goal.dropped. Replay derives
/// cancellation without replacing the durable fail payload or adding events.
#[test]
fn legacy_fail_then_drop_derives_cancelled_without_rewriting_records() {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/legacy_fail_then_drop.json")).unwrap();
    let historical: Vec<Event> = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(&historical).unwrap(), raw);
    let dir = tempfile::tempdir().unwrap();
    let cfg = dvb_engine::Config::dev(dir.path(), "unused-dvb".into());
    std::fs::create_dir_all(&cfg.paths.state).unwrap();
    let path = cfg.paths.state.join("dvb.sqlite");
    drop(Store::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    // Insert earlier-version JSON directly, with its own whitespace; any
    // decode/re-encode rewrite would be visible in the byte comparisons.
    for ev in raw.as_array().unwrap() {
        conn.execute("INSERT INTO events (id, at, actor, project, kind, payload, evidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)", rusqlite::params![ev["id"].as_i64().unwrap(), ev["at"].to_string(), ev["actor"].to_string(), ev["project"].as_str().unwrap(), ev["kind"].as_str().unwrap(), serde_json::to_string_pretty(&ev["payload"]).unwrap()]).unwrap();
    }
    let rows = || {
        conn.prepare("SELECT json_array(id, at, actor, project, kind, payload, evidence) FROM events WHERE id <= ?1 ORDER BY id").unwrap()
        .query_map([i64::try_from(historical.len()).unwrap()], |row| row.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    let bytes = rows();
    let e = dvb_engine::Engine::open(cfg.clone(), Default::default()).unwrap();
    let events = support::events::all(&e);
    let before = serde_json::to_value(&events).unwrap();
    let proj = e.proj();
    let failed = &proj.tasks[&TaskId::new("failed")];
    assert_eq!(failed.view.state, dvb_core::TaskState::Cancelled);
    assert_eq!(failed.view.reason.as_deref(), Some("its goal was dropped"));
    assert!(failed.failed.is_some());
    let kept = &proj.tasks[&TaskId::new("kept-failed")].view;
    assert!(
        matches!(&kept.state, dvb_core::TaskState::Failed { reason } if reason == "push failed: refused")
    );
    assert_eq!(kept.reason, None);
    for t in ["stopped", "kept-stopped"] {
        let view = &proj.tasks[&TaskId::new(t)].view;
        assert_eq!(
            view.state,
            dvb_core::TaskState::Stopped {
                was: Box::new(dvb_core::TaskState::Running)
            }
        );
        assert_eq!(view.reason, None);
    }
    assert_eq!(
        proj.tasks[&TaskId::new("stopped-planned")].view.state,
        dvb_core::TaskState::Stopped {
            was: Box::new(dvb_core::TaskState::Planned)
        }
    );
    assert_eq!(serde_json::to_value(&events).unwrap(), before);
    assert_eq!(rows(), bytes);
    drop(proj);
    drop(e);
    let reopened = dvb_engine::Engine::open(cfg, Default::default()).unwrap();
    assert_eq!(
        reopened.proj().tasks[&TaskId::new("failed")].view.state,
        dvb_core::TaskState::Cancelled
    );
    assert_eq!(rows(), bytes);
}

#[test]
fn work_facts_survive_replay_and_ignore_visits_and_settings() {
    use dvb_core::event::{LapsesOn, QuestionFormat};
    use dvb_core::{QuestionId, TaskState};
    let p = ProjectId::new("p");
    let g = GoalId::new("g");
    let t = TaskId::new("t");
    let mut kinds: Vec<_> = support::events::approved_task_events()
        .into_iter()
        .map(|k| (Actor::Engine, k))
        .collect();
    kinds.extend([
        (
            Actor::Engine,
            EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: QuestionId::new("q"),
                subject: "Choose a format?".into(),
                format: QuestionFormat::Simple,
                options: vec![],
                recommended: 1,
                lapses_on: LapsesOn::ValueVersion(EventId(1)),
            },
        ),
        (
            Actor::Engine,
            EventKind::RunStarted(assignment("r", Some("t"), "backend", None)),
        ),
        (
            Actor::Engine,
            EventKind::TaskStateChanged {
                task: t.clone(),
                from: TaskState::Planned,
                to: TaskState::Running,
            },
        ),
        (
            Actor::Engine,
            EventKind::PrMerged {
                task: t.clone(),
                pr: 135,
                sha: "abc".into(),
            },
        ),
        (
            Actor::Engine,
            EventKind::TaskStateChanged {
                task: t.clone(),
                from: TaskState::Running,
                to: TaskState::Done,
            },
        ),
        (Actor::Engine, EventKind::GoalDone { goal: g }),
        (Actor::Owner, EventKind::ProjectSeen {}),
        (
            Actor::Owner,
            EventKind::SettingsChanged {
                key: "text_size".into(),
                old: serde_json::json!("L"),
                new: serde_json::json!("S"),
                restored_from: None,
            },
        ),
    ]);
    let (roles, hires) = support::events::default_role_hire_events();
    kinds.extend(
        roles
            .into_iter()
            .chain(hires)
            .map(|kind| (Actor::Owner, kind)),
    );
    let events = log(kinds);
    let mut live = Projections::default();
    for e in &events[..8] {
        live.apply(e);
    }
    let finished = live.projects[&p].view.last_finished.as_ref().unwrap();
    assert_eq!(finished.pr, Some(135));
    assert_eq!(finished.text, "x");
    assert_eq!(finished.at, events[7].at);
    for e in &events[8..] {
        live.apply(e);
    }
    let replay = Projections::rebuild(&events);
    assert_eq!(live.projects[&p].view, replay.projects[&p].view);
    assert_eq!(replay.projects[&p].view.last_work_at, Some(events[9].at));
    assert_eq!(replay.projects[&p].view.done_goals, 1);
    let finished = replay.projects[&p].view.last_finished.as_ref().unwrap();
    assert_eq!(finished.pr, None);
    // The durable GoalDone gives the final completion timestamp.
    assert_eq!(finished.at, events[9].at);
    assert_eq!(
        replay.questions[&QuestionId::new("q")].view.opened_at,
        Some(events[4].at)
    );
    let run = replay.tasks[&t].view.current_run.as_ref().unwrap();
    assert_eq!(run.started_at, events[5].at);
    assert_eq!(run.model, "fake-1");
    assert_eq!(run.role, RoleId::new("backend"));
    let mut repeated = events.clone();
    let mut completion = events[9].clone();
    completion.id = EventId(100);
    completion.at = Timestamp(2_000);
    repeated.push(completion);
    assert_eq!(
        Projections::rebuild(&repeated).projects[&p]
            .view
            .last_finished
            .as_ref()
            .unwrap()
            .at,
        events[9].at
    );
}

#[test]
fn later_merge_supersedes_only_older_problems_in_the_same_goal() {
    use dvb_core::TaskState;
    let g = GoalId::new("g");
    let old = TaskId::new("z-old");
    let replacement = TaskId::new("a-replacement");
    let events = log(vec![
        (
            Actor::Engine,
            support::events::project_event(&ProjectId::new("p"), "p", "repo"),
        ),
        (
            Actor::Owner,
            support::events::goal_event(&g, "Fix import", None),
        ),
        (Actor::Engine, support::events::backend_plan_event(&g, &old)),
        (
            Actor::Engine,
            support::events::backend_plan_event(&g, &replacement),
        ),
        (
            Actor::Engine,
            EventKind::TaskStateChanged {
                task: old.clone(),
                from: TaskState::Planned,
                to: TaskState::Failed {
                    reason: "import failed".into(),
                },
            },
        ),
        (
            Actor::Engine,
            EventKind::PrMerged {
                task: replacement,
                pr: 136,
                sha: "fixed".into(),
            },
        ),
        (
            Actor::Engine,
            EventKind::TaskStateChanged {
                task: old.clone(),
                from: TaskState::Failed {
                    reason: "import failed".into(),
                },
                to: TaskState::Stopped {
                    was: Box::new(TaskState::Running),
                },
            },
        ),
    ]);
    let before = Projections::rebuild(&events[..5]);
    assert!(!before.tasks[&old].view.problem_superseded);
    let merged = Projections::rebuild(&events[..6]);
    assert!(merged.tasks[&old].view.problem_superseded);
    let mut repeated = events[..6].to_vec();
    let mut observation = events[4].clone();
    observation.id = EventId(100);
    observation.at = Timestamp(2_000);
    repeated.push(observation);
    assert!(
        Projections::rebuild(&repeated).tasks[&old]
            .view
            .problem_superseded
    );
    let later_problem = Projections::rebuild(&events);
    assert!(!later_problem.tasks[&old].view.problem_superseded);
}

#[test]
fn telemetry_with_retained_history_preserves_task_work_facts_without_recloning_runs() {
    use dvb_core::event::OutputItem;
    let mut kinds: Vec<_> = support::events::approved_task_events()
        .into_iter()
        .map(|kind| (Actor::Engine, kind))
        .collect();
    kinds.push((
        Actor::Engine,
        EventKind::RunStarted(assignment("r", Some("t"), "backend", None)),
    ));
    let mut projection = Projections::rebuild(&log(kinds));
    let template_task = projection.tasks[&TaskId::new("t")].clone();
    let template_run = projection.runs[&RunId::new("r")].clone();
    // Seed retained history directly so setup doesn't dominate this hot-path regression.
    for i in 0..5_000 {
        let task_id = TaskId::new(format!("history-{i}"));
        let run_id = RunId::new(format!("history-{i}"));
        let mut task = template_task.clone();
        task.view.id = task_id.clone();
        task.facts.run = Some(run_id.clone());
        task.facts.builder = Some(run_id.clone());
        let current = task.view.current_run.as_mut().unwrap();
        current.id = run_id.clone();
        current.task = Some(task_id.clone());
        let mut run = template_run.clone();
        run.assignment.run = run_id.clone();
        run.assignment.task = Some(task_id.clone());
        projection
            .goals
            .get_mut(&task.view.goal)
            .unwrap()
            .tasks
            .push(task_id.clone());
        projection.tasks.insert(task_id, task);
        projection.runs.insert(run_id, run);
    }
    let facts: Vec<_> = projection
        .tasks
        .values()
        .map(|t| {
            (
                t.view.clone(),
                t.view.current_run.as_ref().unwrap().model.as_ptr(),
            )
        })
        .collect();
    let before = projection.projects[&ProjectId::new("p")].view.clone();
    let original_model = projection.tasks[&TaskId::new("t")]
        .view
        .current_run
        .as_ref()
        .unwrap()
        .model
        .as_ptr();
    for i in 0..1_000 {
        let kind = if i % 2 == 0 {
            EventKind::RunUsage {
                run: RunId::new("r"),
                uncached_in: 1,
                cached_in: 0,
                out: 1,
                est_cost: 0.01,
            }
        } else {
            EventKind::RunOutput {
                resource: None,
                run: RunId::new("r"),
                item: OutputItem::Text("working".into()),
            }
        };
        projection.apply(&Event {
            id: EventId(10_000 + i),
            at: Timestamp(10_000 + i as i64),
            actor: Actor::Run(RunId::new("r")),
            project: Some(ProjectId::new("p")),
            kind,
            evidence: None,
        });
        assert_eq!(
            projection.tasks[&TaskId::new("t")]
                .view
                .current_run
                .as_ref()
                .unwrap()
                .model
                .as_ptr(),
            original_model
        );
    }
    for (task, (view, allocation)) in projection.tasks.values().zip(facts) {
        assert_eq!(task.view, view);
        // Equality alone would allow rebuilding every view for each output item.
        assert_eq!(
            task.view.current_run.as_ref().unwrap().model.as_ptr(),
            allocation
        );
    }
    let after = &projection.projects[&ProjectId::new("p")].view;
    assert_eq!(after.last_work_at, Some(Timestamp(10_999)));
    assert_eq!(after.done_goals, before.done_goals);
    assert_eq!(after.last_finished, before.last_finished);
}

#[test]
fn current_run_updates_only_for_the_latest_assignment() {
    let mut kinds: Vec<_> = support::events::approved_task_events()
        .into_iter()
        .map(|kind| (Actor::Engine, kind))
        .collect();
    kinds.extend([
        (
            Actor::Engine,
            EventKind::RunStarted(assignment("old", Some("t"), "backend", None)),
        ),
        (
            Actor::Engine,
            EventKind::RunStarted(assignment("new", Some("t"), "code-reviewer", Some("head"))),
        ),
        (
            Actor::Engine,
            EventKind::RunRerouted {
                run: RunId::new("new"),
                task: Some(TaskId::new("t")),
                role: RoleId::new("code-reviewer"),
                from_model: "fake-1".into(),
                to_model: "fake-2".into(),
                vendor: Vendor::Fake,
                effort: Effort::High,
                limited: None,
                resets_at: None,
            },
        ),
        (
            Actor::Engine,
            EventKind::RunEnded {
                run: RunId::new("old"),
                reason: EndReason::Completed,
                detail: None,
            },
        ),
        (
            Actor::Engine,
            EventKind::RunEnded {
                run: RunId::new("new"),
                reason: EndReason::Completed,
                detail: None,
            },
        ),
    ]);
    let events = log(kinds);
    let mut live = Projections::rebuild(&events[..6]);
    let started = live.tasks[&TaskId::new("t")]
        .view
        .current_run
        .as_ref()
        .unwrap()
        .started_at;
    for event in &events[6..8] {
        live.apply(event);
    }
    let current = live.tasks[&TaskId::new("t")]
        .view
        .current_run
        .as_ref()
        .unwrap();
    assert_eq!(current.id, RunId::new("new"));
    assert_eq!(current.model, "fake-2");
    assert_eq!(current.effort, Effort::High);
    assert_eq!(current.started_at, started);
    assert_eq!(current.ended, None);
    live.apply(&events[8]);
    assert_eq!(
        live.tasks[&TaskId::new("t")]
            .view
            .current_run
            .as_ref()
            .unwrap()
            .ended,
        Some(EndReason::Completed)
    );
    assert_eq!(
        live.tasks[&TaskId::new("t")].view,
        Projections::rebuild(&events).tasks[&TaskId::new("t")].view
    );
}
