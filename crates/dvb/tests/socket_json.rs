//! The `--json` outputs through the real CLI and offline socket fixtures or
//! a dev engine seeded with a paused project, so nothing runs.
#[path = "../../../tests/support/http.rs"]
pub mod http;
pub mod support;

use dvb_cli::client::Client;
use dvb_core::event::{CiMode, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Effort, EventId, EventKind, GoalId, HireId, ProjectId, QuestionId, RoleId,
    RunAssignment, RunId, TaskId, TaskState, Vendor,
};
use dvb_engine::{Config, Engine};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};
use support::engine_up;

fn dvb(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

/// The command's stdout as one JSON value; it must succeed.
fn json_of(dir: &Path, args: &[&str]) -> Value {
    let out = dvb(dir, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}: {out:?}"))
}

struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Serving {
    fn drop(&mut self) {
        self.0.abort();
    }
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

fn question(id: &str, task: Option<&str>, lapses_on: LapsesOn) -> EventKind {
    let option = |id, text: &str, effect: &str| QuestionOption {
        id,
        text: text.into(),
        effect: effect.into(),
        approves_plan: false,
        no_effect: false,
        retries_task: None,
    };
    EventKind::QuestionOpened {
        kind: Some(dvb_core::event::QuestionKind::Ordinary),
        main_ci: None,
        task: task.map(TaskId::new),
        question: QuestionId::new(id),
        subject: format!("Subject {id}"),
        format: QuestionFormat::Simple,
        options: vec![
            option(1, "Yes", "Continue"),
            option(2, "Wait", "Keep waiting"),
        ],
        recommended: 2,
        lapses_on,
    }
}

/// Project `p`, paused, with role `r-backend` (`backend`): goal `g` in
/// progress with a running task `t-run` on PR 7, a failed task `t-fail` and a
/// planned task `t-stop`, a done goal `old`, a question on `g`, one on
/// `t-fail` and one on a setting.
async fn seeded(dir: &Path) -> (std::sync::Arc<Engine>, Serving) {
    let e = Engine::open(
        Config::dev(dir, env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let p = ProjectId::new("p");
    let planned = |task: &str| PlannedTask {
        after: vec![],
        task: TaskId::new(task),
        title: format!("Title {task}"),
        role: RoleId::new("r-backend"),
        review: ReviewKind::Code,
        summary: "x".into(),
        routine: false,
    };
    let state = |task: &str, to| EventKind::TaskStateChanged {
        task: TaskId::new(task),
        from: TaskState::Planned,
        to,
    };
    let seed = vec![
        EventKind::ProjectAdded {
            id: p.clone(),
            name: "p".into(),
            repo: "/nowhere".into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        },
        EventKind::ProjectPaused {},
        EventKind::RoleAdded(dvb_core::roles::RoleRecord {
            id: RoleId::new("r-backend"),
            slug: "backend".into(),
            name: "Backend builder".into(),
            job: String::new(),
            lane: String::new(),
            lead: false,
            reports_to: None,
            reviewed_by: None,
            review_instructions: String::new(),
            group: None,
            order: 0,
        }),
        goal("old", "Finished work"),
        EventKind::PlanProposed {
            goal: GoalId::new("old"),
            tasks: vec![planned("t-old")],
        },
        EventKind::GoalApproved {
            goal: GoalId::new("old"),
        },
        state("t-old", TaskState::Done),
        goal("g", "Ship it\nSecond line"),
        EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![planned("t-run"), planned("t-fail"), planned("t-stop")],
        },
        EventKind::GoalApproved {
            goal: GoalId::new("g"),
        },
        state("t-run", TaskState::Running),
        EventKind::PrOpened {
            task: TaskId::new("t-run"),
            pr: 7,
            url: "https://example.invalid/pr/7".into(),
        },
        state(
            "t-fail",
            TaskState::Failed {
                reason: "boom".into(),
            },
        ),
        EventKind::RunStarted(RunAssignment {
            review_stage: None,
            goal_version: None,
            run: RunId::new("r"),
            task: Some(TaskId::new("t-run")),
            role: RoleId::new("r-backend"),
            hire: HireId::new("h"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "Build".into(),
            reviews: None,
            base: None,
            routine: false,
        }),
    ];
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(seed))
        .unwrap();
    let version = e.proj().goals[&GoalId::new("g")].version;
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            question("q-goal", None, LapsesOn::GoalVersion(version)),
            question("q-task", Some("t-fail"), LapsesOn::GoalVersion(version)),
            question("q-setting", None, LapsesOn::ValueVersion(EventId(0))),
        ])
    })
    .unwrap();
    let serving = Serving(tokio::spawn(dvb_engine::serve::serve(e.clone())));
    engine_up(&Client::new(dir.join("engine.sock"))).await;
    (e, serving)
}

