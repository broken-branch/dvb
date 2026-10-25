//! Vendors, efforts and the model pool.

use crate::roles::{HireRecord, HireStatus};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

crate::text::text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
    pub enum Vendor {
        Claude => "claude",
        Codex => "codex",
        /// The scripted test provider; in the pool only in dev mode and tests.
        Fake => "fake",
    }
}

crate::text::text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
    pub enum Effort {
        Low => "low",
        Medium => "medium",
        High => "high",
    }
}

/// One model the install may use. Model ids are full strings
/// (`claude-sonnet-5-5`, `gpt-6.1-sol`, `fake-1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PoolModel {
    pub id: String,
    pub vendor: Vendor,
    pub efforts: Vec<Effort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct ModelPool {
    pub models: Vec<PoolModel>,
}

impl ModelPool {
    pub fn find(&self, id: &str) -> Option<&PoolModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// The install-wide timed model restriction. Its event id identifies replacements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnlyModels {
    pub models: Vec<String>,
    pub ends_at: crate::Timestamp,
}

/// A named hire keeps its model and effort. Other hires use the first named
/// model, at the closest supported effort (ties prefer the lower effort).
pub fn only_model<'a>(
    model: &'a str,
    effort: Effort,
    only: &'a OnlyModels,
    pool: &ModelPool,
) -> Option<(&'a str, Effort)> {
    if only.models.iter().any(|id| id == model) {
        return Some((model, effort));
    }
    let first = only.models.first()?;
    let supported = &pool.find(first)?.efforts;
    let rank = |e| match e {
        Effort::Low => 0_i32,
        Effort::Medium => 1,
        Effort::High => 2,
    };
    let nearest = supported
        .iter()
        .copied()
        .min_by_key(|e| ((rank(*e) - rank(effort)).abs(), rank(*e)))?;
    Some((first, nearest))
}

/// The model a run moves to while its vendor is at its limit:
/// the role's active hire on the orchestrator's vendor if there is one, else the
/// orchestrator's own model at the role's hire's effort. `None` when the
/// orchestrator's model is not in the pool. The only rule; no pairing table.
pub fn fallback_model(
    role_hires: &[HireRecord],
    orchestrator: &HireRecord,
    pool: &ModelPool,
) -> Option<(String, Effort)> {
    let vendor = pool.find(&orchestrator.model)?.vendor;
    let mut active = role_hires.iter().filter(|h| h.status == HireStatus::Active);
    if let Some(h) = active
        .clone()
        .find(|h| pool.find(&h.model).is_some_and(|m| m.vendor == vendor))
    {
        return Some((h.model.clone(), h.effort));
    }
    let effort = active.next().map_or(orchestrator.effort, |h| h.effort);
    Some((orchestrator.model.clone(), effort))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{HireId, RoleId};

    fn pool() -> ModelPool {
        let m = |id: &str, vendor| PoolModel {
            id: id.into(),
            vendor,
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
        };
        ModelPool {
            models: vec![
                m("claude-sonnet-5-5", Vendor::Claude),
                m("claude-haiku-5", Vendor::Claude),
                m("gpt-6.1-sol", Vendor::Codex),
            ],
        }
    }

    fn hire(id: &str, model: &str, effort: Effort, status: HireStatus) -> HireRecord {
        HireRecord {
            id: HireId::new(id),
            role: RoleId::new("role"),
            model: model.into(),
            effort,
            status,
            routine: false,
        }
    }

    #[test]
    fn only_preserves_named_hires_and_chooses_the_nearest_supported_effort() {
        let mut pool = pool();
        pool.models[0].efforts = vec![Effort::High, Effort::Low]; // order does not select ties
        let only = OnlyModels {
            models: vec![pool.models[0].id.clone(), pool.models[1].id.clone()],
            ends_at: crate::Timestamp(1),
        };
        for (asked, nearest) in [
            (Effort::Low, Effort::Low),
            (Effort::Medium, Effort::Low),
            (Effort::High, Effort::High),
        ] {
            assert_eq!(
                only_model("gpt-6.1-sol", asked, &only, &pool),
                Some(("claude-sonnet-5-5", nearest))
            );
            assert_eq!(
                only_model("claude-haiku-5", asked, &only, &pool),
                Some(("claude-haiku-5", asked))
            );
        }
        pool.models[0].efforts = vec![Effort::High];
        assert_eq!(
            only_model("gpt-6.1-sol", Effort::Low, &only, &pool),
            Some(("claude-sonnet-5-5", Effort::High))
        );
    }

    #[test]
    fn fallback_prefers_the_roles_hire_on_the_orchestrators_vendor() {
        let orch = hire("o", "claude-sonnet-5-5", Effort::Medium, HireStatus::Active);
        let hires = [
            hire("a", "gpt-6.1-sol", Effort::High, HireStatus::Active),
            hire("b", "claude-haiku-5", Effort::Low, HireStatus::Retired),
            hire("c", "claude-haiku-5", Effort::High, HireStatus::Active),
        ];
        assert_eq!(
            fallback_model(&hires, &orch, &pool()),
            Some(("claude-haiku-5".into(), Effort::High))
        );
    }

    #[test]
    fn fallback_otherwise_uses_the_orchestrators_model_at_the_roles_effort() {
        let orch = hire("o", "claude-sonnet-5-5", Effort::Medium, HireStatus::Active);
        let hires = [
            hire("a", "gpt-6.1-sol", Effort::High, HireStatus::Active),
            hire("b", "claude-haiku-5", Effort::Low, HireStatus::Retired),
        ];
        assert_eq!(
            fallback_model(&hires, &orch, &pool()),
            Some(("claude-sonnet-5-5".into(), Effort::High))
        );
        // No active hire: the orchestrator's own effort.
        assert_eq!(
            fallback_model(&hires[1..], &orch, &pool()),
            Some(("claude-sonnet-5-5".into(), Effort::Medium))
        );
    }

    #[test]
    fn fallback_needs_the_orchestrators_model_in_the_pool() {
        let orch = hire("o", "unknown-1", Effort::Medium, HireStatus::Active);
        let hires = [hire("a", "gpt-6.1-sol", Effort::High, HireStatus::Active)];
        assert_eq!(fallback_model(&hires, &orch, &pool()), None);
    }
}
