//! Compact status through the real CLI and a dev engine with Fake run records.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::event::{CiMode, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Effort, EventId, EventKind, GoalId, HireId, ProjectId, QuestionId, RoleId,
    RunAssignment, RunId, TaskId, TaskState, Vendor,
};
use dvb_engine::{Config, Engine};
use std::path::Path;
use std::process::Command;
use support::engine_up;

fn status(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .arg("status")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn project(id: &str) -> Vec<EventKind> {
    vec![
        EventKind::ProjectAdded {
            id: ProjectId::new(id),
            name: id.into(),
            repo: "/nowhere".into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        },
        EventKind::ProjectPaused {},
    ]
}

fn goal(id: &str, text: &str) -> EventKind {
    EventKind::GoalStated {
        goal: GoalId::new(id),
        text: text.into(),
        from_issue: None,
        issues: Vec::new(),
        after: None,
    }
}

fn question(id: &str) -> EventKind {
    EventKind::QuestionOpened {
        kind: Some(dvb_core::event::QuestionKind::Ordinary),
        main_ci: None,
        task: None,
        question: QuestionId::new(id),
        subject: "Proceed?".into(),
        format: QuestionFormat::Simple,
        options: vec![
            QuestionOption {
                id: 1,
                text: "Yes".into(),
                effect: "Continue".into(),
                approves_plan: false,
                no_effect: false,
                retries_task: None,
            },
            QuestionOption {
                id: 2,
                text: "Wait".into(),
                effect: "Keep waiting".into(),
                approves_plan: false,
                no_effect: false,
                retries_task: None,
            },
        ],
        recommended: 2,
        lapses_on: LapsesOn::ValueVersion(EventId(0)),
    }
}

fn run(id: &str, task: Option<TaskId>) -> EventKind {
    EventKind::RunStarted(RunAssignment {
        review_stage: None,
        goal_version: None,
        run: RunId::new(id),
        task,
        role: RoleId::new("backend"),
        hire: HireId::new("h"),
        model: "fake-1".into(),
        vendor: Vendor::Fake,
        effort: Effort::Medium,
        reason: "Build".into(),
        reviews: None,
        base: None,
        routine: false,
    })
}

struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Serving {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_status_filters_only_finished_goals_and_preserves_other_output() {
    let tmp = tempfile::Builder::new()
        .prefix("dvbstatus")
        .tempdir()
        .unwrap();
    let dir = tmp.path().join("e");
    let e = Engine::open(
        Config::dev(&dir, env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let unicode = format!("{}🦀", "é".repeat(159));
    let mut seed = project("p");
    // c-failed is p's last goal and ended failed, so its failed task needs
    // the owner as well as p's question.
    // Seed out of API order, with task order different from goal order.
    for (id, text, task, state) in [
        ("b-done", "Completed", "z-done", TaskState::Done),
        ("a-drop", "Dropped", "y-drop", TaskState::Cancelled),
        (
            "d-running",
            "In progress\r\nHidden second line",
            "a-running",
            TaskState::Running,
        ),
        (
            "c-failed",
            &format!("{unicode}more\nHidden failure details"),
            "b-failed",
            TaskState::Failed {
                reason: "boom".into(),
            },
        ),
    ] {
        seed.extend([
            goal(id, text),
            EventKind::PlanProposed {
                goal: GoalId::new(id),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: TaskId::new(task),
                    title: task.into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new(id),
            },
            EventKind::TaskStateChanged {
                task: TaskId::new(task),
                from: TaskState::Planned,
                to: state,
            },
        ]);
    }
    seed.extend([
        EventKind::GoalDropped {
            goal: GoalId::new("a-drop"),
        },
        run("p-run", Some(TaskId::new("a-running"))),
        question("p-question"),
    ]);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(seed))
        .unwrap();

    let ascii = "a".repeat(160);
    let mut seed = project("q");
    seed.extend([
        goal("q-stated", &format!("{ascii}x\nSecond line")),
        goal("q-approved", &format!("{ascii}\nSecond line")),
        EventKind::GoalApproved {
            goal: GoalId::new("q-approved"),
        },
        run("q-run", None),
        question("q-question"),
    ]);
    e.append_checked(Some(&ProjectId::new("q")), Actor::Engine, |_, _| Ok(seed))
        .unwrap();
    let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e)));
    engine_up(&Client::new(dir.join("engine.sock"))).await;

    let questions = |id| {
        format!(
            "Needs you: Proceed? ({id})\n  1. Yes - Continue\n  2. Wait - Keep waiting (recommended)\n"
        )
    };
    let active_goals = format!(
        "Goal c-failed: {unicode}… - Some tasks failed or were stopped\nGoal d-running: In \
                progress - In progress\n"
    );
    let active_tasks =
        "Task a-running: a-running - Working\nTask b-failed: b-failed - Failed: boom\n";
    let failed = "Needs you: b-failed (b-failed) - Failed: dvb retry b-failed\n";
    let paused = "Project p is paused: nothing new starts until dvb resume p\n";
    assert_eq!(
        status(&dir, &["p"]),
        format!(
            "1 running, 2 need you\n{paused}{active_goals}2 finished goals not shown (dvb status p \
                --all)\n{active_tasks}{failed}{}",
            questions("p-question")
        )
    );
    assert_eq!(
        status(&dir, &["p", "--all"]),
        format!(
            "1 running, 2 need you\n{paused}Goal a-drop: Dropped - Dropped\nGoal b-done: Completed - \
                Done\n{active_goals}{active_tasks}Task y-drop: y-drop - Cancelled\nTask \
                z-done: z-done - Done\n{failed}{}",
            questions("p-question")
        )
    );
    let unfinished = format!(
        "1 running, 1 need you\nProject q is paused: nothing new starts until dvb resume q\nGoal q-approved: {ascii} - Approved\nGoal q-stated: {ascii}… - Stated\n{}",
        questions("q-question")
    );
    assert_eq!(status(&dir, &["q"]), unfinished);
    assert_eq!(status(&dir, &["q", "--all"]), unfinished);

    let overall = format!(
        "2 running, 3 need you\n{failed}{}{}",
        questions("p-question"),
        questions("q-question")
    );
    assert_eq!(status(&dir, &[]), overall);
    assert_eq!(status(&dir, &["--all"]), overall);
    for flags in [vec!["--bar"], vec!["--bar", "--all"]] {
        assert_eq!(status(&dir, &flags), "{\"running\":2,\"needs_you\":3}\n");
        let mut scoped = vec!["p"];
        scoped.extend(flags);
        assert_eq!(status(&dir, &scoped), "{\"running\":1,\"needs_you\":2}\n");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_status_shows_memory_wait_and_clears_it_for_builders_and_reviewers() {
    for state in [TaskState::Preparing, TaskState::InReview] {
        let tmp = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
            Default::default(),
        )
        .unwrap();
        let p = ProjectId::new("p");
        let task = TaskId::new("held-task");
        let mut seed = project("p");
        seed.extend([
            goal("g", "Work"),
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Held work".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: state.clone(),
            },
        ]);
        let mut started = run("held-run", Some(task));
        if let EventKind::RunStarted(a) = &mut started
            && state == TaskState::InReview
        {
            a.reviews = Some("head".into());
            a.review_stage = Some(ReviewKind::Code);
        }
        seed.push(started);
        seed.push(EventKind::RunOutput {
            resource: None,
            run: RunId::new("held-run"),
            item: dvb_core::event::OutputItem::Note(
                "Waiting for memory: 0 GB available, needs 10 GB".into(),
            ),
        });
        e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(seed))
            .unwrap();
        let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e.clone())));
        engine_up(&Client::new(tmp.path().join("engine.sock"))).await;
        assert!(status(tmp.path(), &["p"]).contains("Waiting for memory"));
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunOutput {
                resource: None,
                run: RunId::new("held-run"),
                item: dvb_core::event::OutputItem::Note(
                    "Resources available; starting run.".into(),
                ),
            }])
        })
        .unwrap();
        let released = status(tmp.path(), &["p"]);
        assert!(!released.contains("Waiting for memory"));
        if state == TaskState::InReview {
            assert!(released.contains("code review"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_status_lists_each_counted_task_once_with_its_stopped_question() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Engine::open(
        Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    for name in ["p", "q"] {
        let p = ProjectId::new(name);
        let mut seed = project(name);
        seed.push(goal(name, "Work"));
        seed.push(EventKind::PlanProposed {
            goal: GoalId::new(name),
            tasks: ["one", "two", "resume", "failed"]
                .iter()
                .map(|suffix| PlannedTask {
                    after: vec![],
                    task: TaskId::new(format!("{name}-{suffix}")),
                    title: (*suffix).into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                })
                .collect(),
        });
        e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(seed))
            .unwrap();
        for suffix in ["one", "two", "resume", "failed"] {
            let id = format!("{name}-{suffix}");
            let task = TaskId::new(&id);
            let to = if suffix == "failed" {
                TaskState::Failed {
                    reason: "failed".into(),
                }
            } else {
                TaskState::Stopped {
                    was: Box::new(TaskState::InReview),
                }
            };
            let mut q = question(&id);
            if let EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                task: attributed,
                subject,
                options,
                recommended,
                lapses_on,
                ..
            } = &mut q
            {
                *attributed = (suffix != "two").then_some(task.clone());
                if suffix == "two" {
                    *lapses_on = LapsesOn::GoalVersion(e.proj().goals[&GoalId::new(name)].version);
                }
                *subject = format!("Question {id}");
                options[1].text = "Leave it stopped".into();
                *recommended = if suffix == "resume" { 1 } else { 2 };
            }
            // Historical taskless question immediately follows the stopped event.
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::TaskStateChanged {
                        task: task.clone(),
                        from: TaskState::InReview,
                        to,
                    },
                    EventKind::TaskStopped {
                        task,
                        reason: Some("review cap".into()),
                    },
                    q,
                ])
            })
            .unwrap();
        }
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![question(&format!("{name}-general"))])
        })
        .unwrap();
    }
    let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e)));
    engine_up(&Client::new(tmp.path().join("engine.sock"))).await;
    let global = status(tmp.path(), &[]);
    // Per project: four counted tasks, and the general, resume and failed
    // questions; one and two leave their stopped task stopped and count with it.
    assert!(global.starts_with("0 running, 14 need you\n"), "{global}");
    assert_eq!(global.matches("Needs you:").count(), 14, "{global}");
    for name in ["p", "q"] {
        for (suffix, line) in [
            ("failed", "Failed: dvb retry"),
            ("one", "Stopped: dvb resume"),
            ("resume", "Stopped: dvb resume"),
            ("two", "Stopped: dvb resume"),
        ] {
            let task = format!("Needs you: {suffix} ({name}-{suffix}) - {line} {name}-{suffix}\n");
            assert_eq!(global.matches(&task).count(), 1, "{global}");
        }
        for (suffix, shown) in [
            ("one", false),
            ("two", false),
            ("resume", true),
            ("failed", true),
        ] {
            let question = format!("Needs you: Question {name}-{suffix} ({name}-{suffix})\n");
            assert_eq!(global.contains(&question), shown, "{global}");
        }
        assert!(global.contains(&format!("{name}-general")), "{global}");
        assert!(!global.contains("Or answer"), "{global}");
        let scoped = status(tmp.path(), &[name]);
        assert!(scoped.starts_with("0 running, 7 need you\n"), "{scoped}");
        assert_eq!(scoped.matches("Needs you:").count(), 7, "{scoped}");
        assert!(
            scoped.contains(&format!(
                "Needs you: one ({name}-one) - Stopped: dvb resume {name}-one\n  Or answer: \
                Question {name}-one ({name}-one)\n  1. Yes - Continue\n  2. Leave it stopped - \
                Keep waiting (recommended)\n"
            )),
            "{scoped}"
        );
        for suffix in ["one", "two", "resume", "failed"] {
            assert!(
                scoped.contains(&format!("Question {name}-{suffix}")),
                "{scoped}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_status_lists_a_task_stopped_with_no_question() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Engine::open(
        Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let mut seed = project("p");
    seed.extend([
        goal("g", "Work"),
        EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![PlannedTask {
                after: vec![],
                task: TaskId::new("held"),
                title: "Held work".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "x".into(),
                routine: false,
            }],
        },
        EventKind::GoalApproved {
            goal: GoalId::new("g"),
        },
        EventKind::TaskStateChanged {
            task: TaskId::new("held"),
            from: TaskState::Planned,
            to: TaskState::Running,
        },
    ]);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(seed))
        .unwrap();
    let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e)));
    let client = Client::new(tmp.path().join("engine.sock"));
    engine_up(&client).await;
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(tmp.path())
        .args(["stop", "held", "--reason", "Holding for a release"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let questions: Vec<dvb_core::api::QuestionView> =
        client.get("/questions?open=true").await.unwrap();
    assert!(questions.is_empty(), "{questions:?}");
    assert_eq!(
        status(tmp.path(), &[]),
        "0 running, 1 need you\nNeeds you: Held work (held) - Stopped: dvb resume held\n"
    );
    assert_eq!(
        status(tmp.path(), &["--bar"]),
        "{\"running\":0,\"needs_you\":1}\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_goal_after_reaches_the_owner_api_and_status() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Engine::open(
        Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let mut seed = project("p");
    seed.push(goal("predecessor", "First"));
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(seed))
        .unwrap();
    let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e.clone())));
    let client = Client::new(tmp.path().join("engine.sock"));
    engine_up(&client).await;
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(tmp.path())
        .args(["goal", "p", "--after", "predecessor", "Second goal"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(status(tmp.path(), &["p"]).contains("Second goal - Stated; waits for predecessor"));
    let events = support::events::all(&e);
    let stated = events
        .iter()
        .find(|ev| matches!(&ev.kind, EventKind::GoalStated { text, .. } if text == "Second goal"))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&stated.kind).unwrap()["payload"]["after"],
        "predecessor"
    );
    let _: dvb_core::api::GoalView = client.post("/goals/predecessor/drop", &()).await.unwrap();
    let released = status(tmp.path(), &["p"]);
    assert!(released.contains("Second goal - Stated"));
    assert!(!released.contains("waits for predecessor"));
}
