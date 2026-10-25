//! Automatic admission through the real Fake provider, run socket and local
//! Fake forge. Synthetic runs hold the cap; releasing another project's slot
//! must recover the goal-bound orchestrator without starting an unchosen task.

pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{NewHire, QuestionView};
use dvb_core::event::{Answer, EndReason, PlannedTask};
use dvb_core::roles::{HireRecord, ReviewKind};
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, ProjectId, RoleId, RunAssignment, RunId, TaskId,
    Vendor,
};
use dvb_engine::{AppState, Engine, dispatch};
use serde_json::json;
use support::{engine_up, git, run_prompts, until};

async fn fixture(auto: bool, backend_hired: bool) -> (tempfile::TempDir, AppState) {
    let (tmp, e) = support::planning::fixture(
        "dvba",
        backend_hired,
        Actor::Engine,
        |e| {
            e.append_checked(None, Actor::Owner, |_, _| {
                Ok(vec![EventKind::SettingsChanged {
                    key: "max_runs".into(),
                    old: json!(null),
                    new: json!(3),
                    restored_from: None,
                }])
            })
            .unwrap();
        },
        vec![
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "Build with the chosen model".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: TaskId::new("t"),
                    title: "Write a note".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "One edit".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
        ],
    )
    .await;
    // The reply's fetch can overlap the builder's local clone. Incidental Git
    // maintenance leaves a short-lived objects/maintenance.lock that clone can
    // try to copy after it disappears, injecting an unrelated worker failure.
    // This fixture tests admission, with exactly one successful builder start.
    git(
        &e.mirror("project"),
        &["config", "maintenance.auto", "false"],
    );
    if auto {
        e.append_checked(None, Actor::Owner, |_, _| {
            Ok(vec![EventKind::SettingsChanged {
                key: "model_mode".into(),
                old: json!("Custom"),
                new: json!("Auto"),
                restored_from: None,
            }])
        })
        .unwrap();
    }
    (tmp, e)
}

use support::planning::script;

