//! The crash-injection suite. One fake task, goal
//! to merge, with the engine killed at one failpoint per test: after an effect
//! was sent and before its result was recorded, or before a batch of events was
//! written. The engine runs in-process on its own runtime, so a crash stops all
//! of it at once; the Fake forge (GitHub) and the runs (their units) outlive it,
//! as they would. A new engine on the same files reconciles and finishes the
//! task through dvb's own API only: one merge, one PR, nothing edited by hand.

pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{ProjectView, QuestionView, TaskView};
use dvb_core::event::EndReason;
use dvb_core::forge::{Forge, Repo};
use dvb_core::{Event, EventKind, TaskId, TaskState};
use dvb_engine::{Config, Engine};
use dvb_forge::FakeForge;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use support::{add_project, approve, git, seed_origin, state_goal};
use tokio::runtime::Runtime;

/// Every wait here is for an event (a failpoint hit, a state reached) that the
/// run always produces. Bound the entire wait, including each API call, so a
/// stalled connection cannot turn a failed recovery into an unbounded test.
const HANG_GUARD: Duration = Duration::from_secs(60);

struct World {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    seeded: String,
    dir: PathBuf,
    forge: Arc<FakeForge>,
    client: Runtime,
    // fail-rs has a process-wide registry. Hold its scenario lock until the
    // engines and client have stopped, so parallel tests cannot share hits.
    _scenario: fail::FailScenario<'static>,
}

fn world() -> World {
    let scenario = fail::FailScenario::setup();
    // Short: a socket path must fit in 108 bytes.
    let tmp = tempfile::Builder::new().prefix("dvbc").tempdir().unwrap();
    let d = tmp.path().to_path_buf();
    let origin = seed_origin(&d, "main");
    let seeded = git(&origin, &["rev-parse", "main"]);
    World {
        _tmp: tmp,
        origin,
        seeded,
        dir: d.join("e"),
        forge: Arc::new(FakeForge::local_git()),
        client: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
        _scenario: scenario,
    }
}

impl World {
    /// An engine on its own runtime, as its own process would be.
    fn start(&self) -> Runtime {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let cfg = Config::dev(&self.dir, PathBuf::from(env!("CARGO_BIN_EXE_dvb")));
        let forge: Arc<dyn Forge> = self.forge.clone();
        let e = Engine::open(
            cfg,
            dvb_engine::EngineOptions {
                forge: Some(forge),
                launcher: None,
                ..Default::default()
            },
        )
        .unwrap();
        rt.spawn(async move {
            if let Err(err) = dvb_engine::start(e).await {
                eprintln!("engine: {err}");
            }
        });
        rt
    }

    fn c(&self) -> Client {
        Client::new(self.dir.join("engine.sock"))
    }

