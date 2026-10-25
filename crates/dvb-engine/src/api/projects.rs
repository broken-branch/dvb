//! Projects: list and get; pause, resume, checks and organization chart.

use super::history::{newest_first, versions_of};
use super::invalid;
use super::roles::{project_roles, validate};
use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use dvb_core::RoleId;
use dvb_core::api::{Brief, ChartMove, ProjectChecksHistory, ProjectView};
use dvb_core::event::CiMode;
use dvb_core::roles::RoleRecord;
use dvb_core::{Actor, EventKind, ProjectId, Refusal, RefusalCode};

#[utoipa::path(
    get,
    path = "/projects",
    operation_id = "list_projects",
    tag = "projects",
    responses(crate::api::ErrorResponses, (status = 200, description = "Projects", body = Vec<dvb_core::api::ProjectView>))
)]
pub async fn list_projects(State(e): State<AppState>) -> Json<Vec<ProjectView>> {
    Json(e.proj().projects.values().map(|p| p.view.clone()).collect())
}

#[utoipa::path(
    get,
    path = "/projects/{id}",
    operation_id = "get_project",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The project", body = dvb_core::api::ProjectView))
)]
pub async fn get_project(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<Json<ProjectView>, Fail> {
    e.proj()
        .projects
        .get(&id)
        .map(|p| Json(p.view.clone()))
        .ok_or_else(|| Fail::NotFound(format!("project {id}")))
}

/// No new runs and no new effects; what is in flight carries on.
#[utoipa::path(
    post,
    path = "/projects/{id}/pause",
    operation_id = "pause_project",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 204, description = "Paused"))
)]
pub async fn pause_project(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<StatusCode, Fail> {
    crate::owner::pause(&e, &id, true)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/projects/{id}/resume",
    operation_id = "resume_project",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 204, description = "Resumed"))
)]
pub async fn resume_project(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<StatusCode, Fail> {
    crate::owner::pause(&e, &id, false)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The owner's last look at the project: records `project.seen`.
#[utoipa::path(
    post,
    path = "/projects/{id}/seen",
    operation_id = "mark_project_seen",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 204, description = "Recorded `project.seen`: the owner's last look at the project"))
)]
pub async fn mark_project_seen(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<StatusCode, Fail> {
    if !e.proj().projects.contains_key(&id) {
        return Err(e.refuse(
            None,
            Actor::Owner,
            "seen",
            Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")),
        ));
    }
    e.append_checked(Some(&id), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectSeen {}])
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/projects/{id}/brief",
    operation_id = "get_brief",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The project's current brief", body = dvb_core::api::Brief), (status = 404, description = "No such project", body = dvb_core::Refusal))
)]
pub async fn get_brief(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<Json<Brief>, Fail> {
    e.proj()
        .projects
        .get(&id)
        .map(|p| {
            Json(Brief {
                text: p.brief.clone(),
            })
        })
        .ok_or_else(|| Fail::NotFound(format!("project {id}")))
}

/// Sets the project's brief, recorded only as `project.brief_set` in the log.
#[utoipa::path(
    put,
    path = "/projects/{id}/brief",
    operation_id = "set_brief",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    request_body = dvb_core::api::Brief,
    responses(crate::api::ErrorResponses, (status = 200, description = "The brief now in effect; empty text clears it", body = dvb_core::api::Brief))
)]
pub async fn set_brief(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(body): Json<Brief>,
) -> Result<Json<Brief>, Fail> {
    if !e.proj().projects.contains_key(&id) {
        return Err(e.refuse(
            None,
            Actor::Owner,
            "brief",
            Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")),
        ));
    }
    e.append_checked(Some(&id), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectBriefSet {
            brief: body.text.clone(),
        }])
    })?;
    Ok(Json(body))
}