fn by_id<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {list}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_project_add_json_prints_one_project_without_discovery_text() {
    use axum::http::Method;

    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(tmp.path().join("engine.sock")).unwrap();
    let mut requests = http::requests(listener);
    let server = tokio::spawn(async move {
        http::reply(
            &mut requests,
            Method::GET,
            "/github/checks?repo=example-org%2Fproject",
            200,
            json!({"status": "discovered", "default_branch": "main", "head": "abc", "checks": [
                {"name": "rust", "status": "pending", "run": 3, "conclusion": null, "url": null}
            ]}),
        )
        .await;
        http::reply(&mut requests, Method::GET, "/version", 200, json!({})).await;
        http::reply(
            &mut requests,
            Method::POST,
            "/projects",
            201,
            json!({
                "id": "p-added", "name": "example", "repo": "example-org/project",
                "default_branch": "main", "ci": {"expected": ["rust"]}, "paused": false
            }),
        )
        .await;
    });

    let out = dvb(
        tmp.path(),
        &[
            "project",
            "add",
            "example",
            "--repo",
            "example-org/project",
            "--json",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Detected check:"),
        "{out:?}"
    );
    let project: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(project.is_object(), "{project}");
    assert_eq!(project["id"], "p-added");
    assert_eq!(project["name"], "example");
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_status_json_has_the_counts_goals_tasks_questions_and_items() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (e, _serving) = seeded(dir).await;
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::TaskStateChanged {
                task: TaskId::new("t-stop"),
                from: TaskState::Planned,
                to: TaskState::Stopped {
                    was: Box::new(TaskState::Planned),
                },
            },
            EventKind::TaskStopped {
                update: false,
                task: TaskId::new("t-stop"),
                reason: Some("not before Monday".into()),
            },
        ])
    })
    .unwrap();

    let overall = json_of(dir, &["status", "--json"]);
    assert_eq!(overall["running"], 1, "{overall}");
    assert_eq!(overall["needs_you"], 5, "{overall}");
    assert!(overall.get("goals").is_none(), "{overall}");
    let questions = &overall["questions"];
    assert_eq!(questions.as_array().unwrap().len(), 3, "{overall}");
    let q = by_id(questions, "q-goal");
    assert_eq!(q["subject"], "Subject q-goal");
    assert_eq!(q["goal"], "g");
    assert_eq!(q["recommended"], 2);
    assert_eq!(
        q["options"],
        json!([
            {"id": 1, "text": "Yes", "effect": "Continue"},
            {"id": 2, "text": "Wait", "effect": "Keep waiting"},
        ])
    );
    assert_eq!(by_id(questions, "q-task")["task"], "t-fail");
    let items = &overall["needs_you_items"];
    let failed = by_id(items, "t-fail");
    assert_eq!(failed["kind"], "task");
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["command"], "dvb retry t-fail");
    assert_eq!(failed["reason"], "boom");
    assert_eq!(failed["project_name"], "p");
    let stopped = by_id(items, "t-stop");
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["command"], "dvb resume t-stop");
    assert_eq!(stopped["reason"], "not before Monday");
    assert_eq!(by_id(items, "q-goal")["command"], "dvb answer q-goal <n>");
    assert_eq!(by_id(items, "q-goal")["kind"], "question");
    assert_eq!(by_id(items, "q-goal")["project_name"], "p");
    // The paused project is listed, though not counted.
    assert_eq!(
        by_id(items, "p"),
        &json!({"kind": "project", "id": "p", "project": "p", "project_name": "p",
                "title": "p", "command": "dvb resume p"})
    );

    let scoped = json_of(dir, &["status", "p", "--json"]);
    assert_eq!(
        scoped["project"],
        json!({"id": "p", "name": "p", "paused": true})
    );
    assert_eq!(scoped["finished_goals_hidden"], 1, "{scoped}");
    let goals = &scoped["goals"];
    assert_eq!(goals.as_array().unwrap().len(), 1, "{scoped}");
    assert_eq!(by_id(goals, "g")["text"], "Ship it\nSecond line");
    assert_eq!(by_id(goals, "g")["state"], "in_progress");
    let tasks = &scoped["tasks"];
    assert_eq!(tasks.as_array().unwrap().len(), 3, "{scoped}");
    let running = by_id(tasks, "t-run");
    assert_eq!(running["state"], "running");
    assert_eq!(running["pr"], 7);
    assert_eq!(running["role"], "r-backend");
    assert_eq!(running["role_name"], "backend");
    assert_eq!(running["title"], "Title t-run");
    // The same reason as its needs-you item.
    assert_eq!(by_id(tasks, "t-stop")["reason"], "not before Monday");
    assert_eq!(running["goal"], "g");
    assert_eq!(by_id(tasks, "t-fail")["pr"], Value::Null);
    assert_eq!(by_id(tasks, "t-fail")["reason"], "boom");

    let all = json_of(dir, &["status", "p", "--all", "--json"]);
    assert_eq!(all["finished_goals_hidden"], 0, "{all}");
    assert_eq!(by_id(&all["goals"], "old")["state"], "done");
    assert_eq!(by_id(&all["tasks"], "t-old")["state"], "done");

    let text = dvb(dir, &["status", "p"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("Project p is paused: nothing new starts until dvb resume p\n"),
        "{text}"
    );

    let listed = json_of(dir, &["project", "list", "--json"]);
    let listed = by_id(&listed["projects"], "p");
    assert_eq!(listed["name"], "p", "{listed}");
    assert_eq!(listed["paused"], true);
}

