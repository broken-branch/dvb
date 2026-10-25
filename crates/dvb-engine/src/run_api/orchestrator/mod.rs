//! Verbs for the orchestrator (and leads): plan, ask, task start, stop and retry.

use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use dvb_core::{Refusal, RefusalCode};

mod hires;
mod planning;
mod questions;
mod tasks;

pub use hires::{hire, retire};
pub use planning::plan;
pub use questions::ask;
pub use tasks::{task_retry, task_start, task_stop};

#[cfg(test)]
mod tests;

fn not_allowed(detail: impl Into<String>) -> Fail {
    Fail::Refused(Refusal::new(RefusalCode::RoleNotAllowed, detail))
}

/// The caller's role must be the orchestrator (or, where `lead_ok`, a lead).
fn orchestrator(e: &AppState, c: &Caller, lead_ok: bool) -> Result<(), Fail> {
    let proj = e.proj();
    let role = proj
        .projects
        .get(&c.run.project)
        .and_then(|p| p.role(&c.run.assignment.role));
    let result = match role {
        Some(r) if r.slug == dvb_core::roles::ORCHESTRATOR || (lead_ok && r.lead) => Ok(()),
        Some(r) => Err(not_allowed(format!("{} may not do this", r.slug))),
        None => Err(not_allowed("the run has no role")),
    };
    drop(proj);
    result
}
