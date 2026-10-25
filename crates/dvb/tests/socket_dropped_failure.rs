//! Owner status/API, kept evidence and legacy fail/drop replay.
pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::TaskView;
use dvb_core::event::EndReason;
use dvb_core::{
    Actor, Effort, Event, EventKind, HireId, ProjectId, RefusalCode, RoleId, RunAssignment, RunId,
    TaskId, TaskState, Vendor, VerdictId,
};
use dvb_engine::{Config, Engine, publish};
use std::process::Command;
use support::{engine_up, git, seed_origin, until};

struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Serving {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_dropped_failures_are_cancelled_in_status_with_evidence_kept() {
    let tmp = tempfile::Builder::new()
        .prefix("dvbdrop")
        .tempdir()
        .unwrap();
    let origin = seed_origin(tmp.path(), "main");
    let cfg = Config::dev(&tmp.path().join("e"), env!("CARGO_BIN_EXE_dvb").into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    let mirror = e.mirror("project");
    publish::create_mirror(&origin.to_string_lossy(), &mirror)
        .await
        .unwrap();
    let base = git(&mirror, &["rev-parse", "main"]);
    let run = RunId::new("builder");
    let t = TaskId::new("failed");
    let p = ProjectId::new("p");
    let ws = e.cfg.paths.work.join(run.as_str()).join("ws");
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            mirror.to_str().unwrap(),
            ws.to_str().unwrap(),
        ],
    );
    git(&ws, &["config", "user.name", publish::NEUTRAL_NAME]);
    git(&ws, &["config", "user.email", publish::NEUTRAL_EMAIL]);
    std::fs::write(ws.join("kept.txt"), "kept workspace\n").unwrap();
    git(&ws, &["add", "kept.txt"]);
    let tree = git(&ws, &["write-tree"]);
    let head = git(
        &ws,
        &["commit-tree", &tree, "-p", &base, "-m", "kept evidence"],
    );
    git(&ws, &["update-ref", "refs/dvb/collected", &head]);
    git(
        &mirror,
        &[
            "fetch",
            "--quiet",
            ws.to_str().unwrap(),
            "refs/dvb/collected:refs/heads/dvb/failed-1",
        ],
    );
    let legacy: Vec<Event> = serde_json::from_str(include_str!(
        "../../dvb-engine/tests/fixtures/legacy_fail_then_drop.json"
    ))
    .unwrap();
    let mut kinds = Vec::new();
    for ev in legacy {
        if matches!(&ev.kind, EventKind::GoalDropped { .. }) {
            continue;
        }
        if matches!(&ev.kind, EventKind::TaskStateChanged { task, .. } if task == &t) {
            kinds.extend([
                EventKind::RunStarted(RunAssignment {
                    review_stage: None,
                    goal_version: None,
                    run: run.clone(),
                    task: Some(t.clone()),
                    role: RoleId::new("backend"),
                    hire: HireId::new("h"),
                    model: "fake-1".into(),
                    vendor: Vendor::Fake,
                    effort: Effort::Medium,
                    reason: "Build".into(),
                    reviews: None,
                    base: Some(base.clone()),
                    routine: false,
                }),
                EventKind::RunEnded {
                    run: run.clone(),
                    reason: EndReason::Completed,
                    detail: None,
                    stderr: None,
                },
                EventKind::BranchPublished {
                    task: t.clone(),
                    round: 1,
                    branch: "dvb/failed-1".into(),
                    head: head.clone(),
                },
                EventKind::PrOpened {
                    task: t.clone(),
                    pr: 1,
                    url: "https://example.invalid/pr/1".into(),
                },
                EventKind::VerdictRecorded {
                    review_stage: None,
                    verdict: VerdictId::new("v"),
                    task: t.clone(),
                    head: head.clone(),
                    run: RunId::new("review"),
                    role: RoleId::new("code-reviewer"),
                    model: "fake-1".into(),
                    vendor: Vendor::Fake,
                    findings: vec![],
                    summary: "Kept review".into(),
                },
            ]);
        }
        kinds.push(ev.kind);
    }
    kinds.push(EventKind::ProjectPaused {});
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(kinds))
        .unwrap();
    let before = support::events::all(&e);
    let kept_task = e.proj().tasks[&t].clone();
    let serving = Serving(tokio::spawn(dvb_engine::serve::serve(e.clone())));
    let c = Client::new(cfg.paths.engine_sock.clone());
    engine_up(&c).await;
    let view: TaskView = c.get("/tasks/failed").await.unwrap();
    assert!(matches!(view.state, TaskState::Failed { .. }));
    let _: serde_json::Value = c.post("/goals/dropped/drop", &()).await.unwrap();
    let view: TaskView = c.get("/tasks/failed").await.unwrap();
    assert_eq!(view.state, TaskState::Cancelled);
    assert_eq!(view.reason.as_deref(), Some("its goal was dropped"));
    assert_eq!(view.head, Some(head.clone()));
    assert_eq!(view.pr, Some(1));
    let list: Vec<TaskView> = c.get("/tasks?project=p").await.unwrap();
    assert_eq!(list.iter().find(|t| t.id.as_str() == "failed"), Some(&view));
    assert!(matches!(
        list.iter()
            .find(|t| t.id.as_str() == "kept-failed")
            .unwrap()
            .state,
        TaskState::Failed { .. }
    ));
    for id in ["stopped", "kept-stopped"] {
        assert_eq!(
            list.iter().find(|t| t.id.as_str() == id).unwrap().state,
            TaskState::Stopped {
                was: Box::new(TaskState::Running)
            }
        );
    }
    assert_eq!(
        list.iter()
            .find(|t| t.id.as_str() == "stopped-planned")
            .unwrap()
            .state,
        TaskState::Stopped {
            was: Box::new(TaskState::Planned)
        }
    );
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(&cfg.paths.engine_sock)
        .args(["status", "project", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(
        status
            .lines()
            .any(|line| line == "Task failed: failed - Cancelled: its goal was dropped"),
        "{status}"
    );
    assert!(
        status.contains("Task kept-failed: kept-failed - Failed: push failed: refused"),
        "{status}"
    );
    let diff: serde_json::Value = c.get("/tasks/failed/diff").await.unwrap();
    assert_eq!(diff["head"], head);
    assert!(diff["diff"].as_str().unwrap().contains("+kept workspace"));
    let verdict: serde_json::Value = c.get("/tasks/failed/verdict").await.unwrap();
    assert_eq!(verdict["summary"], "Kept review");
    let run_view: serde_json::Value = c.get("/runs/builder").await.unwrap();
    assert_eq!(run_view["model"], "fake-1");
    for (id, expected) in [
        ("failed", RefusalCode::AlreadyTerminal),
        ("stopped", RefusalCode::AlreadyStopped),
    ] {
        let result = c
            .post::<_, TaskView>(&format!("/tasks/{id}/retry"), &())
            .await;
        match result {
            Err(CliError::Refused(r)) => assert_eq!(r.code, expected),
            other => panic!("{other:?}"),
        }
    }
    let records = support::events::all(&e);
    assert_eq!(
        serde_json::to_value(&records[..before.len()]).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    // The evidence is in the mirror and the log; nothing resumes from the
    // dropped task's run directory.
    let run_dir = e.cfg.paths.work.join(run.as_str());
    until("the dropped task's run directory to go", async || {
        (!run_dir.exists()).then_some(())
    })
    .await;
    {
        let proj = e.proj();
        let task = &proj.tasks[&t];
        assert_eq!(task.branch, kept_task.branch);
        assert_eq!(task.facts.builder, kept_task.facts.builder);
        assert_eq!(task.facts.verdict, kept_task.facts.verdict);
        assert_eq!(task.failed, kept_task.failed);
        drop(proj);
    }
    drop(serving);
    let rebuilt = Engine::open(cfg, Default::default()).unwrap();
    assert_eq!(rebuilt.proj().tasks[&t].view, view);
    let after = support::events::all(&rebuilt);
    assert_eq!(after.len(), records.len() + 1);
    assert_eq!(
        serde_json::to_value(&after[..records.len()]).unwrap(),
        serde_json::to_value(records).unwrap()
    );
    assert!(matches!(
        after.last().unwrap().kind,
        EventKind::EngineStarted { .. }
    ));
}
