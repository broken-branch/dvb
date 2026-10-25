//! PR description publication using local Git and FakeForge; no model or network.
use dvb_engine::owner;
mod support;

use dvb_core::event::EndReason;
use dvb_core::forge::{Forge, ForgeError, Repo};
use dvb_core::state::Effect;
use dvb_core::{Actor, Command, EventKind, GoalId, ProjectId, RunId, TaskId, TaskState};
use dvb_engine::{AppState, dispatch, proj::Projections};
use dvb_forge::FakeForge;
use std::sync::Arc;
use support::git::{collect, git, seed_repository};

struct Fixture {
    _tmp: tempfile::TempDir,
    e: AppState,
    fake: Arc<FakeForge>,
    recorded: Arc<support::InterruptedForge>,
    repo: Repo,
    base: String,
}

async fn fixture() -> Fixture {
    fixture_clock(Arc::new(dvb_core::SystemClock)).await
}

async fn fixture_clock(clock: Arc<dyn dvb_core::Clock>) -> Fixture {
    fixture_with(clock, Vec::new()).await
}

async fn fixture_with(
    clock: Arc<dyn dvb_core::Clock>,
    issues: Vec<dvb_core::event::GoalIssue>,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeForge::local_git().with_clock(clock.clone()));
    let recorded = Arc::new(support::InterruptedForge::new(fake.clone()));
    let e = support::engine::open_with(
        &tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            clock: Some(clock),
            launcher: Some(Arc::new(support::launcher::FakeLauncher::default())),
            forge: Some(recorded.clone()),
            ..Default::default()
        },
    );
    e.manual_dispatch();
    let seed = tmp.path().join("seed");
    let origin = tmp.path().join("origin.git");
    seed_repository(&seed, "main", Some(("file", "base\n")), "base");
    git(
        tmp.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let repo = Repo(origin.to_string_lossy().into());
    let mirror = e.mirror("project");
    fake.clone_repo(&repo, &mirror).await.unwrap();
    let base = git(&mirror, &["rev-parse", "main"]);
    e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            support::events::project_event(&ProjectId::new("p"), "project", &repo.0),
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "Goal".into(),
                from_issue: None,
                issues,
                after: None,
            },
            support::events::backend_plan_event(&GoalId::new("g"), &TaskId::new("t")),
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
        ])
    })
    .unwrap();
    Fixture {
        _tmp: tmp,
        e,
        fake,
        recorded,
        repo,
        base,
    }
}

impl Fixture {
    /// The run's workspace with a change on `base`, snapshotted as a driver
    /// collects it, and marked prepared so a resume can carry on in it.
    fn snapshot(&self, name: &str, base: &str) -> String {
        self.snapshot_with(name, base, &[])
    }

    /// `snapshot`, with these files written too.
    fn snapshot_with(&self, name: &str, base: &str, files: &[(&str, &str)]) -> String {
        let ws = self.e.cfg.paths.work.join(name).join("ws");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        git(
            self._tmp.path(),
            &[
                "clone",
                "--quiet",
                self.e.mirror("project").to_str().unwrap(),
                ws.to_str().unwrap(),
            ],
        );
        git(&ws, &["checkout", "--quiet", "--detach", base]);
        std::fs::write(ws.join("file"), format!("changed by {name}\n")).unwrap();
        for (path, text) in files {
            std::fs::write(ws.join(path), text).unwrap();
        }
        git(&ws, &["add", "."]);
        let tree = git(&ws, &["write-tree"]);
        let head = git(&ws, &["commit-tree", &tree, "-p", base, "-m", "snapshot"]);
        collect(&ws, &head);
        std::fs::write(ws.parent().unwrap().join("prepared"), "").unwrap();
        head
    }

    fn collect_round(&self, name: &str, base: &str, summary: Option<&str>) -> String {
        self.collect_files(name, base, summary, &[])
    }

    fn collect_files(
        &self,
        name: &str,
        base: &str,
        summary: Option<&str>,
        files: &[(&str, &str)],
    ) -> String {
        let run = RunId::new(name);
        let head = self.snapshot_with(name, base, files);
        let previous = self.e.proj().tasks[&TaskId::new("t")].view.state.clone();
        let mut events = vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(run.clone())
                    .task(Some(TaskId::new("t")))
                    .reason("Build".into())
                    .base(Some(base.into()))
                    .build(),
            ),
            EventKind::TaskStateChanged {
                task: TaskId::new("t"),
                from: previous,
                to: TaskState::Publishing,
            },
        ];
        if let Some(summary) = summary {
            events.push(EventKind::RunCollected {
                outcome: None,
                run: run.clone(),
                head: None,
                summary: Some(summary.into()),
            });
        }
        // Load the old event shape: driver/recovery sends no summary.
        events.push(
            serde_json::from_value(serde_json::json!({
                "kind": "run.collected", "payload": {"run": name, "head": head}
            }))
            .unwrap(),
        );
        events.push(EventKind::RunEnded {
            run,
            reason: EndReason::Completed,
            detail: None,
            stderr: None,
        });
        self.e
            .append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(events))
            .unwrap();
        head
    }

    /// A builder run of `task` collected through the run API: the builder
    /// saves its summary and outcome, then its driver reports the snapshot
    /// head and the run ends.
    async fn collect_reported(
        &self,
        task: &TaskId,
        name: &str,
        base: &str,
        summary: &str,
        outcome: Option<dvb_core::run_verbs::BuilderOutcome>,
    ) -> String {
        let run = RunId::new(name);
        let head = self.snapshot(name, base);
        let previous = self.e.proj().tasks[task].view.state.clone();
        self.e
            .append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::RunStarted(
                        support::assignment::Assignment::new(run.clone())
                            .task(Some(task.clone()))
                            .hire(dvb_core::HireId::new("h-backend"))
                            .reason("Build".into())
                            .base(Some(base.into()))
                            .build(),
                    ),
                    EventKind::TaskStateChanged {
                        task: task.clone(),
                        from: previous,
                        to: TaskState::Running,
                    },
                ])
            })
            .unwrap();
        for body in [
            dvb_core::run_verbs::CollectedBody {
                outcome,
                head: None,
                summary: Some(summary.into()),
            },
            dvb_core::run_verbs::CollectedBody {
                outcome: None,
                head: Some(head.clone()),
                summary: None,
            },
        ] {
            let caller = dvb_engine::serve::Caller {
                run: self.e.proj().runs[&run].clone(),
            };
            let reply = support::http::run::<serde_json::Value, _>(
                "collected",
                axum::extract::State(self.e.clone()),
                caller,
                axum::Json(body),
            )
            .await
            .unwrap();
            assert!(reply.0["event"].is_u64());
        }
        self.e.end_run(&run, EndReason::Completed, None).unwrap();
        head
    }
}

