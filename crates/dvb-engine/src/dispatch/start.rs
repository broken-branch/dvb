//! Task-start assignment and hire selection for explicit and deferred requests.
use crate::{AppState, Fail};
use dvb_core::run_verbs::TaskStartBody;
use dvb_core::state::Effect;
use dvb_core::{Actor, EventKind, Refusal, RefusalCode, RunAssignment, RunId, TaskState};
/// A task start as the orchestrator asked for it, by `actor`: the orchestrator's
/// own, or the engine's for a start that waited for a free run slot. `waiting`
/// is recorded with a refusal for the cap on concurrent runs.
/// Automatic admission supplies `approved_version` to recheck the goal and
/// never-started task with the start decision, after assignment preparation.
/// A start of a task already started (by approval, for one) changes nothing:
/// it returns the task's run, checked again under the task's lock.
pub(crate) async fn start_as_asked(
    e: &AppState,
    project: &dvb_core::ProjectId,
    actor: Actor,
    body: TaskStartBody,
    waiting: Option<EventKind>,
    approved_version: Option<dvb_core::EventId>,
) -> Result<Option<RunId>, Fail> {
    e.expire_only(None)?;
    let project = project.clone();
    let refuse = |error: Fail| e.refuse(Some(&project), actor.clone(), "task start", error);
    let repeated = || {
        let proj = e.proj();
        let run = (proj.tasks.get(&body.task))
            .filter(|t| t.view.project == project && already_started(&t.view.state))
            .map(|t| t.facts.run.clone());
        drop(proj);
        run
    };
    if let Some(run) = repeated() {
        return Ok(run);
    }
    if let Some(model) = &body.model {
        e.vendor_of(model).map_err(refuse)?;
    }
    if retain_dependency(e, &project, &actor, &body).map_err(refuse)? {
        return Ok(None);
    }
    // The base the run starts from, recorded in its assignment.
    let (repo, mirror, branch) = {
        let proj = e.proj();
        let p = proj
            .projects
            .get(&project)
            .ok_or_else(|| Fail::NotFound(format!("project {project}")))?;
        let result = (
            p.view.repo.clone(),
            e.mirror(&p.view.name),
            p.view.default_branch.clone(),
        );
        drop(proj);
        result
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let base = e
        .fetch(
            forge.as_ref(),
            &dvb_core::forge::Repo(repo),
            &mirror,
            &branch,
        )
        .await
        .map_err(Fail::internal)?;
    // Checked under the projections' lock; a refusal is recorded after it.
    let checked = (|| {
        let proj = e.proj();
        let auto = proj.settings.model_mode() == dvb_core::api::ModelMode::Auto;
        let t = proj
            .tasks
            .get(&body.task)
            .filter(|t| t.view.project == project)
            .ok_or_else(|| Fail::NotFound(format!("task {}", body.task)))?;
        let p = proj
            .projects
            .get(&project)
            .ok_or_else(|| Fail::NotFound(format!("project {project}")))?;
        let role = p
            .role_by_slug(&body.role)
            .filter(|r| r.id == t.view.role)
            .ok_or_else(|| {
                Fail::Refused(Refusal::new(
                    RefusalCode::RoleNotAllowed,
                    format!("the task is planned for another role than {}", body.role),
                ))
            })?;

        let pinned = (actor == Actor::Engine)
            .then(|| t.waiting.as_ref().and_then(|w| w.hire.as_ref()))
            .flatten();
        let routine = body.routine || t.view.routine;
        let existing = proj
            .usable_hire(p, &role.id, body.model.as_deref(), routine, &e.pool)
            .filter(|h| pinned.is_none_or(|id| id == &h.id));
        // Auto: the engine hires the named pool model for the role, on the
        // orchestrator's behalf, at the effort asked for (else the role's own).
        let mut hired = None;
        let hire = match (existing, &body.model) {
            (Some(h), _) => Some(h.clone()),
            (None, Some(model)) if auto && pinned.is_none() => {
                let effort = body.effort.unwrap_or_else(|| {
                    p.hire(&role.id, None)
                        .map_or(dvb_core::Effort::Medium, |h| h.effort)
                });
                let h = dvb_core::roles::HireRecord {
                    id: dvb_core::HireId::new(e.new_id()),
                    role: role.id.clone(),
                    model: model.clone(),
                    effort,
                    status: dvb_core::roles::HireStatus::Active,
                    routine: false,
                };
                hired = Some(h.clone());
                Some(h)
            }
            (None, _) => None,
        }
        .ok_or_else(|| {
            Fail::Refused(Refusal::new(
                RefusalCode::NoActiveHire,
                format!(
                    "no usable active {} hire{}; ask the owner with dvb run ask to hire a model from the selected pool for this role",
                    role.slug,
                    body.model
                        .as_ref()
                        .map(|m| format!(" on {m}"))
                        .unwrap_or_default()
                ),
            ))
        })?;
        let vendor = e
            .pool
            .find(&hire.model)
            .map(|m| m.vendor)
            .ok_or_else(|| Fail::internal("hired model outside install pool"))?;
        let a = RunAssignment {
            review_stage: None,
            goal_version: None,
            run: RunId::new(e.new_id()),
            task: Some(body.task.clone()),
            role: role.id.clone(),
            hire: hire.id.clone(),
            model: hire.model.clone(),
            vendor,
            effort: body.effort.unwrap_or(hire.effort),
            reason: body.reason.clone(),
            reviews: None,
            base: Some(base),
            routine,
        };
        drop(proj);
        Ok((a, hired))
    })();
    let (a, hired) = checked.map_err(refuse)?;
    let _lock = e.lock(&body.task).await;
    if let Some(run) = repeated() {
        return Ok(run);
    }
    match crate::dispatch::runs::start_locked(e, a, actor, hired, waiting, approved_version) {
        Ok(run) => Ok(Some(run)),
        Err(Fail::Refused(r)) if matches!(r.code, RefusalCode::DependencyNotMerged { .. }) => {
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The states a requested start leaves as they are. A fix round of a task in
/// review is the engine's own start, never a requested one.
fn already_started(state: &TaskState) -> bool {
    match state {
        TaskState::Preparing
        | TaskState::Running
        | TaskState::Collecting
        | TaskState::Publishing
        | TaskState::InReview
        | TaskState::AwaitingChecks
        | TaskState::Merging => true,
        TaskState::RetryPending { effect, .. } => *effect != Effect::StartUnit,
        TaskState::Planned
        | TaskState::Stopped { .. }
        | TaskState::Unknown { .. }
        | TaskState::Done
        | TaskState::Cancelled
        | TaskState::Failed { .. } => false,
    }
}

/// Retain the selected hire before fetching a base or allocating a worker.
fn retain_dependency(
    e: &AppState,
    project: &dvb_core::ProjectId,
    actor: &Actor,
    body: &TaskStartBody,
) -> Result<bool, Fail> {
    let mut waits = false;
    e.append_checked(Some(project), actor.clone(), |proj, _| {
        let t = proj
            .tasks
            .get(&body.task)
            .filter(|t| t.view.project == *project)
            .ok_or_else(|| Fail::NotFound(format!("task {}", body.task)))?;
        let p = &proj.projects[project];
        if !t.approved {
            return Err(
                Refusal::new(RefusalCode::GoalNotApproved, "approve the plan first").into(),
            );
        }
        if !p
            .role_by_slug(&body.role)
            .is_some_and(|r| r.id == t.view.role)
        {
            return Err(Refusal::new(
                RefusalCode::RoleNotAllowed,
                "the task is planned for another role",
            )
            .into());
        }
        waits = t.facts.run.is_none()
            && t.view.state == dvb_core::TaskState::Planned
            && !proj.unmet_dependencies(t).is_empty();
        if !waits {
            return Ok(vec![]);
        }
        let routine = body.routine || t.view.routine;
        let selected = proj.usable_hire(p, &t.view.role, body.model.as_deref(), routine, &e.pool);
        let model = body
            .model
            .clone()
            .or_else(|| selected.map(|h| h.model.clone()));
        let effort = body.effort.or_else(|| selected.map(|h| h.effort));
        Ok(
            if t.waiting.as_ref().is_some_and(|w| {
                w.model == model
                    && w.effort == effort
                    && w.reason == body.reason
                    && w.routine == body.routine
            }) {
                vec![]
            } else {
                vec![EventKind::TaskWaiting {
                    task: body.task.clone(),
                    model,
                    effort,
                    reason: body.reason.clone(),
                    routine: body.routine,
                }]
            },
        )
    })?;
    Ok(waits)
}
