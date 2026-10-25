use super::{Project, Projections};
use dvb_core::roles::{HireRecord, HireStatus};
use dvb_core::{Event, EventKind, RoleId};

impl Projections {
    /// The role's first usable active hire (on `model`, when named): a
    /// routine one first for a `routine` task, else a default one first.
    pub(crate) fn usable_hire<'a>(
        &self,
        project: &'a Project,
        role: &RoleId,
        model: Option<&str>,
        routine: bool,
        pool: &dvb_core::ModelPool,
    ) -> Option<&'a HireRecord> {
        project
            .hires
            .iter()
            .filter(|h| {
                &h.role == role
                    && h.status == HireStatus::Active
                    && model.is_none_or(|m| h.model == m)
                    && pool.find(&h.model).is_some()
                    && (self.model_selected(&h.model) || self.only.is_some())
            })
            .min_by_key(|h| h.routine != routine)
    }
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_roles(&mut self, e: &Event) {
        let project = e.project.clone();
        match &e.kind {
            EventKind::RoleAdded(r)
            | EventKind::RoleChanged { role: r, .. }
            | EventKind::RoleRemoved(r) => {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.roles.retain(|old| old.id != r.id);
                    if !matches!(e.kind, EventKind::RoleRemoved(_)) {
                        p.roles.push(r.clone());
                    }
                }
            }
            EventKind::HireAdded(h)
            | EventKind::HireChanged { hire: h, .. }
            | EventKind::HireRetired(h) => {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.hires.retain(|old| old.id != h.id);
                    p.hires.push(h.clone());
                }
            }
            _ => {}
        }
    }
}
