//! Update draining with an actual run driver (`dvb run exec`, a
//! Fake builder in the middle of its edit): at the drain's bound the update
//! stops the run the owner's way before the exec, so no driver spans engine
//! versions, and the stopped work resumes from its kept workspace to a merge.
//! The engine runs in-process on an injected clock; the exec is a stand-in that
//! fails, so the same engine carries on as the target would.

pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{QuestionView, TaskView};
use dvb_core::event::EndReason;
use dvb_core::{Clock, EventKind, FakeClock, SystemClock, TaskState};
use dvb_engine::update::handover::Layout;
use dvb_engine::update::protocol;
use dvb_engine::{Config, Engine};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use support::{add_project, approve, git, seed_origin, state_goal, until};

#[tokio::test(flavor = "multi_thread")]
async fn socket_an_update_stops_a_running_driver_before_the_exec_and_it_resumes() {
    // Short: a socket path must fit in 108 bytes.
    let tmp = tempfile::Builder::new().prefix("dvbu").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");
    let dir = d.join("e");
    std::fs::create_dir_all(dir.join("fake")).unwrap();
    std::fs::write(
        dir.join("fake/builder.json"),
        r#"[{"write": {"path": "docs/first.md", "text": "first\n"}},
            {"sleep_ms": 60000},
            {"write": {"path": "docs/never.md", "text": "never\n"}}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("fake/builder-resume.json"),
        r#"[{"write": {"path": "docs/second.md", "text": "second\n"}}]"#,
    )
    .unwrap();

    let clock = Arc::new(FakeClock::at(SystemClock.now()));
    let cfg = Config::dev(&dir, PathBuf::from(env!("CARGO_BIN_EXE_dvb")));
    let e = Engine::open(
        cfg.clone(),
        dvb_engine::EngineOptions {
            clock: Some(clock.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let engine = tokio::spawn(dvb_engine::start(e.clone()));
    let c = Client::new(dir.join("engine.sock"));
    until("the engine", async || {
        c.get::<serde_json::Value>("/version").await.ok()
    })
    .await;
    let p = add_project(&c, &origin).await;
    state_goal(&c, &p, "Write two notes").await;
    let q = until("the plan question", async || {
        let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
        open.into_iter().next()
    })
    .await;
    approve(&c, &q).await;
    let tasks: Vec<TaskView> = c.get(&format!("/tasks?project={}", p.id)).await.unwrap();
    let task = tasks[0].id.clone();
    // The driver is mid-edit: the first file is written, the run still works.
    let run_dir = until("the first edit", async || {
        let work = std::fs::read_dir(dir.join("work")).ok()?;
        work.filter_map(Result::ok)
            .map(|e| e.path())
            .find(|run| run.join("ws/docs/first.md").exists())
    })
    .await;
    let pid = std::fs::read_to_string(run_dir.join("pid")).unwrap();
    let alive = move || Path::new(&format!("/proc/{}", pid.trim())).exists();
    assert!(alive());

    // What the old engine saw at the exec.
    let at_exec = Arc::new(Mutex::new(None));
    let probe: protocol::Exec = {
        let (e, at_exec, alive, task) = (e.clone(), at_exec.clone(), alive.clone(), task.clone());
        Arc::new(move |_: &Path| {
            let state = e.proj().tasks[&task].view.state.clone();
            *at_exec.lock().unwrap() = Some((protocol::runs_active(&e), alive(), state));
            std::io::Error::other("test: the target is not started")
        })
    };
    let to = "99.0.0".to_owned();
    assert!(e.update.begin(&to));
    e.update.close_admission();
    let handover = {
        let (e, to) = (e.clone(), to.clone());
        tokio::spawn(async move {
            let l = Layout::of(&e.cfg);
            protocol::handover(&e, &l, &to, &probe).await
        })
    };
    // Past the drain's bound while the driver still works.
    while !handover.is_finished() {
        clock.advance_millis(protocol::DRAIN_MILLIS);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    handover.await.unwrap();
    let (active, driver_alive, state) = at_exec.lock().unwrap().take().expect("the exec");
    assert_eq!(active, 0, "no run spans the update");
    assert!(!driver_alive, "the driver was stopped before the exec");
    assert_eq!(
        state,
        TaskState::Stopped {
            was: Box::new(TaskState::Running)
        }
    );
    let events = support::events::all(&e);
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::RunEnded {
            reason: EndReason::Stopped,
            ..
        }
    )));
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::TaskStopped { reason: Some(r), .. } if r.contains("update to dvb 99.0.0")
    )));
    assert!(run_dir.join("ws/docs/first.md").exists());

    // The stopped work resumes from its workspace and merges.
    let _: TaskView = c.post(&format!("/tasks/{task}/resume"), &()).await.unwrap();
    until("the task to be done", async || {
        let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    for f in ["docs/first.md", "docs/second.md"] {
        git(&origin, &["cat-file", "-e", &format!("main:{f}")]);
    }
    let files = git(&origin, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("never.md"), "{files}");
    engine.abort();
}
