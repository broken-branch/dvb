//! Roles and hires. Reporting and review assignment are independent.

use super::history::{newest_first, pick, restore, versions_of};
use super::invalid;
use crate::{AppState, Fail};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use dvb_core::api::{Restore, RoleChanges, RoleFields, RoleHistory};
use dvb_core::roles::RoleRecord;
use dvb_core::{Actor, EventKind, ProjectId, Refusal, RefusalCode, RoleId};
use std::collections::BTreeSet;

pub(super) fn project_roles(
    proj: &crate::proj::Projections,
    id: &ProjectId,
) -> Result<Vec<RoleRecord>, Fail> {
    proj.projects
        .get(id)
        .map(|p| p.roles.clone())
        .ok_or_else(|| Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")).into())
}

pub(crate) fn validate(role: &RoleRecord, roles: &[RoleRecord]) -> Result<(), Fail> {
    if role.name.trim().is_empty()
        || role.name.len() > 200
        || role.job.len() > 16384
        || role.lane.len() > 16384
        || role.review_instructions.len() > 16384
        || role
            .group
            .as_ref()
            .is_some_and(|g| g.trim().is_empty() || g.len() > 200)
    {
        return Err(invalid(
            "give the role a name and keep its instructions within 16 KiB",
        ));
    }
    for id in [&role.reports_to, &role.reviewed_by].into_iter().flatten() {
        if id == &role.id || !roles.iter().any(|r| &r.id == id) {
            return Err(invalid(
                "reporting and review targets must be other roles in this project",
            ));
        }
    }
    let mut seen = BTreeSet::from([role.id.clone()]);
    let mut parent = role.reports_to.as_ref();
    while let Some(id) = parent {
        if !seen.insert(id.clone()) {
            return Err(invalid("reporting lines must not form a cycle"));
        }
        parent = roles
            .iter()
            .find(|r| &r.id == id)
            .and_then(|r| r.reports_to.as_ref());
    }
    Ok(())
}

fn fields(id: RoleId, slug: String, body: RoleFields) -> RoleRecord {
    RoleRecord {
        id,
        slug,
        name: body.name.trim().into(),
        job: body.job,
        lane: body.lane,
        lead: body.lead,
        reports_to: body.reports_to,
        reviewed_by: body.reviewed_by,
        review_instructions: body.review_instructions,
        group: body.group,
        order: body.order,
    }
}

#[utoipa::path(
    get,
    path = "/projects/{id}/roles",
    operation_id = "list_roles",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The project's roles", body = Vec<dvb_core::roles::RoleRecord>))
)]
pub async fn list_roles(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<Json<Vec<RoleRecord>>, Fail> {
    project_roles(&e.proj(), &id).map(Json)
}

#[utoipa::path(
    post,
    path = "/projects/{id}/roles",
    operation_id = "add_role",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    request_body = dvb_core::api::RoleFields,
    responses(crate::api::ErrorResponses, (status = 201, description = "Role added", body = dvb_core::roles::RoleRecord))
)]
pub async fn add_role(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(body): Json<RoleFields>,
) -> Result<(StatusCode, Json<RoleRecord>), Fail> {
    let slug = body
        .name
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let role = fields(RoleId::new(e.new_id()), slug, body);
    e.append_checked(Some(&id), Actor::Owner, |proj, _| {
        let roles = project_roles(proj, &id)?;
        if role.slug.is_empty() || roles.iter().any(|r| r.slug == role.slug) {
            return Err(invalid("choose a unique role name with letters or numbers"));
        }
        validate(&role, &roles)?;
        Ok(vec![EventKind::RoleAdded(role.clone())])
    })?;
    Ok((StatusCode::CREATED, Json(role)))
}