#[tokio::test]
async fn the_goals_last_pr_closes_its_issues_and_earlier_prs_refer_to_them() {
    let carried = |number| dvb_core::event::GoalIssue {
        number,
        url: format!("https://github.com/org/repo/issues/{number}"),
        text: "Fenced issue".into(),
    };
    for (siblings, word) in [(false, "Closes"), (true, "Refs")] {
        let f = fixture_with(
            Arc::new(dvb_core::SystemClock),
            vec![carried(4), carried(7)],
        )
        .await;
        if siblings {
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
                Ok(vec![support::events::backend_plan_event(
                    &GoalId::new("g"),
                    &TaskId::new("later"),
                )])
            })
            .unwrap();
        }
        f.collect_round("b1", &f.base, Some("Changed the file."));
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
        assert_eq!(
            f.fake.pr_body(&f.repo, 1),
            Some(format!(
                "Changed the file.\n\nTask t, round 1, built by run b1.\n\n{word} #4\n{word} #7"
            ))
        );
    }
}

#[tokio::test]
async fn first_summary_is_followed_by_ids_and_second_round_replaces_it() {
    let f = fixture().await;
    f.collect_round(
        "b1",
        &f.base,
        Some("Changed the file. Checked the first round."),
    );
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    assert_eq!(
        f.fake.pr_body(&f.repo, 1).as_deref(),
        Some("Changed the file. Checked the first round.\n\nTask t, round 1, built by run b1.")
    );
    let first = f.e.proj().tasks[&TaskId::new("t")]
        .view
        .head
        .clone()
        .unwrap();
    f.collect_round(
        "b2",
        &first,
        Some("Fixed the file. Checked the second round."),
    );
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    assert_eq!(
        f.fake.pr_body(&f.repo, 1).as_deref(),
        Some("Fixed the file. Checked the second round.\n\nTask t, round 2, built by run b2.")
    );
    let t = f.e.proj().tasks[&TaskId::new("t")].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_ne!(t.view.head.as_ref(), Some(&first));
    assert_eq!(t.view.pr, Some(1));
    assert_eq!(
        f.recorded
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "open_pr")
            .count(),
        1
    );
    let replay = Projections::rebuild(&support::events::all(&f.e));
    assert_eq!(
        replay.runs[&RunId::new("b1")].summary.as_deref(),
        Some("Changed the file. Checked the first round.")
    );
    assert_eq!(
        replay.runs[&RunId::new("b2")].summary.as_deref(),
        Some("Fixed the file. Checked the second round.")
    );
}

#[tokio::test]
async fn no_summary_keeps_the_exact_id_only_body() {
    for summary in [None, Some(" \n\t")] {
        let f = fixture().await;
        f.collect_round("b1", &f.base, summary);
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
        assert_eq!(
            f.fake.pr_body(&f.repo, 1).as_deref(),
            Some("Task t, round 1, built by run b1.")
        );
    }
}

#[tokio::test]
async fn owner_retry_finishes_failed_body_edits_before_review() {
    for retryable in [false, true] {
        let f = fixture().await;
        let task = TaskId::new("t");
        f.collect_round("b1", &f.base, Some("First summary."));
        dispatch::step_now(&f.e, &task).await.unwrap();
        let first = f.e.proj().tasks[&task].view.head.clone().unwrap();
        let original = f.fake.pr_body(&f.repo, 1).unwrap();
        f.collect_round("b2", &first, Some("Replacement summary. Checked the fix."));
        *f.recorded.edit_pr_body_error.lock().unwrap() = Some(ForgeError {
            rate_limit: None,
            retryable,
            detail: "body edit failed".into(),
        });
        loop {
            dispatch::step_now(&f.e, &task).await.unwrap();
            let state = f.e.proj().tasks[&task].view.state.clone();
            if let TaskState::RetryPending { effect, .. } = state {
                assert!(retryable);
                assert_eq!(effect, Effect::OpenPr);
                f.e.decide(
                    f.e.proj(),
                    &task,
                    Actor::Engine,
                    dvb_engine::Decision::new("retry_due", Command::RetryDue),
                    |_| Ok(()),
                )
                .unwrap();
            } else {
                assert!(matches!(state, TaskState::Failed { .. }), "{state:?}");
                break;
            }
        }
        assert_eq!(f.fake.pr_body(&f.repo, 1), Some(original));
        let events = support::events::all(&f.e);
        let replay = Projections::rebuild(&events);
        assert!(replay.tasks[&task].publication_pending);
        assert_eq!(replay.tasks[&task].facts.round, 2);
        assert!(replay.tasks[&task].needs_reviewer());
        assert_eq!(
            f.recorded
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| **c == "edit_pr_body")
                .count(),
            if retryable {
                usize::from(dvb_core::caps::RETRIES_PER_EFFECT) + 1
            } else {
                1
            }
        );
        // Restore the event-derived projection as a restart would, then retry.
        *f.e.proj() = replay;
        *f.recorded.edit_pr_body_error.lock().unwrap() = None;
        let runs = f.e.proj().runs.len();
        f.recorded.calls.lock().unwrap().clear();
        let state = dispatch::retry_failed(&f.e, &task, Actor::Owner, None, |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(state, TaskState::Publishing);
        dispatch::step_now(&f.e, &task).await.unwrap();
        assert_eq!(
            f.fake.pr_body(&f.repo, 1).as_deref(),
            Some("Replacement summary. Checked the fix.\n\nTask t, round 2, built by run b2.")
        );
        assert_eq!(f.e.proj().tasks[&task].view.state, TaskState::InReview);
        assert_eq!(
            f.e.proj().runs.len(),
            runs,
            "publication retry starts no run"
        );
        assert_eq!(*f.recorded.calls.lock().unwrap(), ["edit_pr_body"]);
        let replay = Projections::rebuild(&support::events::all(&f.e));
        assert!(!replay.tasks[&task].publication_pending);
    }
}

