//! Driver reports made by `dvb run exec`; builders also use collected to save
//! their PR summary before the driver snapshots the finished workspace.

use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::event::{OutputItem, RUN_OUTPUT_MAX_BYTES, ServedModel};
use dvb_core::launcher::UnitId;
use dvb_core::run_verbs::{
    CollectedBody, ModelBody, OutputBody, PreflightBody, RateLimitBody, UsageBody,
};
use dvb_core::{Actor, Command, EventKind, Refusal, RefusalCode};

type Ok = Result<Json<serde_json::Value>, Fail>;

fn record(e: &AppState, c: &Caller, kind: EventKind) -> Ok {
    let ev = e.append_checked(
        Some(&c.run.project),
        Actor::Run(c.run.assignment.run.clone()),
        |_, _| Ok(vec![kind]),
    )?;
    Ok(Json(
        serde_json::json!({ "event": ev.first().map(|e| e.id) }),
    ))
}

/// The run's first call, made once by its driver before any session: its unit
/// is active, if the launcher has not said so yet. A later call, from the
/// session, is refused: it must not get the engine to write the marker again.
pub async fn preflight(State(e): State<AppState>, c: Caller, Json(b): Json<PreflightBody>) -> Ok {
    let refused = || {
        Fail::from(Refusal::new(
            RefusalCode::RoleNotAllowed,
            "only the run's driver reports its preflight, once, before its session",
        ))
    };
    if c.run.preflighted {
        return Err(refused());
    }
    if c.run.builds().is_some() {
        e.run_command(
            &c.run.assignment.run,
            crate::Decision::new("unit_active", Command::UnitActive),
        )?;
    }
    let passed = b.checks.iter().all(|c| c.ok);
    let ev = e.append_checked(
        Some(&c.run.project),
        Actor::Run(c.run.assignment.run.clone()),
        |proj, _| {
            let run = proj
                .runs
                .get(&c.run.assignment.run)
                .filter(|r| r.ended.is_none())
                .ok_or_else(|| Refusal::new(RefusalCode::NotARun, "the run has ended"))?;
            if run.preflighted {
                return Err(refused());
            }
            Ok(vec![EventKind::RunPreflight {
                run: c.run.assignment.run.clone(),
                checks: b.checks,
                cli_version: b.cli_version,
                boundary: b.boundary,
            }])
        },
    )?;
    // A builder's workspace that passed preflight is whole: a resume may carry
    // on in it (dispatch's `preserved_workspace`).
    if c.run.builds().is_some() && passed {
        let dir = e.cfg.paths.work.join(c.run.assignment.run.as_str());
        if let Err(err) = crate::launch::write_run_file(&dir.join(crate::launch::PREPARED), "") {
            eprintln!("dvb engine: run {}: {err}", c.run.assignment.run);
        }
    }
    Ok(Json(
        serde_json::json!({ "event": ev.first().map(|e| e.id) }),
    ))
}

/// The served-model evidence. A mismatch kills the unit; the
/// run's driver also stops itself, and `ended` reads its `reason` file.
pub async fn model(State(e): State<AppState>, c: Caller, Json(b): Json<ModelBody>) -> Ok {
    let mismatch = matches!(b.served, ServedModel::Mismatch(_));
    let kind = EventKind::RunModel {
        run: c.run.assignment.run.clone(),
        requested: b.requested,
        served: b.served,
    };
    let reply = record(&e, &c, kind)?;
    if mismatch {
        let unit = UnitId {
            vendor: c.run.assignment.vendor,
            run: c.run.assignment.run.clone(),
        };
        if let Err(err) = e.launcher().stop(&unit).await {
            eprintln!("dvb engine: run {}: stopping the unit: {err}", unit.run);
        }
    }
    Ok(reply)
}

pub(super) fn bounded(mut s: String) -> String {
    s.truncate(dvb_core::text::clip(&s, RUN_OUTPUT_MAX_BYTES).len());
    s
}

pub async fn output(State(e): State<AppState>, c: Caller, Json(b): Json<OutputBody>) -> Ok {
    let item = match b.item {
        OutputItem::Text(t) => OutputItem::Text(bounded(t)),
        OutputItem::ToolCall { name } => OutputItem::ToolCall {
            name: bounded(name),
        },
        OutputItem::Note(t) => OutputItem::Note(bounded(t)),
    };
    let kind = EventKind::RunOutput {
        resource: None,
        run: c.run.assignment.run.clone(),
        item,
    };
    record(&e, &c, kind)
}

