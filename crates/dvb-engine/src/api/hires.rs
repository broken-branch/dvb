//! Hires, retirement and version history.
use super::history::{newest_first, pick, restore, versions_of};
use crate::proj::hires::{create_hire, retire_record, retiring_hires, validate_hire};
use crate::{AppState, Fail};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use dvb_core::api::{
    HireCreated, HireFields, HireHistory, HireView, NewHire, ProjectQuery, Restore,
};
use dvb_core::roles::{HireRecord, HireStatus};
use dvb_core::{Actor, EventKind, HireId};

#[utoipa::path(
    get,
    path = "/hires",
    operation_id = "list_hires",
    tag = "hires",
    params(dvb_core::api::ProjectQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Hires", body = Vec<dvb_core::api::HireView>))
)]
pub async fn list_hires(
    State(e): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Json<Vec<HireView>>, Fail> {
    e.expire_only(None)?;
    let proj = e.proj();
    Ok(Json(
        proj.projects
            .iter()
            .filter(|(id, _)| q.project.as_ref().is_none_or(|p| p == *id))
            .flat_map(|(_, p)| {
                p.hires.iter().map(|hire| HireView {
                    hire: hire.clone(),
                    default_hire: proj
                        .usable_hire(p, &hire.role, None, false, &e.pool)
                        .map(|h| h.id.clone()),
                })
            })
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/hires",
    operation_id = "add_hire",
    tag = "hires",
    request_body = dvb_core::api::NewHire,
    responses(crate::api::ErrorResponses, (status = 201, description = "Hired", body = dvb_core::api::HireCreated))
)]
pub async fn add_hire(
    State(e): State<AppState>,
    Json(body): Json<NewHire>,
) -> Result<(StatusCode, Json<HireCreated>), Fail> {
    create_hire(&e, body, &Actor::Owner, None).map(|h| (StatusCode::CREATED, Json(h)))
}

#[utoipa::path(
    post,
    path = "/hires/{id}/retire",
    operation_id = "retire_hire",
    tag = "hires",
    params(("id" = dvb_core::HireId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Retired", body = dvb_core::roles::HireRecord))
)]
pub async fn retire_hire(
    State(e): State<AppState>,
    Path(id): Path<HireId>,
) -> Result<Json<HireRecord>, Fail> {
    retire_record(&e, &id, &Actor::Owner, None).map(Json)
}

#[utoipa::path(
    get,
    path = "/hires/{id}/history",
    operation_id = "hire_history",
    tag = "hires",
    params(("id" = dvb_core::HireId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The hire's versions, newest first", body = dvb_core::api::HireHistory))
)]
pub async fn hire_history(
    State(e): State<AppState>,
    Path(id): Path<HireId>,
) -> Result<Json<HireHistory>, Fail> {
    let found = versions_of(&e, |k| hire_of(k, &id))?;
    if found.is_empty() {
        return Err(Fail::NotFound(format!("hire {id}")));
    }
    Ok(Json(HireHistory {
        hire: id,
        versions: newest_first(found, hire_fields),
    }))
}

#[utoipa::path(
    post,
    path = "/hires/{id}/restore",
    operation_id = "restore_hire",
    tag = "hires",
    params(("id" = dvb_core::HireId, Path)),
    request_body = dvb_core::api::Restore,
    responses(crate::api::ErrorResponses, (status = 200, description = "Restored", body = dvb_core::roles::HireRecord))
)]
pub async fn restore_hire(
    State(e): State<AppState>,
    Path(id): Path<HireId>,
    Json(body): Json<Restore>,
) -> Result<Json<HireRecord>, Fail> {
    let project = e
        .proj()
        .projects
        .iter()
        .find_map(|(p, v)| v.hires.iter().any(|h| h.id == id).then(|| p.clone()))
        .ok_or_else(|| Fail::NotFound(format!("hire {id}")))?;
    let mut restored = None;
    restore(&e, Some(&project), "hire.restore", |proj| {
        let found = versions_of(&e, |k| hire_of(k, &id))?;
        let hire = pick(&found, &format!("hire {id}"), (body.event, body.current))?;
        let mut kinds = match hire.status {
            HireStatus::Active => {
                validate_hire(&e, proj, &hire.model, hire.effort)?;
                retiring_hires(&proj.projects[&project], &hire.role, hire.routine)
                    .into_iter()
                    .filter(|h| h.id != hire.id)
                    .map(EventKind::HireRetired)
                    .collect::<Vec<_>>()
            }
            HireStatus::Retired => Vec::new(),
        };
        restored = Some(hire.clone());
        kinds.push(EventKind::HireChanged {
            hire,
            restored_from: Some(body.event),
        });
        Ok(kinds)
    })?;
    Ok(Json(
        restored.ok_or_else(|| Fail::internal("restore result missing"))?,
    ))
}

fn hire_of(kind: &EventKind, id: &HireId) -> Option<HireRecord> {
    let (EventKind::HireAdded(h)
    | EventKind::HireRetired(h)
    | EventKind::HireChanged { hire: h, .. }) = kind
    else {
        return None;
    };
    (&h.id == id).then(|| h.clone())
}

fn hire_fields(h: HireRecord) -> HireFields {
    HireFields {
        role: h.role,
        model: h.model,
        effort: h.effort,
        status: h.status,
        routine: h.routine,
    }
}