#[tokio::test]
async fn summary_scan_blocks_fresh_marker_recovered_and_body_edit_publication() {
    for phase in ["fresh", "marker", "edit", "recovered-open"] {
        let f = fixture().await;
        let head = f.collect_round(
            "b1",
            &f.base,
            Some(&format!("Credential ghp_{}", "a".repeat(30))),
        );
        let task = TaskId::new("t");
        if phase == "marker" {
            git(
                &f.e.mirror("project"),
                &[
                    "fetch",
                    "--quiet",
                    f.e.cfg.paths.work.join("b1/ws").to_str().unwrap(),
                    "refs/dvb/collected",
                ],
            );
            git(
                &f.e.mirror("project"),
                &["update-ref", "refs/dvb/published/b1", &head],
            );
        }
        if matches!(phase, "edit" | "recovered-open") {
            f.fake
                .push(&f.repo, &f.e.mirror("project"), "dvb/t-1", &f.base)
                .await
                .unwrap();
            f.fake
                .open_pr(&f.repo, "dvb/t-1", "Task", "Original body")
                .await
                .unwrap();
            let mut events = vec![EventKind::BranchPublished {
                task: task.clone(),
                round: 1,
                branch: "dvb/t-1".into(),
                head: f.base.clone(),
            }];
            if phase == "edit" {
                events.push(EventKind::PrOpened {
                    task: task.clone(),
                    pr: 1,
                    url: "fake://pr/1".into(),
                });
            }
            f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| Ok(events))
                .unwrap();
        }
        dispatch::step_now(&f.e, &task).await.unwrap();
        assert!(
            matches!(f.e.proj().tasks[&task].view.state, TaskState::Failed { .. }),
            "{phase}"
        );
        assert!(
            support::events::all(&f.e)
                .iter()
                .any(|ev| matches!(&ev.kind, EventKind::ScanHit { file, .. } if file == "pr body")),
            "{phase}"
        );
        assert!(
            !f.recorded
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(*c, "push" | "open_pr" | "edit_pr_body")),
            "{phase}"
        );
        if matches!(phase, "edit" | "recovered-open") {
            assert_eq!(f.fake.pr_body(&f.repo, 1).as_deref(), Some("Original body"));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn rate_limited_403_push_waits_then_succeeds_without_spending_attempts() {
    use dvb_core::forge::RateLimit;
    use dvb_core::{Clock, FakeClock, Timestamp};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let f = fixture_clock(clock.clone()).await;
    let task = TaskId::new("t");
    f.collect_round("b1", &f.base, Some("Changed the file."));
    for round in 0..10 {
        f.fake.fail_next(
            "push",
            ForgeError {
                retryable: true,
                detail: "GitHub HTTP 403 (rate limit)".into(),
                rate_limit: Some(RateLimit {
                    reset: Some(clock.now().plus_millis(60_000)),
                }),
            },
        );
        dispatch::step_now(&f.e, &task).await.unwrap();
        let at = f.e.proj().tasks[&task].retry_at.unwrap();
        assert_eq!(at, clock.now().plus_millis(61_000));
        assert!(matches!(
            f.e.proj().tasks[&task].view.state,
            TaskState::RetryPending { attempt: 0, .. }
        ));
        assert_eq!(
            f.e.proj().tasks[&task]
                .facts
                .retries
                .get(&Effect::Push)
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(
            dvb_cli::task_line(&f.e.proj().tasks[&task].view)
                .contains("Waiting for GitHub rate limit; reset at")
        );
        let requests = f.fake.calls();
        if round == 0 {
            // Pausing and stopping remain responsive; resume cannot bypass the forge wait.
            owner::pause(&f.e, &ProjectId::new("p"), true).unwrap();
            dispatch::step_now(&f.e, &task).await.unwrap();
            owner::pause(&f.e, &ProjectId::new("p"), false).unwrap();
            dispatch::stop(&f.e, &task, Actor::Owner).await.unwrap();
            assert!(matches!(
                f.e.proj().tasks[&task].view.state,
                TaskState::Stopped { .. }
            ));
            dispatch::resume(&f.e, &task, Actor::Owner).await.unwrap();
            dispatch::step_now(&f.e, &task).await.unwrap();
            assert_eq!(f.fake.calls(), requests);
            // Restart/reconcile restores the pending state, deadline and visible reset.
            f.e.decide(
                f.e.proj(),
                &task,
                Actor::Engine,
                dvb_engine::Decision::new("restart", Command::Restarted),
                |_| Ok(()),
            )
            .unwrap();
            f.e.decide(
                f.e.proj(),
                &task,
                Actor::Engine,
                dvb_engine::Decision::new(
                    "reconcile",
                    Command::Reconcile(dvb_core::state::Observed {
                        unit: dvb_core::launcher::UnitState::Inactive,
                        head: None,
                    }),
                ),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(f.e.proj().tasks[&task].retry_at, Some(at));
            assert!(
                f.e.proj().tasks[&task]
                    .view
                    .reason
                    .as_ref()
                    .unwrap()
                    .contains("reset at")
            );
            let replay = Projections::rebuild(&support::events::all(&f.e));
            assert_eq!(replay.tasks[&task].view, f.e.proj().tasks[&task].view);
        }
        clock.set(at.plus_millis(-1));
        dispatch::step_now(&f.e, &task).await.unwrap();
        dvb_engine::publish::mirror::sync(&f.e).await.unwrap();
        assert_eq!(
            f.fake.calls(),
            requests,
            "neither push nor mirrors contact GitHub during the shared wait"
        );
        clock.set(at);
        dispatch::step_now(&f.e, &task).await.unwrap(); // RetryDue.
        assert!(!matches!(
            f.e.proj().tasks[&task].view.state,
            TaskState::Failed { .. }
        ));
    }
    // Ten limits exceed the ordinary retry cap; the next real local push still succeeds.
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert!(f.e.proj().tasks[&task].view.pr.is_some());
    assert!(!matches!(
        f.e.proj().tasks[&task].view.state,
        TaskState::Failed { .. }
    ));
    assert_eq!(
        f.fake
            .calls()
            .iter()
            .filter(|op| op.as_str() == "push")
            .count(),
        11
    );
    let events = support::events::all(&f.e);
    let rebuilt = Projections::rebuild(&events);
    assert_eq!(rebuilt.tasks[&task].facts.retries.get(&Effect::Push), None);
    assert_eq!(rebuilt.tasks[&task].view, f.e.proj().tasks[&task].view);
}

#[tokio::test(start_paused = true)]
async fn limits_without_resets_keep_backoff_and_ordinary_push_failures_still_exhaust_retries() {
    use dvb_core::forge::RateLimit;
    use dvb_core::{FakeClock, Timestamp};
    let clock = Arc::new(FakeClock::at(Timestamp(1_000)));
    let f = fixture_clock(clock.clone()).await;
    let task = TaskId::new("t");
    f.collect_round("b1", &f.base, Some("Changed the file."));
    for wait in 1..=10 {
        f.fake.fail_next(
            "push",
            ForgeError {
                retryable: true,
                detail: "GitHub HTTP 429 (rate limit)".into(),
                rate_limit: Some(RateLimit { reset: None }),
            },
        );
        dispatch::step_now(&f.e, &task).await.unwrap();
        let at = f.e.proj().tasks[&task].retry_at.unwrap();
        assert_eq!(
            at.millis() - dvb_core::Clock::now(clock.as_ref()).millis(),
            dvb_core::state::backoff_millis(wait, 2_000, 300_000)
        );
        assert_eq!(
            f.e.proj().tasks[&task].facts.retries.get(&Effect::Push),
            None
        );
        let rebuilt = Projections::rebuild(&support::events::all(&f.e));
        assert_eq!(
            rebuilt.tasks[&task]
                .facts
                .rate_limit_retries
                .get(&Effect::Push),
            Some(&wait)
        );
        let requests = f.fake.calls();
        clock.set(at.plus_millis(-1));
        dispatch::step_now(&f.e, &task).await.unwrap();
        assert_eq!(f.fake.calls(), requests);
        clock.set(at);
        dispatch::step_now(&f.e, &task).await.unwrap();
    }
    for attempt in 1..=dvb_core::caps::RETRIES_PER_EFFECT {
        f.fake.fail_next(
            "push",
            ForgeError {
                retryable: true,
                detail: "unavailable".into(),
                rate_limit: None,
            },
        );
        dispatch::step_now(&f.e, &task).await.unwrap();
        assert_eq!(
            f.e.proj().tasks[&task].facts.retries.get(&Effect::Push),
            Some(&attempt)
        );
        let at = f.e.proj().tasks[&task].retry_at.unwrap();
        assert_eq!(
            at.millis() - dvb_core::Clock::now(clock.as_ref()).millis(),
            dvb_core::state::backoff_millis(attempt, 2_000, 300_000)
        );
        assert_eq!(
            f.e.proj().tasks[&task].view.reason,
            Some(format!("Attempt {attempt} starts automatically"))
        );
        clock.set(at);
        dispatch::step_now(&f.e, &task).await.unwrap();
    }
    f.fake.fail_next(
        "push",
        ForgeError {
            retryable: true,
            detail: "unavailable".into(),
            rate_limit: None,
        },
    );
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert!(
        matches!(&f.e.proj().tasks[&task].view.state, TaskState::Failed { reason } if reason.contains("unavailable"))
    );
}

fn report_outcome(f: &Fixture, run: &str, outcome: dvb_core::run_verbs::BuilderOutcome) {
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunCollected {
            run: RunId::new(run),
            head: None,
            summary: None,
            outcome: Some(outcome),
        }])
    })
    .unwrap();
}

