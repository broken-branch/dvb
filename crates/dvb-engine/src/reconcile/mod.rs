//! Startup recovery marks in-flight tasks unknown, then observes their units
//! and collected heads before deciding their state. Active units are watched,
//! ended units receive durable endings, and failed observations are retried.
//! Dispatch reconciles forge effects after the task's state is known.

use crate::dispatch;
use crate::engine::{AppState, Fail};
use crate::wake;
use crate::{launch, spawn};
use dvb_core::event::EndReason;
use dvb_core::launcher::{UnitId, UnitState};
use dvb_core::state::Observed;
use dvb_core::{Actor, Command, EventKind, RunId, TaskId, TaskState};
use std::time::Duration;

/// How long a task whose reconciliation failed stays unknown before it is tried again.
const AGAIN: Duration = Duration::from_secs(5);

/// How soon a run whose unit could not be read is asked about again.
const RUN_AGAIN: Duration = Duration::from_secs(1);

/// Marks every task that may have something in flight unknown. Called before the
/// engine serves, so the first thing anyone sees is "unknown", never a claim.
pub fn mark(e: &AppState) -> Result<(), Fail> {
    let tasks: Vec<TaskId> = e.proj().tasks.keys().cloned().collect();
    for t in &tasks {
        e.decide(
            e.proj(),
            t,
            Actor::Engine,
            crate::Decision::new("restart", Command::Restarted),
            |_| Ok(()),
        )?;
    }
    Ok(())
}

/// Observes and decides every unknown task, each on its own (a failed
/// observation keeps only its task unknown), and lets everything else carry on.
pub async fn reconcile(e: AppState) -> Result<(), Fail> {
    dispatch::sweep_run_dirs(&e);
    runs(&e).await;
    let unknown: Vec<TaskId> = e
        .proj()
        .tasks
        .values()
        .filter(|t| matches!(t.view.state, TaskState::Unknown { .. }))
        .map(|t| t.view.id.clone())
        .collect();
    for task in unknown {
        spawn("reconcile", until_reconciled(e.clone(), task));
    }
    // The rest carry on now; a reconciled task is kicked by its new state.
    dispatch::kick_all(&e, None);
    // Starts the run cap refused are the engine's own, not a missed wake, and
    // so are the never-started tasks of every approved goal.
    spawn(
        "start waiting and approved tasks",
        dispatch::start_approved_goals(e.clone(), None),
    );
    let projects: Vec<_> = e.proj().projects.keys().cloned().collect();
    for p in projects {
        wake::wake_if_missed(&e, &p);
    }
    Ok(())
}

async fn until_reconciled(e: AppState, task: TaskId) -> Result<(), Fail> {
    loop {
        match task_once(&e, &task).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("dvb engine: reconcile {task}: {err}; again in {AGAIN:?}");
                tokio::time::sleep(AGAIN).await;
            }
        }
    }
}

/// Every run the log has as live is settled: still running (watched again), or
/// ended while the engine was away (`run.ended`, with what the run left
/// behind). A run whose unit cannot be read is asked again on its own until it
/// is settled, so no run is left without a watcher or an end, orchestrator runs
/// included (and a missed wake is given once such an orchestrator run ends).
async fn runs(e: &AppState) {
    let live: Vec<RunId> = e
        .proj()
        .live_runs()
        .map(|r| r.assignment.run.clone())
        .collect();
    for run in live {
        if !settle(e, &run).await {
            spawn("reconcile run", settle_later(e.clone(), run));
        }
    }
}

async fn settle_later(e: AppState, run: RunId) -> Result<(), Fail> {
    loop {
        tokio::time::sleep(RUN_AGAIN).await;
        if settle(&e, &run).await {
            let orchestrator = e
                .proj()
                .runs
                .get(&run)
                .filter(|r| r.assignment.task.is_none())
                .map(|r| r.project.clone());
            if let Some(project) = orchestrator {
                wake::wake_if_missed(&e, &project);
            }
            return Ok(());
        }
    }
}

