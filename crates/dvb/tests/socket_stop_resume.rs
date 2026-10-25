//! Stop and resume in dev mode, through the real binary: a fake builder is
//! stopped with `dvb stop` in the middle of its edit and resumed with `dvb
//! resume`; the task ends Done with the first run's edit in the merged tree.
//! In the second test the engine is killed (`kill -9`) after the stop and
//! started again: the task shows unknown until it is checked, then stopped, and
//! resumes the same way.

pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::{QuestionView, TaskView};
use dvb_core::event::EndReason;
use dvb_core::{Event, EventKind, RefusalCode, TaskId, TaskState};
use std::path::{Path, PathBuf};
use std::process::Command;
use support::{
    Engine, add_project, approve, engine_cmd, engine_up, git, seed_origin, state_goal, until,
};

/// `dvb --dev <dir> <args>`: its exit code and stdout.
fn dvb(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

struct Setup {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    dir: PathBuf,
    engine: Engine,
    c: Client,
    task: TaskId,
}

/// A repo, an engine, a goal approved, and its builder stopped in the middle of
/// its edit with `dvb stop`.
async fn stopped_mid_edit() -> Setup {
    let tmp = tempfile::Builder::new().prefix("dvbt").tempdir().unwrap();
    let d = tmp.path();
    let origin = seed_origin(d, "main");

    // The first builder writes, then works for a long time; the resumed one
    // only adds its own file.
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

    let engine = Engine::spawn(engine_cmd(&dir));
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
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

    // Mid-edit: the first file is written, the run is still working.
    let ws = until("the first edit", async || {
        let work = std::fs::read_dir(dir.join("work")).ok()?;
        work.filter_map(Result::ok)
            .map(|e| e.path().join("ws"))
            .find(|ws| ws.join("docs/first.md").exists())
    })
    .await;
    // Build output the agent made: in the run's build dir and in `ws/target`.
    let run_dir = ws.parent().unwrap().to_owned();
    let built = run_dir.join("build");
    std::fs::create_dir_all(built.join("debug")).unwrap();
    std::fs::write(built.join("debug/bin"), "built").unwrap();
    // Uncommitted source in a directory named `target` is the project's work.
    std::fs::create_dir_all(ws.join("target")).unwrap();
    std::fs::write(ws.join("target/new-source.md"), "mine\n").unwrap();
    let (code, out) = dvb(
        &dir,
        &["stop", task.as_str(), "--reason", "Hold for the other PR"],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Stopped while running"), "{out}");
    let t: TaskView = c.get(&format!("/tasks/{task}")).await.unwrap();
    assert!(
        matches!(t.state, TaskState::Stopped { .. }),
        "{:?}",
        t.state
    );
    match c
        .post::<_, TaskView>(&format!("/tasks/{task}/stop"), &())
        .await
    {
        Err(CliError::Refused(r)) => assert_eq!(r.code, RefusalCode::AlreadyStopped),
        other => panic!("a second stop: {other:?}"),
    }
    // The run ended as stopped; its workspace is kept with the edit.
    until("the run to end", async || {
        let (mut stopped, mut reason) = (false, None);
        c.read_events(0, true, |ev: Event| {
            stopped |= matches!(
                ev.kind,
                EventKind::RunEnded {
                    reason: EndReason::Stopped,
                    ..
                }
            );
            if let EventKind::TaskStopped { reason: r, .. } = ev.kind {
                reason = r;
            }
            true
        })
        .await
        .ok()?;
        assert_eq!(reason.as_deref(), Some("Hold for the other PR"));
        stopped.then_some(())
    })
    .await;
    assert!(ws.join("docs/first.md").exists());
    // The end dropped the build output, never the work.
    until("the build output to go", async || {
        (!run_dir.join("build").exists()).then_some(())
    })
    .await;
    assert!(ws.join("docs/first.md").exists());
    assert!(ws.join("target/new-source.md").exists());
    Setup {
        _tmp: tmp,
        origin,
        dir,
        engine,
        c,
        task,
    }
}

/// `dvb resume`; the task ends Done with both runs' edits and nothing the
/// stopped run would have written after the stop.
async fn resume_to_done(s: &Setup) -> Vec<&'static str> {
    let (code, out) = dvb(&s.dir, &["resume", s.task.as_str()]);
    assert_eq!(code, 0, "{out}");
    until("the task to be done", async || {
        let t: TaskView = s.c.get(&format!("/tasks/{}", s.task)).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    })
    .await;
    for f in ["docs/first.md", "docs/second.md"] {
        git(&s.origin, &["cat-file", "-e", &format!("main:{f}")]);
    }
    let files = git(&s.origin, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!files.contains("never.md"), "{files}");

    let mut resumed_run = false;
    let mut kinds = Vec::new();
    s.c.read_events(0, true, |ev: Event| {
        resumed_run |= matches!(ev.kind, EventKind::TaskStarted { resume: true, .. });
        kinds.push(ev.kind.kind());
        true
    })
    .await
    .unwrap();
    assert!(resumed_run, "{kinds:?}");
    for k in ["task.stopped", "task.resumed", "pr.merged", "task.done"] {
        assert_eq!(
            kinds.iter().filter(|x| **x == k).count(),
            1,
            "{k} in {kinds:?}"
        );
    }
    kinds
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_stopped_edit_survives_resume_and_merges() {
    let s = stopped_mid_edit().await;
    resume_to_done(&s).await;
}

/// Stop mid-edit, `kill -9` the engine, start it again, resume.
#[tokio::test(flavor = "multi_thread")]
async fn socket_a_stopped_edit_survives_kill_9_restart_and_resume() {
    let mut s = stopped_mid_edit().await;
    s.engine.0.kill().unwrap();
    s.engine.0.wait().unwrap();
    s.engine = Engine::spawn(engine_cmd(&s.dir));
    until("the check after the restart", async || {
        let mut checked = false;
        s.c.read_events(0, true, |ev: Event| {
            checked |= matches!(ev.kind, EventKind::EngineReconciled { .. });
            true
        })
        .await
        .ok()?;
        checked.then_some(())
    })
    .await;
    // `dvb watch` shows the task unknown from the restart until it was checked,
    // then stopped again: never running, never done.
    let (code, watch) = dvb(&s.dir, &["watch", "--once"]);
    assert_eq!(code, 0);
    let after = watch.rsplit("engine.started").next().unwrap();
    let unknown = after.find("\"unknown\"").expect(&watch);
    let checked = after.find("engine.reconciled").expect(&watch);
    assert!(unknown < checked, "{after}");
    let t: TaskView = s.c.get(&format!("/tasks/{}", s.task)).await.unwrap();
    assert!(
        matches!(t.state, TaskState::Stopped { .. }),
        "{:?}",
        t.state
    );
    let kinds = resume_to_done(&s).await;
    assert_eq!(kinds.iter().filter(|k| **k == "engine.started").count(), 2);
}