fn incomplete_results() -> Vec<dvb_core::run_verbs::BuilderOutcome> {
    use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
    vec![
        BuilderOutcome::Partial {
            reason: PartialReason::OutOfRole,
            detail: "Needs the backend API".into(),
        },
        BuilderOutcome::Partial {
            reason: PartialReason::CompletionUnmet,
            detail: "Three consecutive passes not achieved".into(),
        },
        BuilderOutcome::Blocked {
            prerequisite: "Backend task merges".into(),
        },
    ]
}

#[tokio::test]
async fn incomplete_results_hold_publication_preserve_work_and_replay() {
    for outcome in incomplete_results() {
        let f = fixture().await;
        let head = f.collect_round(
            "b1",
            &f.base,
            Some(
                "Closes #323. FIXES org/repo#283. Resolves https://github.com/org/repo/issues/324",
            ),
        );
        report_outcome(&f, "b1", outcome.clone());
        for _ in 0..3 {
            dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
        }
        let t = f.e.proj().tasks[&TaskId::new("t")].view.clone();
        assert!(matches!(t.state, TaskState::Stopped { .. }));
        assert!(t.pr.is_none());
        assert!(f.fake.find_pr(&f.repo, "dvb/t-1").await.unwrap().is_none());
        assert_eq!(
            git(
                &f.e.cfg.paths.work.join("b1/ws"),
                &["rev-parse", "refs/dvb/collected"]
            ),
            head
        );
        let replay = Projections::rebuild(&support::events::all(&f.e));
        assert_eq!(replay.tasks[&t.id].view.outcome, outcome);
        assert_eq!(
            replay.runs[&RunId::new("b1")].collected.as_deref(),
            Some(head.as_str())
        );
        if matches!(
            outcome,
            dvb_core::run_verbs::BuilderOutcome::Partial {
                reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                ..
            }
        ) {
            assert!(replay.can_extend_plan(&replay.goals[&GoalId::new("g")]));
        }
    }
}

fn clean_review(f: &Fixture) {
    let task = TaskId::new("t");
    let t = f.e.proj().tasks[&task].clone();
    let head = t.view.head.unwrap();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("review"))
                    .task(Some(task.clone()))
                    .role(dvb_core::RoleId::new("code-reviewer"))
                    .reviews(Some(head.clone()))
                    .build(),
            ),
            EventKind::VerdictRecorded {
                verdict: dvb_core::VerdictId::new("v"),
                task: task.clone(),
                head: head.clone(),
                run: RunId::new("review"),
                role: dvb_core::RoleId::new("code-reviewer"),
                model: "fake-1".into(),
                vendor: dvb_core::Vendor::Fake,
                findings: vec![],
                summary: "Clean review".into(),
                review_stage: None,
            },
            EventKind::RunEnded {
                run: RunId::new("review"),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
            EventKind::TaskStateChanged {
                task,
                from: t.view.state,
                to: TaskState::Merging,
            },
        ])
    })
    .unwrap();
}

/// The run API takes a builder's outcome only before its collection, and
/// every collection passes the publication hold. So a clean-reviewed PR at
/// merge always carries a complete (or owner-waived) result: no API order
/// produces a partial result at merge, and that case is not tested. An
/// incomplete fix round on an open PR is held at publication instead, its
/// existing body made safe, and no later step merges it.
#[tokio::test]
async fn an_incomplete_fix_round_collected_through_the_api_keeps_its_open_pr_unmerged_and_safe() {
    let task = TaskId::new("t");
    for outcome in incomplete_results() {
        let f = fixture().await;
        f.collect_reported(
            &task,
            "b1",
            &f.base,
            "Closes #323, fixes #283, RESOLVED org/repo#324",
            None,
        )
        .await;
        dispatch::step_now(&f.e, &task).await.unwrap();
        assert_eq!(f.e.proj().tasks[&task].view.state, TaskState::InReview);
        assert!(
            f.fake
                .pr_body(&f.repo, 1)
                .unwrap()
                .starts_with("Closes #323")
        );
        let head = f.e.proj().tasks[&task].view.head.clone().unwrap();
        f.collect_reported(
            &task,
            "b2",
            &head,
            "Closes #323, fixes #283, RESOLVED org/repo#324",
            Some(outcome.clone()),
        )
        .await;
        for _ in 0..3 {
            dispatch::step_now(&f.e, &task).await.unwrap();
        }
        let t = f.e.proj().tasks[&task].clone();
        assert!(matches!(t.view.state, TaskState::Stopped { .. }));
        assert_eq!(t.view.outcome, outcome);
        assert_eq!(t.view.head.as_ref(), Some(&head));
        assert_eq!(
            f.fake.pr(&f.repo, 1).await.unwrap().state,
            dvb_core::forge::PrState::Open
        );
        let body = f.fake.pr_body(&f.repo, 1).unwrap().to_ascii_lowercase();
        assert!(
            !body.contains("closes") && !body.contains("fixes") && !body.contains("resolved"),
            "{body}"
        );
        assert!(
            body.contains("refs #323")
                && body.contains("refs #283")
                && body.contains("refs org/repo#324")
        );
    }
}