/// Tokens from the stream, with the estimated cost from the release's price
/// table (`usage/prices.rs`).
pub async fn usage(State(e): State<AppState>, c: Caller, Json(b): Json<UsageBody>) -> Ok {
    let est_cost = crate::usage::prices::estimate(
        c.run.assignment.vendor,
        &c.run.assignment.model,
        b.uncached_in,
        b.cached_in,
        b.out,
    );
    let kind = EventKind::RunUsage {
        run: c.run.assignment.run.clone(),
        uncached_in: b.uncached_in,
        cached_in: b.cached_in,
        out: b.out,
        est_cost,
    };
    record(&e, &c, kind)
}

/// The vendor's window, read by the run's driver. At 100 percent the vendor is
/// paused (`vendors::record`).
pub async fn ratelimit(State(e): State<AppState>, c: Caller, Json(b): Json<RateLimitBody>) -> Ok {
    crate::vendors::record(&e, c.run.assignment.vendor, &b)?;
    Ok(Json(serde_json::json!({})))
}

/// A builder's write-up, or the driver's snapshot head. Summary submission
/// leaves the task running; automatic collection preserves the saved summary.
pub async fn collected(State(e): State<AppState>, c: Caller, Json(b): Json<CollectedBody>) -> Ok {
    let has_head = b.head.is_some();
    let ev = e.append_checked(
        Some(&c.run.project),
        Actor::Run(c.run.assignment.run.clone()),
        |proj, _now| {
            let run = proj
                .runs
                .get(&c.run.assignment.run)
                .filter(|r| r.ended.is_none())
                .ok_or_else(|| Refusal::new(RefusalCode::NotARun, "the run has ended"))?;
            if let Some(outcome) = &b.outcome
                && outcome
                    .reason()
                    .is_some_and(|s| s.trim().is_empty() || s.len() > 16 * 1024)
            {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "outcome reason must contain 1 to 16384 bytes",
                )
                .into());
            }
            if b.summary.is_some() || b.outcome.is_some() {
                let task = run.builds().and_then(|task| proj.tasks.get(task));
                if !task.is_some_and(|t| {
                    t.facts.run.as_ref() == Some(&run.assignment.run) && run.collected.is_none()
                }) {
                    return Err(Refusal::new(
                        RefusalCode::RoleNotAllowed,
                        "only the current builder may submit its summary before collection",
                    )
                    .into());
                }
            }
            Ok(vec![EventKind::RunCollected {
                outcome: b.outcome,
                run: run.assignment.run.clone(),
                head: b.head,
                summary: b.summary.map(|s| s.trim().to_owned()),
            }])
        },
    )?;
    if has_head && c.run.builds().is_some() {
        e.run_command(
            &c.run.assignment.run,
            crate::Decision::new("collected", Command::HeadWritten),
        )?;
    }
    Ok(Json(
        serde_json::json!({ "event": ev.first().map(|e| e.id) }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use dvb_core::event::{CiMode, EndReason, PlannedTask};
    use dvb_core::roles::ReviewKind;
    use dvb_core::{GoalId, ProjectId, RoleId, RunAssignment, RunId, TaskId, TaskState};

    /// An engine with one running task, `t` in project `p`.
    fn engine_with_task() -> (tempfile::TempDir, AppState, ProjectId, TaskId) {
        let tmp = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(tmp.path(), "unused-dvb".into()),
            Default::default(),
        )
        .unwrap();
        let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::ProjectAdded {
                    id: p.clone(),
                    name: "p".into(),
                    repo: "local".into(),
                    default_branch: "main".into(),
                    ci: CiMode::NoCi,
                },
                EventKind::GoalStated {
                    goal: g.clone(),
                    text: "Goal".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal: g,
                    tasks: vec![PlannedTask {
                        after: vec![],
                        task: t.clone(),
                        title: "Task".into(),
                        role: RoleId::new("backend"),
                        review: ReviewKind::Code,
                        summary: "Work".into(),
                        routine: false,
                    }],
                },
                EventKind::TaskStateChanged {
                    task: t.clone(),
                    from: TaskState::Planned,
                    to: TaskState::Running,
                },
            ])
        })
        .unwrap();
        (tmp, e, p, t)
    }

    fn assignment(t: &TaskId, name: &str, reviews: Option<String>) -> RunAssignment {
        crate::test_support::assignment::Assignment::new(RunId::new(name))
            .task(Some(t.clone()))
            .reason("Work".into())
            .reviews(reviews)
            .base(Some("base".into()))
            .build()
    }

    #[tokio::test]
    async fn preflight_never_writes_through_a_planted_link_and_is_given_once() {
        use dvb_core::event::{PreflightCheck, PreflightCode};
        let (tmp, e, p, t) = engine_with_task();
        let run = RunId::new("builder");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(assignment(
                &t,
                run.as_str(),
                None,
            ))])
        })
        .unwrap();
        let dir = e.cfg.paths.work.join(run.as_str());
        std::fs::create_dir_all(&dir).unwrap();
        // Stands for the engine's database or App key, outside the run directory.
        let outside = tmp.path().join("dvb.sqlite");
        std::fs::write(&outside, "the engine's data").unwrap();
        let marker = dir.join(crate::launch::PREPARED);
        std::os::unix::fs::symlink(&outside, &marker).unwrap();
        let call = async || {
            let caller = Caller {
                run: e.proj().runs[&run].clone(),
            };
            preflight(
                State(e.clone()),
                caller,
                Json(PreflightBody {
                    checks: vec![PreflightCheck {
                        code: PreflightCode::PfUid,
                        ok: true,
                        detail: String::new(),
                    }],
                    cli_version: None,
                    boundary: "fake".into(),
                }),
            )
            .await
        };
        let _ = call().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the engine's data"
        );
        assert!(std::fs::symlink_metadata(&marker).unwrap().is_file());
        // The session, running now, plants the link again and reports a preflight.
        std::fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink(&outside, &marker).unwrap();
        assert!(
            matches!(call().await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the engine's data"
        );
        assert!(std::fs::symlink_metadata(&marker).unwrap().is_symlink());
        let preflights = crate::test_support::events::all(&e)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::RunPreflight { .. }))
            .count();
        assert_eq!(preflights, 1);
    }

    #[tokio::test]
    async fn summary_is_bound_to_the_current_builder_and_survives_driver_collection_and_replay() {
        let (_tmp, e, p, t) = engine_with_task();
        let assignment = |name: &str, reviews| assignment(&t, name, reviews);
        let call = async |run: &RunId, head, summary: Option<&str>| {
            let caller = Caller {
                run: e.proj().runs[run].clone(),
            };
            collected(
                State(e.clone()),
                caller,
                Json(CollectedBody {
                    outcome: None,
                    head,
                    summary: summary.map(str::to_owned),
                }),
            )
            .await
        };
        let old = RunId::new("old");
        let run = RunId::new("builder");
        for name in [old.as_str(), run.as_str()] {
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunStarted(assignment(name, None))])
            })
            .unwrap();
        }
        assert!(
            matches!(call(&old, None, Some("stale")).await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
        );
        let _ = call(&run, None, Some(" First summary ")).await.unwrap();
        let _ = call(&run, None, Some(" Corrected summary ")).await.unwrap();
        let outcome = dvb_core::run_verbs::BuilderOutcome::Blocked {
            prerequisite: "backend task merges".into(),
        };
        let current_caller = Caller {
            run: e.proj().runs[&run].clone(),
        };
        let _ = collected(
            State(e.clone()),
            current_caller,
            Json(CollectedBody {
                head: None,
                summary: None,
                outcome: Some(outcome.clone()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(e.proj().tasks[&t].view.state, TaskState::Running);
        assert_eq!(e.proj().runs[&run].collected, None);
        assert_eq!(e.proj().runs[&old].summary, None);
        let _ = call(&run, Some("snapshot".into()), None).await.unwrap();
        assert_eq!(e.proj().tasks[&t].view.state, TaskState::Collecting);
        assert_eq!(
            e.proj().runs[&run].summary.as_deref(),
            Some("Corrected summary")
        );
        assert!(
            matches!(call(&run, None, Some("too late")).await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
        );
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunEnded {
                run: run.clone(),
                reason: EndReason::Completed,
                detail: None,
            }])
        })
        .unwrap();
        assert!(
            matches!(call(&run, None, Some("ended")).await, Err(Fail::Refused(r)) if r.code == RefusalCode::NotARun)
        );
        let review = RunId::new("reviewer");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(assignment(
                review.as_str(),
                Some("snapshot".into()),
            ))])
        })
        .unwrap();
        assert!(
            matches!(call(&review, None, Some("reviewer")).await, Err(Fail::Refused(r)) if r.code == RefusalCode::RoleNotAllowed)
        );
        let replay = crate::proj::Projections::rebuild(&crate::test_support::events::all(&e));
        assert_eq!(
            replay.runs[&run].summary.as_deref(),
            Some("Corrected summary")
        );
        assert_eq!(replay.runs[&run].collected.as_deref(), Some("snapshot"));
        assert_eq!(replay.runs[&run].outcome, outcome);
        assert_eq!(replay.tasks[&t].view.outcome, outcome);
    }
}
