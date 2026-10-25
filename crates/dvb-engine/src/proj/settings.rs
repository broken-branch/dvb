use super::Projections;
use dvb_core::{Event, EventKind};

impl Projections {
    pub fn max_runs(&self) -> u64 {
        self.settings
            .values
            .get(&dvb_core::api::SettingsKey::MaxRuns)
            .and_then(serde_json::Value::as_u64)
            .filter(|n| *n >= 1)
            .unwrap_or(3)
    }

    /// The project is named in `autonomous_projects`.
    pub(crate) fn autonomous(&self, project: &dvb_core::ProjectId) -> bool {
        self.projects
            .get(project)
            .is_some_and(|p| self.settings.autonomous(&p.view.name))
    }

    pub(crate) fn model_selected(&self, model: &str) -> bool {
        self.settings.model_selected(model)
    }

    pub(super) fn fold_settings(&mut self, e: &Event) {
        let project = e.project.clone();
        if let EventKind::SettingsChanged { key, new, .. } = &e.kind {
            // Project checks and removed historical settings are not install settings.
            if key == "project_checks" {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id))
                    && let Ok(ci) = serde_json::from_value(new.clone())
                {
                    p.view.ci = ci;
                }
            } else if let Ok(key) = key.parse::<dvb_core::api::SettingsKey>() {
                self.settings.values.insert(key, new.clone());
            }
        } else if let EventKind::OnlyStarted(only) = &e.kind {
            self.only = Some((e.id, only.clone()));
        } else if let EventKind::OnlyEnded { started } = &e.kind
            && self.only.as_ref().is_some_and(|(id, _)| id == started)
        {
            self.only = None;
        }
    }
}