async fn completion_answer(f: &Fixture, choice: u32) -> dvb_core::QuestionId {
    let id =
        f.e.proj()
            .questions
            .values()
            .find(|q| matches!(q.kind, dvb_core::event::QuestionKind::Completion { .. }))
            .unwrap()
            .view
            .id
            .clone();
    let (status, _) = support::http::call(
        &f.e,
        axum::http::Method::POST,
        &format!("/questions/{id}/answer"),
        Some(serde_json::json!({"choice":choice})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    id
}

#[tokio::test]
async fn completion_question_is_durable_once_and_leave_stopped_survives_repeated_dispatch() {
    let f = fixture().await;
    // An autonomous project still asks the owner whether to merge anyway.
    let (status, body) = support::http::call(
        &f.e,
        axum::http::Method::PUT,
        "/settings",
        Some(serde_json::json!({"values": {"autonomous_projects": ["project"]}})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    f.collect_round("b1", &f.base, None);
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    for _ in 0..3 {
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    }
    assert!(
        f.e.proj()
            .questions
            .values()
            .all(|q| q.view.open && q.view.answer.is_none())
    );
    let events = support::events::all(&f.e);
    let replay = Projections::rebuild(&events);
    assert_eq!(replay.questions.len(), 1);
    assert!(
        replay
            .questions
            .values()
            .next()
            .unwrap()
            .view
            .subject
            .contains("Three consecutive passes not achieved")
    );
    completion_answer(&f, 3).await;
    for _ in 0..3 {
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    }
    assert_eq!(f.e.proj().questions.len(), 1);
    assert!(matches!(
        f.e.proj().tasks[&TaskId::new("t")].view.state,
        TaskState::Stopped { .. }
    ));
}

/// The prompt of the task's current run (its reviewer once in review).
async fn current_prompt(f: &Fixture, template: &str) -> String {
    let (run, project) = {
        let proj = f.e.proj();
        let run = proj.tasks[&TaskId::new("t")].facts.run.clone().unwrap();
        (
            proj.runs[&run].clone(),
            proj.projects[&ProjectId::new("p")].clone(),
        )
    };
    let vars = dvb_engine::prompt::context(&f.e, &run, &project)
        .await
        .unwrap();
    dvb_engine::prompt::render(template, &vars, false)
}

/// "Merge anyway" stays with the task: a later round reporting the same
/// unmet condition goes on to review and CI with no new question, and its
/// reviewer is told the condition is waived. A different condition still
/// asks again.
#[tokio::test]
async fn completion_override_still_requires_review_and_carries_to_later_rounds_of_the_same_condition()
 {
    let f = fixture().await;
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    let task = TaskId::new("t");
    f.collect_round("b1", &f.base, Some("Fixes #283"));
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &task).await.unwrap();
    let id = completion_answer(&f, 1).await;
    dispatch::step_now(&f.e, &task).await.unwrap();
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&task].view.state, TaskState::InReview);
    assert_eq!(
        f.fake.pr(&f.repo, 1).await.unwrap().state,
        dvb_core::forge::PrState::Open
    );
    assert!(f.fake.pr_body(&f.repo, 1).unwrap().contains("Refs #283"));
    let reviewer = current_prompt(&f, include_str!("../../../prompts/code-reviewer.md")).await;
    assert!(
        reviewer.contains("## Completion conditions the owner waived"),
        "{reviewer}"
    );
    assert!(
        reviewer.contains("- Three consecutive passes not achieved\n"),
        "{reviewer}"
    );

    // A fix round reports the same condition again, worded with other spacing.
    let head = f.e.proj().tasks[&task].view.head.clone().unwrap();
    f.collect_round("b2", &head, None);
    report_outcome(
        &f,
        "b2",
        dvb_core::run_verbs::BuilderOutcome::Partial {
            reason: dvb_core::run_verbs::PartialReason::CompletionUnmet,
            detail: "Three consecutive  passes\nnot achieved ".into(),
        },
    );
    dispatch::step_now(&f.e, &task).await.unwrap();
    dispatch::step_now(&f.e, &task).await.unwrap();
    let t = f.e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_ne!(t.view.head.as_ref(), Some(&head));
    assert_eq!(f.e.proj().questions.len(), 1);
    assert!(!f.e.proj().questions[&id].view.open);
    let builder = {
        let proj = f.e.proj();
        let project = proj.projects[&ProjectId::new("p")].clone();
        (proj.runs[&RunId::new("b2")].clone(), project)
    };
    let vars = dvb_engine::prompt::context(&f.e, &builder.0, &builder.1)
        .await
        .unwrap();
    let prompt =
        dvb_engine::prompt::render(include_str!("../../../prompts/builder.md"), &vars, false);
    assert!(
        prompt.contains("- Three consecutive passes not achieved\n"),
        "{prompt}"
    );

    // Another unmet condition is the owner's decision again.
    let head = f.e.proj().tasks[&task].view.head.clone().unwrap();
    f.collect_round("b3", &head, None);
    report_outcome(
        &f,
        "b3",
        dvb_core::run_verbs::BuilderOutcome::Partial {
            reason: dvb_core::run_verbs::PartialReason::CompletionUnmet,
            detail: "The release fixture is missing".into(),
        },
    );
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert!(matches!(
        f.e.proj().tasks[&task].view.state,
        TaskState::Stopped { .. }
    ));
    assert_eq!(f.e.proj().questions.len(), 2);
    let newest =
        f.e.proj()
            .questions
            .values()
            .find(|q| q.view.open)
            .unwrap()
            .view
            .id
            .clone();
    assert_ne!(newest, id);
}

#[tokio::test]
async fn completion_retry_starts_one_new_builder_and_its_result_cannot_reuse_the_override() {
    let f = fixture().await;
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    f.collect_round("b1", &f.base, None);
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    completion_answer(&f, 2).await;
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    let successor = f.e.proj().tasks[&TaskId::new("t")]
        .facts
        .builder
        .clone()
        .unwrap();
    assert_ne!(successor, RunId::new("b1"));
    assert_eq!(
        f.e.proj().runs[&successor].outcome,
        dvb_core::run_verbs::BuilderOutcome::Complete
    );
    assert!(
        f.e.proj().runs[&successor]
            .assignment
            .reason
            .contains("Three consecutive passes not achieved")
    );
    for _ in 0..3 {
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    }
    assert_eq!(
        f.e.proj().tasks[&TaskId::new("t")].facts.builder.as_ref(),
        Some(&successor)
    );
}

/// A builder stopped on an unmet completion condition before it
/// published, retried with the owner's text, gets one builder round on its
/// kept work with the text; that round's complete result publishes and is
/// reviewed against the text.
#[tokio::test]
async fn a_completion_stop_without_a_head_retried_with_text_builds_on_its_kept_work() {
    const TEXT: &str = "The journal line exists on the host: point 1 needs no change.";
    let f = fixture().await;
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    let task = TaskId::new("t");
    let kept = f.collect_round("b1", &f.base, Some("Points 2-6 done"));
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &task).await.unwrap();
    let t = f.e.proj().tasks[&task].clone();
    assert!(matches!(t.view.state, TaskState::Stopped { .. }));
    assert_eq!((t.view.head, t.view.pr), (None, None));
    let question =
        f.e.proj()
            .questions
            .values()
            .next()
            .unwrap()
            .view
            .id
            .clone();

    // Without text it is still a resume, not a retry.
    match dispatch::retry_failed(&f.e, &task, Actor::Owner, None, |_| Ok(())).await {
        Err(dvb_engine::Fail::Refused(r)) => {
            assert_eq!(r.code, dvb_core::RefusalCode::AlreadyStopped);
        }
        other => panic!("{other:?}"),
    }
    let state = dispatch::retry_failed(&f.e, &task, Actor::Owner, Some(TEXT.into()), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(state, TaskState::Preparing);
    assert!(!f.e.proj().questions[&question].view.open);
    let builder = f.e.proj().tasks[&task].facts.builder.clone().unwrap();
    assert_ne!(builder, RunId::new("b1"));
    assert_eq!(
        f.e.proj().runs[&builder].assignment.base.as_deref(),
        Some(f.base.as_str())
    );
    let prompt = current_prompt(&f, include_str!("../../../prompts/builder.md")).await;
    assert!(prompt.contains("## Retry context"), "{prompt}");
    assert!(prompt.contains(TEXT), "{prompt}");

    // It carries on in the kept workspace and finishes the task.
    support::wait::SLOW
        .until("the retried builder to run", || {
            f.e.proj().tasks[&task].view.state == TaskState::Running
        })
        .await;
    let ws = f.e.cfg.paths.work.join(builder.as_str()).join("ws");
    assert_eq!(git(&ws, &["rev-parse", "refs/dvb/collected"]), kept);
    std::fs::write(ws.join("notes"), "point 1 confirmed\n").unwrap();
    git(&ws, &["add", "."]);
    let tree = git(&ws, &["write-tree"]);
    let head = git(&ws, &["commit-tree", &tree, "-p", &f.base, "-m", "finish"]);
    collect(&ws, &head);
    f.e.append_checked(
        Some(&ProjectId::new("p")),
        Actor::Run(builder.clone()),
        |_, _| {
            Ok(vec![EventKind::RunCollected {
                outcome: Some(dvb_core::run_verbs::BuilderOutcome::Complete),
                run: builder.clone(),
                head: Some(head.clone()),
                summary: Some("Points 1-6 done".into()),
            }])
        },
    )
    .unwrap();
    f.e.run_command(
        &builder,
        dvb_engine::Decision::new("collected", Command::HeadWritten),
    )
    .unwrap();
    f.e.end_run(&builder, EndReason::Completed, None).unwrap();
    for _ in 0..3 {
        dispatch::step_now(&f.e, &task).await.unwrap();
    }
    let t = f.e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert_eq!(t.view.pr, Some(1));
    assert_eq!(
        f.fake.pr(&f.repo, 1).await.unwrap().state,
        dvb_core::forge::PrState::Open
    );
    let reviewer = current_prompt(&f, include_str!("../../../prompts/code-reviewer.md")).await;
    assert!(reviewer.contains(&format!("1. {TEXT}")), "{reviewer}");
}

/// A stopped task with no head and no kept work is still refused as
/// stopped, with text too.
#[tokio::test]
async fn a_stopped_task_without_kept_work_retried_with_text_is_refused_as_stopped() {
    let f = fixture().await;
    let task = TaskId::new("t");
    f.collect_round("b1", &f.base, None);
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &task).await.unwrap();
    std::fs::remove_file(f.e.cfg.paths.work.join("b1").join("prepared")).unwrap();
    match dispatch::retry_failed(&f.e, &task, Actor::Owner, Some("Go on".into()), |_| Ok(())).await
    {
        Err(dvb_engine::Fail::Refused(r)) => {
            assert_eq!(r.code, dvb_core::RefusalCode::AlreadyStopped);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn completion_merge_override_retains_ci_and_merges_only_with_a_clean_review_and_green_ci() {
    let f = fixture().await;
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "project_checks".into(),
            old: serde_json::json!("no_ci"),
            new: serde_json::json!({"expected":["rust"]}),
            restored_from: None,
        }])
    })
    .unwrap();
    f.collect_round("b1", &f.base, None);
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    completion_answer(&f, 1).await;
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    clean_review(&f);
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    assert_eq!(
        f.fake.pr(&f.repo, 1).await.unwrap().state,
        dvb_core::forge::PrState::Open
    );
    let head = f.e.proj().tasks[&TaskId::new("t")]
        .view
        .head
        .clone()
        .unwrap();
    f.fake.set_checks(
        &f.repo,
        &head,
        vec![dvb_core::forge::CheckObservation {
            name: "rust".into(),
            status: dvb_core::event::ChecksStatus::Success,
            run: None,
            conclusion: Some("success".into()),
            url: None,
        }],
    );
    // A pending CI observation may have returned the task to checks.
    for _ in 0..3 {
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    }
    assert_eq!(
        f.fake.pr(&f.repo, 1).await.unwrap().state,
        dvb_core::forge::PrState::Merged
    );
}

