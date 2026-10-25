//! Tasks: list and get; stop, resume and retry.

use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use dvb_core::api::{DiffView, ProjectQuery, RetryTask, StopTask, TaskView, VerdictView};
use dvb_core::git::Git;
use dvb_core::{Actor, Refusal, RefusalCode, TaskId};

#[utoipa::path(
    get,
    path = "/tasks",
    operation_id = "list_tasks",
    tag = "tasks",
    params(dvb_core::api::ProjectQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Tasks", body = Vec<dvb_core::api::TaskView>))
)]
pub async fn list_tasks(
    State(e): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Json<Vec<TaskView>> {
    Json(
        e.proj()
            .tasks
            .values()
            .filter(|t| q.project.as_ref().is_none_or(|p| &t.view.project == p))
            .map(|t| t.view.clone())
            .collect(),
    )
}

#[utoipa::path(
    get,
    path = "/tasks/{id}",
    operation_id = "get_task",
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The task", body = dvb_core::api::TaskView))
)]
pub async fn get_task(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<TaskView>, Fail> {
    view(&e, &id)
}

fn view(e: &AppState, id: &TaskId) -> Result<Json<TaskView>, Fail> {
    e.proj()
        .tasks
        .get(id)
        .map(|t| Json(t.view.clone()))
        .ok_or_else(|| Fail::NotFound(format!("task {id}")))
}

/// Stops a task in any state; a stopped or ended task is refused with
/// `ALREADY_STOPPED` or `ALREADY_TERMINAL`. The reason is recorded on
/// `task.stopped`; with one, the orchestrator is not woken about the stop.
#[utoipa::path(
    post,
    path = "/tasks/{id}/stop",
    operation_id = "stop_task",
    request_body = Option<dvb_core::api::StopTask>,
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Stopped", body = dvb_core::api::TaskView))
)]
pub async fn stop_task(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
    bytes: Bytes,
) -> Result<Json<TaskView>, Fail> {
    let body = super::optional_body::<StopTask>(&bytes)?.unwrap_or_default();
    let reason = body.reason.filter(|r| !r.trim().is_empty());
    let _exists = view(&e, &id)?;
    crate::dispatch::stop_with(&e, &id, Actor::Owner, reason, |_| Ok(())).await?;
    view(&e, &id)
}

/// Resumes a stopped task from the state it stopped in.
#[utoipa::path(
    post,
    path = "/tasks/{id}/resume",
    operation_id = "resume_task",
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Resumed", body = dvb_core::api::TaskView))
)]
pub async fn resume_task(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<TaskView>, Fail> {
    let _exists = view(&e, &id)?;
    crate::dispatch::resume(&e, &id, Actor::Owner).await?;
    view(&e, &id)
}

/// Retries a failed task: a builder on its head after failed CI or used-up
/// review rounds, its merge with no run for an approved head that failed only
/// to merge, else a new run from its kept workspace or head. A task stopped
/// or in review with a PR gets a builder on its PR's head (a review in
/// progress is stopped first); any other state is refused with a code.
#[utoipa::path(
    post,
    path = "/tasks/{id}/retry",
    operation_id = "retry_task",
    request_body = Option<dvb_core::api::RetryTask>,
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Retried: a new run, or back to its checks and merge", body = dvb_core::api::TaskView), (status = 409, description = "Refused, with a code: only a failed task, or one stopped or in review with a PR, is retried", body = dvb_core::Refusal))
)]
pub async fn retry_task(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
    bytes: Bytes,
) -> Result<Json<TaskView>, Fail> {
    let body = super::optional_body::<RetryTask>(&bytes)?.unwrap_or_default();
    let _exists = view(&e, &id)?;
    crate::dispatch::retry_failed(&e, &id, Actor::Owner, body.text, |_| Ok(())).await?;
    view(&e, &id)
}

#[utoipa::path(
    get,
    path = "/tasks/{id}/diff",
    operation_id = "get_task_diff",
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The head's diff against its base", body = dvb_core::api::DiffView))
)]
pub async fn get_task_diff(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<DiffView>, Fail> {
    let _task = e.lock(&id).await;
    let (mirror, head, stage) = {
        let proj = e.proj();
        let task = proj
            .tasks
            .get(&id)
            .ok_or_else(|| Fail::NotFound(format!("task {id}")))?;
        let head =
            task.view.head.clone().ok_or_else(|| {
                Refusal::new(RefusalCode::HeadMismatch, "no published change yet")
            })?;
        let project = &proj.projects[&task.view.project];
        let result = (e.mirror(&project.view.name), head, task.next_review());
        drop(proj);
        result
    };
    let base = crate::publish::review_base(&e, &id, &head, stage)
        .await
        .ok_or_else(|| Refusal::new(RefusalCode::HeadMismatch, "no publication base recorded"))?;
    let output = Git::new(&mirror)
        .output(
            &[
                "-c",
                "core.attributesFile=/dev/null",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &base,
                &head,
                "--",
            ],
            &[("GIT_ATTR_NOSYSTEM", "1")],
            None,
        )
        .await
        .map_err(|error| Fail::context("git diff", error))?;
    let diff = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(Json(DiffView {
        task: id,
        base,
        head,
        diff,
    }))
}

#[utoipa::path(
    get,
    path = "/tasks/{id}/verdict",
    operation_id = "get_task_verdict",
    tag = "tasks",
    params(("id" = dvb_core::TaskId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The verdict on the current head", body = dvb_core::api::VerdictView))
)]
pub async fn get_task_verdict(
    State(e): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<VerdictView>, Fail> {
    // Read the head and its recorded evidence under the same projection guard.
    let proj = e.proj();
    let task = proj
        .tasks
        .get(&id)
        .ok_or_else(|| Fail::NotFound(format!("task {id}")))?;
    let head = task
        .view
        .head
        .as_ref()
        .ok_or_else(|| Refusal::new(RefusalCode::HeadMismatch, "no published change yet"))?;
    let result = task
        .recorded_verdicts
        .get(head)
        .cloned()
        .map(Json)
        .ok_or_else(|| Fail::NotFound(format!("no verdict for task {id} on {head}")));
    drop(proj);
    result
}
