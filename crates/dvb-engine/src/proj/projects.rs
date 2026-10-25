use super::Projections;
use dvb_core::api::{FinishedWork, GoalState, ProjectView};
use dvb_core::roles::{HireRecord, HireStatus, RoleRecord};
use dvb_core::{Event, EventId, EventKind, GoalId, ProjectId, QuestionId, RoleId, Timestamp};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Project {
    pub view: ProjectView,
    pub roles: Vec<RoleRecord>,
    pub hires: Vec<HireRecord>,
    /// The owner's current brief; empty when none is set.
    pub brief: String,
    /// GitHub allowed no ruleset on its default branch (`project.ruleset_missing`).
    pub ruleset_missing: bool,
    pub(crate) orchestrator_start: EventId,
    pub(crate) orchestrator_failures: (u8, Option<Timestamp>),
    pub(crate) answers: BTreeMap<QuestionId, EventId>,
    pub(crate) approvals: BTreeMap<GoalId, EventId>,
    pub(crate) last_done: Option<GoalId>,
}

impl Project {
    pub fn role_by_slug(&self, slug: &str) -> Option<&RoleRecord> {
        self.roles.iter().find(|r| r.slug == slug)
    }

    pub fn role(&self, id: &RoleId) -> Option<&RoleRecord> {
        self.roles.iter().find(|r| &r.id == id)
    }

    /// The role's default hire: its first active hire that is not routine
    /// (else its routine one), or the same on `model`.
    pub fn hire(&self, role: &RoleId, model: Option<&str>) -> Option<&HireRecord> {
        self.hire_for(role, model, false)
    }

    /// As `hire`, but a routine hire first when `routine` asks for one.
    pub fn hire_for(
        &self,
        role: &RoleId,
        model: Option<&str>,
        routine: bool,
    ) -> Option<&HireRecord> {
        self.hires
            .iter()
            .filter(|h| {
                &h.role == role
                    && h.status == HireStatus::Active
                    && model.is_none_or(|m| h.model == m)
            })
            .min_by_key(|h| h.routine != routine)
    }

    /// The role's active routine hire, if it has one.
    pub fn routine_hire(&self, role: &RoleId) -> Option<&HireRecord> {
        self.hire_for(role, None, true).filter(|h| h.routine)
    }
}

impl Projections {
    /// A removed project leaves every view with its goals, tasks, runs and
    /// questions, so nothing schedules, counts or shows them again.
    fn remove_project(&mut self, id: &ProjectId) {
        self.projects.remove(id);
        self.goals.retain(|_, g| &g.project != id);
        self.tasks.retain(|_, t| &t.view.project != id);
        self.runs.retain(|_, r| &r.project != id);
        self.questions.retain(|_, q| &q.view.project != id);
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_projects(
        &mut self,
        e: &Event,
        final_goals: &[(GoalId, GoalState)],
        goal_states: &[(GoalId, GoalState)],
        completing_goal: Option<&GoalId>,
    ) {
        let project = e.project.clone();
        match &e.kind {
            EventKind::ProjectAdded {
                id,
                name,
                repo,
                default_branch,
                ci,
            } => {
                self.projects.insert(
                    id.clone(),
                    Project {
                        view: ProjectView {
                            id: id.clone(),
                            name: name.clone(),
                            repo: repo.clone(),
                            default_branch: default_branch.clone(),
                            ci: ci.clone(),
                            paused: false,
                            last_work_at: None,
                            done_goals: 0,
                            last_finished: None,
                            issues_found: None,
                            publication_mismatch: None,
                        },
                        roles: Vec::new(),
                        hires: Vec::new(),
                        brief: String::new(),
                        ruleset_missing: false,
                        orchestrator_start: EventId(0),
                        orchestrator_failures: (0, None),
                        answers: BTreeMap::new(),
                        approvals: BTreeMap::new(),
                        last_done: None,
                    },
                );
            }
            EventKind::ProjectPaused {} | EventKind::ProjectResumed {} => {
                let paused = matches!(e.kind, EventKind::ProjectPaused {});
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.view.paused = paused;
                }
            }
            EventKind::ProjectBriefSet { brief } => {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.brief.clone_from(brief);
                }
            }
            EventKind::ProjectRulesetMissing {} | EventKind::ProjectRulesetInstalled {} => {
                let missing = matches!(e.kind, EventKind::ProjectRulesetMissing {});
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.ruleset_missing = missing;
                }
            }
            EventKind::ProjectRepoChanged { to, .. } => {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.view.repo.clone_from(to);
                }
            }
            EventKind::ProjectRemoved { .. } => {
                if let Some(id) = project {
                    self.remove_project(&id);
                }
            }
            EventKind::ProjectIssuesFound(counts) => {
                if let Some(p) = project.and_then(|id| self.projects.get_mut(&id)) {
                    p.view.issues_found = Some(counts.clone());
                }
            }
            _ => {}
        }
        // Work facts belong to the replayable projection, never the cockpit feed.
        if let Some(id) = &e.project {
            let work = e.kind.is_work();
            let done: Vec<_> = goal_states
                .iter()
                .filter(|(_, state)| *state == GoalState::Done)
                .filter_map(|(goal, _)| self.goals.get(goal))
                .collect();
            let finished_goal = done.iter().find(|g| {
                completing_goal == Some(&g.id)
                    || !final_goals.contains(&(g.id.clone(), GoalState::Done))
            });
            let finished = if let Some(g) = finished_goal {
                Some(FinishedWork {
                    text: g.text.clone(),
                    pr: None,
                    at: e.at,
                })
            } else if let EventKind::PrMerged { task, pr, .. } = &e.kind {
                self.tasks.get(task).map(|t| FinishedWork {
                    text: t.view.title.clone(),
                    pr: Some(*pr),
                    at: e.at,
                })
            } else {
                None
            };
            if let Some(p) = self.projects.get_mut(id) {
                if work {
                    p.view.last_work_at = Some(e.at);
                }
                p.view.done_goals = u32::try_from(done.len()).unwrap_or(u32::MAX);
                if let Some(finished) = finished {
                    p.view.last_finished = Some(finished);
                }
            }
        }
    }
}