fn restart(f: &mut Fixture) {
    f.e = support::engine::open_with(
        &f._tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(f.recorded.clone()),
            launcher: Some(Arc::new(support::launcher::FakeLauncher::default())),
            ..Default::default()
        },
    );
    f.e.manual_dispatch();
}

#[tokio::test]
async fn completion_answers_survive_restart_before_their_effect_without_duplicate_questions() {
    for choice in [1, 2, 3] {
        let mut f = fixture().await;
        let (roles, hires) = support::events::default_role_hire_events();
        f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
            Ok(roles.into_iter().chain(hires).collect())
        })
        .unwrap();
        f.collect_round("b1", &f.base, None);
        report_outcome(&f, "b1", incomplete_results()[1].clone());
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
        restart(&mut f);
        assert_eq!(f.e.proj().questions.len(), 1);
        completion_answer(&f, choice).await;
        restart(&mut f);
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
        let t = f.e.proj().tasks[&TaskId::new("t")].clone();
        match choice {
            1 => assert_eq!(t.view.state, TaskState::Publishing),
            2 => assert_ne!(t.facts.builder, Some(RunId::new("b1"))),
            3 => assert!(matches!(t.view.state, TaskState::Stopped { .. })),
            _ => unreachable!(),
        }
        assert_eq!(f.e.proj().questions.len(), 1);
    }
}

#[tokio::test]
async fn prerequisite_merge_automatically_admits_the_approved_dependent_once_on_merged_base() {
    let f = fixture().await;
    let dependent = TaskId::new("caller");
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
        events.push(EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![dvb_core::event::PlannedTask {
                task: dependent.clone(),
                title: "Caller".into(),
                role: dvb_core::RoleId::new("backend"),
                review: dvb_core::roles::ReviewKind::Code,
                summary: "Use merged API".into(),
                after: vec![TaskId::new("t")],
                routine: false,
            }],
        });
        Ok(events)
    })
    .unwrap();
    assert!(f.e.proj().tasks[&dependent].facts.run.is_none());
    assert_eq!(
        f.e.proj().tasks[&dependent].view.waiting_for,
        vec![TaskId::new("t")]
    );
    f.collect_round("b1", &f.base, None);
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    clean_review(&f);
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    support::wait::SLOW
        .until("dependent to start after merge", || {
            f.e.proj().tasks[&dependent].facts.run.is_some()
        })
        .await;
    for _ in 0..3 {
        dispatch::start_waiting(f.e.clone()).await.unwrap();
    }
    let proj = f.e.proj();
    let merged = proj.tasks[&TaskId::new("t")].merged.clone().unwrap();
    let runs: Vec<_> = proj
        .runs
        .values()
        .filter(|r| r.assignment.task.as_ref() == Some(&dependent))
        .map(|r| r.assignment.clone())
        .collect();
    drop(proj);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].base.as_deref(), Some(merged.as_str()));
    assert_eq!(runs[0].hire.as_str(), "h-backend");
}

#[tokio::test]
async fn completion_override_does_not_undo_a_later_explicit_stop() {
    let f = fixture().await;
    f.collect_round("b1", &f.base, None);
    report_outcome(&f, "b1", incomplete_results()[1].clone());
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    completion_answer(&f, 1).await;
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    dispatch::stop(&f.e, &TaskId::new("t"), Actor::Owner)
        .await
        .unwrap();
    for _ in 0..3 {
        dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    }
    assert!(matches!(
        f.e.proj().tasks[&TaskId::new("t")].view.state,
        TaskState::Stopped { .. }
    ));
    assert!(f.fake.find_pr(&f.repo, "dvb/t-1").await.unwrap().is_none());
}

