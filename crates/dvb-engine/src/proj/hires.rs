//! Hire creation, replacement, authorization and retirement rules.
use crate::{AppState, Fail};
use dvb_core::api::{HireCreated, NewHire};
use dvb_core::roles::{HireRecord, HireStatus};
use dvb_core::{Actor, EventKind, HireId, ProjectId, Refusal, RefusalCode, RoleId};
fn invalid(detail: &str) -> Fail {
    Refusal::new(RefusalCode::InvalidValue, detail).into()
}
/// Owner and orchestrator hires share pool, effort and project validation.
pub(crate) fn create_hire(
    e: &AppState,
    body: NewHire,
    actor: &Actor,
    scope: Option<&ProjectId>,
) -> Result<HireCreated, Fail> {
    let project = e
        .proj()
        .projects
        .iter()
        .filter(|(id, _)| scope.is_none_or(|s| s == *id))
        .find_map(|(id, p)| p.role(&body.role).map(|_| id.clone()))
        .ok_or_else(|| invalid("hire into an existing role"))?;
    let hire = HireRecord {
        id: HireId::new(e.new_id()),
        role: body.role,
        model: body.model,
        effort: body.effort,
        status: HireStatus::Active,
        routine: body.routine,
    };
    let mut retired = Vec::new();
    e.append_checked(Some(&project), actor.clone(), |proj, _now| {
        validate_hire(e, proj, &hire.model, hire.effort)?;
        if !proj.projects[&project]
            .roles
            .iter()
            .any(|r| r.id == hire.role)
        {
            return Err(invalid("hire into an existing role"));
        }
        authorize_hire(proj, &project, actor)?;
        retired = retiring_hires(&proj.projects[&project], &hire.role, hire.routine);
        let mut kinds = retired
            .iter()
            .cloned()
            .map(EventKind::HireRetired)
            .collect::<Vec<_>>();
        kinds.push(EventKind::HireAdded(hire.clone()));
        Ok(kinds)
    })?;
    Ok(HireCreated { hire, retired })
}

pub(crate) fn validate_hire(
    e: &AppState,
    proj: &crate::proj::Projections,
    model_id: &str,
    effort: dvb_core::Effort,
) -> Result<(), Fail> {
    let model = e
        .pool
        .find(model_id)
        .ok_or_else(|| Refusal::new(RefusalCode::ModelNotInPool, "the model is not available"))?;
    if !model.efforts.contains(&effort) {
        return Err(Refusal::new(
            RefusalCode::ModelNotInPool,
            "the effort is not available for this model",
        )
        .into());
    }
    if !proj.model_selected(model_id) {
        return Err(Refusal::new(
            RefusalCode::ModelNotInPool,
            "the model is outside the selected pool",
        )
        .into());
    }
    Ok(())
}

/// Build replacement records from the projections held by the event writer:
/// the role's active hires of the same kind (routine or default).
pub(crate) fn retiring_hires(
    project: &crate::proj::Project,
    role: &RoleId,
    routine: bool,
) -> Vec<HireRecord> {
    project
        .hires
        .iter()
        .filter(|h| &h.role == role && h.status == HireStatus::Active && h.routine == routine)
        .cloned()
        .map(|mut h| {
            h.status = HireStatus::Retired;
            h
        })
        .collect()
}

pub(crate) fn retire_record(
    e: &AppState,
    id: &HireId,
    actor: &Actor,
    scope: Option<&ProjectId>,
) -> Result<HireRecord, Fail> {
    let (project, mut hire) = e
        .proj()
        .projects
        .iter()
        .filter(|(id, _)| scope.is_none_or(|s| s == *id))
        .find_map(|(p, v)| {
            v.hires
                .iter()
                .find(|h| &h.id == id)
                .map(|h| (p.clone(), h.clone()))
        })
        .ok_or_else(|| Fail::NotFound(format!("hire {id}")))?;
    e.append_checked(Some(&project), actor.clone(), |proj, _now| {
        authorize_hire(proj, &project, actor)?;
        hire = proj.projects[&project]
            .hires
            .iter()
            .find(|h| &h.id == id)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("hire {id}")))?;
        if hire.status == HireStatus::Active {
            hire.status = HireStatus::Retired;
            Ok(vec![EventKind::HireRetired(hire.clone())])
        } else {
            Ok(vec![])
        }
    })?;
    Ok(hire)
}

fn authorize_hire(
    proj: &crate::proj::Projections,
    project: &ProjectId,
    actor: &Actor,
) -> Result<(), Fail> {
    match actor {
        Actor::Owner | Actor::Engine => Ok(()),
        Actor::Run(id) => {
            let run = proj
                .runs
                .get(id)
                .filter(|r| r.ended.is_none())
                .ok_or_else(|| Refusal::new(RefusalCode::NotARun, "the run has ended"))?;
            if &run.project != project
                || !proj.projects[project]
                    .role(&run.assignment.role)
                    .is_some_and(|r| r.slug == dvb_core::roles::ORCHESTRATOR)
            {
                return Err(Refusal::new(
                    RefusalCode::RoleNotAllowed,
                    "only this project's orchestrator may hire or retire",
                )
                .into());
            }
            Ok(())
        }
    }
}
