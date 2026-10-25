//! Repointing a project at its repository's new name after a rename or transfer.

use super::invalid;
use crate::AppState;
use crate::engine::Fail;
use crate::publish;
use axum::Json;
use axum::extract::{Path, State};
use dvb_core::api::{ProjectRepo, ProjectView};
use dvb_core::forge::Repo;
use dvb_core::git::Git;
use dvb_core::{Actor, EventKind, ProjectId, Refusal, RefusalCode};
use dvb_forge::scan::ScanPolicy;

/// Only the same repository, read by its id through the App's installation, is
/// accepted. Goals, tasks and history stay; the mirror's receipts move with it.
#[utoipa::path(
    put,
    path = "/projects/{id}/repo",
    operation_id = "set_project_repo",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    request_body = dvb_core::api::ProjectRepo,
    responses(crate::api::ErrorResponses, (status = 200, description = "The project under its repository's new name; records `project.repo_changed`", body = dvb_core::api::ProjectView))
)]
pub async fn set_project_repo(
    State(e): State<AppState>,
    Path(id): Path<ProjectId>,
    Json(body): Json<ProjectRepo>,
) -> Result<Json<ProjectView>, Fail> {
    let refuse = |code, detail: String| {
        e.refuse(
            Some(&id),
            Actor::Owner,
            "project.repo",
            Refusal::new(code, detail),
        )
    };
    let to = Repo(body.repo.clone());
    let (name, from) = {
        let proj = e.proj();
        let Some(project) = proj.projects.get(&id) else {
            drop(proj);
            return Err(e.refuse(
                None,
                Actor::Owner,
                "project.repo",
                Refusal::new(RefusalCode::ProjectUnknown, format!("no project {id}")),
            ));
        };
        if proj.projects.values().any(|p| p.view.repo == to.0) {
            drop(proj);
            return Err(refuse(
                RefusalCode::InvalidValue,
                format!("a project already uses repository {}", to.0),
            ));
        }
        (project.view.name.clone(), Repo(project.view.repo.clone()))
    };
    // Owner-typed config, not dvb's writing: the owner's name may be in it.
    if let Some(hit) = ScanPolicy::default().scan("repository", &to.0) {
        return Err(e.refuse(
            Some(&id),
            Actor::Owner,
            "project.repo",
            publish::project::scan_refusal(hit),
        ));
    }
    let forge = e
        .forge()
        .ok_or_else(|| invalid("GitHub App is not configured. Run dvb github connect."))?;
    // No mirror pass writes receipts under the old name while they move.
    let _mirror = e.mirror_retries.lock().await;
    let same = forge.repository_id(&to).await.map_err(Fail::internal)?;
    let Some(same) = same else {
        return Err(refuse(
            RefusalCode::RepositoryMismatch,
            format!(
                "dvb's GitHub App reaches no repository named {}; check the name and that the App's installation includes it",
                to.0
            ),
        ));
    };
    if forge.repository_id(&from).await.map_err(Fail::internal)? != Some(same) {
        return Err(refuse(
            RefusalCode::RepositoryMismatch,
            format!(
                "{} is not the repository {} was; a project keeps its repository",
                to.0, from.0
            ),
        ));
    }
    forge.repoint_mirror(&from, &to).map_err(Fail::internal)?;
    let mirror = e.mirror(&name);
    if mirror.exists() {
        // GitHub clones use this canonical URL; the local Fake uses the repo path.
        let origin = Git::new(&mirror)
            .run(&["config", "--get", "remote.origin.url"], &[])
            .await
            .map_err(Fail::internal)?;
        let url = |repo: &Repo| format!("https://github.com/{}.git", repo.0);
        let moved = if origin == url(&from) {
            Some(url(&to))
        } else {
            (origin == from.0).then(|| to.0.clone())
        };
        if let Some(moved) = moved {
            Git::new(&mirror)
                .run(&["config", "remote.origin.url", &moved], &[])
                .await
                .map_err(Fail::internal)?;
        }
    }
    e.append_checked(Some(&id), Actor::Owner, |proj, _| {
        if proj.projects.get(&id).map(|p| p.view.repo.as_str()) != Some(from.0.as_str()) {
            return Err(Refusal::new(
                RefusalCode::ValueChanged,
                format!("project {name} changed repository meanwhile"),
            )
            .into());
        }
        Ok(vec![EventKind::ProjectRepoChanged {
            from: from.0.clone(),
            to: to.0.clone(),
        }])
    })?;
    e.proj()
        .projects
        .get(&id)
        .map(|p| Json(p.view.clone()))
        .ok_or_else(|| Fail::internal("project vanished"))
}
