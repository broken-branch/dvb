//! Shared setup for the binary tests: a seeded origin, the engine child process,
//! polling, and the API calls that state a goal and approve its plan. Each test
//! binary exposes this module publicly so unused public fixtures need no suppression.
use dvb_cli::client::Client;
use dvb_core::api::{GoalView, NewGoal, NewProject, ProjectView, QuestionView};
use dvb_core::event::{Answer, CiMode};
use dvb_core::{Event, EventKind, RunId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A running engine child process, killed and reaped on drop.
pub struct Engine(pub Child);

impl Engine {
    pub fn spawn(mut cmd: Command) -> Engine {
        Engine(cmd.spawn().unwrap())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `dvb --dev <dir> engine`, for the caller to adjust (environment, stdio) and
/// hand to [`Engine::spawn`].
pub fn engine_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dvb"));
    cmd.arg("--dev").arg(dir).arg("engine");
    cmd
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A bare `origin.git` in `d` whose `branch` holds one seed commit with a README.
pub fn seed_origin(d: &Path, branch: &str) -> PathBuf {
    let origin = d.join("origin.git");
    let seed = d.join("seed");
    git(
        d,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            branch,
            origin.to_str().unwrap(),
        ],
    );
    git(d, &["init", "-q", "-b", branch, seed.to_str().unwrap()]);
    std::fs::write(seed.join("README.md"), "# dvb\n").unwrap();
    git(&seed, &["add", "."]);
    git(
        &seed,
        &[
            "-c",
            "user.name=s",
            "-c",
            "user.email=s@example.invalid",
            "commit",
            "-qm",
            "Seed",
        ],
    );
    git(&seed, &["push", "-q", origin.to_str().unwrap(), branch]);
    origin
}

/// Polls every 100 ms, `attempts` times; panics naming `what` if `f` never
/// returns a value. Counted, not timed: the clock check forbids reading the time.
pub async fn until_for<T>(what: &str, attempts: u32, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..attempts {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// [`until_for`] with the usual one-minute budget.
pub async fn until<T>(what: &str, f: impl AsyncFnMut() -> Option<T>) -> T {
    until_for(what, 600, f).await
}

pub async fn engine_up(c: &Client) {
    until("the engine", async || {
        c.get::<serde_json::Value>("/version").await.ok()
    })
    .await;
}

/// Each run's prompt, read while its run directory is there: an ended
/// orchestrator's and a done task's run directories are removed.
pub fn run_prompts(c: &Client, work: &Path) -> Arc<Mutex<BTreeMap<RunId, String>>> {
    let prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let c = Client::new(c.sock.clone());
    let work = work.to_owned();
    let kept = prompts.clone();
    tokio::spawn(async move {
        tokio::time::timeout(
            Duration::from_secs(120),
            c.read_events(0, false, |ev: Event| {
                if let EventKind::RunStarted(a) = ev.kind {
                    let dir = work.join(a.run.as_str());
                    let kept = kept.clone();
                    tokio::spawn(async move {
                        // The Fake script is written after the whole prompt.
                        for _ in 0..1000 {
                            if dir.join("fake-script.json").exists()
                                && let Ok(prompt) = std::fs::read_to_string(dir.join("prompt.md"))
                            {
                                kept.lock().unwrap().insert(a.run, prompt);
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    });
                }
                true
            }),
        )
        .await
        .expect("timed out capturing run prompts")
        .unwrap();
    });
    prompts
}

/// Adds the local origin as project `dvb`.
pub async fn add_project(c: &Client, origin: &Path) -> ProjectView {
    c.post(
        "/projects",
        &NewProject {
            name: "dvb".into(),
            create: false,
            repo: origin.to_string_lossy().into_owned(),
            default_branch: None,
            ci: CiMode::NoCi,
        },
    )
    .await
    .unwrap()
}

pub async fn state_goal(c: &Client, project: &ProjectView, text: &str) -> GoalView {
    c.post(
        &format!("/projects/{}/goals", project.id),
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

/// The owner's yes to a plan question.
pub async fn approve(c: &Client, q: &QuestionView) -> QuestionView {
    let yes = q.options.iter().find(|o| o.approves_plan).unwrap().id;
    c.post(&format!("/questions/{}/answer", q.id), &Answer::Choice(yes))
        .await
        .unwrap()
}

pub mod events;
pub mod planning;
