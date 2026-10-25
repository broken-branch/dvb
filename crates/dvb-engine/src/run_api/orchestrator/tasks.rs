use super::orchestrator;
use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::TaskId;
use dvb_core::api::TaskView;
use dvb_core::event::Answer;
use dvb_core::run_verbs::{TaskRetryBody, TaskStartBody, TaskStartReply, TaskStopBody};
use dvb_core::{Actor, EventKind, Refusal, RefusalCode};

/// Checks role, model and hire here, then goal approval and caps in `decide`;
/// every refusal is recorded as `command.refused`. A start refused for the cap
/// on concurrent runs is also recorded as `task.waiting`, in the same
/// transaction: the engine starts the task as asked when a run ends
/// (`dispatch::start_waiting`).
pub async fn task_start(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<TaskStartBody>,
) -> Result<Json<TaskStartReply>, Fail> {
    let actor = Actor::Run(c.run.assignment.run.clone());
    let project = c.run.project.clone();
    orchestrator(&e, &c, true)
        .map_err(|f| e.refuse(Some(&project), actor.clone(), "task start", f))?;
    let waiting = EventKind::TaskWaiting {
        task: body.task.clone(),
        model: body.model.clone(),
        effort: body.effort,
        reason: body.reason.clone(),
        routine: body.routine,
    };
    let body_task = body.task.clone();
    let run =
        crate::dispatch::start_as_asked(&e, &project, actor, body, Some(waiting), None).await?;
    let waiting_for = e
        .proj()
        .tasks
        .get(&body_task)
        .map(|t| t.view.waiting_for.clone())
        .unwrap_or_default();
    Ok(Json(TaskStartReply { run, waiting_for }))
}

/// The orchestrator (or a lead) stops a task of its project, as the owner would.
pub async fn task_stop(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<TaskStopBody>,
) -> Result<Json<TaskView>, Fail> {
    let actor = Actor::Run(c.run.assignment.run.clone());
    let project = c.run.project.clone();
    orchestrator(&e, &c, true)
        .map_err(|f| e.refuse(Some(&project), actor.clone(), "task stop", f))?;
    let theirs = e
        .proj()
        .tasks
        .get(&body.task)
        .is_some_and(|t| t.view.project == project);
    if !theirs {
        return Err(Fail::NotFound(format!("task {}", body.task)));
    }
    crate::dispatch::stop(&e, &body.task, actor).await?;
    let view = e.proj().tasks.get(&body.task).map(|t| t.view.clone());
    view.map(Json)
        .ok_or_else(|| Fail::NotFound(format!("task {}", body.task)))
}

/// The owner's yes to retrying `task`: an answer, since the task last
/// failed or stopped and since its last retry, that chose an option naming
/// it in `retries_task`.
fn owner_yes(
    proj: &crate::proj::Projections,
    project: &dvb_core::ProjectId,
    task: &TaskId,
) -> Result<(), Refusal> {
    let since = proj
        .tasks
        .get(task)
        .and_then(|t| t.problem_at.max(t.retried_at));
    let yes = proj
        .questions
        .values()
        .filter(|q| &q.view.project == project && q.answered_at > since)
        .any(|q| {
            let Some(Answer::Choice(n)) = &q.view.answer else {
                return false;
            };
            q.view
                .options
                .iter()
                .any(|o| o.id == *n && o.retries_task.as_ref() == Some(task))
        });
    if yes {
        Ok(())
    } else {
        Err(Refusal::new(
            RefusalCode::GoalNotApproved,
            "the owner has not said yes to retrying this task since it last failed, stopped or was retried; ask with an option whose retries_task names it",
        ))
    }
}

/// The orchestrator retries a failed task of its project, only with the
/// owner's yes, checked again under the task's lock with the decision,
/// so one yes is one retry even when retries race.
pub async fn task_retry(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<TaskRetryBody>,
) -> Result<Json<TaskView>, Fail> {
    let actor = Actor::Run(c.run.assignment.run.clone());
    let project = c.run.project.clone();
    let refuse = |error: Fail| e.refuse(Some(&project), actor.clone(), "task retry", error);
    orchestrator(&e, &c, false).map_err(refuse)?;
    let theirs = e
        .proj()
        .tasks
        .get(&body.task)
        .is_some_and(|t| t.view.project == project);
    if !theirs {
        return Err(Fail::NotFound(format!("task {}", body.task)));
    }
    let early = retry_allowed(&e.proj(), &project, &body.task);
    early.map_err(|r| refuse(Fail::Refused(r)))?;
    crate::dispatch::retry_failed(&e, &body.task, actor.clone(), body.text, |proj| {
        retry_allowed(proj, &project, &body.task)
    })
    .await?;
    let view = e.proj().tasks.get(&body.task).map(|t| t.view.clone());
    view.map(Json)
        .ok_or_else(|| Fail::NotFound(format!("task {}", body.task)))
}

fn retry_allowed(
    proj: &crate::proj::Projections,
    project: &dvb_core::ProjectId,
    task: &TaskId,
) -> Result<(), Refusal> {
    let scope_recovery = proj.tasks.get(task).is_some_and(|t| {
        &t.view.project == project
            && t.approved
            && matches!(t.view.state, dvb_core::TaskState::Stopped { .. })
            && matches!(
                t.view.outcome,
                dvb_core::run_verbs::BuilderOutcome::Partial {
                    reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                    ..
                }
            )
            && t.facts
                .builder
                .as_ref()
                .and_then(|id| proj.runs.get(id))
                .and_then(|r| r.outcome_at)
                .is_some_and(|at| {
                    proj.goals
                        .get(&t.view.goal)
                        .is_some_and(|g| g.planned.is_some_and(|plan| plan > at))
                })
    });
    if scope_recovery {
        Ok(())
    } else {
        owner_yes(proj, project, task)
    }
}