    fn until<T>(&self, what: &str, mut f: impl AsyncFnMut(&Client) -> Option<T>) -> T {
        let c = self.c();
        self.client.block_on(async {
            let reached = tokio::time::timeout(HANG_GUARD, async {
                loop {
                    if let Some(v) = f(&c).await {
                        return v;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            if let Ok(v) = reached {
                return v;
            }
            let mut log = Vec::new();
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                c.read_events(0, true, |ev: Event| {
                    log.push(dvb_cli::line(&ev));
                    true
                }),
            )
            .await;
            for run in std::fs::read_dir(self.dir.join("work"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let text = std::fs::read_to_string(run.path().join("run.log")).unwrap_or_default();
                log.push(format!("run {:?}:\n{text}", run.file_name()));
            }
            panic!("timed out waiting for {what}:\n{}", log.join("\n"));
        })
    }

    /// Project, goal, and the owner's yes; the task.
    fn goal(&self) -> TaskId {
        self.stated();
        self.approve()
    }

    /// The project and its goal.
    fn stated(&self) -> ProjectView {
        self.until("the engine", async |c| {
            c.get::<serde_json::Value>("/version").await.ok()
        });
        let c = self.c();
        self.client.block_on(async {
            let p = add_project(&c, &self.origin).await;
            state_goal(&c, &p, "Add a note").await;
            p
        })
    }

    /// The owner's yes to the plan question; the plan's first task.
    fn approve(&self) -> TaskId {
        let c = self.c();
        let q = self.until("the plan question", async |c| {
            let open: Vec<QuestionView> = c.get("/questions?open=true").await.ok()?;
            open.into_iter().next()
        });
        self.client.block_on(async {
            approve(&c, &q).await;
            let tasks: Vec<TaskView> = c.get("/tasks").await.unwrap();
            tasks[0].id.clone()
        })
    }

    /// Kills every run still going, as a reboot would.
    fn reboot(&self) {
        for entry in std::fs::read_dir(self.dir.join("work")).unwrap().flatten() {
            if let Ok(pid) = std::fs::read_to_string(entry.path().join("pid")) {
                let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
            }
        }
    }

    /// The task ends Done with one merge commit on `main` and one PR; returns
    /// the event kinds.
    fn done_once(&self, task: &TaskId) -> Vec<&'static str> {
        self.until("the task to be done", async |c| {
            let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
            assert!(
                !matches!(t.state, TaskState::Failed { .. }),
                "{:?}",
                t.state
            );
            (t.state == TaskState::Done).then_some(())
        });
        assert_eq!(
            git(
                &self.origin,
                &["rev-list", "--count", &format!("{}..main", self.seeded)]
            ),
            "1"
        );
        // No PR opened after the task's (project setup opened one before it).
        let t: TaskView = self
            .client
            .block_on(self.c().get(&format!("/tasks/{task}")))
            .unwrap();
        let pr = t.pr.expect("the task's PR");
        let repo = Repo(self.origin.to_string_lossy().into_owned());
        let next = self.client.block_on(self.forge.pr(&repo, pr + 1));
        assert!(next.is_err(), "a second PR: {next:?}");
        let mut kinds = Vec::new();
        self.client
            .block_on(self.c().read_events(0, true, |ev: Event| {
                kinds.push(ev.kind.kind());
                true
            }))
            .unwrap();
        for k in ["pr.opened", "pr.merged", "task.done"] {
            assert_eq!(
                kinds.iter().filter(|x| **x == k).count(),
                1,
                "{k} in {kinds:?}"
            );
        }
        kinds
    }
}

/// The engine dies at the `nth` hit of `point`: the task that hit it panics
/// there (so whatever it held, a request's connection included, is dropped as
/// a dying process would drop it), and the test then stops the engine's runtime.
fn arm(point: &'static str, nth: u32) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let hits = AtomicU32::new(0);
    fail::cfg_callback(point, move || {
        if hits.fetch_add(1, Ordering::SeqCst) + 1 == nth {
            let _ = tx.lock().unwrap().send(());
            panic!("crash injected at {point}");
        }
    })
    .unwrap();
    rx
}

// Linux's FIFO open waits here until a reader arrives. Check every thread:
// the Fake's synchronous write runs on a runtime worker, not the main thread.
fn waiting_at_fifo(pid: &str) -> bool {
    std::fs::read_dir(format!("/proc/{}/task", pid.trim()))
        .into_iter()
        .flatten()
        .flatten()
        .any(|thread| {
            std::fs::read_to_string(thread.path().join("wchan"))
                .is_ok_and(|wchan| wchan.trim() == "wait_for_partner")
        })
}

fn read_fifo(gate: &Path, timeout: &str) -> std::process::Output {
    Command::new("timeout")
        .args(["--kill-after=1s", timeout, "cat"])
        .arg(gate)
        .output()
        .unwrap()
}

fn gated_builder(w: &World) -> PathBuf {
    std::fs::create_dir_all(w.dir.join("fake")).unwrap();
    let gate = w.dir.join("builder-gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        w.dir.join("fake/builder.json"),
        serde_json::json!([
            {"write": {"path": "docs/note.md", "text": "note\n"}},
            {"write": {"path": gate, "text": "ready"}}
        ])
        .to_string(),
    )
    .unwrap();
    gate
}

// A failed gate assertion must not leave a builder blocked in FIFO open.
struct RunCleanup(PathBuf);

impl Drop for RunCleanup {
    fn drop(&mut self) {
        for run in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if let Ok(pid) = std::fs::read_to_string(run.path().join("pid")) {
                let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
            }
        }
    }
}

// Declared after the restarted runtime, so unwinding releases the callback
// before dropping that runtime (which waits for its workers to finish).
struct ReconcileRelease(mpsc::Sender<()>);

impl Drop for ReconcileRelease {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn fifo_read_times_out_if_the_builder_never_reaches_it() {
    let tmp = tempfile::tempdir().unwrap();
    let gate = tmp.path().join("gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let read = read_fifo(&gate, "0.1s");
    assert_eq!(read.status.code(), Some(124), "{read:?}");
}

#[test]
fn fifo_writer_is_waiting_before_the_gate_opens() {
    let tmp = tempfile::tempdir().unwrap();
    let gate = tmp.path().join("gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let mut writer = Command::new("sh")
        .args(["-c", "printf ready > \"$1\"", "writer"])
        .arg(&gate)
        .spawn()
        .unwrap();
    let pid = writer.id().to_string();
    let waiting = (0..100).any(|_| {
        if waiting_at_fifo(&pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
        false
    });
    let read = waiting.then(|| read_fifo(&gate, "1s"));
    let _ = writer.kill();
    let _ = writer.wait();
    assert!(waiting, "the writer was not observed waiting at the FIFO");
    let read = read.unwrap();
    assert!(read.status.success(), "{read:?}");
    assert_eq!(read.stdout, b"ready");
}

/// Runs the task to the crash at `point`, restarts, and checks it ends Done
/// once; `reboot` kills the runs too. Returns the event kinds.
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_at(point: &'static str, nth: u32, reboot: bool) -> Vec<&'static str> {
    let w = world();
    let crashed = arm(point, nth);
    let engine = w.start();
    let task = w.goal();
    crashed
        .recv_timeout(HANG_GUARD)
        .unwrap_or_else(|_| panic!("{point} #{nth} was not reached"));
    drop(engine);
    fail::remove(point);
    if reboot {
        w.reboot();
    }
    let _engine = w.start();
    let kinds = w.done_once(&task);
    assert!(kinds.contains(&"engine.reconciled"), "{kinds:?}");
    kinds
}

#[test]
fn crash_while_recording_the_task_start() {
    // The start is lost with the orchestrator's call; after the restart the
    // orchestrator is woken again for the approved plan. Nothing was in
    // flight, so nothing needs a check.
    let w = world();
    let crashed = arm("append.task.state", 1);
    let engine = w.start();
    let task = w.goal();
    crashed.recv_timeout(HANG_GUARD).unwrap();
    drop(engine);
    fail::remove("append.task.state");
    let _engine = w.start();
    w.done_once(&task);
}

#[test]
fn crash_after_the_builders_unit_starts() {
    // 1 and 2 are the orchestrator's wakes.
    crash_at("effect.start_unit.sent", 3, false);
}

#[test]
fn crash_after_the_push_before_it_is_recorded() {
    let kinds = crash_at("effect.push.sent", 1, false);
    // The publication is found in the mirror and pushed again, not rebuilt.
    assert_eq!(
        kinds.iter().filter(|k| **k == "branch.published").count(),
        1
    );
}

#[test]
fn crash_after_the_pr_opens_before_it_is_recorded() {
    crash_at("effect.open_pr.sent", 1, false);
}

#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_after_the_merge_before_it_is_recorded() {
    let w = world();
    let _cleanup = RunCleanup(w.dir.join("work"));
    let gate = gated_builder(&w);
    let crashed = arm("effect.merge.sent", 1);
    let engine = w.start();
    let task = w.goal();
    // A late approval reply can also receive goal completion in its prompt,
    // but the fixed Fake approval script only starts tasks. Finish that reply
    // before the merge crash so recovery uses the next-goal Fake script.
    w.until("the approval orchestrator to finish", async |c| {
        let mut approved = false;
        let mut approval_run = None;
        let mut completed = false;
        c.read_events(0, true, |ev: Event| {
            if matches!(ev.kind, EventKind::GoalApproved { .. }) {
                approved = true;
            }
            if let EventKind::RunStarted(a) = &ev.kind
                && approved
                && a.is_orchestrator()
            {
                approval_run = Some(a.run.clone());
                completed = false;
            }
            if let EventKind::RunEnded { run, reason, .. } = &ev.kind
                && approval_run.as_ref() == Some(run)
                && *reason == EndReason::Completed
            {
                completed = true;
            }
            true
        })
        .await
        .ok()?;
        completed.then_some(())
    });
    let read = read_fifo(&gate, "60s");
    assert!(read.status.success(), "the FIFO read failed: {read:?}");
    assert_eq!(read.stdout, b"ready");
    crashed.recv_timeout(HANG_GUARD).unwrap();
    drop(engine);
    fail::remove("effect.merge.sent");
    // Hold startup restoration at its boundary. Reconciliation must wait for
    // it, even with a landed remote merge ready to recover on another worker.
    let restored = Arc::new(AtomicBool::new(false));
    let (starting_tx, starting_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    fail::cfg_callback("startup.goal_effects.before", move || {
        starting_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(HANG_GUARD)
            .expect("startup restoration gate was not released");
    })
    .unwrap();
    let completed = restored.clone();
    fail::cfg_callback("startup.goal_effects.restored", move || {
        completed.store(true, Ordering::SeqCst);
    })
    .unwrap();
    let (recovered_tx, recovered_rx) = mpsc::channel();
    fail::cfg_callback("effect.merge.recorded", move || {
        recovered_tx.send(restored.load(Ordering::SeqCst)).unwrap();
    })
    .unwrap();
    let _engine = w.start();
    let release = ReconcileRelease(release_tx);
    starting_rx
        .recv_timeout(HANG_GUARD)
        .expect("startup did not reach restoration");
    release.0.send(()).unwrap();
    assert!(
        recovered_rx
            .recv_timeout(HANG_GUARD)
            .expect("merge was not recovered"),
        "merge recovery raced startup completion restoration"
    );
    for point in [
        "startup.goal_effects.before",
        "startup.goal_effects.restored",
        "effect.merge.recorded",
    ] {
        fail::remove(point);
    }
    w.done_once(&task);
    // The recovered merge is finished like any other: the task done, then the
    // goal, and the orchestrator woken to ask about the next one.
    let kinds = w.until("the orchestrator's next question", async |c| {
        let mut kinds = Vec::new();
        c.read_events(0, true, |ev: Event| {
            kinds.push(ev.kind.kind());
            true
        })
        .await
        .ok()?;
        let done = kinds.iter().position(|k| *k == "goal.done")?;
        kinds[done..].contains(&"question.opened").then_some(kinds)
    });
    let at = |k: &str| kinds.iter().position(|x| *x == k).unwrap_or(usize::MAX);
    assert!(at("task.done") < at("goal.done"), "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "goal.done").count(), 1);
    assert!(kinds.contains(&"engine.reconciled"), "{kinds:?}");
}

#[test]
fn crash_while_recording_the_verdict() {
    crash_at("append.verdict.recorded", 1, false);
}

#[test]
fn crash_while_the_builders_head_moves_the_task() {
    // task.state #3 is running -> collecting, as the builder reports its head.
    crash_at("append.task.state", 3, false);
}

#[test]
fn a_reboot_while_the_builder_starts() {
    // task.state #2 is the builder's unit becoming active.
    crash_at("append.task.state", 2, true);
}

#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_between_a_stop_and_stopping_the_unit() {
    let w = world();
    std::fs::create_dir_all(w.dir.join("fake")).unwrap();
    std::fs::write(
        w.dir.join("fake/builder.json"),
        r#"[{"write": {"path": "docs/first.md", "text": "first\n"}}, {"sleep_ms": 60000}]"#,
    )
    .unwrap();
    std::fs::write(
        w.dir.join("fake/builder-resume.json"),
        r#"[{"write": {"path": "docs/second.md", "text": "second\n"}}]"#,
    )
    .unwrap();
    let crashed = arm("effect.stop_unit.before", 1);
    let engine = w.start();
    let task = w.goal();
    w.until("the first edit", async |_| {
        let work = std::fs::read_dir(w.dir.join("work")).ok()?;
        work.flatten()
            .any(|e| e.path().join("ws/docs/first.md").exists())
            .then_some(())
    });
    let (c, t) = (w.c(), task.clone());
    let _stop = w.client.spawn(async move {
        let _ = c
            .post::<_, TaskView>(&format!("/tasks/{t}/stop"), &())
            .await;
    });
    crashed.recv_timeout(HANG_GUARD).unwrap();
    drop(engine);
    fail::remove("effect.stop_unit.before");
    // The new engine finds the stopped task's unit still running and stops it.
    let _engine = w.start();
    w.until("the stopped task to be checked", async |c| {
        let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
        matches!(t.state, TaskState::Stopped { .. }).then_some(())
    });
    w.until("its unit to be stopped", async |c| {
        let mut ended = false;
        c.read_events(0, true, |ev: Event| {
            ended |= matches!(
                ev.kind,
                EventKind::RunEnded {
                    reason: EndReason::Stopped,
                    ..
                }
            );
            true
        })
        .await
        .ok()?;
        ended.then_some(())
    });
    w.client
        .block_on(
            w.c()
                .post::<_, TaskView>(&format!("/tasks/{task}/resume"), &()),
        )
        .unwrap();
    let kinds = w.done_once(&task);
    assert!(kinds.contains(&"engine.reconciled"), "{kinds:?}");
    for f in ["docs/first.md", "docs/second.md"] {
        git(&w.origin, &["cat-file", "-e", &format!("main:{f}")]);
    }
}

/// A builder that outlives the engine reports its head while the restarted
/// engine is still checking it: the check sees its unit active, and the head
/// and the end that came meanwhile still count.
#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn a_builder_that_collects_while_the_engine_checks_it() {
    let w = world();
    let _cleanup = RunCleanup(w.dir.join("work"));
    // Keep the builder active through the restart without relying on git's speed.
    let gate = gated_builder(&w);
    let engine = w.start();
    let task = w.goal();
    let run_dir = w.until("the builder waiting at the FIFO", async |_| {
        std::fs::read_dir(w.dir.join("work"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.join("ws/docs/note.md").exists()
                    && std::fs::read_to_string(p.join("pid")).is_ok_and(|pid| waiting_at_fifo(&pid))
            })
    });
    drop(engine);
    let (observed_tx, observed_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    fail::cfg_callback("reconcile.observed", move || {
        observed_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(HANG_GUARD)
            .expect("the builder did not collect while reconciliation waited");
    })
    .unwrap();
    let _engine = w.start();
    let release = ReconcileRelease(release_tx);
    observed_rx
        .recv_timeout(HANG_GUARD)
        .expect("reconciliation did not reach its observation");
    // The engine reconciles before it binds the run socket. Open the gate only
    // once the restarted engine serves, or the builder's next report is refused
    // and the run ends before it collects.
    w.until("the restarted engine to serve", async |c| {
        c.get::<serde_json::Value>("/version").await.ok()?;
        std::os::unix::net::UnixStream::connect(w.dir.join("run.sock")).ok()
    });
    let read = read_fifo(&gate, "60s");
    assert!(read.status.success(), "the FIFO read failed: {read:?}");
    assert_eq!(read.stdout, b"ready");
    w.until("the builder's collected head", async |_| {
        std::fs::read_to_string(run_dir.join("head"))
            .ok()
            .filter(|head| head.trim().len() == 40)
    });
    release.0.send(()).unwrap();
    w.done_once(&task);
    let mut collecting = false;
    let mut log = Vec::new();
    w.client
        .block_on(w.c().read_events(0, true, |ev: Event| {
            log.push(dvb_cli::line(&ev));
            collecting |= matches!(
                ev.kind,
                EventKind::EngineReconciled {
                    state: TaskState::Collecting,
                    ..
                }
            );
            true
        }))
        .unwrap();
    assert!(
        collecting,
        "the check did not count the head that came:\n{}",
        log.join("\n")
    );
}

/// Shutdown in the first write's output report prevents the next FIFO write.
/// Reading that abandoned gate must time out and clean up rather than hang CI.
#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn shutdown_during_the_first_write_report_cannot_hang_the_gate() {
    let w = world();
    let _cleanup = RunCleanup(w.dir.join("work"));
    let gate = gated_builder(&w);
    let (crashed_tx, crashed_rx) = mpsc::channel();
    let work = w.dir.join("work");
    fail::cfg_callback("append.run.output", move || {
        let edited = std::fs::read_dir(&work)
            .into_iter()
            .flatten()
            .flatten()
            .any(|run| run.path().join("ws/docs/note.md").exists());
        if edited {
            let _ = crashed_tx.send(());
            panic!("crash during the first write's output report");
        }
    })
    .unwrap();
    let engine = w.start();
    w.goal();
    crashed_rx
        .recv_timeout(HANG_GUARD)
        .expect("the first write's output report was not reached");
    drop(engine);
    fail::remove("append.run.output");
    let read = read_fifo(&gate, "0.1s");
    assert_eq!(read.status.code(), Some(124), "{read:?}");
}

/// The orchestrator saved its plan and the engine died before the question:
/// after the restart the engine asks about the saved plan (no second plan).
#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_between_the_plan_and_its_question() {
    let w = world();
    let crashed = arm("append.question.opened", 1);
    let engine = w.start();
    w.stated();
    crashed.recv_timeout(HANG_GUARD).unwrap();
    drop(engine);
    fail::remove("append.question.opened");
    let _engine = w.start();
    let task = w.approve();
    let kinds = w.done_once(&task);
    assert_eq!(kinds.iter().filter(|k| **k == "plan.proposed").count(), 1);
    let tasks: Vec<TaskView> = w.client.block_on(w.c().get("/tasks")).unwrap();
    assert_eq!(tasks.len(), 1);
}

/// A plan of two tasks; one started before the engine died recording the next.
/// After the restart the remaining approved task is started too.
#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_between_two_task_starts() {
    let w = world();
    let _cleanup = RunCleanup(w.dir.join("work"));
    let gate = gated_builder(&w);
    let goal = include_str!("../../dvb-engine/src/fake_scripts/orchestrator-goal.json").replace(
        "[{\"title\": \"{{goal_text}}\", \"role\": \"backend\", \"summary\": \"A one-file edit, \
                made by the Fake provider.\", \"review\": \"code\"}]",
        "[{\"title\": \"One\", \"role\": \"backend\", \"summary\": \"One.\", \"review\": \
                \"code\"}, {\"title\": \"Two\", \"role\": \"backend\", \"summary\": \
                \"Two.\", \"review\": \"code\"}]",
    );
    assert!(goal.contains("\"Two\""));
    std::fs::write(w.dir.join("fake/orchestrator-goal.json"), goal).unwrap();
    // Automatic admission can run both builders before either merge. Give
    // them independent edits so this tests recovery rather than conflicts.
    std::fs::write(
        w.dir.join("fake/builder.json"),
        serde_json::json!([
            {"write": {"path": "docs/{{task}}.md", "text": "{{task_title}}\n"}},
            {"write": {"path": gate, "text": "ready"}}
        ])
        .to_string(),
    )
    .unwrap();
    let crashed = arm("append.task.started", 2);
    let engine = w.start();
    w.goal();
    crashed.recv_timeout(HANG_GUARD).unwrap();
    // Keep later admission attempts at the same crash boundary while the
    // surviving worker finishes preflight and blocks outside the engine.
    fail::cfg("append.task.started", "panic").unwrap();
    w.until("the first builder at its gate", async |_| {
        std::fs::read_dir(w.dir.join("work"))
            .ok()?
            .flatten()
            .filter_map(|run| std::fs::read_to_string(run.path().join("pid")).ok())
            .any(|pid| waiting_at_fifo(&pid))
            .then_some(())
    });
    let tasks: Vec<TaskView> = w.client.block_on(w.c().get("/tasks")).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(
        tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count(),
        1
    );
    assert_eq!(
        tasks
            .iter()
            .filter(|t| t.state == TaskState::Planned)
            .count(),
        1
    );
    drop(engine);
    fail::remove("append.task.started");
    std::fs::write(
        w.dir.join("fake/builder.json"),
        r#"[{"write": {"path": "docs/{{task}}.md", "text": "{{task_title}}\n"}}]"#,
    )
    .unwrap();
    let _engine = w.start();
    w.until("the restarted engine", async |c| {
        c.get::<serde_json::Value>("/version").await.ok()
    });
    assert!(read_fifo(&gate, "60s").status.success());
    w.until("both tasks to be done", async |c| {
        let tasks: Vec<TaskView> = c.get("/tasks").await.ok()?;
        (tasks.len() == 2 && tasks.iter().all(|t| t.state == TaskState::Done)).then_some(())
    });
    let kinds = w.until("the goal to be done", async |c| {
        let mut kinds = Vec::new();
        c.read_events(0, true, |ev: Event| {
            kinds.push(ev.kind.kind());
            true
        })
        .await
        .ok()?;
        kinds.contains(&"goal.done").then_some(kinds)
    });
    for (k, n) in [("pr.merged", 2), ("goal.done", 1), ("plan.proposed", 1)] {
        assert_eq!(
            kinds.iter().filter(|x| **x == k).count(),
            n,
            "{k}: {kinds:?}"
        );
    }
}

/// The engine dies while a stopped builder's workspace moves to its resumed
/// run, after the workspace and before the marker follows: the next start still
/// finds the prepared workspace, so the first run's edit is not lost.
#[test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "World holds the failpoint registry and temporary files until the engine runtimes and \
                cleanup guards have stopped"
)]
fn crash_while_the_workspace_moves_to_the_resumed_run() {
    let w = world();
    std::fs::create_dir_all(w.dir.join("fake")).unwrap();
    std::fs::write(
        w.dir.join("fake/builder.json"),
        r#"[{"write": {"path": "docs/first.md", "text": "first\n"}},
            {"sleep_ms": 60000}]"#,
    )
    .unwrap();
    std::fs::write(
        w.dir.join("fake/builder-resume.json"),
        r#"[{"write": {"path": "docs/second.md", "text": "second\n"}}]"#,
    )
    .unwrap();
    let crashed = arm("flow.workspace.moved", 1);
    let engine = w.start();
    let task = w.goal();
    let work = w.dir.join("work");
    w.until("the first edit", async |_| {
        std::fs::read_dir(&work)
            .ok()?
            .flatten()
            .find_map(|run| run.path().join("ws/docs/first.md").exists().then_some(()))
    });
    let c = w.c();
    w.client.block_on(async {
        let _: TaskView = c.post(&format!("/tasks/{task}/stop"), &()).await.unwrap();
    });
    w.until("the run to end", async |c| {
        let mut ended = false;
        c.read_events(0, true, |ev: Event| {
            ended |= matches!(
                ev.kind,
                EventKind::RunEnded {
                    reason: EndReason::Stopped,
                    ..
                }
            );
            true
        })
        .await
        .ok()?;
        ended.then_some(())
    });
    w.client.block_on(async {
        let _: TaskView = c.post(&format!("/tasks/{task}/resume"), &()).await.unwrap();
    });
    crashed
        .recv_timeout(HANG_GUARD)
        .expect("the move was not reached");
    drop(engine);
    fail::remove("flow.workspace.moved");
    let _engine = w.start();
    w.until("the task to be done", async |c| {
        let t: TaskView = c.get(&format!("/tasks/{task}")).await.ok()?;
        (t.state == TaskState::Done).then_some(())
    });
    for f in ["docs/first.md", "docs/second.md"] {
        git(&w.origin, &["cat-file", "-e", &format!("main:{f}")]);
    }
}