/// Replace the saved merge check choice; empty expected lists never mean no CI.
#[utoipa::path(
    put,
    path = "/projects/{id}/checks",
    operation_id = "set_project_checks",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    request_body = CiMode,
    responses(crate::api::ErrorResponses, (status = 200, description = "Project with the saved check choice", body = dvb_core::api::ProjectView))
)]
pub async fn set_project_checks(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(ci): Json<CiMode>,
) -> Result<Json<ProjectView>, Fail> {
    let _checks = e.lock_project_checks(&id).await;
    // Validate before touching GitHub; keep the same project-check lock as merge.
    let project = (|| {
        let proj = e.proj();
        let project = proj.projects.get(&id).ok_or_else(|| {
            Fail::Refused(Refusal::new(
                RefusalCode::ProjectUnknown,
                format!("no project {id}"),
            ))
        })?;
        crate::publish::project::expected_ci(&[], &ci)?;
        let view = project.view.clone();
        drop(proj);
        if let CiMode::Expected(names) = &ci {
            // Owner-typed config, not dvb's writing: the owner's name may be in it.
            let policy = dvb_forge::scan::ScanPolicy::default();
            if let Some(hit) = names
                .iter()
                .find_map(|name| policy.scan("required check", name))
            {
                return Err(crate::publish::project::scan_refusal(hit));
            }
        }
        Ok(view)
    })()
    .map_err(|err| e.refuse(Some(&id), Actor::Owner, "project.checks", err))?;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let names = match &ci {
        CiMode::Expected(names) => names.as_slice(),
        CiMode::NoCi => &[],
    };
    let repo = dvb_core::forge::Repo(project.repo);
    // GitHub Free allows no ruleset on a private repository; the checks are
    // saved and dvb still merges only when they pass.
    let installed = forge
        .rulesets_allowed(&repo)
        .await
        .map_err(Fail::internal)?;
    if installed {
        forge
            .install_ruleset(&repo, &project.default_branch, names)
            .await
            .map_err(Fail::internal)?;
    }
    let result = e.append_checked(Some(&id), Actor::Owner, |proj, _now| {
        let Some(project) = proj.projects.get(&id) else {
            return Err(
                Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")).into(),
            );
        };
        let mut kinds = vec![EventKind::SettingsChanged {
            key: "project_checks".into(),
            old: serde_json::json!(project.view.ci),
            new: serde_json::json!(ci),
            restored_from: None,
        }];
        match (installed, project.ruleset_missing) {
            (false, false) => kinds.push(EventKind::ProjectRulesetMissing {}),
            (true, true) => kinds.push(EventKind::ProjectRulesetInstalled {}),
            (false, true) | (true, false) => {}
        }
        if ci == CiMode::NoCi
            && let Some(task) = proj.tasks.values().find(|t| t.view.project == id)
        {
            kinds.extend(
                proj.questions
                    .values()
                    .filter(|q| q.view.project == id && q.view.open && q.main_ci.is_some())
                    .map(|q| EventKind::QuestionClosed {
                        question: q.view.id.clone(),
                        task: task.view.id.clone(),
                    }),
            );
        }
        Ok(kinds)
    });
    result.map_err(|error| e.refuse(Some(&id), Actor::Owner, "project.checks", error))?;
    get_project(State(e), Path(id)).await
}

/// Validate a complete candidate tree, then append all changes in one transaction.
#[utoipa::path(post, path = "/projects/{id}/chart", operation_id = "move_chart", tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)), request_body = dvb_core::api::ChartMove,
    responses(crate::api::ErrorResponses, (status = 200, description = "Updated chart", body = Vec<dvb_core::roles::RoleRecord>),
    (status = 409, description = "Refused atomically", body = dvb_core::Refusal)))]
pub async fn move_chart(
    State(e): State<AppState>,
    Path(project): Path<ProjectId>,
    Json(movement): Json<ChartMove>,
) -> Result<Json<Vec<RoleRecord>>, Fail> {
    let mut changed = Vec::new();
    e.append_checked(Some(&project), Actor::Owner, |proj, _| {
        let old = project_roles(proj, &project)?;
        let roles = chart_move(&old, movement)?;
        let kinds = roles
            .iter()
            .filter(|r| old.iter().find(|o| o.id == r.id) != Some(*r))
            .cloned()
            .map(|role| EventKind::RoleChanged {
                role,
                restored_from: None,
            })
            .collect();
        changed = roles;
        Ok(kinds)
    })?;
    Ok(Json(changed))
}

