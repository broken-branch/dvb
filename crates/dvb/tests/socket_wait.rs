//! `dvb wait` through the real CLI against a dev engine seeded with Fake records.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::event::{CiMode, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, EventId, EventKind, GoalId, ProjectId, QuestionId, RoleId, TaskId, TaskState,
};
use dvb_engine::{AppState, Config, Engine};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use support::engine_up;

struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Serving {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    e: AppState,
    sock: PathBuf,
    /// Each goal's `goal.stated` event, the version its questions lapse on.
    versions: BTreeMap<&'static str, EventId>,
}

impl Fixture {
    /// Project `p` with goals `g` and `other`, each planned with one task
    /// (`g-task`, `other-task`) and approved unless `approved` is false. The
    /// project has no roles, so nothing starts; a paused project ends a wait.
    fn new(approved: bool) -> Fixture {
        let tmp = tempfile::Builder::new()
            .prefix("dvbwait")
            .tempdir()
            .unwrap();
        let dir = tmp.path().join("e");
        let e = Engine::open(
            Config::dev(&dir, env!("CARGO_BIN_EXE_dvb").into()),
            Default::default(),
        )
        .unwrap();
        let mut seed = vec![EventKind::ProjectAdded {
            id: ProjectId::new("p"),
            name: "p".into(),
            repo: "/nowhere".into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        }];
        for goal in ["g", "other"] {
            seed.extend([
                EventKind::GoalStated {
                    goal: GoalId::new(goal),
                    text: "Work".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal: GoalId::new(goal),
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: TaskId::new(format!("{goal}-task")),
                        title: "Build".into(),
                        role: RoleId::new("backend"),
                        review: ReviewKind::Code,
                        summary: "x".into(),
                        routine: false,
                    }],
                },
            ]);
            if approved {
                seed.push(EventKind::GoalApproved {
                    goal: GoalId::new(goal),
                });
            }
        }
        let events = e
            .append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(seed))
            .unwrap();
        let versions = events
            .iter()
            .filter_map(|ev| {
                let EventKind::GoalStated { goal, .. } = &ev.kind else {
                    return None;
                };
                Some((if goal.as_str() == "g" { "g" } else { "other" }, ev.id))
            })
            .collect();
        let sock = dir.join("engine.sock");
        Fixture {
            _tmp: tmp,
            e,
            sock,
            versions,
        }
    }

    async fn serve(&self) -> Serving {
        let serving = Serving(tokio::spawn(dvb_engine::serve::serve(self.e.clone())));
        engine_up(&Client::new(self.sock.clone())).await;
        serving
    }

    fn append(&self, kinds: Vec<EventKind>) {
        self.e
            .append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(kinds))
            .unwrap();
    }

    fn question(&self, id: &str, goal: &str, plan: bool) -> EventKind {
        let option = |id, text: &str, effect: &str, approves_plan| QuestionOption {
            id,
            text: text.into(),
            effect: effect.into(),
            approves_plan,
            no_effect: false,
            retries_task: None,
        };
        EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new(id),
            subject: "Approve the plan?".into(),
            format: QuestionFormat::Simple,
            options: vec![
                option(1, "Approve", "Start the tasks", plan),
                option(2, "Not yet", "Hold the plan", false),
            ],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(self.versions[goal]),
        }
    }
}

fn state(task: &str, to: TaskState) -> EventKind {
    EventKind::TaskStateChanged {
        task: TaskId::new(task),
        from: TaskState::Planned,
        to,
    }
}

fn failed(task: &str, reason: &str) -> EventKind {
    state(
        task,
        TaskState::Failed {
            reason: reason.into(),
        },
    )
}

fn cmd(sock: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dvb"));
    cmd.arg("--sock").arg(sock).arg("wait").args(args);
    cmd
}

fn wait(sock: &Path, args: &[&str]) -> (i32, String) {
    let out = cmd(sock, args).output().unwrap();
    result(&out)
}

