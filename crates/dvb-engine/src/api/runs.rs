//! Runs and their live output.

use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::extract::{Path, Query, State};
use dvb_core::api::{AfterQuery, RunOutputLine, RunView};
use dvb_core::{EventId, EventKind, RunId};

#[utoipa::path(
    get,
    path = "/runs/{id}",
    operation_id = "get_run",
    tag = "runs",
    params(("id" = dvb_core::RunId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The run", body = dvb_core::api::RunView))
)]
pub async fn get_run(
    State(e): State<AppState>,
    Path(id): Path<RunId>,
) -> Result<Json<RunView>, Fail> {
    let proj = e.proj();
    let Some(r) = proj.runs.get(&id) else {
        return Err(Fail::NotFound(format!("run {id}")));
    };
    let view = RunView {
        outcome: r.outcome.clone(),
        id: r.assignment.run.clone(),
        project: r.project.clone(),
        task: r.assignment.task.clone(),
        role: r.assignment.role.clone(),
        hire: r.assignment.hire.clone(),
        model: r.assignment.model.clone(),
        vendor: r.assignment.vendor,
        effort: r.assignment.effort,
        reason: r.assignment.reason.clone(),
        reviews: r.assignment.reviews.clone(),
        started_at: r.started_at,
        ended: r.ended.clone(),
    };
    drop(proj);
    Ok(Json(view))
}

/// The run's live lines (`run.output`) after `after`, oldest first.
#[utoipa::path(
    get,
    path = "/runs/{id}/output",
    operation_id = "get_run_output",
    tag = "runs",
    params(("id" = dvb_core::RunId, Path), dvb_core::api::AfterQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "The run's live lines after `after`", body = Vec<dvb_core::api::RunOutputLine>))
)]
pub async fn get_run_output(
    State(e): State<AppState>,
    Path(id): Path<RunId>,
    Query(q): Query<AfterQuery>,
) -> Result<Json<Vec<RunOutputLine>>, Fail> {
    if !e.proj().runs.contains_key(&id) {
        return Err(Fail::NotFound(format!("run {id}")));
    }
    Ok(Json(
        e.store
            .select(
                q.after.unwrap_or(EventId(0)),
                dvb_store::Select::RunOutput(&id),
            )?
            .into_iter()
            .filter_map(|ev| {
                if let EventKind::RunOutput {
                    run,
                    mut item,
                    resource,
                } = ev.kind
                    && run == id
                {
                    if let Some(resource) = resource
                        && let dvb_core::event::OutputItem::Note(detail) = &item
                    {
                        item = dvb_core::event::OutputItem::Note(
                            dvb_core::wording::resource_notice(resource, detail),
                        );
                    }
                    Some(RunOutputLine {
                        id: ev.id,
                        at: ev.at,
                        item,
                    })
                } else {
                    None
                }
            })
            .collect(),
    ))
}
