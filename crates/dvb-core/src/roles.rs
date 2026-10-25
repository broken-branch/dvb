//! Roles, hires, the seven default roles and review routing.

use crate::ids::{HireId, RoleId};
use crate::model::Effort;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Which reviewer a task's head needs when no lead or assignment decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Code,
    Design,
}

/// A role in a project's company.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RoleRecord {
    pub id: RoleId,
    /// Stable name used by routing and prompts: the defaults' are `orchestrator`,
    /// `architect`, `frontend`, `backend`, `code-reviewer`, `design-reviewer`,
    /// `clerk`; a new role's is made from its name.
    pub slug: String,
    pub name: String,
    pub job: String,
    /// What the role decides (and writes SWOTs for).
    pub lane: String,
    /// Leads review their reports' work.
    pub lead: bool,
    pub reports_to: Option<RoleId>,
    /// Review assignment, apart from reporting.
    pub reviewed_by: Option<RoleId>,
    pub review_instructions: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub order: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HireStatus {
    Active,
    Retired,
}

/// An agent hired into a role: a model and an effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HireRecord {
    pub id: HireId,
    pub role: RoleId,
    pub model: String,
    pub effort: Effort,
    pub status: HireStatus,
    /// The role's routine hire: it builds routine tasks and every fix round
    /// on an open PR. A role has at most one active routine hire beside its
    /// default hire; a new hire replaces only the active hire of its own kind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub routine: bool,
}

/// One of the seven roles every project starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultRole {
    pub slug: &'static str,
    pub name: &'static str,
    pub job: &'static str,
    pub lane: &'static str,
}

pub const ORCHESTRATOR: &str = "orchestrator";
pub const CLERK: &str = "clerk";
pub const CODE_REVIEWER: &str = "code-reviewer";
pub const DESIGN_REVIEWER: &str = "design-reviewer";

pub const DEFAULTS: [DefaultRole; 7] = [
    DefaultRole {
        slug: ORCHESTRATOR,
        name: "Orchestrator",
        job: "Turns goals into plans of PR-sized tasks, runs the team, and asks the owner the questions.",
        lane: "Planning and product choices.",
    },
    DefaultRole {
        slug: "architect",
        name: "Architect",
        job: "Shapes the project's structure: modules, contracts, data and dependencies.",
        lane: "Architecture decisions only.",
    },
    DefaultRole {
        slug: "frontend",
        name: "Frontend builder",
        job: "Writes the code and tests for the user interface.",
        lane: "Implementation choices in the frontend.",
    },
    DefaultRole {
        slug: "backend",
        name: "Backend builder",
        job: "Writes the code and tests for everything behind the interface.",
        lane: "Implementation choices in the backend.",
    },
    DefaultRole {
        slug: CODE_REVIEWER,
        name: "Code reviewer",
        job: "Reviews correctness, tests, interfaces and architecture, and returns a verdict.",
        lane: "Review findings only.",
    },
    DefaultRole {
        slug: DESIGN_REVIEWER,
        name: "Design reviewer",
        job: "Reviews interaction, accessibility and consistency with the approved design, and returns a verdict.",
        lane: "Review findings only.",
    },
    DefaultRole {
        slug: "clerk",
        name: "Clerk",
        job: "Small judgment jobs, such as summarising a long log or labelling an incoming issue.",
        lane: "None; it makes no decisions that need a SWOT.",
    },
];

/// The seven default records for a new project, with ids from `new_id`. All
/// report to the orchestrator; none is a lead; `reviewed_by` is unset.
pub fn default_records(mut new_id: impl FnMut(&DefaultRole) -> RoleId) -> Vec<RoleRecord> {
    let ids: Vec<RoleId> = DEFAULTS.iter().map(&mut new_id).collect();
    let orchestrator = ids[0].clone();
    DEFAULTS
        .iter()
        .zip(ids)
        .map(|(d, id)| RoleRecord {
            reports_to: (d.slug != ORCHESTRATOR).then(|| orchestrator.clone()),
            id,
            slug: d.slug.to_owned(),
            name: d.name.to_owned(),
            job: d.job.to_owned(),
            lane: d.lane.to_owned(),
            lead: false,
            reviewed_by: None,
            review_instructions: String::new(),
            group: None,
            order: 0,
        })
        .collect()
}

/// What review routing needs to know about the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewSubject<'a> {
    pub review: ReviewKind,
    /// The roles whose runs built the head under review.
    pub built_by: &'a [RoleId],
}