fn result(out: &Output) -> (i32, String) {
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout.clone()).unwrap(),
    )
}

/// A wait left blocking while the test changes the engine; `--timeout 1`
/// bounds it if it never ends.
fn spawn(sock: &Path, args: &[&str]) -> tokio::task::JoinHandle<Output> {
    let child = cmd(sock, args)
        .args(["--timeout", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
}

const PLAN: &str = "Question q-plan: Approve the plan?\n  1. Approve - Start the tasks (recommended)\n  2. Not yet - Hold the plan\n";

#[tokio::test(flavor = "multi_thread")]
async fn until_plan_returns_the_plan_question_with_its_id_and_options() {
    let f = Fixture::new(false);
    let _serving = f.serve().await;
    assert_eq!(
        wait(&f.sock, &["g", "--until", "plan", "--timeout", "0"]),
        (4, String::new())
    );
    let waiting = spawn(&f.sock, &["g", "--until", "plan"]);
    f.append(vec![f.question("q-plan", "g", true)]);
    assert_eq!(result(&waiting.await.unwrap()), (0, PLAN.into()));
    // Already open when it starts: at once, with no timeout.
    assert_eq!(wait(&f.sock, &["g", "--until", "plan"]), (0, PLAN.into()));
    let (code, json) = wait(&f.sock, &["g", "--until", "plan", "--json"]);
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["outcome"], "plan_question", "{json}");
    assert_eq!(json["question"]["id"], "q-plan");
    assert_eq!(
        json["question"]["options"][0],
        serde_json::json!({"id": 1, "text": "Approve", "effect": "Start the tasks"})
    );
    // Waiting for Done, the plan question is a question of the goal.
    assert_eq!(wait(&f.sock, &["g"]), (7, PLAN.into()));
    // The owner's yes closes the question: the plan is no longer asked.
    let _: serde_json::Value = Client::new(f.sock.clone())
        .post(
            "/questions/q-plan/answer",
            &dvb_core::event::Answer::Choice(1),
        )
        .await
        .unwrap();
    assert_eq!(
        wait(&f.sock, &["g", "--until", "plan"]),
        (0, "Goal g: Work - Approved\n".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn done_exits_0_with_the_goal_s_merged_pull_requests() {
    let f = Fixture::new(true);
    let _serving = f.serve().await;
    let waiting = spawn(&f.sock, &["g", "--until", "done"]);
    f.append(vec![
        state("other-task", TaskState::Done),
        EventKind::PrMerged {
            task: TaskId::new("other-task"),
            pr: 6,
            sha: "1111".into(),
        },
        EventKind::GoalDone {
            goal: GoalId::new("other"),
        },
        state("g-task", TaskState::Done),
        EventKind::PrMerged {
            task: TaskId::new("g-task"),
            pr: 7,
            sha: "abcd".into(),
        },
        EventKind::GoalDone {
            goal: GoalId::new("g"),
        },
    ]);
    let done = "Goal g: Done\nMerged PR #7 as abcd (task g-task)\n";
    assert_eq!(result(&waiting.await.unwrap()), (0, done.into()));
    assert_eq!(wait(&f.sock, &["g"]), (0, done.into()));
    let (code, json) = wait(&f.sock, &["g", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::json!({
            "outcome": "done", "goal": "g", "state": "done",
            "merged": [{"task": "g-task", "pr": 7, "commit": "abcd"}],
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_of_the_goal_exits_7_and_one_of_another_goal_does_not() {
    let f = Fixture::new(true);
    let _serving = f.serve().await;
    f.append(vec![f.question("q-other", "other", false)]);
    assert_eq!(wait(&f.sock, &["g", "--timeout", "0"]), (4, String::new()));
    let waiting = spawn(&f.sock, &["g"]);
    f.append(vec![f.question("q-plan", "g", false)]);
    assert_eq!(result(&waiting.await.unwrap()), (7, PLAN.into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_usage_error_alone_exits_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("wait")
        .output()
        .unwrap();
    assert_eq!(result(&out), (2, String::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_task_of_the_goal_exits_3_and_one_of_another_goal_does_not() {
    let f = Fixture::new(true);
    let _serving = f.serve().await;
    f.append(vec![failed("other-task", "other boom")]);
    assert_eq!(wait(&f.sock, &["g", "--timeout", "0"]), (4, String::new()));
    let waiting = spawn(&f.sock, &["g"]);
    f.append(vec![failed("g-task", "boom")]);
    let failure = "Task g-task failed: boom\nRetry it: dvb retry g-task\n";
    assert_eq!(result(&waiting.await.unwrap()), (3, failure.into()));
    // Already failed when it starts: at once.
    assert_eq!(wait(&f.sock, &["g"]), (3, failure.into()));
    assert_eq!(
        wait(&f.sock, &["other"]),
        (
            3,
            "Task other-task failed: other boom\nRetry it: dvb retry other-task\n".into()
        )
    );
    let (code, json) = wait(&f.sock, &["g", "--json"]);
    assert_eq!(code, 3);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::json!({
            "outcome": "failed", "goal": "g", "state": "failed",
            "task": {"id": "g-task", "title": "Build", "reason": "boom",
                     "command": "dvb retry g-task"},
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_task_of_the_goal_exits_6_with_its_resume_command() {
    let f = Fixture::new(true);
    let _serving = f.serve().await;
    let client = Client::new(f.sock.clone());
    let stop = |task: &'static str| {
        let client = &client;
        async move {
            let body = dvb_core::api::StopTask {
                reason: Some("not before Monday".into()),
            };
            let _: serde_json::Value = client
                .post(&format!("/tasks/{task}/stop"), &body)
                .await
                .unwrap();
        }
    };
    stop("other-task").await;
    assert_eq!(wait(&f.sock, &["g", "--timeout", "0"]), (4, String::new()));
    let waiting = spawn(&f.sock, &["g"]);
    stop("g-task").await;
    let stopped = "Task g-task stopped: not before Monday\nResume it: dvb resume g-task\n";
    assert_eq!(result(&waiting.await.unwrap()), (6, stopped.into()));
    let (code, json) = wait(&f.sock, &["g", "--json"]);
    assert_eq!(code, 6);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["outcome"], "stopped", "{json}");
    // Its only task ended stopped: the goal has not failed.
    assert_eq!(json["state"], "stopped", "{json}");
    assert_eq!(
        json["task"],
        serde_json::json!({"id": "g-task", "title": "Build", "reason": "not before Monday",
                           "command": "dvb resume g-task"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_project_exits_6_and_needs_you_with_dvb_resume_project() {
    let f = Fixture::new(false);
    let _serving = f.serve().await;
    assert_eq!(wait(&f.sock, &["g", "--timeout", "0"]), (4, String::new()));
    let waiting = spawn(&f.sock, &["g", "--until", "plan"]);
    f.append(vec![EventKind::ProjectPaused {}]);
    let paused = "Project p is paused: nothing new starts until dvb resume p\n";
    assert_eq!(result(&waiting.await.unwrap()), (6, paused.into()));
    let (code, json) = wait(&f.sock, &["g", "--json"]);
    assert_eq!(code, 6);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::json!({
            "outcome": "project_paused", "goal": "g", "state": "stated",
            "command": "dvb resume p",
        })
    );
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(&f.sock)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        status["needs_you_items"],
        serde_json::json!([{
            "kind": "project", "id": "p", "project": "p", "project_name": "p",
            "title": "p", "command": "dvb resume p",
        }])
    );
    // A question of the goal is printed before the pause.
    f.append(vec![f.question("q-plan", "g", false)]);
    assert_eq!(wait(&f.sock, &["g"]), (7, PLAN.into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_on_hold_exits_6_and_needs_you_with_dvb_resume_goal() {
    let f = Fixture::new(false);
    let _serving = f.serve().await;
    f.append(vec![f.question("q-plan", "g", true)]);
    let _: serde_json::Value = Client::new(f.sock.clone())
        .post(
            "/questions/q-plan/answer",
            &dvb_core::event::Answer::Choice(2),
        )
        .await
        .unwrap();
    let parked = "Goal g: Work - Plan on hold\nAsk it again: dvb resume g\n";
    assert_eq!(wait(&f.sock, &["g", "--until", "plan"]), (6, parked.into()));
    assert_eq!(wait(&f.sock, &["g"]), (6, parked.into()));
    let (code, json) = wait(&f.sock, &["g", "--until", "plan", "--json"]);
    assert_eq!(code, 6);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::json!({
            "outcome": "plan_parked", "goal": "g", "state": "stated",
            "command": "dvb resume g",
        })
    );
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(&f.sock)
        .args(["status", "p", "--json"])
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // The engine asks about the other goal's saved plan on its own.
    let items = status["needs_you_items"].as_array().unwrap();
    assert_eq!(
        items.iter().find(|i| i["id"] == "g"),
        Some(&serde_json::json!({
            "kind": "plan", "id": "g", "project": "p", "project_name": "p",
            "title": "Work", "command": "dvb resume g",
        })),
        "{status}"
    );
    // Resuming asks the plan again under a new id, which --json gives.
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(&f.sock)
        .args(["resume", "g", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let resumed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let again = resumed["question"]["id"].as_str().unwrap().to_owned();
    assert_ne!(again, "q-plan");
    assert_eq!(
        resumed,
        serde_json::json!({
            "goal": {"id": "g", "project": "p", "text": "Work", "state": "stated",
                     "plan_parked": false},
            "question": {
                "id": again, "project": "p", "goal": "g", "task": null,
                "subject": "Approve the plan?", "recommended": 1,
                "options": [
                    {"id": 1, "text": "Approve", "effect": "Start the tasks"},
                    {"id": 2, "text": "Not yet", "effect": "Hold the plan"},
                ],
            },
        })
    );
    let (code, json) = wait(&f.sock, &["g", "--until", "plan", "--json"]);
    assert_eq!(code, 0);
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["question"]["id"], again.as_str(), "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_dropped_goal_exits_5() {
    let f = Fixture::new(true);
    let _serving = f.serve().await;
    let out = cmd(&f.sock, &["nope", "--timeout", "0"]).output().unwrap();
    assert_eq!(result(&out), (5, String::new()));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "dvb: refused: NOT_FOUND (no goal nope)\n"
    );
    let waiting = spawn(&f.sock, &["g"]);
    f.append(vec![EventKind::GoalDropped {
        goal: GoalId::new("g"),
    }]);
    assert_eq!(
        result(&waiting.await.unwrap()),
        (5, "Goal g: Work - Dropped\n".into())
    );
}

/// The engine on its own runtime, so shutting it down closes every connection.
fn serve_runtime(e: &AppState) -> tokio::runtime::Runtime {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.spawn(dvb_engine::serve::serve(e.clone()));
    rt
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wait_reconnects_while_the_engine_is_down_and_carries_on() {
    use std::io::BufRead;
    let f = Fixture::new(true);
    let mut child = cmd(&f.sock, &["g", "--timeout", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let reconnecting = async |lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>| {
        let line = lines.recv().await.unwrap();
        assert!(line.ends_with("; reconnecting"), "{line}");
    };
    // No engine yet: it retries.
    reconnecting(&mut lines).await;
    let first = serve_runtime(&f.e);
    engine_up(&Client::new(f.sock.clone())).await;
    while lines.try_recv().is_ok() {}
    first.shutdown_background();
    reconnecting(&mut lines).await;
    let second = serve_runtime(&f.e);
    engine_up(&Client::new(f.sock.clone())).await;
    f.append(vec![
        state("g-task", TaskState::Done),
        EventKind::GoalDone {
            goal: GoalId::new("g"),
        },
    ]);
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert_eq!(result(&out), (0, "Goal g: Done\n".into()));
    second.shutdown_background();
}