fn chart_move(old: &[RoleRecord], movement: ChartMove) -> Result<Vec<RoleRecord>, Fail> {
    let mut roles = old.to_vec();
    let index = |id: &RoleId| {
        old.iter()
            .position(|r| &r.id == id)
            .ok_or_else(|| invalid("choose a role in this project"))
    };
    match movement {
        ChartMove::Report { role, parent } => {
            let i = index(&role)?;
            if let Some(p) = &parent {
                index(p)?;
            }
            let previous = old[i].reports_to.clone();
            roles[i].reports_to.clone_from(&parent);
            for candidate in [previous, parent].into_iter().flatten() {
                let has_reports = roles
                    .iter()
                    .any(|r| r.reports_to.as_ref() == Some(&candidate));
                if let Some(lead) = roles
                    .iter_mut()
                    .find(|r| r.id == candidate && r.reports_to.is_some())
                {
                    lead.lead = has_reports;
                }
            }
            roles[i].group = None;
            roles[i].order = old
                .iter()
                .map(|r| r.order)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
        }
        ChartMove::Group {
            roles: members,
            group,
        } => {
            if members.is_empty() {
                return Err(invalid("choose group members"));
            }
            let indices = members.iter().map(index).collect::<Result<Vec<_>, _>>()?;
            let parent = &old[indices[0]].reports_to;
            if indices.iter().any(|i| &old[*i].reports_to != parent) {
                return Err(invalid("group members must report to the same role"));
            }
            for i in indices {
                roles[i].group.clone_from(&group);
            }
        }
        ChartMove::Reorder { role, before } => {
            let i = index(&role)?;
            let j = before.as_ref().map(index).transpose()?;
            if Some(i) == j {
                return Ok(roles);
            }
            if j.is_some_and(|j| old[i].reports_to != old[j].reports_to) {
                return Err(invalid("reorder roles on the same reporting line"));
            }
            let mut siblings = (0..roles.len())
                .filter(|k| roles[*k].reports_to == old[i].reports_to)
                .collect::<Vec<_>>();
            siblings.sort_by_key(|k| roles[*k].order);
            siblings.retain(|k| *k != i);
            let position = siblings
                .iter()
                .position(|k| Some(*k) == j)
                .unwrap_or(siblings.len());
            siblings.insert(position, i);
            for (order, k) in siblings.into_iter().enumerate() {
                roles[k].order = u32::try_from(order).unwrap_or(u32::MAX);
            }
        }
        ChartMove::Swap { role, lead } => {
            let (i, j) = (index(&role)?, index(&lead)?);
            if old[i].reports_to.as_ref() != Some(&lead) {
                return Err(invalid("swap with the role directly above this one"));
            }
            for r in &mut roles {
                if r.id == role {
                    r.reports_to.clone_from(&old[j].reports_to);
                } else if r.id == lead {
                    r.reports_to = Some(role.clone());
                } else if r.reports_to.as_ref() == Some(&role) {
                    r.reports_to = Some(lead.clone());
                } else if r.reports_to.as_ref() == Some(&lead) {
                    r.reports_to = Some(role.clone());
                }
            }
            roles[i].lead = old[j].lead;
            roles[j].lead = old[i].lead;
            roles[i].group.clone_from(&old[j].group);
            roles[j].group.clone_from(&old[i].group);
            roles[i].order = old[j].order;
            roles[j].order = old[i].order;
        }
    }
    for role in &roles {
        validate(role, &roles)?;
    }
    Ok(roles)
}

/// Read the initial choice and later owner writes, scoped by the event project.
#[utoipa::path(
    get,
    path = "/projects/{id}/checks/history",
    operation_id = "project_checks_history",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Check choices, newest first", body = dvb_core::api::ProjectChecksHistory), (status = 404, description = "Unknown project"))
)]
pub async fn project_checks_history(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<Json<ProjectChecksHistory>, Fail> {
    if !e.proj().projects.contains_key(&id) {
        return Err(Fail::NotFound(format!("project {id}")));
    }
    let found = versions_of(&e, |kind| {
        if let EventKind::ProjectAdded { ci, .. } = kind {
            return Some(ci.clone());
        }
        if let EventKind::SettingsChanged { key, new, .. } = kind
            && key == "project_checks"
        {
            return serde_json::from_value(new.clone()).ok();
        }
        None
    })?
    .into_iter()
    .filter(|(ev, _)| ev.project.as_ref() == Some(&id))
    .collect();
    Ok(Json(ProjectChecksHistory {
        project: id,
        versions: newest_first(found, |ci| ci),
    }))
}
