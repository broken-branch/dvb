//! In-memory views folded from durable events and rebuilt at startup.

use dvb_core::{Actor, Event, EventId, EventKind, GoalId, ProjectId, QuestionId, RunId, TaskId};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Projections {
    pub projects: BTreeMap<ProjectId, Project>,
    pub goals: BTreeMap<GoalId, Goal>,
    pub tasks: BTreeMap<TaskId, Task>,
    pub runs: BTreeMap<RunId, Run>,
    pub questions: BTreeMap<QuestionId, Question>,
    /// One durable install-wide switch, keyed by its start event.
    pub only: Option<(EventId, dvb_core::model::OnlyModels)>,
    /// Admission settings, folded for recovery and new goal selection.
    pub settings: dvb_core::api::Settings,
    pub(crate) goal_versions: BTreeMap<EventId, GoalId>,
    pub(crate) dropped_goals: std::collections::BTreeSet<GoalId>,
    pub(crate) failed_runs: std::collections::BTreeSet<RunId>,
    /// Historical engine failure questions immediately follow their task problem.
    previous_problem: Option<(EventId, ProjectId, TaskId)>,
    /// Resource notices and current holds, including taskless pre-launch waits.
    /// Polling reads these folds without reading retained output.
    resource_notices: BTreeMap<RunId, ResourceNotices>,
}

impl Default for Projections {
    fn default() -> Self {
        Self {
            settings: dvb_core::api::Settings::defaults(),
            projects: BTreeMap::new(),
            goals: BTreeMap::new(),
            tasks: BTreeMap::new(),
            runs: BTreeMap::new(),
            questions: BTreeMap::new(),
            only: None,
            goal_versions: BTreeMap::new(),
            dropped_goals: Default::default(),
            failed_runs: Default::default(),
            previous_problem: None,
            resource_notices: BTreeMap::new(),
        }
    }
}

mod goals;
mod projects;
mod questions;
mod roles;
mod runs;
mod settings;
mod tasks;

pub use dvb_core::event::QuestionKind;
pub use goals::Goal;
pub use projects::Project;
pub use questions::Question;
pub(crate) use questions::{DROP_GOAL, PLAN_AGAIN};
use runs::ResourceNotices;
pub use runs::Run;
use tasks::task_final;
pub use tasks::{Reviewed, Task, Waiting};

#[cfg(test)]
mod tests;

impl Projections {
    pub fn goal_view(&self, g: &Goal) -> dvb_core::api::GoalView {
        dvb_core::api::GoalView {
            waits_for: self.waits_for(g).cloned(),
            id: g.id.clone(),
            project: g.project.clone(),
            text: g.text.clone(),
            state: self.goal_state(g),
            plan_parked: self.plan_parked(g).is_some(),
        }
    }

    pub fn rebuild(events: &[Event]) -> Self {
        let mut p = Self::default();
        for e in events {
            p.apply(e);
        }
        p
    }

    pub fn apply(&mut self, e: &Event) {
        let previous_problem = self
            .previous_problem
            .take()
            .filter(|(id, project, _)| id.0 + 1 == e.id.0 && e.project.as_ref() == Some(project));
        if let EventKind::TaskFailed { task, .. } | EventKind::TaskStopped { task, .. } = &e.kind
            && let Some(project) = &e.project
        {
            self.previous_problem = Some((e.id, project.clone(), task.clone()));
        }
        // Output and usage cannot change goal completion, assignments or problems.
        // Keep this hot replay/live path independent of retained task history.
        if matches!(
            e.kind,
            EventKind::RunOutput { .. } | EventKind::RunUsage { .. }
        ) {
            if e.actor == Actor::Engine
                && let EventKind::RunOutput {
                    run,
                    item: dvb_core::event::OutputItem::Note(text),
                    resource,
                } = &e.kind
            {
                self.resource_note(run, *resource, text);
            }
            if let Some(p) = e.project.as_ref().and_then(|id| self.projects.get_mut(id)) {
                p.view.last_work_at = Some(e.at);
            }
            return;
        }
        let project = e.project.clone();
        let completing_goal = if let EventKind::GoalDone { goal } = &e.kind {
            self.goals
                .get(goal)
                .filter(|g| g.finished.is_none())
                .map(|g| g.id.clone())
        } else {
            None
        };

        // Goal Failed is derived from tasks, so capture both explicit and
        // task-derived final transitions, without treating repeated observations
        // of the same final state as a new release (Failed -> Dropped is new).
        let final_goals: Vec<_> = self
            .goal_states(project.as_ref())
            .into_iter()
            .filter(|(_, state)| goals::goal_final(*state))
            .collect();
        let task_final = if let EventKind::TaskStateChanged { task, to, .. } = &e.kind {
            task_final(to)
                && self
                    .tasks
                    .get(task)
                    .is_some_and(|t| !task_final(&t.view.state))
        } else {
            false
        };
        self.fold_runs(e);
        // A historical plan without a project did not complete the fold.
        if matches!(e.kind, EventKind::PlanProposed { .. }) && project.is_none() {
            return;
        }
        self.fold_tasks(e);
        let goal_states = self.fold_goals(e, &final_goals, task_final);
        self.fold_questions(e, previous_problem.as_ref());
        self.fold_roles(e);
        self.fold_settings(e);
        self.fold_projects(e, &final_goals, &goal_states, completing_goal.as_ref());
        self.refresh_task_results();
        let stale: Vec<_> = self
            .questions
            .values()
            .filter_map(|q| {
                if let QuestionKind::Completion { task, run, result } = &q.kind {
                    let current = self
                        .tasks
                        .get(task)
                        .and_then(|t| crate::dispatch::completion_result(self, t));
                    if current != Some((run.clone(), *result)) {
                        return Some(q.view.id.clone());
                    }
                }
                None
            })
            .collect();
        for id in stale {
            if let Some(q) = self.questions.get_mut(&id) {
                q.view.open = false;
            }
        }
    }
}

pub(crate) mod hires;
