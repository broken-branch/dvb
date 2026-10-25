//! Task scheduling and effects derived from task state.
//!
//! One dispatcher holds each task's lock while reading state, sending effects
//! and recording results. State changes, run ends, unpause and startup wake it.
//! Publication markers and forge reads make effect recovery idempotent.

use crate::engine::{AppState, Fail};
use crate::proj::{Project, Task};
use crate::spawn;
use checks::checks;
use dvb_core::{ProjectId, TaskId, TaskState};
use effects::{publish, retry, review};
use merge::merge;
use runs::{launch_later, unhold};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use control::stop_again;

mod checks;
mod control;
mod effects;
mod merge;
mod outcomes;
pub(crate) use outcomes::result as completion_result;
pub(crate) use outcomes::waivers as completion_waivers;
mod start;
pub(crate) use runs::{
    BUILD_DIR, drop_build_output, drop_finished_runs, round_hire, sweep_run_dirs,
};
pub(crate) use start::start_as_asked;
mod runs;

pub(crate) use runs::Launch;
pub(crate) use runs::decide_launch;
pub(crate) use runs::launch_events;
pub use runs::run_ended;
#[cfg(any(test, feature = "test-support"))]
pub use runs::start_task;
pub use runs::start_waiting;
pub(crate) use runs::wait_launch;
pub(crate) use runs::{start_approved, start_approved_goals};

pub use control::resume;
pub use control::retry_failed;
pub(crate) use control::round_cap_question_event;
pub use control::stop;
pub use control::stop_for_update;
pub(crate) use control::stop_unit;
pub(crate) use control::stop_with;

/// A running dispatcher: woken by `notify`, `dirty` when kicked since its last
/// look at the task.
pub struct Running {
    notify: Arc<Notify>,
    dirty: bool,
}

enum Next {
    /// Look again after this long (a retry's backoff, the CI poll).
    Wait(Duration),
    /// Nothing to do until the task is kicked.
    Idle,
}

/// A failed step is tried again after this long, doubling up to the cap and
/// starting over after a step that succeeds.
const STEP_BACKOFF: Duration = Duration::from_secs(30);
const STEP_BACKOFF_MAX: Duration = Duration::from_secs(600);

/// Wakes the task's dispatcher, starting one if none is running.
pub fn kick(e: &AppState, task: &TaskId) {
    e.remember();
    if !e.dispatch_enabled() {
        return;
    }
    let mut running = e.dispatching.lock().unwrap_or_else(dvb_core::sync::recover);
    if let Some(r) = running.get_mut(task) {
        r.dirty = true;
        r.notify.notify_one();
        return;
    }
    let notify = Arc::new(Notify::new());
    running.insert(
        task.clone(),
        Running {
            notify: notify.clone(),
            dirty: false,
        },
    );
    drop(running);
    tokio::spawn(dispatcher(e.clone(), task.clone(), notify));
}

/// Kicks every task that has not ended, of one project or of all.
pub fn kick_all(e: &AppState, project: Option<&ProjectId>) {
    let tasks: Vec<TaskId> = e
        .proj()
        .tasks
        .values()
        .filter(|t| project.is_none_or(|p| &t.view.project == p))
        .filter(|t| !ended(&t.view.state))
        .map(|t| t.view.id.clone())
        .collect();
    for t in &tasks {
        kick(e, t);
    }
}

fn ended(s: &TaskState) -> bool {
    matches!(
        s,
        TaskState::Done | TaskState::Cancelled | TaskState::Failed { .. }
    )
}

async fn dispatcher(e: AppState, task: TaskId, notify: Arc<Notify>) {
    let mut backoff = STEP_BACKOFF;
    loop {
        let next = match step(&e, &task).await {
            Ok(next) => {
                backoff = STEP_BACKOFF;
                next
            }
            Err(err) => {
                eprintln!("dvb engine: task {task}: {err}; retrying in {backoff:?}");
                let wait = backoff;
                backoff = Duration::from_millis(
                    u64::try_from(dvb_core::state::backoff_millis(
                        2,
                        i64::try_from(backoff.as_millis()).unwrap_or(i64::MAX),
                        i64::try_from(STEP_BACKOFF_MAX.as_millis()).unwrap_or(i64::MAX),
                    ))
                    .unwrap_or(u64::MAX),
                );
                Next::Wait(wait)
            }
        };
        if let Next::Wait(d) = next {
            tokio::select! {
                () = tokio::time::sleep(d) => {}
                () = notify.notified() => {}
            }
        }
        let mut running = e.dispatching.lock().unwrap_or_else(dvb_core::sync::recover);
        let Some(r) = running.get_mut(&task) else {
            return;
        };
        match next {
            Next::Wait(_) => r.dirty = false,
            Next::Idle if r.dirty => r.dirty = false,
            Next::Idle => {
                running.remove(&task);
                return;
            }
        }
        drop(running);
    }
}

