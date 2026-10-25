//! Project registration clones or creates a bare mirror and records the CI
//! choice, default roles and hires from the model pool.

use super::invalid;
use crate::AppState;
use crate::engine::Fail;
use crate::publish;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use dvb_core::api::{NewProject, ProjectView};
use dvb_core::git::Git;
use dvb_core::roles::{HireRecord, HireStatus, default_records};
use dvb_core::{Actor, Effort, EventKind, HireId, ProjectId, Refusal, RefusalCode, RoleId};
use dvb_forge::scan::ScanPolicy;

#[utoipa::path(
    post,
    path = "/projects",
    operation_id = "add_project",
    tag = "projects",
    request_body = dvb_core::api::NewProject,
    responses(crate::api::ErrorResponses, (status = 201, description = "Project added", body = dvb_core::api::ProjectView))
)]
pub async fn add_project(
    State(e): State<AppState>,
    Json(body): Json<NewProject>,
) -> Result<(StatusCode, Json<ProjectView>), Fail> {
    let forge = e
        .forge()
        .ok_or_else(|| invalid("GitHub App is not configured. Run dvb github connect."))?;
    let model = e.default_hire_model();
    let named = |name: &str| {
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if body.name.is_empty() || !named(&body.name) {
        return Err(Refusal::new(
            RefusalCode::InvalidProjectName,
            format!("bad project name {:?}", body.name),
        )
        .into());
    }
    let mirror = e.mirror(&body.name);
    let repo = dvb_core::forge::Repo(body.repo.clone());
    if e.proj().projects.values().any(|p| p.view.name == body.name) {
        return Err(invalid(format!("project {} exists", body.name)));
    }
    if mirror.exists() {
        // Check request identity before create_or_resume can create a repository.
        // GitHub clones use this canonical URL; the local Fake uses the repo path.
        let origin = Git::new(&mirror)
            .run(&["config", "--get", "remote.origin.url"], &[])
            .await
            .map_err(Fail::internal)?;
        if origin != repo.0 && origin != format!("https://github.com/{}.git", repo.0) {
            return Err(invalid("existing mirror belongs to a different repository"));
        }
        if let Some(branch) = &body.default_branch {
            let head = Git::new(&mirror)
                .run(&["symbolic-ref", "HEAD"], &[])
                .await
                .map_err(Fail::internal)?;
            if head != format!("refs/heads/{branch}") {
                return Err(invalid("existing mirror has a different default branch"));
            }
        }
    }
    // A failed CI choice, clone or backstop leaves no unregistered final mirror
    // that would prevent retry. Remote provisioning effects remain private.
    let staging = tempfile::tempdir_in(&e.cfg.paths.mirrors).map_err(Fail::internal)?;
    let staged_mirror = staging.path().join("mirror.git");
    let prepared = {
        // Refuse unscanned inputs and silent CI before any creation or setup effect.
        // Owner-typed config, not dvb's writing: the owner's name may be in it.
        let policy = ScanPolicy::default();
        for (name, value) in [
            ("project name", body.name.as_str()),
            ("repository", body.repo.as_str()),
            // An absent branch is the empty text, which has no hit.
            (
                "default branch",
                body.default_branch.as_deref().unwrap_or_default(),
            ),
        ] {
            if let Some(hit) = policy.scan(name, value) {
                return Err(publish::project::scan_refusal(hit));
            }
        }
        if let dvb_core::event::CiMode::Expected(names) = &body.ci {
            for name in names {
                if let Some(hit) = policy.scan("required check", name) {
                    return Err(publish::project::scan_refusal(hit));
                }
            }
        }
        if body.create {
            if !forge.can_create_repo() {
                return Err(invalid(publish::project::PERSONAL_CREATE));
            }
            publish::project::expected_ci(&[], &body.ci)?;
            let (org, name) = body
                .repo
                .rsplit_once('/')
                .ok_or_else(|| invalid("expected organization/repository"))?;
            publish::project::create_or_resume(&e, forge.as_ref(), &body.name, &repo, org, name)
                .await?;
        }
        publish::project::prepare(
            forge.as_ref(),
            &repo,
            &staged_mirror,
            body.create,
            body.default_branch.as_deref(),
            &body.ci,
        )
        .await?
    };
    let default_branch = prepared.default.clone();

    // Whom the repository's recent dvb commits name; the add goes on either way.
    let recent = if body.create {
        None
    } else {
        publish::project::recent_dvb_author(forge.as_ref(), &repo, &staged_mirror, &default_branch)
            .await
            .unwrap_or_else(|error| {
                eprintln!(
                    "dvb engine: project {}: recent dvb commits: {error}",
                    body.name
                );
                None
            })
    };
    if mirror.exists() {
        // Recover a crash after finalizing the mirror but before ProjectAdded.
        // Both add modes can reuse an unregistered mirror, but only for the
        // same repository, default branch and head, before any setup publication.
        for args in [
            vec!["config", "--get", "remote.origin.url"],
            vec!["symbolic-ref", "HEAD"],
        ] {
            let old = Git::new(&mirror)
                .run(&args, &[])
                .await
                .map_err(Fail::internal)?;
            let ready = Git::new(&staged_mirror)
                .run(&args, &[])
                .await
                .map_err(Fail::internal)?;
            if old != ready {
                return Err(invalid(
                    "existing mirror belongs to a different repository or default branch",
                ));
            }
        }
        let reference = format!("refs/heads/{default_branch}");
        let old = Git::new(&mirror)
            .run(&["rev-parse", "--verify", &reference], &[])
            .await
            .map_err(Fail::internal)?;
        let ready = Git::new(&staged_mirror)
            .run(&["rev-parse", "--verify", &reference], &[])
            .await
            .map_err(Fail::internal)?;
        if old != ready {
            return Err(invalid("existing mirror differs from prepared repository"));
        }
    }
    let ci = publish::project::publish_setup(
        &e,
        forge.as_ref(),
        &repo,
        &staged_mirror,
        body.create,
        &prepared,
    )
    .await?;
    if !mirror.exists() {
        std::fs::rename(&staged_mirror, &mirror).map_err(Fail::internal)?;
    }
    let id = ProjectId::new(e.new_id());
    let roles = default_records(|_| RoleId::new(e.new_id()));
    let hires: Vec<HireRecord> = roles
        .iter()
        .filter_map(|r| {
            model.as_ref().map(|model| HireRecord {
                id: HireId::new(e.new_id()),
                role: r.id.clone(),
                model: model.id.clone(),
                effort: model.efforts.first().copied().unwrap_or(Effort::Medium),
                status: HireStatus::Active,
                routine: false,
            })
        })
        .collect();
    let mut kinds = vec![EventKind::ProjectAdded {
        id: id.clone(),
        name: body.name.clone(),
        repo: body.repo.clone(),
        default_branch: default_branch.clone(),
        ci,
    }];
    if !prepared.rulesets {
        kinds.push(EventKind::ProjectRulesetMissing {});
    }
    kinds.extend(roles.into_iter().map(EventKind::RoleAdded));
    kinds.extend(hires.into_iter().map(EventKind::HireAdded));
    e.append_checked(Some(&id), Actor::Owner, |proj, _now| {
        if proj.projects.values().any(|p| p.view.name == body.name) {
            return Err(Refusal::new(
                RefusalCode::ValueChanged,
                format!("project {} exists", body.name),
            )
            .into());
        }
        Ok(kinds)
    })?;
    // What dvb found among the open issues; the project is added either way.
    match crate::issues::list(&e, &repo).await {
        Ok(listed) => {
            e.append_checked(Some(&id), Actor::Engine, |_, _| {
                Ok(vec![EventKind::ProjectIssuesFound(crate::issues::counts(
                    &listed,
                ))])
            })?;
        }
        Err(error) => eprintln!("dvb engine: project {}: open issues: {error}", body.name),
    }
    let mut view = e
        .proj()
        .projects
        .get(&id)
        .map(|p| p.view.clone())
        .ok_or_else(|| Fail::internal("project vanished"))?;
    view.publication_mismatch =
        recent.and_then(|recent| publish::project::mismatch(recent, &e.publication()));
    Ok((StatusCode::CREATED, Json(view)))
}