#[utoipa::path(
    patch,
    path = "/roles/{id}",
    operation_id = "change_role",
    tag = "roles",
    params(("id" = dvb_core::RoleId, Path)),
    request_body = dvb_core::api::RoleChanges,
    responses(crate::api::ErrorResponses, (status = 200, description = "Role changed", body = dvb_core::roles::RoleRecord))
)]
pub async fn change_role(
    State(e): State<AppState>,
    Path(id): Path<RoleId>,
    Json(body): Json<RoleChanges>,
) -> Result<Json<RoleRecord>, Fail> {
    let project = e
        .proj()
        .projects
        .iter()
        .find_map(|(p, v)| v.role(&id).map(|_| p.clone()))
        .ok_or_else(|| Fail::NotFound(format!("role {id}")))?;
    let mut changed = None;
    e.append_checked(Some(&project), Actor::Owner, |proj, _| {
        let roles = project_roles(proj, &project)?;
        let old = roles
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("role {id}")))?;
        // The slug is a stable routing identity, including for renamed defaults.
        let role = fields(
            id,
            old.slug,
            RoleFields {
                name: body.name.unwrap_or(old.name),
                job: body.job.unwrap_or(old.job),
                lane: body.lane.unwrap_or(old.lane),
                lead: body.lead.unwrap_or(old.lead),
                reports_to: body.reports_to.unwrap_or(old.reports_to),
                reviewed_by: body.reviewed_by.unwrap_or(old.reviewed_by),
                review_instructions: body.review_instructions.unwrap_or(old.review_instructions),
                group: body.group.unwrap_or(old.group),
                order: body.order.unwrap_or(old.order),
            },
        );
        validate(&role, &roles)?;
        changed = Some(role.clone());
        Ok(vec![EventKind::RoleChanged {
            role,
            restored_from: None,
        }])
    })?;
    let role = changed.ok_or_else(|| Fail::internal("role change result missing"))?;
    Ok(Json(role))
}

#[utoipa::path(
    get,
    path = "/roles/{id}/history",
    operation_id = "role_history",
    tag = "roles",
    params(("id" = dvb_core::RoleId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The role's versions, newest first", body = dvb_core::api::RoleHistory))
)]
pub async fn role_history(
    State(e): State<AppState>,
    Path(id): Path<RoleId>,
) -> Result<Json<RoleHistory>, Fail> {
    let found = versions_of(&e, |k| role_of(k, &id))?;
    if found.is_empty() {
        return Err(Fail::NotFound(format!("role {id}")));
    }
    Ok(Json(RoleHistory {
        role: id,
        versions: newest_first(found, role_fields),
    }))
}

#[utoipa::path(
    post,
    path = "/roles/{id}/restore",
    operation_id = "restore_role",
    tag = "roles",
    params(("id" = dvb_core::RoleId, Path)),
    request_body = dvb_core::api::Restore,
    responses(crate::api::ErrorResponses, (status = 200, description = "Restored", body = dvb_core::roles::RoleRecord))
)]
pub async fn restore_role(
    State(e): State<AppState>,
    Path(id): Path<RoleId>,
    Json(body): Json<Restore>,
) -> Result<Json<RoleRecord>, Fail> {
    let project = e
        .proj()
        .projects
        .iter()
        .find_map(|(p, v)| v.role(&id).map(|_| p.clone()))
        .ok_or_else(|| Fail::NotFound(format!("role {id}")))?;
    let mut restored = None;
    restore(&e, Some(&project), "role.restore", |proj| {
        let found = versions_of(&e, |k| role_of(k, &id))?;
        let role = pick(&found, &format!("role {id}"), (body.event, body.current))?;
        // The same check as an ordinary role write, against the roles as they are now.
        let roles = proj
            .projects
            .get(&project)
            .map(|p| p.roles.clone())
            .unwrap_or_default();
        validate(&role, &roles)?;
        restored = Some(role.clone());
        Ok(vec![EventKind::RoleChanged {
            role,
            restored_from: Some(body.event),
        }])
    })?;
    Ok(Json(
        restored.ok_or_else(|| Fail::internal("restore result missing"))?,
    ))
}

fn role_of(kind: &EventKind, id: &RoleId) -> Option<RoleRecord> {
    let (EventKind::RoleAdded(r)
    | EventKind::RoleRemoved(r)
    | EventKind::RoleChanged { role: r, .. }) = kind
    else {
        return None;
    };
    (&r.id == id).then(|| r.clone())
}

fn role_fields(r: RoleRecord) -> RoleFields {
    RoleFields {
        name: r.name,
        job: r.job,
        lane: r.lane,
        lead: r.lead,
        reports_to: r.reports_to,
        reviewed_by: r.reviewed_by,
        review_instructions: r.review_instructions,
        group: r.group,
        order: r.order,
    }
}

#[cfg(test)]
mod tests {
    use crate::usage::prices::model_pool;

    #[test]
    fn model_pool_includes_fake_only_in_dev_mode() {
        assert!(
            model_pool(false)
                .models
                .iter()
                .all(|m| m.vendor != dvb_core::Vendor::Fake)
        );
        assert_eq!(model_pool(true).models[0].id, "fake-1");
    }
}
