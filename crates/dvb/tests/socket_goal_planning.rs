//! Goal identity and empty-run recovery through real Fake workers and sockets.
use dvb_engine::owner;
pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::{GoalState, GoalView, NewGoal, QuestionView};
use dvb_core::event::{Answer, EndReason, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::launcher::{LaunchError, Launcher, UnitId, UnitState};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, ProjectId, QuestionId, RefusalCode, RoleId,
    RunAssignment, RunId, TaskId, TaskState,
};
use dvb_engine::{AppState, Engine, dispatch, reconcile, wake};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use support::{engine_up, run_prompts, until};

use support::planning::script;

async fn fixture() -> (tempfile::TempDir, AppState) {
    support::planning::fixture("dvbg", true, Actor::Owner, |_| {}, vec![]).await
}

async fn state(c: &Client, text: &str) -> GoalView {
    c.post(
        "/projects/p/goals",
        &NewGoal {
            text: text.into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
    )
    .await
    .unwrap()
}

async fn plan_question(e: &AppState, goal: &GoalId) -> QuestionView {
    until("the goal's own plan and question", async || {
        let proj = e.proj();
        let g = &proj.goals[goal];
        assert!(g.tasks.iter().all(|t| proj.tasks[t].view.title == g.text));
        proj.questions
            .values()
            .find(|q| q.view.open && q.lapses_on == LapsesOn::GoalVersion(g.version))
            .filter(|_| g.planned.is_some())
            .map(|q| q.view.clone())
    })
    .await
}

async fn idle(e: &AppState) {
    until("all Fake runs to end", async || {
        (!e.proj()
            .live_runs()
            .any(|r| r.project == ProjectId::new("p")))
        .then_some(())
    })
    .await;
}

type Serving = tokio::task::JoinHandle<std::io::Result<()>>;

/// Stops serving `e`, reopens it from its log and serves it again.
async fn restart(e: &AppState, serving: Serving, c: &Client) -> (AppState, Serving) {
    e.quiesce();
    serving.abort();
    let _ = serving.await;
    let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    engine_up(c).await;
    (e, serving)
}

fn goal_runs(e: &AppState) -> usize {
    e.proj()
        .runs
        .values()
        .filter(|r| r.project == ProjectId::new("p") && r.assignment.reason == "A goal was stated.")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_owner_goal_is_planned_once_with_and_without_a_what_next_answer() {
    for answering in [false, true] {
        let (tmp, e) = fixture().await;
        let p = ProjectId::new("p");
        let old = GoalId::new("done");
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::GoalStated {
                goal: old.clone(),
                text: "Previous work".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            }])
        })
        .unwrap();
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::GoalDone { goal: old }])
        })
        .unwrap();
        if answering {
            let version = e.proj().goals[&GoalId::new("done")].version;
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::QuestionOpened {
                    kind: Some(dvb_core::event::QuestionKind::Ordinary),
                    main_ci: None,
                    task: None,
                    question: QuestionId::new("next"),
                    subject: "What next?".into(),
                    format: QuestionFormat::Simple,
                    options: vec![QuestionOption {
                        id: 1,
                        text: "I will name it".into(),
                        effect: "Wait for the owner's goal".into(),
                        approves_plan: false,
                        no_effect: false,
                        retries_task: None,
                    }],
                    recommended: 1,
                    lapses_on: LapsesOn::GoalVersion(version),
                }])
            })
            .unwrap();
            // Force the reported Done-bound answer path to try copying the
            // owner's waiting goal. Recovery must then plan that owner goal.
            script(
                &tmp,
                "orchestrator-answer",
                &json!([
                    {"run": ["plan", {"goal": "Add a parser", "tasks": [
                        {"title": "Add a parser", "role": "backend", "summary": "One edit"}
                    ]}]}
                ]),
            );
        }
        owner::pause(&e, &p, true).unwrap();
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let owner = state(&c, "Add a parser").await;
        if answering {
            let _: QuestionView = c
                .post("/questions/next/answer", &Answer::Choice(1))
                .await
                .unwrap();
        }
        owner::pause(&e, &p, false).unwrap();
        plan_question(&e, &owner.id).await;
        idle(&e).await;
        let events = support::events::all(&e);
        let stated: Vec<_> = events
            .iter()
            .filter(|ev| {
                matches!(&ev.kind,
            EventKind::GoalStated { text, .. } if text == "Add a parser")
            })
            .collect();
        assert_eq!(stated.len(), 1);
        assert_eq!(stated[0].actor, Actor::Owner);
        assert_eq!(e.proj().goals.len(), 2); // completed predecessor + one new goal
        assert_eq!(e.proj().goals[&owner.id].tasks.len(), 1);
        if answering {
            assert!(events.iter().any(|ev| matches!(&ev.kind,
                EventKind::RunStarted(a) if a.reason == "The owner answered a question.")));
            let answer_run = e
                .proj()
                .runs
                .values()
                .find(|r| r.assignment.reason == "The owner answered a question.")
                .unwrap()
                .assignment
                .run
                .clone();
            assert!(events.iter().any(|ev| matches!(&ev.kind,
                EventKind::RunOutput { run, item: dvb_core::event::OutputItem::ToolCall { name }, .. }
                if run == &answer_run && name == "dvb run plan")));
        }
        serving.abort();
        let _ = serving.await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_two_unplanned_goals_keep_distinct_plans_and_actual_worker_prompts() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    script(
        &tmp,
        "builder",
        &json!([
            {"write": {"path": "docs/{{task}}.md", "text": "# {{task_title}}\n"}}
        ]),
    );
    let gate = gate(&tmp, "planning");
    let mut steps: serde_json::Value = serde_json::from_str(include_str!(
        "../../dvb-engine/src/fake_scripts/orchestrator-goal.json"
    ))
    .unwrap();
    steps
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"write": {"path": gate, "text": "ready"}}));
    script(&tmp, "orchestrator-goal", &steps);
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let prompts = run_prompts(&c, &e.cfg.paths.work);
    let first = state(&c, "Add a parser").await;
    let first_run = blocked(&e, &gate).await;
    let second = state(&c, "Add an exporter").await;
    // Both different-text goals await a plan at the same time, including while
    // the first actual planner is live. No plan/approval can hide misbinding.
    assert!(e.proj().goals[&first.id].planned.is_none());
    assert!(e.proj().goals[&second.id].planned.is_none());
    release(&gate).await;
    let q1 = plan_question(&e, &first.id).await;
    let q2 = plan_question(&e, &second.id).await;
    idle(&e).await;
    {
        let proj = e.proj();
        assert_eq!(
            proj.runs[&first_run].goal_version,
            Some(proj.goals[&first.id].version)
        );
    }
    for g in [&first, &second] {
        let proj = e.proj();
        let run = proj
            .runs
            .values()
            .find(|r| r.goal_version == Some(proj.goals[&g.id].version))
            .unwrap();
        let prompt = prompts.lock().unwrap()[&run.assignment.run].clone();
        let goal_text = prompt
            .split_once("## This run's goal\n\n")
            .unwrap()
            .1
            .split_once("\n\n")
            .unwrap()
            .1
            .split_once("\n\n")
            .unwrap()
            .0;
        assert_eq!(goal_text, g.text);
        let events = support::events::all(&e);
        assert!(events.iter().any(
            |ev| matches!(&ev.kind, EventKind::PlanProposed { goal, tasks }
            if goal == &g.id && tasks.len() == 1 && tasks[0].title == g.text)
        ));
        drop(proj);
    }
    support::approve(&c, &q1).await;
    until("the first approved goal's builder admission", async || {
        let proj = e.proj();
        proj.goals[&first.id]
            .tasks
            .iter()
            .any(|t| proj.tasks[t].facts.builder.is_some())
            .then_some(())
    })
    .await;
    support::approve(&c, &q2).await;
    until("both goals to complete", async || {
        let proj = e.proj();
        [first.id.clone(), second.id.clone()]
            .iter()
            .all(|id| proj.goals[id].state == GoalState::Done)
            .then_some(())
    })
    .await;
    idle(&e).await;
    {
        let proj = e.proj();
        assert_eq!(proj.goals.len(), 2);
        for (goal, other) in [(&first, &second), (&second, &first)] {
            let g = &proj.goals[&goal.id];
            assert_eq!(g.tasks.len(), 1);
            let task = &g.tasks[0];
            assert_eq!(proj.tasks[task].view.title, goal.text);
            let runs: Vec<_> = proj
                .runs
                .values()
                .filter(|r| r.assignment.task.as_ref() == Some(task))
                .collect();
            assert!(runs.iter().any(|r| r.assignment.reviews.is_none()));
            assert!(runs.iter().any(|r| r.assignment.reviews.is_some()));
            for run in runs {
                let text = prompts.lock().unwrap()[&run.assignment.run].clone();
                let section = text
                    .split_once("## Owner's goal (read-only context)\n\n")
                    .unwrap()
                    .1;
                assert!(
                    section
                        .split_once("\n\n")
                        .unwrap()
                        .1
                        .starts_with(&format!("{}\n", goal.text)),
                    "{section}"
                );
                assert!(!section.contains(&other.text));
            }
        }
        drop(proj);
    }
    serving.abort();
    let _ = serving.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_done_can_propose_a_new_goal_when_no_owner_goal_waits() {
    let (tmp, e) = fixture().await;
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("done"),
            text: "Previous work".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let version = e.proj().goals[&GoalId::new("done")].version;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::GoalDone {
                goal: GoalId::new("done"),
            },
            EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: QuestionId::new("next"),
                subject: "What next?".into(),
                format: QuestionFormat::Simple,
                options: vec![],
                recommended: 1,
                lapses_on: LapsesOn::GoalVersion(version),
            },
        ])
    })
    .unwrap();
    script(
        &tmp,
        "orchestrator-answer",
        &json!([
            {"run": ["plan", {"goal": "Add a parser", "tasks": [
                {"title": "Add a parser", "role": "backend", "summary": "One edit"}
            ]}]},
            {"run": ["ask", {"subject": "Start the parser plan?", "options": [
                {"text": "Yes", "effect": "Build it", "approves_plan": true}
            ], "recommended": 1}]}
        ]),
    );
    owner::pause(&e, &p, true).unwrap();
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let _: QuestionView = c
        .post(
            "/questions/next/answer",
            &Answer::Reply("Add a parser".into()),
        )
        .await
        .unwrap();
    owner::pause(&e, &p, false).unwrap();
    let goal = until("a next goal proposed by the Fake run", async || {
        e.proj()
            .goals
            .values()
            .find(|g| g.text == "Add a parser")
            .map(|g| g.id.clone())
    })
    .await;
    plan_question(&e, &goal).await;
    idle(&e).await;
    assert_eq!(e.proj().goals.len(), 2);
    assert_eq!(e.proj().goals[&GoalId::new("done")].state, GoalState::Done);
    let events = support::events::all(&e);
    let proposed = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::GoalStated { goal: id, .. } if id == &goal))
        .unwrap();
    let Actor::Run(run) = &proposed.actor else {
        panic!("the run proposes the continuation");
    };
    assert_eq!(e.proj().runs[run].goal_version, Some(proposed.id));
    let reopened = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    assert_eq!(reopened.proj().runs[run].goal_version, Some(proposed.id));
    serving.abort();
}