async fn finished(e: &AppState) {
    until("the task and its Fake runs to finish", async || {
        let proj = e.proj();
        (proj.tasks[&TaskId::new("t")].view.state == dvb_core::TaskState::Done
            && proj
                .questions
                .values()
                .any(|q| q.view.subject.starts_with("Done:"))
            && !proj.live_runs().any(|r| r.project == ProjectId::new("p")))
        .then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_auto_waits_for_a_fake_orchestrator_choice_and_uses_the_second_model() {
    let (tmp, e) = fixture(true, true).await;
    script(
        &tmp,
        "orchestrator-approved",
        &json!([
            {
                "run": [
                    "decide",
                    {
                        "title": "Choosing a model",
                        "choice": "fake-next",
                        "reason": "Task needs high effort"
                    }
                ]
            },
            {
                "sleep_ms": 1000
            },
            {
                "run": [
                    "task start",
                    {
                        "task": "{{task}}",
                        "role": "{{task_role}}",
                        "model": "fake-next",
                        "effort": "high",
                        "reason": "Task needs high effort"
                    }
                ]
            }
        ]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    dispatch::start_waiting(e.clone()).await.unwrap();
    until("the Fake orchestrator's decision", async || {
        support::events::all(&e).iter().any(|ev| {
            matches!(&ev.kind, EventKind::DecisionRecorded { title, .. } if title == "Choosing a model")
        }).then_some(())
    }).await;
    assert!(e.proj().tasks[&TaskId::new("t")].facts.run.is_none());
    let a = until("the builder with the chosen model", async || {
        let proj = e.proj();
        proj.tasks[&TaskId::new("t")]
            .facts
            .run
            .as_ref()
            .map(|r| proj.runs[r].assignment.clone())
    })
    .await;
    assert_eq!(a.model, "fake-next");
    assert_eq!(a.effort, Effort::High);
    assert_eq!(a.reason, "Task needs high effort");
    {
        let proj = e.proj();
        let orchestrator = proj
            .runs
            .values()
            .find(|r| r.assignment.task.is_none())
            .unwrap();
        assert_eq!(
            orchestrator.goal_version,
            Some(proj.goals[&GoalId::new("g")].version)
        );
    }
    until("the Fake builder to finish", async || {
        e.proj().runs[&a.run].ended.as_ref().map(|_| ())
    })
    .await;
    finished(&e).await;
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_missing_hire_at_the_cap_recovers_an_owner_question_and_a_deferred_start() {
    // Cover an unqueued task and both forms of existing waiting request,
    // replayed after an engine restart. None may hide a missing hire.
    for waiting_actor in [
        None,
        Some(Actor::Engine),
        Some(Actor::Run(RunId::new("old"))),
    ] {
        let (tmp, e) = fixture(false, false).await;
        script(
            &tmp,
            "orchestrator-approved",
            &json!([
                {
                    "run": [
                        "task start",
                        {
                            "task": "{{task}}",
                            "role": "{{task_role}}",
                            "reason": "Continue the plan"
                        }
                    ]
                },
                {
                    "run": [
                        "ask",
                        {
                            "subject": "Hire a backend model?",
                            "options": [
                                {
                                    "text": "Hire fake-1",
                                    "effect": "Start backend work",
                                    "approves_plan": false
                                }
                            ],
                            "recommended": 1
                        }
                    ]
                }
            ]),
        );
        script(
            &tmp,
            "orchestrator-answer",
            &json!([
                           {"run": ["task start",
            {"task": "{{task}}", "role": "{{task_role}}", "reason": "The owner added the hire"}]}
                       ]),
        );
        if let Some(actor) = &waiting_actor {
            e.append_checked(Some(&ProjectId::new("p")), actor.clone(), |_, _| {
                Ok(vec![EventKind::TaskWaiting {
                    task: TaskId::new("t"),
                    model: None,
                    effort: None,
                    reason: "Continue the plan".into(),
                    routine: false,
                }])
            })
            .unwrap();
        }
        let initial_waiting = e.proj().tasks[&TaskId::new("t")]
            .waiting
            .as_ref()
            .map(|w| w.since);
        // These slots belong to another project, with no child processes.
        let busy: Vec<_> = (0..3).map(|i| RunId::new(format!("busy-{i}"))).collect();
        e.append_checked(
            Some(&ProjectId::new("other-project")),
            Actor::Engine,
            |_, _| {
                Ok(busy
                    .iter()
                    .map(|run| {
                        EventKind::RunStarted(RunAssignment {
                            review_stage: None,
                            goal_version: None,
                            run: run.clone(),
                            task: None,
                            role: RoleId::new("orchestrator"),
                            hire: HireId::new("other"),
                            model: "fake-1".into(),
                            vendor: Vendor::Fake,
                            effort: Effort::Medium,
                            reason: "Other goal".into(),
                            reviews: None,
                            base: None,
                            routine: false,
                        })
                    })
                    .collect())
            },
        )
        .unwrap();
        let cfg = e.cfg.clone();
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let prompts = run_prompts(&c, &e.cfg.paths.work);
        dispatch::start_waiting(e.clone()).await.unwrap();
        // Await the wake explicitly too: cap admission must produce no run.
        dvb_engine::wake::wake(
            e.clone(),
            ProjectId::new("p"),
            dvb_engine::wake::Wake::Approved,
            0,
            0,
        )
        .await
        .unwrap();
        assert_eq!(e.proj().live_runs().count(), busy.len());
        assert!(e.proj().tasks[&TaskId::new("t")].facts.run.is_none());
        {
            let proj = e.proj();
            assert!(proj.has_waiting_tasks(&proj.goals[&GoalId::new("g")], &e.pool));
            drop(proj);
        }
        e.end_run(&busy[0], EndReason::Completed, None).unwrap();
        let q = until(
            "the owner hire question after another project's slot release",
            async || {
                let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
                open.into_iter()
                    .find(|q| q.subject == "Hire a backend model?")
            },
        )
        .await;
        let orchestrator = e
            .proj()
            .runs
            .values()
            .find(|r| r.project == ProjectId::new("p") && r.assignment.task.is_none())
            .unwrap()
            .clone();
        let prompt = until("the orchestrator's captured prompt", async || {
            prompts
                .lock()
                .unwrap()
                .get(&orchestrator.assignment.run)
                .cloned()
        })
        .await;
        assert!(prompt.contains("Returned to you for model selection or a missing usable hire"));
        if e.proj().tasks[&TaskId::new("t")].waiting.is_some() {
            assert!(prompt.contains("Preserve the saved start choice"));
        }
        assert!(e.proj().tasks[&TaskId::new("t")].facts.run.is_none());
        let hire: HireRecord = c
            .post(
                "/hires",
                &NewHire {
                    role: RoleId::new("backend"),
                    model: "fake-1".into(),
                    effort: Effort::High,
                    routine: false,
                },
            )
            .await
            .unwrap();
        let _: QuestionView = c
            .post(&format!("/questions/{}/answer", q.id), &Answer::Choice(1))
            .await
            .unwrap();
        let run = until(
            "the deferred builder after the owner adds a hire",
            async || e.proj().tasks[&TaskId::new("t")].facts.run.clone(),
        )
        .await;
        let a = e.proj().runs[&run].assignment.clone();
        assert_eq!(a.hire, hire.id);
        assert_eq!(a.model, "fake-1");
        assert_eq!(a.effort, Effort::High);
        assert!(e.proj().live_runs().count() <= busy.len());
        until("the Fake builder to finish", async || {
            e.proj().runs[&run].ended.as_ref().map(|_| ())
        })
        .await;
        finished(&e).await;
        let events = support::events::all(&e);
        // Check the whole lifetime, including a reply orchestrator racing the
        // builder for the released slot, rather than only the sample above.
        let mut live = std::collections::HashSet::new();
        for event in &events {
            if let EventKind::RunStarted(a) = &event.kind {
                live.insert(a.run.clone());
                assert!(
                    live.len() <= 3,
                    "event {} admitted {} ({}) with {} live runs",
                    event.id,
                    a.run,
                    a.reason,
                    live.len()
                );
            } else if let EventKind::RunEnded { run, .. } = &event.kind {
                live.remove(run);
            }
        }
        let starts: Vec<_> = events
            .iter()
            .filter(|ev| {
                matches!(
                    &ev.kind,
                    EventKind::RunStarted(a)
                        if a.task == Some(TaskId::new("t")) && a.reviews.is_none()
                )
            })
            .collect();
        let logs: Vec<_> = starts
            .iter()
            .map(|event| {
                let EventKind::RunStarted(a) = &event.kind else {
                    unreachable!()
                };
                (
                    a.run.clone(),
                    std::fs::read_to_string(e.cfg.paths.work.join(a.run.as_str()).join("run.log")),
                )
            })
            .collect();
        assert_eq!(
            starts.len(),
            1,
            "waiting actor: {waiting_actor:?}; logs: {logs:#?}; events: {events:#?}"
        );
        let hired = events
            .iter()
            .find(|ev| matches!(&ev.kind, EventKind::HireAdded(h) if h.id == hire.id))
            .unwrap();
        assert_eq!(hired.actor, Actor::Owner);
        assert!(hired.id < starts[0].id);
        let answered = events
            .iter()
            .find(|ev| {
                matches!(
                    &ev.kind,
                    EventKind::QuestionAnswered { question, answer: Answer::Choice(1) }
                        if question == &q.id
                )
            })
            .unwrap();
        let assert_answer_author = |request: &dvb_core::Event| {
            let Actor::Run(id) = &request.actor else {
                panic!("the answer request must come from its orchestrator");
            };
            let proj = e.proj();
            let answering = &proj.runs[id];
            assert_ne!(answering.assignment.run, orchestrator.assignment.run);
            assert_eq!(answering.project, ProjectId::new("p"));
            assert!(answering.assignment.task.is_none());
            assert_eq!(answering.assignment.role, RoleId::new("orchestrator"));
            assert_eq!(
                answering.goal_version,
                Some(proj.goals[&GoalId::new("g")].version)
            );
            assert_eq!(
                answering.assignment.reason,
                "The owner answered a question."
            );
            let author_started = events
                .iter()
                .find(|ev| matches!(&ev.kind, EventKind::RunStarted(a) if &a.run == id))
                .unwrap();
            assert!(answered.id < author_started.id);
            assert!(author_started.id < request.id);
        };
        // The executor can be the engine even when the answer orchestrator
        // authored the request: a cap refusal saves task.waiting first. Use
        // the latest durable request before this start, not the initial one.
        if starts[0].actor == Actor::Engine {
            let saved = events.iter().rev().find(|ev| {
                ev.id < starts[0].id
                    && matches!(&ev.kind, EventKind::TaskWaiting { task, .. } if task.as_str() == "t")
            });
            if let Some(saved) = saved {
                let EventKind::TaskWaiting {
                    model,
                    effort,
                    reason,
                    ..
                } = &saved.kind
                else {
                    unreachable!("selected a task.waiting event");
                };
                assert_eq!(a.model, model.as_deref().unwrap_or(&hire.model));
                assert_eq!(a.effort, effort.unwrap_or(hire.effort));
                assert_eq!(a.reason, *reason);
                if Some(saved.id) == initial_waiting {
                    assert_eq!(&saved.actor, waiting_actor.as_ref().unwrap());
                    assert_eq!(reason, "Continue the plan");
                } else if saved.actor == Actor::Engine {
                    assert!(initial_waiting.is_none());
                    assert_eq!(reason, "Continue the approved plan");
                } else {
                    assert_eq!(reason, "The owner added the hire");
                    assert_answer_author(saved);
                    let refused = events.iter().rev().find(|ev| ev.id < saved.id).unwrap();
                    assert_eq!(refused.actor, saved.actor);
                    assert!(matches!(
                        &refused.kind,
                        EventKind::CommandRefused(r)
                            if r.refusal.current().unwrap().code
                                == dvb_core::RefusalCode::CapReached {
                                    which: dvb_core::Cap::ConcurrentRuns
                                }
                    ));
                }
            } else {
                assert!(initial_waiting.is_none());
                assert_eq!(a.reason, "Continue the approved plan");
            }
        } else {
            assert_eq!(a.reason, "The owner added the hire");
            assert_answer_author(starts[0]);
        }
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::QuestionOpened { subject, .. }
                        if subject == "Hire a backend model?"
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::QuestionAnswered { question, answer: Answer::Choice(1) }
                        if question == &q.id
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::TaskStarted { task, .. }
                        if task.as_str() == "t"
                ))
                .count(),
            1
        );
        assert!(events.iter().any(|ev| matches!(
            &ev.kind,
            EventKind::RunOutput { item: dvb_core::event::OutputItem::Text(s), .. }
                if s == "fake provider: 0 model calls"
        )));
        serving.abort();
    }
}