/// One dispatcher step for the task now, under its lock (tests, and callers
/// that need its result rather than a kick).
#[cfg(any(test, feature = "test-support"))]
pub async fn step_now(e: &AppState, task: &TaskId) -> Result<(), Fail> {
    e.remember();
    step(e, task).await.map(|_| ())
}

/// One look at the task, under its lock. A step is an effect section of the
/// update protocol: an update waits for a step that has begun, and one that
/// has not waits until the update is over; it is taken before the task's lock,
/// so nothing waits at the gate holding a task.
async fn step(e: &AppState, task: &TaskId) -> Result<Next, Fail> {
    let _fx = e.update.effect().await;
    let _lock = e.lock(task).await;
    let (t, p) = read(e, task)?;
    // A unit a stop could not confirm stopped is stopped again.
    if matches!(t.view.state, TaskState::Stopped { .. }) {
        let next = stop_again(e, &t).await?;
        if matches!(next, Next::Wait(_)) {
            return Ok(next);
        }
        if let Some(stop) = t.update_stop {
            // Keep the durable intent until unpause; recording a run would reserve a slot.
            if p.view.paused {
                return Ok(Next::Idle);
            }
            control::resume_locked(e, task, dvb_core::Actor::Engine, Some(stop)).await?;
            return Ok(Next::Idle);
        }
        if t.view.outcome != dvb_core::run_verbs::BuilderOutcome::Complete && !p.view.paused {
            return outcomes::stopped(e, &t, &p).await;
        }
        return Ok(next);
    }
    // A paused project starts no new effect; unpausing kicks it.
    if p.view.paused {
        return Ok(Next::Idle);
    }
    // A run held back by the pause starts now.
    if let Some(run) = t.facts.run.as_ref()
        && unhold(e, run)
        && let Some(a) = e.proj().runs.get(run).map(|r| r.assignment.clone())
    {
        spawn("launch the run", launch_later(e.clone(), a));
        return Ok(Next::Idle);
    }
    match t.view.state.clone() {
        TaskState::Publishing => publish(e, &t, &p).await,
        TaskState::InReview => review(e, &t, &p).await,
        TaskState::AwaitingChecks => checks(e, &t, &p).await,
        TaskState::Merging => merge(e, &t, &p).await,
        TaskState::RetryPending { effect, .. } => retry(e, &t, &p, effect).await,
        TaskState::Planned
        | TaskState::Preparing
        | TaskState::Running
        | TaskState::Collecting
        | TaskState::Stopped { .. }
        | TaskState::Unknown { .. }
        | TaskState::Done
        | TaskState::Cancelled
        | TaskState::Failed { .. } => Ok(Next::Idle),
    }
}

fn read(e: &AppState, task: &TaskId) -> Result<(Task, Project), Fail> {
    let proj = e.proj();
    let t = proj
        .tasks
        .get(task)
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
    let p = proj
        .projects
        .get(&t.view.project)
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("project {}", t.view.project)))?;
    drop(proj);
    Ok((t, p))
}

/// Whether the task is still in `state` in an unpaused project: checked after
/// every awaited observation, before anything further is sent.
fn still(e: &AppState, task: &TaskId, state: &TaskState) -> Result<bool, Fail> {
    let (t, p) = read(e, task)?;
    Ok(&t.view.state == state && !p.view.paused)
}

fn slot_free(e: &AppState) -> bool {
    let proj = e.proj();
    u64::try_from(proj.live_runs().count()).unwrap_or(u64::MAX) < proj.max_runs()
}
