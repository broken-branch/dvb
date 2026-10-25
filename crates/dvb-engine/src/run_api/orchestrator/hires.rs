use super::{not_allowed, orchestrator};
use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::Actor;
use dvb_core::api::NewHire;
use dvb_core::roles::HireRecord;
use dvb_core::run_verbs::{HireBody, HireReply, RetireBody};

pub async fn hire(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<HireBody>,
) -> Result<Json<HireReply>, Fail> {
    orchestrator(&e, &c, false)?;
    let (role, effort) = {
        let proj = e.proj();
        let p = &proj.projects[&c.run.project];
        let role = p
            .role_by_slug(&body.role)
            .ok_or_else(|| not_allowed("hire into an existing role in this project"))?;
        let effort = body.effort.unwrap_or_else(|| {
            p.hire(&role.id, None)
                .map_or(dvb_core::Effort::Medium, |h| h.effort)
        });
        let role = role.id.clone();
        drop(proj);
        (role, effort)
    };
    let hire = crate::proj::hires::create_hire(
        &e,
        NewHire {
            role,
            model: body.model,
            effort,
            routine: false,
        },
        &Actor::Run(c.run.assignment.run),
        Some(&c.run.project),
    )?;
    Ok(Json(HireReply { hire: hire.hire.id }))
}

pub async fn retire(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<RetireBody>,
) -> Result<Json<HireRecord>, Fail> {
    orchestrator(&e, &c, false)?;
    crate::proj::hires::retire_record(
        &e,
        &body.hire,
        &Actor::Run(c.run.assignment.run),
        Some(&c.run.project),
    )
    .map(Json)
}