/// A refusal or a name or id with nothing behind it: `dvb: refused: <CODE>
/// (<detail>)` on stderr and exit 1; 2 is only for a usage error.
#[tokio::test(flavor = "multi_thread")]
async fn socket_refusals_and_unknown_names_exit_1_in_one_form() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (_e, _serving) = seeded(dir).await;
    for (args, detail) in [
        (&["status", "nope", "--json"][..], "no project nope"),
        (&["goal", "nope", "Some", "work"], "no project nope"),
        (&["task", "verdict", "BOGUS", "--json"], ""),
        (&["resume", "nope"], "(no task, goal or project nope)\n"),
    ] {
        let out = dvb(dir, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.starts_with("dvb: refused: NOT_FOUND (") && stderr.contains(detail),
            "{args:?}: {stderr}"
        );
    }
    let usage = dvb(dir, &["answer", "q-goal"]);
    assert_eq!(usage.status.code(), Some(2), "{usage:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_stop_pause_and_resume_json_print_what_they_changed() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (_e, _serving) = seeded(dir).await;
    let task = |id: &str, state: &str, reason: Value| {
        json!({"task": {
            "id": id, "goal": "g", "state": state, "title": format!("Title {id}"),
            "role": "r-backend", "role_name": "backend", "pr": null, "reason": reason,
            "waiting_for": [],
        }})
    };

    let stopped = json_of(
        dir,
        &["stop", "t-stop", "--reason", "not before Monday", "--json"],
    );
    assert_eq!(
        stopped,
        task("t-stop", "stopped", json!("not before Monday")),
        "{stopped}"
    );

    let resumed = json_of(dir, &["resume", "p", "--json"]);
    assert_eq!(resumed["project"]["id"], "p", "{resumed}");
    assert_eq!(resumed["project"]["name"], "p");
    assert_eq!(resumed["project"]["paused"], false);

    let resumed = json_of(dir, &["resume", "t-stop", "--json"]);
    assert_eq!(resumed["task"]["id"], "t-stop", "{resumed}");
    assert_ne!(resumed["task"]["state"], "stopped");

    let paused = json_of(dir, &["pause", "p", "--json"]);
    assert_eq!(paused["project"]["id"], "p", "{paused}");
    assert_eq!(paused["project"]["paused"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_answer_json_names_the_answer_and_its_goal_and_task() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (_e, _serving) = seeded(dir).await;

    let chose = json_of(dir, &["answer", "q-goal", "2", "--json"]);
    assert_eq!(chose["question"], "q-goal", "{chose}");
    assert_eq!(chose["answer"], json!({"choice": 2}));
    assert_eq!(chose["option"]["text"], "Wait");
    assert_eq!(chose["goal"], "g");
    assert_eq!(chose["task"], Value::Null);

    let replied = json_of(dir, &["answer", "q-task", "--reply", "Try again", "--json"]);
    assert_eq!(
        replied["answer"],
        json!({"reply": "Try again"}),
        "{replied}"
    );
    assert_eq!(replied["option"], Value::Null);
    assert_eq!(replied["task"], "t-fail");
    assert_eq!(replied["goal"], "g");

    let setting = json_of(dir, &["answer", "q-setting", "1", "--json"]);
    assert_eq!(setting["goal"], Value::Null, "{setting}");
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_goal_json_prints_the_id_and_the_drop_and_after_results() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (_e, _serving) = seeded(dir).await;

    let stated = json_of(dir, &["goal", "p", "Second", "goal", "--json"]);
    let id = stated["goal"].as_str().unwrap().to_owned();
    assert_eq!(stated, json!({"goal": id}));

    let after = json_of(dir, &["goal", "after", &id, "g", "--json"]);
    assert_eq!(after["id"], id.as_str(), "{after}");
    assert_eq!(after["text"], "Second goal");
    assert_eq!(after["waits_for"], "g");
    assert_eq!(after["state"], "stated");

    let dropped = json_of(dir, &["goal", "drop", &id, "--json"]);
    assert_eq!(dropped["id"], id.as_str(), "{dropped}");
    assert_eq!(dropped["state"], "dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_watch_json_prints_one_event_per_line_and_resumes_after_an_id() {
    let tmp = tempfile::Builder::new().prefix("dvbj").tempdir().unwrap();
    let dir = tmp.path();
    let (e, _serving) = seeded(dir).await;

    let lines = |args: &[&str]| -> Vec<Value> {
        let mut all = vec!["watch", "--once", "--json"];
        all.extend(args);
        let out = dvb(dir, &all);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect()
    };
    let first = lines(&[]);
    let stated = first
        .iter()
        .find(|ev| ev["kind"] == "goal.stated" && ev["payload"]["goal"] == "g")
        .unwrap();
    assert_eq!(stated["project"], "p");
    assert_eq!(stated["payload"]["text"], "Ship it\nSecond line");
    assert_eq!(stated["text"], "Goal: Ship it\nSecond line");
    assert!(stated["time"].as_str().unwrap().ends_with('Z'), "{stated}");
    assert!(
        first
            .iter()
            .all(|ev| ev["id"].is_u64() && ev["kind"].is_string())
    );
    let last = first.last().unwrap()["id"].as_u64().unwrap();

    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![goal("later", "Later")])
    })
    .unwrap();
    let after = last.to_string();
    let next = lines(&["--after", &after]);
    assert!(!next.is_empty());
    assert!(
        next.iter().all(|ev| ev["id"].as_u64().unwrap() > last),
        "{next:?}"
    );
    assert!(
        next.iter().any(|ev| ev["payload"]["goal"] == "later"),
        "{next:?}"
    );
}
