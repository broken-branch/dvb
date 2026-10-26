//! Removing a project from this install: its event, then its files on this machine.

use crate::AppState;
use crate::engine::Fail;
use crate::proj::Projections;
use axum::Json;
use axum::extract::{Path, State};
use dvb_core::api::ProjectRemoval;
use dvb_core::forge::Repo;
use dvb_core::{Actor, EventKind, ProjectId, Refusal, RefusalCode, TaskState};
use std::path::PathBuf;

/// Refused while the project has running or open tasks, or a run still working.
/// Frees its run directories, mirror, toolchain cache and mirror receipts; makes
/// no GitHub call.
#[utoipa::path(
    post,
    path = "/projects/{id}/remove",
    operation_id = "remove_project",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The removed project and the files that could not be freed; records `project.removed`", body = dvb_core::api::ProjectRemoval))
)]
pub async fn remove_project(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
) -> Result<Json<ProjectRemoval>, Fail> {
    let found = {
        let proj = e.proj();
        proj.projects
            .get(&id)
            .map(|p| (p.view.clone(), busy(&proj, &id, &p.view.name)))
    };
    let Some((project, refusal)) = found else {
        return Err(e.refuse(
            None,
            Actor::Owner,
            "project.remove",
            Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")),
        ));
    };
    if let Some(refusal) = refusal {
        return Err(e.refuse(Some(&id), Actor::Owner, "project.remove", refusal));
    }
    // The mirror job running now finishes before the receipts go.
    let mirror = crate::publish::mirror::lock_for_change(&e, &id, "project.remove").await?;
    let mut runs = Vec::new();
    e.append_checked(Some(&id), Actor::Owner, |proj, _| {
        if !proj.projects.contains_key(&id) {
            return Err(Refusal::new(
                RefusalCode::ProjectUnknown,
                format!("project {} was removed meanwhile", project.name),
            )
            .into());
        }
        if let Some(refusal) = busy(proj, &id, &project.name) {
            return Err(refusal.into());
        }
        runs = proj
            .runs
            .values()
            .filter(|r| r.project == id)
            .map(|r| r.assignment.run.clone())
            .collect();
        Ok(vec![EventKind::ProjectRemoved {
            name: project.name.clone(),
            repo: project.repo.clone(),
        }])
    })?;
    // After the append no pass sees the project, so its files are freed unlocked.
    let mut not_freed = Vec::new();
    if let Some(forge) = e.forge()
        && let Err(err) = forge.forget_mirror(&Repo(project.repo.clone()))
    {
        not_freed.push(format!("github-mirror.json: {}", err.detail));
    }
    drop(mirror);
    let mut dirs: Vec<PathBuf> = runs
        .iter()
        .map(|run| e.cfg.paths.work.join(run.as_str()))
        .collect();
    dirs.push(e.mirror(&project.name));
    dirs.push(e.cfg.paths.mirrors.join(".cache").join(id.as_str()));
    // Runs leave these directories group-writable as they end
    // (`dvb_run::share_run`), so the engine need not own them; `remove_dir_all`
    // removes a link a run left, never what it points to.
    let dirs = tokio::task::spawn_blocking(move || {
        dirs.iter()
            .filter_map(|dir| match std::fs::remove_dir_all(dir) {
                Ok(()) => None,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => Some(format!("{}: {err}", dir.display())),
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(Fail::internal)?;
    not_freed.extend(dirs);
    Ok(Json(ProjectRemoval { project, not_freed }))
}

/// Why the project cannot be removed yet: each task that is running or open,
/// or whose run is still ending, and any orchestrator run still working.
fn busy(proj: &Projections, id: &ProjectId, name: &str) -> Option<Refusal> {
    let working = |task: Option<&dvb_core::TaskId>| {
        proj.runs
            .values()
            .any(|r| &r.project == id && r.assignment.task.as_ref() == task && r.ended.is_none())
    };
    let mut tasks = Vec::new();
    let mut lines = Vec::new();
    for t in proj.tasks.values().filter(|t| &t.view.project == id) {
        let (task, title) = (&t.view.id, &t.view.title);
        let line = match &t.view.state {
            TaskState::Stopped { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. } => {
                if !working(Some(task)) {
                    continue;
                }
                format!(
                    "{task} \"{title}\" has a run still ending: remove the project once it has ended"
                )
            }
            state @ (TaskState::Planned
            | TaskState::Preparing
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::InReview
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Unknown { .. }) => format!(
                "{task} \"{title}\" ({}): stop it with dvb stop {task}",
                dvb_core::wording::state_word(state)
            ),
        };
        tasks.push(task.clone());
        lines.push(line);
    }
    if working(None) {
        lines.push(format!(
            "the orchestrator is working: pause the project with dvb pause {name} and remove it once its run has ended"
        ));
    }
    (!lines.is_empty()).then(|| {
        Refusal::new(
            RefusalCode::ProjectBusy { tasks },
            format!("{name} has running or open work: {}", lines.join("; ")),
        )
    })
}