// The existing Fake write step provides a deterministic process gate without
// sleeps or a test-only provider dialect. All waits and readers are bounded.
fn gate(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = tmp.path().join(name);
    assert!(
        Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    path
}

async fn blocked(e: &AppState, gate: &Path) -> RunId {
    until("the Fake worker at its FIFO gate", async || {
        e.proj().live_runs().find_map(|r| {
            let dir = e.cfg.paths.work.join(r.assignment.run.as_str());
            let script = std::fs::read_to_string(dir.join("fake-script.json")).ok()?;
            if !script.contains(gate.to_str().unwrap()) {
                return None;
            }
            let pid = std::fs::read_to_string(dir.join("pid")).ok()?;
            std::fs::read_dir(format!("/proc/{}/task", pid.trim()))
                .ok()?
                .flatten()
                .any(|t| {
                    std::fs::read_to_string(t.path().join("wchan"))
                        .is_ok_and(|s| s.trim() == "wait_for_partner")
                })
                .then(|| r.assignment.run.clone())
        })
    })
    .await
}

async fn release(gate: &Path) {
    let path = gate.to_owned();
    let out = tokio::task::spawn_blocking(move || {
        Command::new("timeout")
            .args(["--kill-after=1s", "5s", "cat"])
            .arg(path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    std::fs::remove_file(gate).unwrap();
    std::fs::write(gate, "released").unwrap();
}

struct RunCleanup(PathBuf);
impl Drop for RunCleanup {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if let Ok(pid) = std::fs::read_to_string(entry.path().join("pid")) {
                // Never kill a reused pid: verify the durable run identity.
                if std::fs::read(format!("/proc/{}/cmdline", pid.trim())).is_ok_and(|cmd| {
                    cmd.split(|b| *b == 0)
                        .any(|arg| arg == entry.file_name().as_encoded_bytes())
                }) {
                    let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
                }
            }
        }
    }
}

fn runs_for(e: &AppState, goal: &GoalId) -> Vec<RunId> {
    let proj = e.proj();
    let version = proj.goals[goal].version;
    proj.runs
        .values()
        .filter(|r| r.assignment.task.is_none() && r.goal_version == Some(version))
        .map(|r| r.assignment.run.clone())
        .collect()
}

async fn empty_end(e: &AppState, goal: &GoalId) -> RunId {
    until("the first empty planner to finish", async || {
        let runs = runs_for(e, goal);
        assert!(runs.len() <= 1, "empty planning loop");
        runs.first()
            .and_then(|run| e.proj().runs[run].ended.as_ref().map(|_| run.clone()))
    })
    .await
}

async fn hold_question(e: &AppState, goal: &GoalId) -> QuestionView {
    until("the durable empty-planning question", async || {
        let proj = e.proj();
        let version = proj.goals[goal].version;
        proj.questions
            .values()
            .find(|q| {
                q.view.open
                    && q.lapses_on == LapsesOn::GoalVersion(version)
                    && q.view.subject.contains("planned nothing")
            })
            .map(|q| q.view.clone())
    })
    .await
}

async fn passes(e: &AppState) {
    for _ in 0..5 {
        wake::wake_if_missed(e, &ProjectId::new("p"));
        wake::wake(e.clone(), ProjectId::new("p"), wake::Wake::Goal, 0, 0)
            .await
            .unwrap();
        dispatch::start_waiting(e.clone()).await.unwrap();
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_worded_plan_reply_replaces_once_and_approval_starts_only_revised_tasks() {
    for asks in [true, false] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        let revised = json!({"tasks": [{"title": "Revised task", "role": "backend", "summary": "Use docs/reply.txt"}]});
        let mut initial: serde_json::Value = serde_json::from_str(include_str!(
            "../../dvb-engine/src/fake_scripts/orchestrator-goal.json"
        ))
        .unwrap();
        // Merely having an unapproved plan does not permit another proposal.
        initial
            .as_array_mut()
            .unwrap()
            .insert(1, json!({"run": ["plan", revised]}));
        script(&tmp, "orchestrator-goal", &initial);
        let replying = gate(&tmp, "revised-plan-saved");
        let building = gate(&tmp, "revised-builder");
        let reviewing = gate(&tmp, "revised-reviewer");
        script(
            &tmp,
            "builder",
            &json!([
                {"write": {"path": building, "text": "ready"}},
                {"write": {"path": "docs/reply.txt", "text": "Synthetic revision\n"}}
            ]),
        );
        script(
            &tmp,
            "reviewer",
            &json!([
                {"write": {"path": reviewing, "text": "ready"}},
                {"run": ["verdict", {"findings": [], "summary": "Synthetic review passed"}]}
            ]),
        );
        let mut answer_script = json!([
            {
                "run": [
                    "decide",
                    {
                        "title": "Filename",
                        "choice": "Use docs/reply.txt",
                        "reason": "The owner requested it"
                    }
                ]
            },
            // A reply does not permit changing the goal or accepting an invalid role.
            {
                "run": [
                    "plan",
                    {
                        "goal": "A different goal",
                        "tasks": []
                    }
                ]
            },
            {
                "run": [
                    "plan",
                    {
                        "tasks": [
                            {
                                "title": "Invalid",
                                "role": "missing-role",
                                "summary": "Invalid"
                            }
                        ]
                    }
                ]
            },
            {
                "run": [
                    "plan",
                    revised
                ]
            },
            {
                "run": [
                    "plan",
                    revised
                ]
            },
            {
                "run": [
                    "ask",
                    {
                        "subject": "Approve revised task?",
                        "options": [
                            {
                                "text": "Yes",
                                "effect": "Build the revision",
                                "approves_plan": true
                            }
                        ],
                        "recommended": 1
                    }
                ]
            },
            {
                "write": {
                    "path": replying,
                    "text": "ready"
                }
            }
        ]);
        if !asks {
            // Completion/restart recovery must not reuse the old answered question.
            answer_script.as_array_mut().unwrap().remove(5);
        }
        script(&tmp, "orchestrator-answer", &answer_script);
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let goal = state(&c, "Add a parser").await;
        let original = plan_question(&e, &goal.id).await;
        idle(&e).await;
        let version = e.proj().goals[&goal.id].version;
        let old = e.proj().goals[&goal.id].tasks[0].clone();
        assert!(e.proj().tasks[&old].facts.run.is_none());
        let _: QuestionView = c
            .post(
                &format!("/questions/{}/answer", original.id),
                &Answer::Reply("Please use docs/reply.txt instead.".into()),
            )
            .await
            .unwrap();
        let reply_run = blocked(&e, &replying).await;
        let prompt =
            std::fs::read_to_string(e.cfg.paths.work.join(reply_run.as_str()).join("prompt.md"))
                .unwrap();
        assert!(prompt.contains("Please use docs/reply.txt instead."));
        assert_eq!(e.proj().runs[&reply_run].goal_version, Some(version));
        release(&replying).await;
        idle(&e).await;
        passes(&e).await;
        {
            let proj = e.proj();
            let g = &proj.goals[&goal.id];
            assert_eq!(proj.goals.len(), 1);
            assert_eq!(g.text, "Add a parser");
            assert_eq!(g.version, version);
            assert_eq!(g.state, GoalState::Stated);
            assert_eq!(g.tasks.len(), 2);
            assert_eq!(proj.tasks[&old].view.state, TaskState::Cancelled);
            assert!(g.tasks.iter().all(|t| proj.tasks[t].facts.run.is_none()));
            assert_eq!(proj.questions.len(), 2);
            assert_eq!(proj.questions.values().filter(|q| q.view.open).count(), 1);
        }
        let events = support::events::all(&e);
        let plans = events
            .iter()
            .filter(
                |ev| matches!(&ev.kind, EventKind::PlanProposed { goal: id, .. } if id == &goal.id),
            )
            .count();
        assert_eq!(plans, 2);
        assert!(events.iter().any(|ev| {
            matches!(&ev.kind, EventKind::PlanCancelled { goal: id, tasks }
                if id == &goal.id && tasks == std::slice::from_ref(&old))
        }));

        // Replayed cancellation prevents recovery/automatic admission of old tasks.
        owner::pause(&e, &ProjectId::new("p"), true).unwrap();
        let (e, serving) = restart(&e, serving, &c).await;
        reconcile::reconcile(e.clone()).await.unwrap();
        owner::pause(&e, &ProjectId::new("p"), false).unwrap();
        passes(&e).await;
        let question = e
            .proj()
            .questions
            .values()
            .find(|q| q.view.open)
            .unwrap()
            .view
            .clone();
        assert_eq!(
            question.subject,
            if asks {
                "Approve revised task?\n\nPlan:\n1. backend — Revised task\n"
            } else {
                "Start this plan? Add a parser\n\nPlan:\n1. backend — Revised task\n"
            }
        );
        assert_eq!(e.proj().questions.len(), 2);
        {
            let proj = e.proj();
            assert!(
                proj.goals[&goal.id]
                    .tasks
                    .iter()
                    .all(|t| proj.tasks[t].facts.run.is_none())
            );
            drop(proj);
        }
        support::approve(&c, &question).await;
        let builder = blocked(&e, &building).await;
        assert_worker_plan_reply(
            &e,
            &builder,
            "Add a parser",
            "Please use docs/reply.txt instead.",
        );
        let task = e.proj().runs[&builder].assignment.task.clone().unwrap();
        assert_ne!(task, old);
        assert_eq!(e.proj().tasks[&task].view.title, "Revised task");
        assert!(e.proj().tasks[&old].facts.run.is_none());
        assert!(!support::events::all(&e).iter().any(
            |ev| matches!(&ev.kind, EventKind::RunStarted(a) if a.task.as_ref() == Some(&old))
        ));
        release(&building).await;
        let reviewer = blocked(&e, &reviewing).await;
        assert_eq!(
            e.proj().runs[&reviewer].assignment.task.as_ref(),
            Some(&task)
        );
        assert_worker_plan_reply(
            &e,
            &reviewer,
            "Add a parser",
            "Please use docs/reply.txt instead.",
        );
        release(&reviewing).await;
        idle(&e).await;
        serving.abort();
    }
}

fn assert_worker_plan_reply(e: &AppState, run: &RunId, goal: &str, reply: &str) {
    let text =
        std::fs::read_to_string(e.cfg.paths.work.join(run.as_str()).join("prompt.md")).unwrap();
    let context = text
        .split_once("## Owner's goal (read-only context)")
        .unwrap()
        .1;
    assert!(context.find(goal).unwrap() < context.find(reply).unwrap());
    assert!(context.contains("after the goal text (take precedence where they differ)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_task_prompts_keep_only_prior_plan_replies_in_answer_order() {
    // Seed durable history, then run actual Fake builders and reviewers. Question
    // ids deliberately sort opposite to answer order; ordering comes from events.
    for worded in [true, false] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        let p = ProjectId::new("p");
        let goal = GoalId::new("synthetic-goal");
        let task = TaskId::new("synthetic-revised-task");
        let text = "Synthetic goal: use the original filename.";
        let first = "Synthetic first change: use the revised filename.\nKeep {{literal}} and café.";
        let second = "Synthetic second change: keep one line.";
        let stated = e
            .append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![EventKind::GoalStated {
                    goal: goal.clone(),
                    text: text.into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                }])
            })
            .unwrap()[0]
            .id;
        let other = e
            .append_checked(Some(&p), Actor::Owner, |_, _| {
                Ok(vec![
                    EventKind::GoalStated {
                        goal: GoalId::new("other-goal"),
                        text: "Other synthetic goal".into(),
                        from_issue: None,
                        issues: Vec::new(),
                        after: None,
                    },
                    EventKind::GoalDropped {
                        goal: GoalId::new("other-goal"),
                    },
                ])
            })
            .unwrap()[0]
            .id;
        let question = |id: &str, version, approves_plan| EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new(id),
            subject: format!("Synthetic question {id}"),
            format: QuestionFormat::QuickCompare,
            options: vec![QuestionOption {
                id: 1,
                text: "Yes".into(),
                effect: "Proceed".into(),
                approves_plan,
                no_effect: false,
                retries_task: None,
            }],
            recommended: 1,
            lapses_on: version,
        };
        let answer = |id: &str, text: &str| EventKind::QuestionAnswered {
            question: QuestionId::new(id),
            answer: Answer::Reply(text.into()),
        };
        let plan = |id: TaskId| EventKind::PlanProposed {
            goal: goal.clone(),
            tasks: vec![PlannedTask {
                after: vec![],
                task: id,
                title: "Synthetic task".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "Write a synthetic fixture.".into(),
                routine: false,
            }],
        };
        let original = TaskId::new("synthetic-original-task");
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![
                plan(original.clone()),
                question("a-second", LapsesOn::GoalVersion(stated), true),
                question("z-first", LapsesOn::GoalVersion(stated), true),
                // Answers to unrelated goals, non-plan questions and unbound values
                // must not become instructions for this task.
                question("other", LapsesOn::GoalVersion(other), true),
                answer("other", "Excluded other goal reply"),
                question("non-plan", LapsesOn::GoalVersion(stated), false),
                answer("non-plan", "Excluded non-plan reply"),
                question("value", LapsesOn::ValueVersion(stated), true),
                answer("value", "Excluded value reply"),
                if worded {
                    answer("z-first", first)
                } else {
                    EventKind::QuestionAnswered {
                        question: QuestionId::new("z-first"),
                        answer: Answer::Choice(1),
                    }
                },
                if worded {
                    answer("a-second", second)
                } else {
                    EventKind::QuestionAnswered {
                        question: QuestionId::new("a-second"),
                        answer: Answer::Choice(1),
                    }
                },
                EventKind::PlanCancelled {
                    goal: goal.clone(),
                    tasks: vec![original.clone()],
                },
                EventKind::TaskCancelled {
                    task: original.clone(),
                    reason: Some("Synthetic plan replaced".into()),
                },
                EventKind::TaskStateChanged {
                    task: original,
                    from: TaskState::Planned,
                    to: TaskState::Cancelled,
                },
                // This question predates the originating plan but its answer does not.
                question("late", LapsesOn::GoalVersion(stated), true),
                plan(task.clone()),
                answer("late", "Excluded later reply"),
                question("approval", LapsesOn::GoalVersion(stated), true),
                EventKind::QuestionAnswered {
                    question: QuestionId::new("approval"),
                    answer: Answer::Choice(1),
                },
                EventKind::GoalApproved { goal: goal.clone() },
            ])
        })
        .unwrap();
        let historical = format!(
            "## Owner's goal (read-only context)\n\nThe task summary is the work to do; this goal \
                is the source to check it against. Reviewers may quote it when a change \
                does not meet it. This prompt context is private: do not copy it into \
                published files, branches, commits, PRs, issues, board cards or \
                published logs.\n\n{text}\n\n"
        );
        let expected = if worded {
            format!(
                "{historical}### Owner's plan replies after the goal text (take precedence where they \
                differ)\n\n{first}\n\n{second}\n\n"
            )
        } else {
            historical
        };
        let building = gate(&tmp, "ordered-builder");
        let reviewing = gate(&tmp, "ordered-reviewer");
        script(
            &tmp,
            "builder",
            &json!([
                {"write": {"path": building, "text": "ready"}},
                {"write": {"path": "docs/synthetic.txt", "text": "Synthetic fixture\n"}}
            ]),
        );
        script(
            &tmp,
            "reviewer",
            &json!([
                {"write": {"path": reviewing, "text": "ready"}},
                {"run": ["verdict", {"findings": [], "summary": "Synthetic review passed"}]}
            ]),
        );
        // Reopen to prove the same filtering and ordering after event replay.
        let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        dispatch::start_waiting(e.clone()).await.unwrap();
        for (gate, heading) in [
            (&building, "# Backend builder"),
            (&reviewing, "# Code reviewer"),
        ] {
            let run = blocked(&e, gate).await;
            let prompt =
                std::fs::read_to_string(e.cfg.paths.work.join(run.as_str()).join("prompt.md"))
                    .unwrap();
            assert!(prompt.starts_with(heading));
            assert_eq!(e.proj().runs[&run].assignment.task.as_ref(), Some(&task));
            let (r, project) = {
                let proj = e.proj();
                (proj.runs[&run].clone(), proj.projects[&p].clone())
            };
            let delivered = dvb_engine::prompt::context(&e, &r, &project).await.unwrap();
            let context = &delivered
                .iter()
                .find(|(key, _)| *key == "owner_goal")
                .unwrap()
                .1;
            assert_eq!(context, &expected);
            assert!(prompt.contains(&expected));
            for excluded in [
                "Excluded other goal reply",
                "Excluded non-plan reply",
                "Excluded value reply",
                "Excluded later reply",
            ] {
                assert!(!prompt.contains(excluded));
            }
            if !worded {
                assert!(!prompt.contains("### Owner's plan replies"));
            }
            release(gate).await;
        }
        assert_eq!(e.proj().goals[&goal].text, text);
        assert_eq!(e.proj().goals[&goal].version, stated);
        idle(&e).await;
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_decision_only_planner_holds_or_reasks_and_recovers_one_owner_question() {
    for replying in [false, true] {
        let (tmp, e) = fixture().await;
        script(
            &tmp,
            if replying {
                "orchestrator-answer"
            } else {
                "orchestrator-goal"
            },
            &json!([
                {"run": ["decide", {"title": "Planning", "choice": "Need more context", "reason": "Record the choice"}]}
            ]),
        );
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let prompts = run_prompts(&c, &e.cfg.paths.work);
        let goal = state(&c, "Add a parser").await;
        let version = e.proj().goals[&goal.id].version;
        let original = if replying {
            let question = plan_question(&e, &goal.id).await;
            idle(&e).await;
            let _: QuestionView = c
                .post(
                    &format!("/questions/{}/answer", question.id),
                    &Answer::Reply("Please use docs/reply.txt instead.".into()),
                )
                .await
                .unwrap();
            Some(question)
        } else {
            None
        };
        let run = if replying {
            until("the decision-only reply planner to finish", async || {
                e.proj()
                    .runs
                    .values()
                    .find(|r| {
                        r.goal_version == Some(version)
                            && r.assignment.reason == "The owner answered a question."
                            && r.ended.is_some()
                    })
                    .map(|r| r.assignment.run.clone())
            })
            .await
        } else {
            empty_end(&e, &goal.id).await
        };
        // With a saved plan the reply attempt did not plan nothing: the
        // plan is asked about again instead of holding the goal.
        assert_eq!(e.proj().goals[&goal.id].planning_held, !replying);
        if replying {
            let prompt = prompts.lock().unwrap()[&run].clone();
            assert!(prompt.contains("Please use docs/reply.txt instead."));
        }
        let owner_question = async |e: &AppState| {
            if replying {
                until("the saved plan asked again", async || {
                    e.proj()
                        .questions
                        .values()
                        .find(|q| {
                            q.view.open
                                && q.lapses_on == LapsesOn::GoalVersion(version)
                                && q.view.options.iter().any(|o| o.approves_plan)
                        })
                        .map(|q| q.view.clone())
                })
                .await
            } else {
                hold_question(e, &goal.id).await
            }
        };
        let question = owner_question(&e).await;
        assert_eq!(
            question
                .options
                .iter()
                .map(|o| o.text.as_str())
                .collect::<Vec<_>>(),
            if replying {
                ["Yes, start it", "Not yet"]
            } else {
                ["Plan it again", "Drop the goal"]
            }
        );
        assert!(
            support::events::all(&e)
                .iter()
                .any(|ev| matches!(ev.kind, EventKind::DecisionRecorded { .. })
                    && ev.actor == Actor::Run(run.clone()))
        );
        assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Completed));
        passes(&e).await;
        let (e, serving) = restart(&e, serving, &c).await;
        reconcile::reconcile(e.clone()).await.unwrap();
        passes(&e).await;
        assert_eq!(owner_question(&e).await.id, question.id);
        {
            let proj = e.proj();
            let g = &proj.goals[&goal.id];
            assert_eq!(g.state, GoalState::Stated);
            assert_eq!(g.text, goal.text);
            assert_eq!(g.version, version);
            assert_eq!(proj.goals.len(), 1);
            assert_eq!(proj.questions.len(), if replying { 2 } else { 1 });
            assert_eq!(proj.questions.values().filter(|q| q.view.open).count(), 1);
            assert_eq!(proj.runs.len(), if replying { 2 } else { 1 });
            assert!(proj.runs.contains_key(&run));
            if let Some(original) = &original {
                assert!(!proj.questions[&original.id].view.open);
                assert_eq!(g.tasks.len(), 1);
                assert!(g.planned.is_some());
                let task = &proj.tasks[&g.tasks[0]];
                assert_eq!(task.view.state, TaskState::Planned);
                assert!(task.facts.run.is_none());
            }
            drop(proj);
        }
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_numbered_plan_answers_do_not_permit_revision() {
    for answer in [1, 2] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        let building = gate(&tmp, "numbered-builder");
        script(
            &tmp,
            "builder",
            &json!([{ "write": {"path": building, "text": "ready"}}]),
        );
        let attempt = json!([
            {
                "run": [
                    "plan",
                    {
                        "tasks": [
                            {
                                "title": "Forbidden revision",
                                "role": "backend",
                                "summary": "No worded reply"
                            }
                        ]
                    }
                ]
            }
        ]);
        script(&tmp, "orchestrator-answer", &attempt);
        script(&tmp, "orchestrator-approved", &attempt);
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let goal = state(&c, "Add a parser").await;
        let question = plan_question(&e, &goal.id).await;
        idle(&e).await;
        let task = e.proj().goals[&goal.id].tasks[0].clone();
        let _: QuestionView = c
            .post(
                &format!("/questions/{}/answer", question.id),
                &Answer::Choice(answer),
            )
            .await
            .unwrap();
        if answer == 1 {
            let builder = blocked(&e, &building).await;
            assert_eq!(
                e.proj().runs[&builder].assignment.task.as_ref(),
                Some(&task)
            );
            release(&building).await;
        }
        idle(&e).await;
        assert_eq!(
            e.proj().goals[&goal.id].tasks.as_slice(),
            std::slice::from_ref(&task)
        );
        if answer == 2 {
            // It parks the plan: no run gets the answer.
            assert_eq!(runs_for(&e, &goal.id).len(), 1);
            assert_eq!(e.proj().goals[&goal.id].state, GoalState::Stated);
            assert_eq!(e.proj().tasks[&task].view.state, TaskState::Planned);
            assert!(e.proj().tasks[&task].facts.run.is_none());
        }
        assert!(
            !support::events::all(&e)
                .iter()
                .any(|ev| matches!(ev.kind, EventKind::PlanCancelled { .. }))
        );
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_not_yet_parks_the_plan_and_resume_asks_the_same_question_again() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let building = gate(&tmp, "not-yet-builder");
    script(
        &tmp,
        "builder",
        &json!([{ "write": {"path": building, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let original = plan_question(&e, &goal.id).await;
    idle(&e).await;
    let task = e.proj().goals[&goal.id].tasks[0].clone();
    let not_yet = original
        .options
        .iter()
        .find(|o| o.text == "Not yet")
        .unwrap()
        .id;
    let refused = c
        .post::<_, QuestionView>(&format!("/goals/{}/resume", goal.id), &())
        .await;
    assert!(
        matches!(&refused, Err(CliError::Refused(r)) if r.code == RefusalCode::PlanNotParked),
        "{refused:?}"
    );
    let _: QuestionView = c
        .post(
            &format!("/questions/{}/answer", original.id),
            &Answer::Choice(not_yet),
        )
        .await
        .unwrap();
    // The plan is parked: the question closes, no run starts and nothing
    // asks again, however often recovery looks.
    passes(&e).await;
    idle(&e).await;
    let parked = |e: &AppState| {
        assert_eq!(runs_for(e, &goal.id).len(), 1);
        let proj = e.proj();
        let g = &proj.goals[&goal.id];
        assert!(!g.planning_held);
        assert_eq!(g.state, GoalState::Stated);
        assert_eq!(proj.tasks[&task].view.state, TaskState::Planned);
        assert!(!proj.questions.values().any(|q| q.view.open));
        drop(proj);
    };
    parked(&e);
    let goals: Vec<GoalView> = c.get("/goals?project=p").await.unwrap();
    assert!(goals[0].plan_parked);
    let shown = status(&e).await;
    assert!(shown.starts_with("0 running, 0 need you\n"), "{shown}");
    assert!(shown.contains("Add a parser - Plan on hold"), "{shown}");
    // Listed with its command, though not counted.
    assert!(
        shown.contains(&format!(
            "Needs you: Add a parser ({}) - Plan on hold: dvb resume {}\n",
            goal.id, goal.id
        )),
        "{shown}"
    );
    // A restart's recovery does not ask about the parked plan either.
    let (e, serving) = restart(&e, serving, &c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    passes(&e).await;
    parked(&e);
    // Resuming asks the same plan question again, with no run and no new plan.
    let out = cli(&e, &["resume", goal.id.as_str()]).await;
    let again = until("the parked plan asked again", async || {
        e.proj()
            .questions
            .values()
            .find(|q| q.view.open)
            .map(|q| q.view.clone())
    })
    .await;
    assert_eq!(
        out,
        format!("Needs you: {} ({})\n", again.subject, again.id)
    );
    assert_ne!(again.id, original.id);
    assert_eq!(again.subject, original.subject);
    assert_eq!(again.options, original.options);
    assert_eq!(again.recommended, original.recommended);
    assert_eq!(again.plan, original.plan);
    // The answered question's id names the one asking again.
    let stale = c
        .post::<_, QuestionView>(
            &format!("/questions/{}/answer", original.id),
            &Answer::Choice(1),
        )
        .await;
    assert!(
        matches!(&stale, Err(CliError::Refused(r)) if r.code == RefusalCode::AlreadyTerminal
            && r.detail.ends_with(&format!("the plan is asked again in question {}", again.id))),
        "{stale:?}"
    );
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal.id).len(), 1);
    let goals: Vec<GoalView> = c.get("/goals?project=p").await.unwrap();
    assert!(!goals[0].plan_parked);
    assert!(
        status(&e)
            .await
            .contains(&format!("Needs you: {} ({})", again.subject, again.id))
    );
    support::approve(&c, &again).await;
    let builder = blocked(&e, &building).await;
    assert_eq!(
        e.proj().runs[&builder].assignment.task.as_ref(),
        Some(&task)
    );
    assert_eq!(e.proj().goals[&goal.id].tasks, vec![task]);
    release(&building).await;
    idle(&e).await;
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_held_goal_ignores_other_builder_output_and_usage_until_task_final() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let before = gate(&tmp, "before-telemetry");
    let after = gate(&tmp, "after-telemetry");
    script(
        &tmp,
        "builder",
        &json!([
            {"write": {"path": before, "text": "ready"}},
            {"sleep_ms": 0},
            {"usage": {"uncached_in": 17, "cached_in": 3, "out": 9}},
            {"write": {"path": after, "text": "ready"}},
            {"write": {"path": "docs/result.md", "text": "built"}}
        ]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let building = state(&c, "Build a parser").await;
    let q = plan_question(&e, &building.id).await;
    support::approve(&c, &q).await;
    let builder = blocked(&e, &before).await;
    script(&tmp, "orchestrator-goal", &json!([]));
    let held = state(&c, "Plan an exporter").await;
    let planner = empty_end(&e, &held.id).await;
    let boundary = e.store.last_id().unwrap();
    release(&before).await;
    assert_eq!(blocked(&e, &after).await, builder);
    let events = e.store.after(boundary).unwrap();
    assert!(
        events
            .iter()
            .any(|ev| matches!(&ev.kind, EventKind::RunOutput { run, .. } if run == &builder))
    );
    assert!(events.iter().any(
        |ev| matches!(&ev.kind, EventKind::RunUsage { run, uncached_in: 17, .. } if run == &builder)
    ));
    passes(&e).await;
    assert_eq!(runs_for(&e, &held.id), vec![planner]);
    let question = hold_question(&e, &held.id).await;
    let task = e.proj().goals[&building.id].tasks[0].clone();
    assert!(!matches!(
        e.proj().tasks[&task].view.state,
        TaskState::Done | TaskState::Failed { .. } | TaskState::Cancelled
    ));
    // The final task transition releases one successor, even with many wakes.
    release(&after).await;
    until("the builder task to become final", async || {
        (e.proj().tasks[&task].view.state == TaskState::Done).then_some(())
    })
    .await;
    passes(&e).await;
    until("exactly one released planner to finish", async || {
        let runs = runs_for(&e, &held.id);
        assert!(
            runs.len() <= 2,
            "one finalization released multiple attempts"
        );
        (runs.len() == 2 && e.proj().runs[&runs[1]].ended.is_some()).then_some(())
    })
    .await;
    passes(&e).await;
    assert_eq!(runs_for(&e, &held.id).len(), 2);
    assert!(!e.proj().questions[&question.id].view.open);
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_two_held_goals_stay_idle_across_dispatcher_passes() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let first_gate = gate(&tmp, "first");
    let second_gate = gate(&tmp, "second");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": first_gate, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let a = state(&c, "Add a parser").await;
    let first = blocked(&e, &first_gate).await;
    let b = state(&c, "Add an exporter").await;
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": second_gate, "text": "ready"}}]),
    );
    release(&first_gate).await;
    let second = blocked(&e, &second_gate).await;
    owner::pause(&e, &ProjectId::new("p"), true).unwrap();
    script(&tmp, "orchestrator-goal", &json!([]));
    release(&second_gate).await;
    idle(&e).await;
    assert_eq!(runs_for(&e, &a.id), vec![first.clone()]);
    assert_eq!(runs_for(&e, &b.id), vec![second.clone()]);
    owner::pause(&e, &ProjectId::new("p"), false).unwrap();
    passes(&e).await;
    assert_eq!(runs_for(&e, &a.id), vec![first]);
    assert_eq!(runs_for(&e, &b.id), vec![second]);
    let qa = hold_question(&e, &a.id).await;
    let qb = hold_question(&e, &b.id).await;
    assert_ne!(qa.id, qb.id);
    assert_eq!(
        e.proj().questions.values().filter(|q| q.view.open).count(),
        2
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_note_only_planner_holds_its_goal() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "run": ["note", {"text": "nothing to do"}]}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let run = empty_end(&e, &goal.id).await;
    assert!(support::events::all(&e).iter().any(|ev| matches!(&ev.kind,
        EventKind::RunOutput { resource: None,
run: id, item: dvb_core::event::OutputItem::Note(text) }
        if id == &run && text == "nothing to do")));
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal.id), vec![run]);
    hold_question(&e, &goal.id).await;
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_mirror_observations_do_not_release_empty_planning() {
    let (tmp, e) = fixture().await;
    script(&tmp, "orchestrator-goal", &json!([]));
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let run = empty_end(&e, &goal.id).await;
    for _ in 0..5 {
        e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(vec![EventKind::GithubMirrorObserved {
                goal: goal.id.clone(),
                task: None,
                mirrored: true,
                detail: None,
            }])
        })
        .unwrap();
        passes(&e).await;
        assert_eq!(runs_for(&e, &goal.id), vec![run.clone()]);
    }
    hold_question(&e, &goal.id).await;
    serving.abort();
}

fn default_planner(tmp: &tempfile::TempDir) {
    script(
        tmp,
        "orchestrator-goal",
        &serde_json::from_str(include_str!(
            "../../dvb-engine/src/fake_scripts/orchestrator-goal.json"
        ))
        .unwrap(),
    );
}

async fn status(e: &AppState) -> String {
    cli(e, &["status", "p"]).await
}

async fn cli(e: &AppState, args: &[&str]) -> String {
    let root = e.cfg.paths.state.parent().unwrap().to_owned();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(root)
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_empty_planning_question_is_durable_visible_and_both_answers_act() {
    for choice in [1, 2] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        script(&tmp, "orchestrator-goal", &json!([]));
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let goal = state(&c, "Add a parser\nKeep the current grammar").await;
        let run = empty_end(&e, &goal.id).await;
        let q = hold_question(&e, &goal.id).await;
        assert_eq!(
            q.subject,
            "Add a parser: the planner started for it and planned nothing"
        );
        assert_eq!(
            q.options
                .iter()
                .map(|o| o.text.as_str())
                .collect::<Vec<_>>(),
            ["Plan it again", "Drop the goal"]
        );
        assert!(
            status(&e)
                .await
                .contains(&format!("Needs you: {} ({})", q.subject, q.id))
        );
        passes(&e).await;
        assert_eq!(goal_runs(&e), 1);
        serving.abort();
        let _ = serving.await;
        let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        engine_up(&c).await;
        reconcile::reconcile(e.clone()).await.unwrap();
        passes(&e).await;
        assert_eq!(hold_question(&e, &goal.id).await.id, q.id);
        assert_eq!(runs_for(&e, &goal.id), vec![run]);
        let events = support::events::all(&e);
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(
                    &ev.kind,
                    EventKind::QuestionOpened { question, .. }
                        if question == &q.id
                ))
                .count(),
            1
        );
        assert!(
            status(&e)
                .await
                .contains(&format!("Needs you: {} ({})", q.subject, q.id))
        );
        default_planner(&tmp);
        let url = format!("/questions/{}/answer", q.id);
        let answer = Answer::Choice(choice);
        let (a, b) = tokio::join!(
            c.post::<_, QuestionView>(&url, &answer),
            c.post::<_, QuestionView>(&url, &answer)
        );
        assert_eq!(
            usize::from(a.is_ok()) + usize::from(b.is_ok()),
            1,
            "one answer must win"
        );
        if choice == 1 {
            plan_question(&e, &goal.id).await;
            idle(&e).await;
            passes(&e).await;
            assert_eq!(runs_for(&e, &goal.id).len(), 2);
            assert_eq!(e.proj().goals[&goal.id].state, GoalState::Stated);
        } else {
            passes(&e).await;
            assert_eq!(e.proj().goals[&goal.id].state, GoalState::Dropped);
            assert_eq!(runs_for(&e, &goal.id).len(), 1);
        }
        assert!(!e.proj().questions[&q.id].view.open);
        assert_eq!(
            support::events::all(&e)
                .iter()
                .filter(|ev| matches!(&ev.kind,
            EventKind::QuestionAnswered { question, .. } if question == &q.id))
                .count(),
            1
        );
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_restart_interrupted_planner_runs_again_and_plans() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let gate = gate(&tmp, "interrupted");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": gate, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let run = blocked(&e, &gate).await;
    // A reboot kills the worker without an end receipt. Quiescing the old
    // writer prevents its watcher from manufacturing a pre-reboot end.
    e.update.close_admission();
    e.quiesce();
    let unit = dvb_core::launcher::UnitId {
        vendor: dvb_core::Vendor::Fake,
        run: run.clone(),
    };
    e.launcher().stop(&unit).await.unwrap();
    until("the rebooted worker to exit", async || {
        (e.launcher().state(&unit).await.unwrap() != dvb_core::launcher::UnitState::Active)
            .then_some(())
    })
    .await;
    assert!(e.proj().runs[&run].ended.is_none());
    serving.abort();
    let _ = serving.await;
    default_planner(&tmp);
    let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    engine_up(&c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    plan_question(&e, &goal.id).await;
    idle(&e).await;
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Interrupted));
    assert_eq!(runs_for(&e, &goal.id).len(), 2);
    assert!(
        !e.proj()
            .questions
            .values()
            .any(|q| q.view.subject.contains("planned nothing"))
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_update_interrupted_planner_recovers_without_an_empty_hold() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let gate = gate(&tmp, "updating");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": gate, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let run = blocked(&e, &gate).await;
    owner::pause(&e, &ProjectId::new("p"), true).unwrap();
    e.update.close_admission();
    dispatch::stop_for_update(&e, "test-target").await;
    assert!(matches!(
        e.proj().runs[&run].ended,
        Some(EndReason::Stopped | EndReason::Interrupted | EndReason::Crashed)
    ));
    default_planner(&tmp);
    serving.abort();
    let _ = serving.await;
    let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    engine_up(&c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    owner::pause(&e, &ProjectId::new("p"), false).unwrap();
    plan_question(&e, &goal.id).await;
    idle(&e).await;
    assert_eq!(runs_for(&e, &goal.id).len(), 2);
    assert!(
        !e.proj()
            .questions
            .values()
            .any(|q| q.view.subject.contains("planned nothing"))
    );
    serving.abort();
}

/// A Fake unit with a lost source stop acknowledgement. Gate the target stop
/// so an empty completion can race its outstanding request after replay.
struct LostStopAck {
    run: RunId,
    state: Mutex<UnitState>,
    calls: AtomicU32,
    target_stopping: tokio::sync::Semaphore,
    confirm: tokio::sync::Semaphore,
    target_loses_ack: bool,
}

#[async_trait::async_trait]
impl Launcher for LostStopAck {
    async fn start(&self, unit: &UnitId) -> Result<(), LaunchError> {
        panic!("unexpected start while paused: {}", unit.run)
    }

    async fn stop(&self, unit: &UnitId) -> Result<(), LaunchError> {
        assert_eq!(unit.run, self.run);
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(LaunchError("lost stop acknowledgement".into()));
        }
        self.target_stopping.add_permits(1);
        self.confirm.acquire().await.unwrap().forget();
        if self.target_loses_ack {
            return Err(LaunchError("target lost stop acknowledgement".into()));
        }
        *self.state.lock().unwrap() = UnitState::Inactive;
        Ok(())
    }

    async fn state(&self, unit: &UnitId) -> Result<UnitState, LaunchError> {
        assert_eq!(unit.run, self.run);
        Ok(*self.state.lock().unwrap())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_unconfirmed_update_stop_survives_restart_and_recovers_without_a_hold() {
    unconfirmed_update_stop(false).await;
    unconfirmed_update_stop(true).await;
}

async fn unconfirmed_update_stop(target_loses_ack: bool) {
    let (tmp, seeded) = fixture().await;
    let cfg = seeded.cfg.clone();
    drop(seeded);
    let run = RunId::new("unconfirmed-update-stop");
    let launcher = Arc::new(LostStopAck {
        run: run.clone(),
        state: Mutex::new(UnitState::Active),
        calls: AtomicU32::new(0),
        target_stopping: tokio::sync::Semaphore::new(0),
        confirm: tokio::sync::Semaphore::new(0),
        target_loses_ack,
    });
    let e = Engine::open(
        cfg,
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let p = ProjectId::new("p");
    let goal = GoalId::new("updating-goal");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: goal.clone(),
            text: "Add a parser".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let version = e.proj().goals[&goal].version;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(dvb_core::RunAssignment {
            review_stage: None,
            run: run.clone(),
            task: None,
            goal_version: Some(version),
            role: RoleId::new("orchestrator"),
            hire: HireId::new("h-orchestrator"),
            model: "fake-1".into(),
            vendor: dvb_core::Vendor::Fake,
            effort: Effort::Medium,
            reason: "A goal was stated.".into(),
            reviews: None,
            base: None,
            routine: false,
        })])
    })
    .unwrap();
    e.remember();
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    let unit = UnitId {
        vendor: dvb_core::Vendor::Fake,
        run: run.clone(),
    };
    owner::pause(&e, &p, true).unwrap();
    e.update.close_admission();
    dispatch::stop_for_update(&e, "test-target").await;
    assert_eq!(launcher.calls.load(Ordering::SeqCst), 1);
    assert!(e.proj().runs[&run].ended.is_none());
    assert!(e.proj().runs[&run].stop_requested);
    assert_eq!(launcher.state(&unit).await.unwrap(), UnitState::Active);
    e.quiesce();
    let e = Engine::open(
        e.cfg.clone(),
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(launcher.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    e.remember();
    assert!(
        e.proj().runs[&run].stop_requested,
        "replay retains stop intent"
    );
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    engine_up(&c).await;
    let reconciling = tokio::spawn(reconcile::reconcile(e.clone()));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        launcher.target_stopping.acquire(),
    )
    .await
    .expect("the target must retry the outstanding stop")
    .unwrap()
    .forget();
    assert!(e.proj().runs[&run].interrupted);
    assert!(
        !support::events::all(&e).iter().any(|ev| {
            matches!(&ev.kind, EventKind::RunSurvivedRestart { run: id } if id == &run)
        })
    );
    // Exercise the same completion path a watcher uses while the target
    // still has an unacknowledged stop. This is not a natural completion.
    *launcher.state.lock().unwrap() = UnitState::Inactive;
    if target_loses_ack {
        dispatch::run_ended(&e, &run, EndReason::Completed)
            .await
            .unwrap();
    }
    launcher.confirm.add_permits(1);
    reconciling.await.unwrap().unwrap();
    idle(&e).await;
    let expected = if target_loses_ack {
        EndReason::Interrupted
    } else {
        EndReason::Stopped
    };
    assert_eq!(e.proj().runs[&run].ended, Some(expected.clone()));
    assert!(!e.proj().goals[&goal].planning_held);
    assert_eq!(launcher.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        support::events::all(&e)
            .iter()
            .filter(|ev| {
                matches!(&ev.kind, EventKind::RunStopRequested { run: id } if id == &run)
            })
            .count(),
        1,
        "stop retries retain one durable request"
    );

    // A further replay retains the stopped outcome, then unpause delivers one
    // real planner that saves a plan rather than asking for an empty-run retry.
    e.quiesce();
    serving.abort();
    let _ = serving.await;
    default_planner(&tmp);
    let e = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    engine_up(&c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    assert_eq!(e.proj().runs[&run].ended, Some(expected));
    assert!(!e.proj().goals[&goal].planning_held);
    owner::pause(&e, &ProjectId::new("p"), false).unwrap();
    plan_question(&e, &goal).await;
    idle(&e).await;
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal).len(), 2);
    assert!(!e.proj().questions.values().any(|q| {
        q.view.subject.contains("planned nothing")
            || q.view.subject.starts_with("The orchestrator stopped")
    }));
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_release_closes_old_question_and_rejects_stale_answers() {
    for redirect in [false, true] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        script(&tmp, "orchestrator-goal", &json!([]));
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let goal = state(&c, "Add a parser").await;
        empty_end(&e, &goal.id).await;
        let q = hold_question(&e, &goal.id).await;
        let old_version = e.proj().goals[&goal.id].version;
        let gate = gate(&tmp, "released-planner");
        let mut steps: serde_json::Value = serde_json::from_str(include_str!(
            "../../dvb-engine/src/fake_scripts/orchestrator-goal.json"
        ))
        .unwrap();
        steps
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"write": {"path": gate, "text": "ready"}}));
        script(&tmp, "orchestrator-goal", &steps);
        if redirect {
            let _: GoalView = c
                .post(
                    &format!("/goals/{}/redirect", goal.id),
                    &json!({"text": "Add a better parser"}),
                )
                .await
                .unwrap();
            assert_ne!(e.proj().goals[&goal.id].version, old_version);
        } else {
            e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
                Ok(vec![
                    EventKind::GoalStated {
                        goal: GoalId::new("other"),
                        text: "Other work".into(),
                        from_issue: None,
                        issues: Vec::new(),
                        after: None,
                    },
                    EventKind::GoalDropped {
                        goal: GoalId::new("other"),
                    },
                ])
            })
            .unwrap();
        }
        let (a, b) = tokio::join!(
            wake::wake(e.clone(), ProjectId::new("p"), wake::Wake::Goal, 0, 0),
            wake::wake(e.clone(), ProjectId::new("p"), wake::Wake::Goal, 0, 0)
        );
        a.unwrap();
        b.unwrap();
        let run = blocked(&e, &gate).await;
        passes(&e).await;
        assert!(!e.proj().questions[&q.id].view.open);
        for choice in [1, 2] {
            assert!(
                c.post::<_, QuestionView>(
                    &format!("/questions/{}/answer", q.id),
                    &Answer::Choice(choice)
                )
                .await
                .is_err()
            );
        }
        assert_eq!(e.proj().goals[&goal.id].state, GoalState::Stated);
        assert_eq!(
            e.proj()
                .runs
                .values()
                .filter(|r| r.assignment.task.is_none())
                .count(),
            2
        );
        release(&gate).await;
        plan_question(&e, &goal.id).await;
        idle(&e).await;
        passes(&e).await;
        assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Completed));
        assert_eq!(
            e.proj()
                .runs
                .values()
                .filter(|r| r.assignment.task.is_none())
                .count(),
            2
        );
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_retried_planner_saved_plan_recovers_approval_despite_old_hold_question() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    script(&tmp, "orchestrator-goal", &json!([]));
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    empty_end(&e, &goal.id).await;
    let q = hold_question(&e, &goal.id).await;
    script(
        &tmp,
        "orchestrator-goal",
        &json!([
            {"run": ["plan", {"tasks": [{"title": "{{goal_text}}", "role": "backend", "summary": "One edit"}]}]}
        ]),
    );
    let _: QuestionView = c
        .post(&format!("/questions/{}/answer", q.id), &Answer::Choice(1))
        .await
        .unwrap();
    let approval = plan_question(&e, &goal.id).await;
    idle(&e).await;
    passes(&e).await;
    assert_ne!(q.id, approval.id);
    assert!(approval.options.iter().any(|o| o.approves_plan));
    assert!(!e.proj().questions[&q.id].view.open);
    assert_eq!(runs_for(&e, &goal.id).len(), 2);
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_owner_drop_releases_another_held_goal_without_a_manual_wake() {
    for via_question in [false, true] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        script(&tmp, "orchestrator-goal", &json!([]));
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let a = state(&c, "Add a parser").await;
        let first = empty_end(&e, &a.id).await;
        let qa = hold_question(&e, &a.id).await;
        let b = state(&c, "Add an exporter").await;
        empty_end(&e, &b.id).await;
        let qb = hold_question(&e, &b.id).await;
        let next = gate(&tmp, "released-by-drop");
        script(
            &tmp,
            "orchestrator-goal",
            &json!([{ "write": {"path": next, "text": "ready"}}]),
        );
        if via_question {
            let _: QuestionView = c
                .post(&format!("/questions/{}/answer", qb.id), &Answer::Choice(2))
                .await
                .unwrap();
        } else {
            let _: GoalView = c
                .post(&format!("/goals/{}/drop", b.id), &json!({}))
                .await
                .unwrap();
        }
        // The actual owner action must supply recovery; no dispatcher helper
        // or direct flow wake is used to make this successor appear.
        let successor = blocked(&e, &next).await;
        assert_ne!(successor, first);
        assert_eq!(runs_for(&e, &a.id).len(), 2);
        assert_eq!(runs_for(&e, &b.id).len(), 1);
        assert_eq!(e.proj().goals[&b.id].state, GoalState::Dropped);
        assert!(!e.proj().questions[&qa.id].view.open);
        assert!(!e.proj().questions[&qb.id].view.open);
        release(&next).await;
        idle(&e).await;
        hold_question(&e, &a.id).await;
        assert_eq!(runs_for(&e, &a.id).len(), 2);
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_no_change_publication_releases_held_goal_without_a_manual_wake() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let builder_gate = gate(&tmp, "no-change-builder");
    script(
        &tmp,
        "builder",
        &json!([{ "write": {"path": builder_gate, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let building = state(&c, "Build a parser").await;
    let q = plan_question(&e, &building.id).await;
    support::approve(&c, &q).await;
    let builder = blocked(&e, &builder_gate).await;
    script(&tmp, "orchestrator-goal", &json!([]));
    let held = state(&c, "Plan an exporter").await;
    let first = empty_end(&e, &held.id).await;
    let question = hold_question(&e, &held.id).await;
    let task = e.proj().goals[&building.id].tasks[0].clone();
    let next = gate(&tmp, "released-by-no-changes");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": next, "text": "ready"}}]),
    );
    release(&builder_gate).await;
    until("the no-change publication to fail the task", async || {
        matches!(&e.proj().tasks[&task].view.state,
            TaskState::Failed { reason, .. } if reason.contains("no changes"))
        .then_some(())
    })
    .await;
    assert_eq!(e.proj().runs[&builder].ended, Some(EndReason::Completed));
    let successor = blocked(&e, &next).await;
    assert_ne!(successor, first);
    assert_eq!(runs_for(&e, &held.id).len(), 2);
    assert!(!e.proj().questions[&question.id].view.open);
    release(&next).await;
    idle(&e).await;
    hold_question(&e, &held.id).await;
    assert_eq!(runs_for(&e, &held.id).len(), 2);
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_planner_question_waits_for_actual_answer_and_releases_replayed_hold() {
    for prior_hold in [false, true] {
        let (tmp, e) = fixture().await;
        let _cleanup = RunCleanup(e.cfg.paths.work.clone());
        let asked = gate(&tmp, "after-asking");
        script(
            &tmp,
            "orchestrator-goal",
            &json!([
                {"run": ["ask", {
                    "subject": "Which grammar?",
                    "options": [{"text": "Current grammar", "effect": "Keep compatibility", "approves_plan": false}],
                    "recommended": 1
                }]},
                {"write": {"path": asked, "text": "ready"}}
            ]),
        );
        e.remember();
        let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
        let c = Client::new(e.cfg.paths.engine_sock.clone());
        engine_up(&c).await;
        let goal = state(&c, "Add a parser").await;
        let first = blocked(&e, &asked).await;
        let question = e
            .proj()
            .questions
            .values()
            .find(|q| q.view.subject == "Which grammar?")
            .unwrap()
            .view
            .clone();
        script(&tmp, "orchestrator-goal", &json!([]));
        release(&asked).await;
        idle(&e).await;
        passes(&e).await;
        assert_eq!(runs_for(&e, &goal.id), vec![first.clone()]);
        assert!(e.proj().questions[&question.id].view.open);
        assert!(!e.proj().goals[&goal.id].planning_held);

        // An unanswered planner question blocks only its own goal version.
        let other = state(&c, "Add an exporter").await;
        let other_run = empty_end(&e, &other.id).await;
        hold_question(&e, &other.id).await;
        passes(&e).await;
        assert_eq!(runs_for(&e, &goal.id), vec![first.clone()]);
        assert_eq!(runs_for(&e, &other.id), vec![other_run]);

        // Reproduce the durable empty successor emitted by older engines.
        // Recovery must accept the original question's answer even with this hold.
        let hold = if prior_hold {
            let mut assignment = e.proj().runs[&first].assignment.clone();
            assignment.run = RunId::new("historical-empty-successor");
            e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::RunStarted(assignment.clone()),
                    EventKind::RunEnded {
                        run: assignment.run,
                        reason: EndReason::Completed,
                        detail: None,
                        stderr: None,
                    },
                ])
            })
            .unwrap();
            wake::wake_if_missed(&e, &ProjectId::new("p"));
            Some(hold_question(&e, &goal.id).await)
        } else {
            None
        };
        owner::pause(&e, &ProjectId::new("p"), true).unwrap();
        let (e, serving) = restart(&e, serving, &c).await;
        reconcile::reconcile(e.clone()).await.unwrap();
        assert_eq!(e.proj().goals[&goal.id].planning_held, prior_hold);

        let answer = if prior_hold {
            Answer::Reply("Use grammar v2, preserving Unicode.".into())
        } else {
            Answer::Choice(1)
        };
        let _: QuestionView = c
            .post(&format!("/questions/{}/answer", question.id), &answer)
            .await
            .unwrap();
        assert!(!e.proj().goals[&goal.id].planning_held);
        if let Some(hold) = &hold {
            assert!(!e.proj().questions[&hold.id].view.open);
        }
        let answering = gate(&tmp, "answer-delivered");
        script(
            &tmp,
            "orchestrator-answer",
            &json!([
                {
                    "run": [
                        "plan",
                        {
                            "tasks": [
                                {
                                    "title": "Add a parser",
                                    "role": "backend",
                                    "summary": "Use the answered grammar"
                                }
                            ]
                        }
                    ]
                },
                {
                    "run": [
                        "ask",
                        {
                            "subject": "Approve parser?",
                            "options": [
                                {
                                    "text": "Yes",
                                    "effect": "Build it",
                                    "approves_plan": true
                                }
                            ],
                            "recommended": 1
                        }
                    ]
                },
                {
                    "write": {
                        "path": answering,
                        "text": "ready"
                    }
                }
            ]),
        );
        let baseline = runs_for(&e, &goal.id).len();
        // The accepted answer stays pending through another restart while paused.
        let (e, serving) = restart(&e, serving, &c).await;
        reconcile::reconcile(e.clone()).await.unwrap();
        assert!(!e.proj().goals[&goal.id].planning_held);
        owner::pause(&e, &ProjectId::new("p"), false).unwrap();
        let reply_run = blocked(&e, &answering).await;
        assert_eq!(runs_for(&e, &goal.id).len(), baseline + 1);
        assert_eq!(
            e.proj().runs[&reply_run].assignment.reason,
            "The owner answered a question."
        );
        let prompt =
            std::fs::read_to_string(e.cfg.paths.work.join(reply_run.as_str()).join("prompt.md"))
                .unwrap();
        let said = if prior_hold {
            "Use grammar v2, preserving Unicode."
        } else {
            "Current grammar (Keep compatibility)"
        };
        assert_eq!(
            prompt
                .matches(&format!("Which grammar? [goal: Add a parser] -> {said}"))
                .count(),
            1
        );
        assert!(!e.proj().goals[&goal.id].planning_held);
        release(&answering).await;
        idle(&e).await;
        passes(&e).await;
        assert_eq!(runs_for(&e, &goal.id).len(), baseline + 1);
        assert_eq!(e.proj().runs[&reply_run].ended, Some(EndReason::Completed));
        serving.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_surviving_planner_completes_normally_and_replays_without_redelivering_reply() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let asking = gate(&tmp, "asking");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([
            {
                "run": [
                    "ask",
                    {
                        "subject": "Which grammar?",
                        "options": [
                            {
                                "text": "Current",
                                "effect": "Keep it",
                                "approves_plan": false
                            }
                        ],
                        "recommended": 1
                    }
                ]
            },
            {
                "write": {
                    "path": asking,
                    "text": "ready"
                }
            }
        ]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    blocked(&e, &asking).await;
    let question = e
        .proj()
        .questions
        .values()
        .find(|q| q.view.subject == "Which grammar?")
        .unwrap()
        .view
        .clone();
    release(&asking).await;
    idle(&e).await;
    let before = gate(&tmp, "answer-before-restart");
    let after = gate(&tmp, "answer-after-restart");
    script(
        &tmp,
        "orchestrator-answer",
        &json!([
            {
                "write": {
                    "path": before,
                    "text": "ready"
                }
            },
            {
                "run": [
                    "plan",
                    {
                        "tasks": [
                            {
                                "title": "Add a parser",
                                "role": "backend",
                                "summary": "Use the owner's grammar"
                            }
                        ]
                    }
                ]
            },
            {
                "run": [
                    "ask",
                    {
                        "subject": "Approve parser?",
                        "options": [
                            {
                                "text": "Yes",
                                "effect": "Build it",
                                "approves_plan": true
                            }
                        ],
                        "recommended": 1
                    }
                ]
            },
            {
                "write": {
                    "path": after,
                    "text": "ready"
                }
            }
        ]),
    );
    let _: QuestionView = c
        .post(
            &format!("/questions/{}/answer", question.id),
            &Answer::Reply("Use Unicode grammar".into()),
        )
        .await
        .unwrap();
    let run = blocked(&e, &before).await;
    let prompt =
        std::fs::read_to_string(e.cfg.paths.work.join(run.as_str()).join("prompt.md")).unwrap();
    assert_eq!(
        prompt
            .matches("Which grammar? [goal: Add a parser] -> Use Unicode grammar")
            .count(),
        1
    );
    e.update.close_admission();
    let (e, serving) = restart(&e, serving, &c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    assert!(
        support::events::all(&e)
            .iter()
            .any(|ev| matches!(&ev.kind, EventKind::RunSurvivedRestart { run: id } if id == &run))
    );
    release(&before).await;
    assert_eq!(blocked(&e, &after).await, run);
    plan_question(&e, &goal.id).await;
    release(&after).await;
    idle(&e).await;
    passes(&e).await;
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Completed));
    assert_eq!(runs_for(&e, &goal.id).len(), 2);
    assert!(
        !e.proj()
            .questions
            .values()
            .any(|q| q.view.subject.starts_with("The orchestrator stopped"))
    );
    assert_eq!(
        support::events::all(&e)
            .iter()
            .filter(|ev| matches!(
                &ev.kind,
                EventKind::RunEnded { run: id, reason: EndReason::Completed, .. }
                    if id == &run
            ))
            .count(),
        1
    );
    let (e, serving) = restart(&e, serving, &c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal.id).len(), 2);
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Completed));
    assert!(!e.proj().runs[&run].interrupted);
    assert!(
        !e.proj()
            .questions
            .values()
            .any(|q| q.view.subject.starts_with("The orchestrator stopped"))
    );
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_empty_surviving_planner_completes_normally_and_is_held_after_replay() {
    let (tmp, e) = fixture().await;
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let before = gate(&tmp, "empty-before-restart");
    script(
        &tmp,
        "orchestrator-goal",
        &json!([{ "write": {"path": before, "text": "ready"}}]),
    );
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let goal = state(&c, "Add a parser").await;
    let run = blocked(&e, &before).await;
    e.update.close_admission();
    let (e, serving) = restart(&e, serving, &c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    release(&before).await;
    idle(&e).await;
    let hold = hold_question(&e, &goal.id).await;
    assert_eq!(e.proj().runs[&run].ended, Some(EndReason::Completed));
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal.id), vec![run.clone()]);
    let (e, serving) = restart(&e, serving, &c).await;
    reconcile::reconcile(e.clone()).await.unwrap();
    passes(&e).await;
    assert_eq!(runs_for(&e, &goal.id), vec![run]);
    assert_eq!(hold_question(&e, &goal.id).await.id, hold.id);
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_max_runs_raise_automatically_launches_waiting_fake_workers_of_every_role() {
    let (tmp, e) = fixture().await;
    // Concurrent fetches and worker clones must not race Git maintenance.
    support::git(
        &e.mirror("project"),
        &["config", "maintenance.auto", "false"],
    );
    let _cleanup = RunCleanup(e.cfg.paths.work.clone());
    let building = gate(&tmp, "capacity-builder");
    let reviewing = gate(&tmp, "capacity-reviewer");
    let planning = gate(&tmp, "capacity-planner");
    for (name, path) in [
        ("builder", &building),
        ("reviewer", &reviewing),
        ("orchestrator-goal", &planning),
    ] {
        script(
            &tmp,
            name,
            &json!([{"write": {"path": path, "text": "ready"}}]),
        );
    }
    e.remember();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let _: dvb_core::api::Settings = c
        .put("/settings", &json!({"values": {"max_runs": 1}}))
        .await
        .unwrap();
    let p = ProjectId::new("p");
    let g = GoalId::new("approved");
    let builder = TaskId::new("capacity-build");
    let reviewer = TaskId::new("capacity-review");
    let next = TaskId::new("capacity-next");
    let head = support::git(&e.mirror("project"), &["rev-parse", "main"]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::GoalStated {
                goal: g.clone(),
                text: "Approved work".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: g.clone(),
                tasks: [&builder, &reviewer, &next]
                    .into_iter()
                    .map(|t| PlannedTask {
                        after: vec![],
                        task: t.clone(),
                        title: t.to_string(),
                        role: RoleId::new("backend"),
                        review: ReviewKind::Code,
                        summary: "One edit".into(),
                        routine: false,
                    })
                    .collect(),
            },
            EventKind::GoalApproved { goal: g },
            EventKind::TaskWaiting {
                task: builder.clone(),
                model: None,
                effort: None,
                reason: "Waiting for capacity".into(),
                routine: false,
            },
            EventKind::BranchPublished {
                task: reviewer.clone(),
                round: 1,
                branch: "review-me".into(),
                head,
            },
            EventKind::TaskStateChanged {
                task: reviewer.clone(),
                from: TaskState::Planned,
                to: TaskState::InReview,
            },
        ])
    })
    .unwrap();
    let busy = RunId::new("other-project-run");
    e.append_checked(Some(&ProjectId::new("other")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(RunAssignment {
            review_stage: None,
            goal_version: None,
            run: busy.clone(),
            task: None,
            role: RoleId::new("orchestrator"),
            hire: HireId::new("h-orchestrator"),
            model: "fake-1".into(),
            vendor: dvb_core::Vendor::Fake,
            effort: Effort::Medium,
            reason: "Other project".into(),
            reviews: None,
            base: None,
            routine: false,
        })])
    })
    .unwrap();
    let waiting_goal = state(&c, "Waiting planner").await;
    dispatch::step_now(&e, &reviewer).await.unwrap();
    dispatch::start_waiting(e.clone()).await.unwrap();
    wake::wake(e.clone(), p.clone(), wake::Wake::Goal, 0, 0)
        .await
        .unwrap();
    assert_eq!(e.proj().live_runs().count(), 1);
    assert!(e.proj().tasks[&builder].facts.run.is_none());
    assert!(e.proj().tasks[&reviewer].facts.run.is_none());
    assert!(runs_for(&e, &waiting_goal.id).is_empty());

    // Only the settings write can wake these workers; no run ends or resumes.
    let _: dvb_core::api::Settings = c
        .put("/settings", &json!({"values": {"max_runs": 4}}))
        .await
        .unwrap();
    let b = blocked(&e, &building).await;
    let r = blocked(&e, &reviewing).await;
    let o = blocked(&e, &planning).await;
    {
        let proj = e.proj();
        assert_eq!(proj.live_runs().count(), 4);
        assert_eq!(proj.runs[&b].assignment.task.as_ref(), Some(&builder));
        assert_eq!(proj.runs[&r].assignment.task.as_ref(), Some(&reviewer));
        assert!(proj.runs[&r].assignment.reviews.is_some());
        drop(proj);
    }
    assert_eq!(
        runs_for(&e, &waiting_goal.id).as_slice(),
        std::slice::from_ref(&o)
    );
    let _: dvb_core::api::Settings = c
        .put("/settings", &json!({"values": {"max_runs": 1}}))
        .await
        .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::TaskWaiting {
            task: next.clone(),
            model: None,
            effort: None,
            reason: "Wait after lowering".into(),
            routine: false,
        }])
    })
    .unwrap();
    dispatch::start_waiting(e.clone()).await.unwrap();
    dispatch::step_now(&e, &reviewer).await.unwrap();
    assert_eq!(blocked(&e, &building).await, b);
    assert_eq!(blocked(&e, &reviewing).await, r);
    assert_eq!(blocked(&e, &planning).await, o);
    assert_eq!(e.proj().live_runs().count(), 4);
    assert!(
        [&b, &r, &o]
            .into_iter()
            .all(|run| e.proj().runs[run].ended.is_none())
    );
    e.end_run(&busy, EndReason::Completed, None).unwrap();
    dispatch::start_waiting(e.clone()).await.unwrap();
    assert_eq!(e.proj().live_runs().count(), 3);
    assert!(e.proj().tasks[&next].facts.run.is_none());
    // Repeating the same setting and recovery cannot launch duplicates.
    let _: dvb_core::api::Settings = c
        .put("/settings", &json!({"values": {"max_runs": 1}}))
        .await
        .unwrap();
    assert_eq!(runs_for(&e, &waiting_goal.id), [o]);
    for task in [&builder, &reviewer] {
        assert_eq!(
            e.proj()
                .runs
                .values()
                .filter(|r| r.assignment.task.as_ref() == Some(task))
                .count(),
            1
        );
    }
    e.quiesce();
    serving.abort();
}