#[tokio::test]
async fn completion_recheck_waits_for_the_stopped_worker_to_end_before_reusing_work() {
    let f = fixture().await;
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
        events.extend([
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("live"))
                    .task(Some(TaskId::new("t")))
                    .base(Some(f.base.clone()))
                    .build(),
            ),
            EventKind::RunCollected {
                run: RunId::new("live"),
                head: None,
                summary: None,
                outcome: Some(incomplete_results()[1].clone()),
            },
            EventKind::TaskStateChanged {
                task: TaskId::new("t"),
                from: TaskState::Planned,
                to: TaskState::Stopped {
                    was: Box::new(TaskState::Running),
                },
            },
        ]);
        Ok(events)
    })
    .unwrap();
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    completion_answer(&f, 2).await;
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    assert_eq!(
        f.e.proj().tasks[&TaskId::new("t")].facts.builder,
        Some(RunId::new("live"))
    );
    f.e.end_run(&RunId::new("live"), EndReason::Stopped, None)
        .unwrap();
    dispatch::step_now(&f.e, &TaskId::new("t")).await.unwrap();
    assert_ne!(
        f.e.proj().tasks[&TaskId::new("t")].facts.builder,
        Some(RunId::new("live"))
    );
}

fn orchestrator_runs(e: &AppState) -> Vec<RunId> {
    e.proj()
        .runs
        .values()
        .filter(|r| r.assignment.is_orchestrator())
        .map(|r| r.assignment.run.clone())
        .collect()
}

#[tokio::test]
async fn a_blocked_result_wakes_the_orchestrator_once_and_resumes_when_its_task_merges() {
    let mut f = fixture().await;
    let (p, prerequisite, blocked) = (ProjectId::new("p"), TaskId::new("t"), TaskId::new("caller"));
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&p), Actor::Engine, |_, _| {
        let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
        events.push(EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![dvb_core::event::PlannedTask {
                task: blocked.clone(),
                title: "Caller".into(),
                role: dvb_core::RoleId::new("backend"),
                review: dvb_core::roles::ReviewKind::Code,
                summary: "Use the API".into(),
                after: vec![],
                routine: false,
            }],
        });
        Ok(events)
    })
    .unwrap();
    f.collect_round("b1", &f.base, None);
    dispatch::step_now(&f.e, &prerequisite).await.unwrap();
    let outcome = dvb_core::run_verbs::BuilderOutcome::Blocked {
        prerequisite: "t".into(),
    };
    f.collect_reported(
        &blocked,
        "c1",
        &f.base,
        "Waiting for the API",
        Some(outcome.clone()),
    )
    .await;
    dispatch::step_now(&f.e, &blocked).await.unwrap();
    assert!(matches!(
        f.e.proj().tasks[&blocked].view.state,
        TaskState::Stopped { .. }
    ));
    support::wait::SLOW
        .until("the orchestrator to be woken", || {
            orchestrator_runs(&f.e).len() == 1
        })
        .await;
    let woken = orchestrator_runs(&f.e).remove(0);
    let version = f.e.proj().goals[&GoalId::new("g")].version;
    assert_eq!(f.e.proj().runs[&woken].goal_version, Some(version));
    let prompt =
        std::fs::read_to_string(f.e.cfg.paths.work.join(woken.as_str()).join("prompt.md")).unwrap();
    assert!(
        prompt.contains(
            r#"
  Task caller builder outcome: {"prerequisite":"t","status":"blocked"}. Reason: t
  Task caller resumes by itself when task t merges."#
        ),
        "{prompt}"
    );
    dispatch::run_ended(&f.e, &woken, EndReason::Completed)
        .await
        .unwrap();
    // Neither another wake, a later step nor a restart repeats it, and the
    // stopped task does not start before its prerequisite merges.
    for restarted in [false, true] {
        if restarted {
            restart(&mut f);
        }
        dvb_engine::wake::wake_if_missed(&f.e, &p);
        dvb_engine::wake::wake(
            f.e.clone(),
            p.clone(),
            dvb_engine::wake::Wake::Approved,
            0,
            0,
        )
        .await
        .unwrap();
        dispatch::step_now(&f.e, &blocked).await.unwrap();
        assert_eq!(orchestrator_runs(&f.e), vec![woken.clone()]);
        let t = f.e.proj().tasks[&blocked].clone();
        assert!(matches!(t.view.state, TaskState::Stopped { .. }));
        assert_eq!(t.facts.builder, Some(RunId::new("c1")));
    }
    clean_review(&f);
    dispatch::step_now(&f.e, &prerequisite).await.unwrap();
    let merged = f.e.proj().tasks[&prerequisite].merged.clone().unwrap();
    dispatch::step_now(&f.e, &blocked).await.unwrap();
    let t = f.e.proj().tasks[&blocked].clone();
    let resumed = t.facts.builder.clone().unwrap();
    assert_ne!(resumed, RunId::new("c1"));
    assert!(!matches!(t.view.state, TaskState::Stopped { .. }));
    // The new run carries on in the kept workspace and merges the main
    // that has the prerequisite, as a conflict round does.
    let run = f.e.proj().runs[&resumed].assignment.clone();
    assert_eq!(run.base.as_deref(), Some(f.base.as_str()));
    assert!(
        run.reason
            .starts_with("Task x (t), which this task was blocked on, has merged into main.")
    );
    assert_eq!(
        git(
            &f.e.mirror("project"),
            &["rev-parse", "refs/dvb/merges/caller"]
        ),
        merged
    );
    for _ in 0..3 {
        dispatch::step_now(&f.e, &blocked).await.unwrap();
    }
    restart(&mut f);
    dispatch::step_now(&f.e, &blocked).await.unwrap();
    let builders =
        f.e.proj()
            .runs
            .values()
            .filter(|r| r.assignment.task.as_ref() == Some(&blocked))
            .count();
    assert_eq!(builders, 2);
    assert_eq!(orchestrator_runs(&f.e), vec![woken]);
}

/// A file the builder wrote with a literal token.
fn token_file() -> String {
    format!(
        "fn token() -> &'static str {{\n    \"ghp_{}\"\n}}\n",
        "a".repeat(30)
    )
}

fn scan_hits(f: &Fixture) -> Vec<(String, u32, String)> {
    support::events::all(&f.e)
        .into_iter()
        .filter_map(|ev| {
            if let EventKind::ScanHit {
                file, line, rule, ..
            } = ev.kind
            {
                Some((file, line, rule))
            } else {
                None
            }
        })
        .collect()
}

fn forge_writes(f: &Fixture) -> Vec<&'static str> {
    f.recorded
        .calls
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|c| matches!(*c, "push" | "open_pr" | "edit_pr_body"))
        .collect()
}