/// The one reviewer accountable for a head:
///
/// 1. the builder role's lead, if it has one and the lead did not build this head;
/// 2. else the builder role's `reviewed_by`, if set;
/// 3. else `design-reviewer` for a `Design` task, otherwise `code-reviewer`.
///
/// `roles` are the project's roles. `None` only when rule 3's role was removed.
pub fn accountable_reviewer(
    task: &ReviewSubject<'_>,
    builder_role: &RoleId,
    roles: &[RoleRecord],
) -> Option<RoleId> {
    let find = |id: &RoleId| roles.iter().find(|r| &r.id == id);
    let builder = find(builder_role);

    let lead = builder
        .and_then(|b| b.reports_to.as_ref())
        .and_then(find)
        .filter(|l| l.lead && !task.built_by.contains(&l.id));
    if let Some(lead) = lead {
        return Some(lead.id.clone());
    }
    if let Some(assigned) = builder.and_then(|b| b.reviewed_by.clone()) {
        return Some(assigned);
    }
    let slug = match task.review {
        ReviewKind::Design => DESIGN_REVIEWER,
        ReviewKind::Code => CODE_REVIEWER,
    };
    roles.iter().find(|r| r.slug == slug).map(|r| r.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> Vec<RoleRecord> {
        default_records(|d| RoleId::new(d.slug))
    }

    fn role_mut<'a>(roles: &'a mut [RoleRecord], id: &str) -> &'a mut RoleRecord {
        roles.iter_mut().find(|r| r.id.as_str() == id).unwrap()
    }

    #[test]
    fn seven_defaults_report_to_the_orchestrator() {
        let roles = roles();
        let slugs: Vec<&str> = roles.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "orchestrator",
                "architect",
                "frontend",
                "backend",
                "code-reviewer",
                "design-reviewer",
                "clerk"
            ]
        );
        assert_eq!(roles[0].reports_to, None);
        for r in &roles[1..] {
            assert_eq!(r.reports_to, Some(RoleId::new("orchestrator")));
        }
        assert!(roles.iter().all(|r| !r.lead && r.reviewed_by.is_none()));
    }

    #[test]
    fn no_lead_no_assignment_goes_by_review_kind() {
        let roles = roles();
        let backend = RoleId::new("backend");
        let built = [backend.clone()];
        let code = ReviewSubject {
            review: ReviewKind::Code,
            built_by: &built,
        };
        let design = ReviewSubject {
            review: ReviewKind::Design,
            built_by: &built,
        };
        assert_eq!(
            accountable_reviewer(&code, &backend, &roles),
            Some(RoleId::new("code-reviewer"))
        );
        assert_eq!(
            accountable_reviewer(&design, &backend, &roles),
            Some(RoleId::new("design-reviewer"))
        );
    }

    #[test]
    fn assignment_beats_review_kind() {
        let mut roles = roles();
        role_mut(&mut roles, "frontend").reviewed_by = Some(RoleId::new("design-reviewer"));
        let frontend = RoleId::new("frontend");
        let task = ReviewSubject {
            review: ReviewKind::Code,
            built_by: std::slice::from_ref(&frontend),
        };
        assert_eq!(
            accountable_reviewer(&task, &frontend, &roles),
            Some(RoleId::new("design-reviewer"))
        );
    }

    #[test]
    fn a_lead_reviews_unless_it_built_the_head() {
        let mut roles = roles();
        role_mut(&mut roles, "architect").lead = true;
        role_mut(&mut roles, "backend").reports_to = Some(RoleId::new("architect"));
        role_mut(&mut roles, "backend").reviewed_by = Some(RoleId::new("design-reviewer"));
        let backend = RoleId::new("backend");
        let architect = RoleId::new("architect");

        let built = [backend.clone()];
        let task = ReviewSubject {
            review: ReviewKind::Code,
            built_by: &built,
        };
        assert_eq!(
            accountable_reviewer(&task, &backend, &roles),
            Some(architect.clone())
        );

        let built = [backend.clone(), architect];
        let task = ReviewSubject {
            review: ReviewKind::Code,
            built_by: &built,
        };
        assert_eq!(
            accountable_reviewer(&task, &backend, &roles),
            Some(RoleId::new("design-reviewer"))
        );
    }

    #[test]
    fn a_manager_that_is_not_a_lead_does_not_review() {
        let roles = roles();
        let backend = RoleId::new("backend");
        let task = ReviewSubject {
            review: ReviewKind::Code,
            built_by: std::slice::from_ref(&backend),
        };
        assert_ne!(
            accountable_reviewer(&task, &backend, &roles),
            Some(RoleId::new("orchestrator"))
        );
    }

    #[test]
    fn removed_reviewer_role_gives_none() {
        let roles: Vec<RoleRecord> = roles()
            .into_iter()
            .filter(|r| r.slug != CODE_REVIEWER)
            .collect();
        let backend = RoleId::new("backend");
        let task = ReviewSubject {
            review: ReviewKind::Code,
            built_by: std::slice::from_ref(&backend),
        };
        assert_eq!(accountable_reviewer(&task, &backend, &roles), None);
    }
}