/// One look at a live run: whether it is settled (watched, or ended).
async fn settle(e: &AppState, run: &RunId) -> bool {
    let Some(r) = e
        .proj()
        .runs
        .get(run)
        .filter(|r| r.ended.is_none())
        .cloned()
    else {
        return true;
    };
    let unit = UnitId {
        vendor: r.assignment.vendor,
        run: r.assignment.run.clone(),
    };
    let reason = match e.launcher().state(&unit).await {
        Ok(UnitState::Active) => {
            if r.assignment.task.is_none() && r.stop_requested {
                // A lost acknowledgement does not revoke a stop. Retry it on
                // the target rather than certify this unit as a normal survivor.
                return match dispatch::stop_unit(e, &r, EndReason::Stopped).await {
                    Ok(()) => true,
                    Err(err) => {
                        eprintln!("dvb engine: reconcile stop {run}: {err}");
                        false
                    }
                };
            }
            if r.assignment.task.is_none() && r.interrupted {
                // Persist the observation before installing its watcher. Old
                // startup Completed records lack this evidence and keep their
                // historical interruption meaning without rewriting the log.
                if let Err(err) = e.append_checked(Some(&r.project), Actor::Engine, |proj, _now| {
                    Ok(proj
                        .runs
                        .get(run)
                        .filter(|live| {
                            live.ended.is_none() && live.interrupted && !live.stop_requested
                        })
                        .map(|_| vec![EventKind::RunSurvivedRestart { run: run.clone() }])
                        .unwrap_or_default())
                }) {
                    eprintln!("dvb engine: observe surviving run {run}: {err}");
                    return false;
                }
            }
            spawn(
                "watch run",
                launch::watch(e.clone(), run.clone(), r.assignment.vendor),
            );
            return true;
        }
        // A builder that wrote its head or a reviewer that gave its verdict
        // finished. Anything else was cut off, including a planner with no end.
        Ok(UnitState::Inactive) => {
            let finished = match (&r.assignment.task, &r.assignment.reviews) {
                (None, _) => false,
                (Some(_), Some(_)) => r.verdict_given,
                (Some(_), None) => {
                    r.collected.is_some() || head_file(e, &r.assignment.run).is_some()
                }
            };
            if finished {
                EndReason::Completed
            } else {
                EndReason::Interrupted
            }
        }
        Ok(UnitState::Failed) => EndReason::Crashed,
        Err(err) => {
            eprintln!("dvb engine: reconcile run {run}: {err}; again in {RUN_AGAIN:?}");
            return false;
        }
    };
    // The run's end, with the driver's own account, settles a task still
    // unknown from the restart (and records `engine.reconciled`).
    if let Err(err) = dispatch::run_ended(e, run, reason).await {
        eprintln!("dvb engine: reconcile run {run}: {err}");
        return false;
    }
    true
}

/// The `head` file a run's collect step wrote, if any.
fn head_file(e: &AppState, run: &RunId) -> Option<String> {
    let text =
        crate::launch::read_run_file(&e.cfg.paths.work.join(run.as_str()).join("head")).ok()?;
    let head = text.trim();
    (!head.is_empty()).then(|| head.to_owned())
}

/// One unknown task: observe, decide, and stop a unit that outlived a stop.
pub(crate) async fn task_once(e: &AppState, task: &TaskId) -> Result<(), Fail> {
    let _lock = e.lock(task).await;
    let t = e
        .proj()
        .tasks
        .get(task)
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
    let TaskState::Unknown { .. } = &t.view.state else {
        return Ok(());
    };
    let current = t
        .facts
        .run
        .as_ref()
        .and_then(|r| e.proj().runs.get(r).cloned());
    let unit = match current.as_ref().filter(|r| r.ended.is_none()) {
        Some(r) => e
            .launcher()
            .state(&UnitId {
                vendor: r.assignment.vendor,
                run: r.assignment.run.clone(),
            })
            .await
            .map_err(Fail::internal)?,
        None => UnitState::Inactive,
    };
    fail::fail_point!("reconcile.observed");
    // Reports the run made while it was checked (its head, its end) count.
    let current = t
        .facts
        .run
        .as_ref()
        .and_then(|r| e.proj().runs.get(r).cloned());
    let mut pre = Vec::new();
    // The builder's collected head, from its file when its report was lost.
    let head = current
        .as_ref()
        .filter(|r| r.builds().is_some())
        .and_then(|r| {
            let head = r
                .collected
                .clone()
                .or_else(|| head_file(e, &r.assignment.run))?;
            if r.collected.is_none() {
                pre.push(EventKind::RunCollected {
                    outcome: None,
                    run: r.assignment.run.clone(),
                    head: Some(head.clone()),
                    summary: None,
                });
            }
            Some(head)
        });
    // What the forge says about a merge is read by the merge step itself.
    let seen = Observed { unit, head };
    let state = e.decide(
        e.proj(),
        task,
        Actor::Engine,
        crate::Decision {
            pre,
            ..crate::Decision::new("reconcile", Command::Reconcile(seen))
        },
        |_| Ok(()),
    )?;
    let Some(r) = current else {
        return Ok(());
    };
    match (&state, &r.ended) {
        // The run ended while it was checked: its end, ignored while the task
        // was unknown, is the task's now.
        (
            TaskState::Preparing | TaskState::Running | TaskState::Collecting | TaskState::InReview,
            Some(reason),
        ) => {
            if !r.verdict_given {
                let cmd = Command::UnitEnded {
                    reason: reason.clone(),
                    detail: r.ended_detail.clone(),
                    stderr: r.ended_stderr.clone(),
                };
                e.decide(
                    e.proj(),
                    task,
                    Actor::Engine,
                    crate::Decision::new("unit_ended", cmd),
                    |_| Ok(()),
                )?;
            }
        }
        // A unit that outlived a stop is stopped.
        (TaskState::Stopped { .. }, None) if unit == UnitState::Active => {
            let unit = UnitId {
                vendor: r.assignment.vendor,
                run: r.assignment.run.clone(),
            };
            if let Err(err) = e.launcher().stop(&unit).await {
                eprintln!("dvb engine: stop {}: {err}", unit.unit_name());
            }
        }
        (
            TaskState::Preparing | TaskState::Running | TaskState::Collecting | TaskState::InReview,
            None,
        )
        | (TaskState::Stopped { .. }, None | Some(_))
        | (
            TaskState::Planned
            | TaskState::Publishing
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Unknown { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. },
            None | Some(_),
        ) => {}
    }
    Ok(())
}

mod collection;
pub(crate) use collection::recover_collection;