/// A hit in the builder's own file: one fix round, in its kept workspace,
/// told the file, line and rule; nothing is pushed.
async fn hit_sent_back(f: &Fixture) -> RunId {
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    let task = TaskId::new("t");
    let token = token_file();
    f.collect_files("b1", &f.base, Some("Built."), &[("token.rs", &token)]);
    dispatch::step_now(&f.e, &task).await.unwrap();
    let t = f.e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::Preparing);
    assert_eq!(
        scan_hits(f),
        [("token.rs".into(), 2, "github-token".into())]
    );
    assert!(forge_writes(f).is_empty());
    let fix = t.facts.builder.clone().unwrap();
    assert_ne!(fix, RunId::new("b1"));
    let run = f.e.proj().runs[&fix].clone();
    assert_eq!(run.assignment.base.as_deref(), Some(f.base.as_str()));
    let project = f.e.proj().projects[&ProjectId::new("p")].clone();
    let vars = dvb_engine::prompt::context(&f.e, &run, &project)
        .await
        .unwrap();
    let prompt =
        dvb_engine::prompt::render(include_str!("../../../prompts/builder.md"), &vars, false);
    assert!(prompt.contains("## Fix what failed"), "{prompt}");
    assert!(
        prompt.contains("publish blocked: github-token in token.rs:2."),
        "{prompt}"
    );
    assert!(prompt.contains("build fake tokens at run time"), "{prompt}");
    assert!(
        !prompt.contains(&format!("ghp_{}", "a".repeat(30))),
        "{prompt}"
    );
    fix
}

#[tokio::test]
async fn a_run_work_dir_publishes_and_the_pr_body_shows_no_run_id() {
    let f = fixture().await;
    let task = TaskId::new("t");
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        Ok(roles.into_iter().chain(hires).collect())
    })
    .unwrap();
    let run = "01M59VRF3PQDEY1AQYY29F3TS6";
    let cache = format!("/var/lib/dvb/work/{run}/ws/node_modules/.cache/file\n");
    let summary = format!("Built in /var/lib/dvb/work/{run}/ws.");
    f.collect_files("b1", &f.base, Some(&summary), &[("cache.txt", &cache)]);
    dispatch::step_now(&f.e, &task).await.unwrap();
    let t = f.e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert!(scan_hits(&f).is_empty());
    assert_eq!(forge_writes(&f), ["push", "open_pr"]);
    let body = f.fake.pr_body(&f.repo, 1).unwrap();
    assert!(
        body.starts_with("Built in /var/lib/dvb/work/<run>/ws."),
        "{body}"
    );
    let head = t.view.head.unwrap();
    let pushed = git(
        &f.e.mirror("project"),
        &["show", &format!("{head}:cache.txt")],
    );
    assert_eq!(pushed, cache.trim_end());
}

#[tokio::test]
async fn a_scan_hit_in_the_builders_file_gets_one_fix_round_and_a_clean_head_publishes() {
    let f = fixture().await;
    let task = TaskId::new("t");
    hit_sent_back(&f).await;
    // The round's builder takes the token out (a run carrying on in the
    // kept workspace collects it).
    f.collect_files(
        "b2",
        &f.base,
        Some("Built the token at run time."),
        &[(
            "token.rs",
            "fn token() -> String {\n    format!(\"ghp_{}\", \"a\".repeat(30))\n}\n",
        )],
    );
    dispatch::step_now(&f.e, &task).await.unwrap();
    let t = f.e.proj().tasks[&task].clone();
    assert_eq!(t.view.state, TaskState::InReview);
    assert!(!t.scan_hit);
    assert_eq!(scan_hits(&f).len(), 1);
    assert_eq!(forge_writes(&f), ["push", "open_pr"]);
    let head = t.view.head.unwrap();
    let pushed = git(
        &f.e.mirror("project"),
        &["show", &format!("{head}:token.rs")],
    );
    assert!(pushed.contains("format!"), "{pushed}");
    let replay = Projections::rebuild(&support::events::all(&f.e));
    assert!(!replay.tasks[&task].scan_hit);
}

#[tokio::test]
async fn an_unrelated_run_end_wakes_a_scan_fix_round_waiting_for_a_slot() {
    let mut f = fixture().await;
    let task = TaskId::new("t");
    let other = RunId::new("other-reviewer");
    let (roles, hires) = support::events::default_role_hire_events();
    f.e.append_checked(Some(&ProjectId::new("p")), Actor::Engine, |_, _| {
        let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
        events.extend([
            EventKind::SettingsChanged {
                key: "max_runs".into(),
                old: serde_json::json!(null),
                new: serde_json::json!(1),
                restored_from: None,
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(other.clone())
                    .reviews(Some("unrelated-head".into()))
                    .build(),
            ),
        ]);
        Ok(events)
    })
    .unwrap();
    f.collect_files(
        "b1",
        &f.base,
        Some("Built."),
        &[("token.rs", &token_file())],
    );
    // Reopen with automatic dispatch so only the unrelated run's end can
    // wake the publication after its first look finds every slot occupied.
    f.e = support::engine::open_with(
        &f._tmp.path().join("engine"),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(f.recorded.clone()),
            launcher: Some(Arc::new(support::launcher::FakeLauncher::default())),
            ..Default::default()
        },
    );
    assert_eq!(f.e.proj().live_runs().count(), 1);
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert_eq!(f.e.proj().tasks[&task].view.state, TaskState::Publishing);
    assert!(scan_hits(&f).is_empty());
    assert!(forge_writes(&f).is_empty());

    dispatch::run_ended(&f.e, &other, EndReason::Completed)
        .await
        .unwrap();
    support::wait::FAST
        .until("the scan fix round after an unrelated run ends", || {
            f.e.proj().tasks[&task].scan_hit
        })
        .await;
    dispatch::step_now(&f.e, &task).await.unwrap();
    assert_eq!(
        scan_hits(&f),
        [("token.rs".into(), 2, "github-token".into())]
    );
    assert!(forge_writes(&f).is_empty());
    let proj = f.e.proj();
    let builders: Vec<_> = proj
        .runs
        .values()
        .filter(|r| r.builds() == Some(&task))
        .collect();
    assert_eq!(builders.len(), 2);
    let fix = &proj.runs[proj.tasks[&task].facts.builder.as_ref().unwrap()];
    assert_eq!(fix.assignment.reason, "Fix the publish scan hit.");
    assert_eq!(proj.live_runs().count(), 1);
}

#[tokio::test]
async fn a_scan_hit_still_there_after_its_fix_round_fails_the_task() {
    let f = fixture().await;
    let task = TaskId::new("t");
    hit_sent_back(&f).await;
    let token = token_file();
    f.collect_files("b2", &f.base, Some("Built."), &[("token.rs", &token)]);
    dispatch::step_now(&f.e, &task).await.unwrap();
    let state = f.e.proj().tasks[&task].view.state.clone();
    assert!(
        matches!(&state, TaskState::Failed { reason }
            if reason.ends_with("publish blocked: github-token in token.rs:2")),
        "{state:?}"
    );
    assert_eq!(scan_hits(&f).len(), 2);
    assert!(forge_writes(&f).is_empty());
    // One fix round only: b1, the engine's round, and b2.
    let builders = f.e.proj().runs.len();
    assert_eq!(builders, 3);
    let replay = Projections::rebuild(&support::events::all(&f.e));
    assert!(replay.tasks[&task].scan_hit);
}
