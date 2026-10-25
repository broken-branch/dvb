use super::{Projections, Task};
use crate::wake::Wake;
use dvb_core::api::{GoalState, TaskView};
use dvb_core::event::LapsesOn;
use dvb_core::{
    Actor, Event, EventId, EventKind, GoalId, ProjectId, QuestionId, RunId, TaskId, TaskState,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Goal {
    pub after: Option<GoalId>,
    pub id: GoalId,
    pub project: ProjectId,
    pub text: String,
    pub state: GoalState,
    /// The first linked issue of historical statements; restatements keep it.
    pub from_issue: Option<u64>,
    pub closed_issues: Vec<u64>,
    /// The trusted issue content the goal carries into prompts.
    pub attached: Vec<dvb_core::event::GoalIssue>,
    /// The id of the event that last set the goal's text.
    pub version: EventId,
    /// The id of the event that first stated the goal: goals are handled in this order.
    pub stated: EventId,
    pub tasks: Vec<TaskId>,
    /// The id of the last `plan.proposed` for it.
    pub planned: Option<EventId>,
    /// The id of the event that completed the goal.
    pub finished: Option<EventId>,
    /// Latest orchestrator or pre-launch fetch hold attempt for this version.
    /// A release admits one successor.
    pub planning_run: Option<RunId>,
    pub planning_held: bool,
    /// The owner's request for a proposal from the open issues
    /// (`proposal.requested`), not a goal. It is Done from the start, so the
    /// orchestrator proposes the next goal as it does after a goal is done; it
    /// has no tasks and is left out of goal views and counts.
    pub proposal: bool,
}

impl Goal {
    /// Issues the engine closes when the goal is Done, in the order linked.
    pub fn issues(&self) -> Vec<u64> {
        let mut issues: Vec<u64> = self.from_issue.into_iter().collect();
        for issue in &self.attached {
            if !issues.contains(&issue.number) {
                issues.push(issue.number);
            }
        }
        issues
    }

    /// Issues still to close once the goal is Done.
    pub fn open_issues(&self) -> Vec<u64> {
        let mut issues = self.issues();
        issues.retain(|i| !self.closed_issues.contains(i));
        issues
    }
}

/// What the orchestrator reads as the goal of a run answering a proposal request.
pub const PROPOSAL_TEXT: &str =
    "The owner asked for the next goal to be proposed from the project's open issues.";

pub(super) fn goal_final(state: GoalState) -> bool {
    matches!(
        state,
        GoalState::Done | GoalState::Failed | GoalState::Dropped
    )
}

impl Projections {
    /// The project's newest goal, in any state.
    pub fn latest_goal(&self, project: &ProjectId) -> Option<&Goal> {
        self.goals
            .values()
            .filter(|g| &g.project == project && !g.proposal)
            .max_by_key(|g| g.version)
    }

    /// A goal of the project that is Stated, approved or in progress: its own
    /// runs carry on, so finishing another goal asks nothing about what is next.
    pub fn goal_under_way(&self, project: &ProjectId) -> Option<&Goal> {
        self.goals_in_order(project).into_iter().find(|g| {
            matches!(
                self.goal_state(g),
                GoalState::Stated | GoalState::Approved | GoalState::InProgress
            )
        })
    }

    /// The project's goals, in the order they were stated.
    pub fn goals_in_order(&self, project: &ProjectId) -> Vec<&Goal> {
        let mut goals: Vec<&Goal> = self
            .goals
            .values()
            .filter(|g| &g.project == project)
            .collect();
        goals.sort_by_key(|g| g.stated);
        goals
    }

    /// The state the owner sees: an approved goal follows its tasks.
    pub fn goal_state(&self, g: &Goal) -> GoalState {
        if g.state != GoalState::Approved {
            return g.state;
        }
        let states: Vec<&TaskState> = g
            .tasks
            .iter()
            .filter_map(|t| self.tasks.get(t))
            .map(|t| &t.view.state)
            .filter(|s| **s != TaskState::Cancelled)
            .collect();
        let ended = |s: &&TaskState| {
            matches!(
                s,
                TaskState::Done | TaskState::Failed { .. } | TaskState::Stopped { .. }
            )
        };
        if states.is_empty() {
            g.state
        } else if states.iter().all(|s| **s == TaskState::Done) {
            GoalState::Done
        } else if dvb_core::state::goal_failed(true, states.iter().copied()) {
            GoalState::Failed
        } else if states
            .iter()
            .any(|s| !matches!(s, TaskState::Planned) && !ended(s))
        {
            GoalState::InProgress
        } else {
            g.state
        }
    }

    /// A stated goal whose current version has no plan yet.
    pub fn needs_plan(g: &Goal) -> bool {
        g.state == GoalState::Stated && !g.planned.is_some_and(|p| p > g.version)
    }

    /// A stated goal whose current version has a saved plan waiting for the owner's yes.
    pub(crate) fn plan_unapproved(&self, g: &Goal) -> bool {
        g.state == GoalState::Stated && !Self::needs_plan(g) && !self.plan_tasks(g).is_empty()
    }

    /// The question that parked the goal's saved plan: the latest answered
    /// question about the current plan, while no question of the goal is open.
    /// Resuming asks it again; a newer plan or answer unparks it.
    pub(crate) fn plan_parked(&self, g: &Goal) -> Option<&super::Question> {
        if !self.plan_unapproved(g) || self.goal_question_open(g) {
            return None;
        }
        self.questions
            .values()
            .filter(|q| {
                q.lapses_on == LapsesOn::GoalVersion(g.version)
                    && g.planned.is_some_and(|p| q.opened_at > p)
                    && q.plan_question
            })
            .filter(|q| q.answered_at.is_some())
            .max_by_key(|q| q.answered_at)
            .filter(|q| q.parks_plan())
    }

    /// Any question still open for the goal's current version.
    pub(crate) fn goal_question_open(&self, g: &Goal) -> bool {
        self.questions
            .values()
            .any(|q| q.view.open && q.lapses_on == LapsesOn::GoalVersion(g.version))
    }

    /// Dependencies use the same final-state projection as owner status and recovery.
    pub fn waits_for<'g>(&self, g: &'g Goal) -> Option<&'g GoalId> {
        if matches!(g.state, GoalState::Done | GoalState::Dropped) {
            return None;
        }
        g.after.as_ref().filter(|id| {
            self.goals.get(*id).is_none_or(|predecessor| {
                !matches!(
                    self.goal_state(predecessor),
                    GoalState::Done | GoalState::Dropped
                )
            })
        })
    }

    /// Saved plans and questions can predate an owner change to the goal order.
    pub(crate) fn check_goal_order(&self, g: &Goal) -> Result<(), dvb_core::Refusal> {
        if let Some(after) = self.waits_for(g) {
            return Err(dvb_core::Refusal::new(
                dvb_core::RefusalCode::ValueChanged,
                format!(
                    "goal {} waits for goal {after}; wait until it is Done or Dropped",
                    g.id
                ),
            ));
        }
        Ok(())
    }

    /// An empty, naturally completed attempt or a terminal fetch holds planning.
    /// The fold consumes each owner/final-state release on successor admission.
    pub fn plan_wake_ready(&self, g: &Goal) -> bool {
        Self::needs_plan(g)
            && self.waits_for(g).is_none()
            && !g.planning_held
            && !self.orchestrator_question_open(g)
    }

    pub(crate) fn orchestrator_question_open(&self, g: &Goal) -> bool {
        self.questions.values().any(|q| {
            q.orchestrator && q.view.open && q.lapses_on == LapsesOn::GoalVersion(g.version)
        })
    }

    /// A reply can release the attempt already in flight when it arrived, but
    /// cannot release another empty attempt that already received that reply.
    pub(crate) fn answer_releases_hold(&self, g: &Goal) -> bool {
        let started = g
            .planning_run
            .as_ref()
            .and_then(|run| self.runs.get(run))
            .map(|r| r.started)
            .or_else(|| {
                Self::planning_question(g)
                    .and_then(|id| self.questions.get(&id))
                    .map(|q| q.opened_at)
            })
            .unwrap_or(EventId(0));
        self.questions.values().any(|q| {
            (q.orchestrator || q.plan_question)
                && q.answered_at.is_some_and(|at| at > started)
                && q.lapses_on == LapsesOn::GoalVersion(g.version)
        })
    }

    pub(crate) fn planning_question(g: &Goal) -> Option<QuestionId> {
        g.planning_run
            .as_ref()
            .map(|run| QuestionId::new(format!("planning-{}-{run}", g.version)))
    }

    pub fn goal_needs_orchestrator(&self, g: &Goal, pool: &dvb_core::ModelPool) -> bool {
        g.state == GoalState::Approved
            && g.tasks.iter().filter_map(|t| self.tasks.get(t)).any(|t| {
                t.approved
                    && t.facts.run.is_none()
                    && t.view.state == TaskState::Planned
                    && t.waiting
                        .as_ref()
                        .is_none_or(|w| w.approved_version.is_none_or(|v| v == g.version))
                    && self.needs_orchestrator(t, pool)
            })
    }

    /// The goal an orchestrator run woken for `wake` works on, decided once when
    /// the run is admitted and persisted in its assignment. A stated goal
    /// with no plan is planned first (oldest first); an approval starts the
    /// oldest approved plan needing a choice or hire; an answer stays on the
    /// answered question's goal; a completed goal is the one just done. A
    /// current approval or goal-bound answer wake stays idle if its goal is
    /// ineligible; other wakes fall back to the oldest goal not done, else
    /// the newest.
    pub fn goal_for_wake(
        &self,
        project: &ProjectId,
        wake: Option<Wake>,
        pool: &dvb_core::ModelPool,
    ) -> Option<&Goal> {
        self.select_goal_for_wake(project, wake, false, pool)
    }

    /// Historical starts predate persisted bindings. Their approval wake used
    /// the oldest approved plan with a Planned, approved task and no waiting
    /// request, regardless of hires, model mode, previous runs or questions.
    /// Keep this attribution independent of current admission rules.
    pub(super) fn select_goal_for_wake(
        &self,
        project: &ProjectId,
        wake: Option<Wake>,
        legacy: bool,
        pool: &dvb_core::ModelPool,
    ) -> Option<&Goal> {
        let goals = self.goals_in_order(project);
        let first = |f: &dyn Fn(&&Goal) -> bool| {
            goals.iter().copied().find(|g| {
                (legacy || self.waits_for(g).is_none())
                    && (legacy
                        || wake != Some(Wake::Answer)
                        || !g.planning_held
                        || self.answer_releases_hold(g))
                    && f(g)
            })
        };
        match wake {
            Some(Wake::Approved) if legacy => first(&|g| {
                g.state == GoalState::Approved
                    && g.tasks.iter().filter_map(|t| self.tasks.get(t)).any(|t| {
                        t.approved && t.view.state == TaskState::Planned && t.waiting.is_none()
                    })
            }),
            Some(Wake::Approved) => {
                // A delayed approval wake must stay idle once its work is
                // waiting for the owner or has already been admitted. The
                // historical branch above still retains its replay fallback.
                return first(&|g| {
                    self.goal_needs_orchestrator(g, pool) && !self.goal_question_open(g)
                })
                .or_else(|| {
                    first(&|g| self.has_waiting_tasks(g, pool) && !self.goal_question_open(g))
                });
            }
            Some(Wake::Answer) => {
                let answered = self
                    .questions
                    .values()
                    .filter(|q| &q.view.project == project && q.planning_run.is_none())
                    // A no-effect or parking answer wakes nothing, so it chooses
                    // no goal; historical starts were woken by parking answers.
                    .filter(|q| !q.no_effect() && (legacy || !q.parks_plan()))
                    .filter_map(|q| match q.lapses_on {
                        LapsesOn::GoalVersion(v) => q.answered_at.map(|at| (at, v)),
                        LapsesOn::ValueVersion(_) => None,
                    })
                    .filter(|(_, v)| {
                        goals
                            .iter()
                            .any(|g| g.version == *v && g.state != GoalState::Dropped)
                    })
                    .max();
                let goal = answered.and_then(|(_, v)| first(&|g| g.version == v));
                // Admission must not consume a goal's reply in another goal's
                // prompt while the answered goal is held or ordered to wait.
                if !legacy && answered.is_some() {
                    return goal;
                }
                goal
            }
            Some(Wake::GoalDone) => goals
                .iter()
                .copied()
                .filter(|g| g.finished.is_some())
                .max_by_key(|g| g.finished),
            _ => first(&|g| Self::needs_plan(g)),
        }
        .or_else(|| first(&|g| Self::needs_plan(g)))
        .or_else(|| first(&|g| !matches!(g.state, GoalState::Done | GoalState::Dropped)))
        .or_else(|| {
            goals.iter().copied().rfind(|g| {
                g.state != GoalState::Dropped
                    && (legacy || self.waits_for(g).is_none())
                    && (legacy
                        || wake != Some(Wake::Answer)
                        || !g.planning_held
                        || self.answer_releases_hold(g))
            })
        })
    }

    pub(super) fn goal_states(&self, project: Option<&ProjectId>) -> Vec<(GoalId, GoalState)> {
        self.goals
            .values()
            .filter(|g| Some(&g.project) == project && !g.proposal)
            .map(|g| (g.id.clone(), self.goal_state(g)))
            .collect()
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_goals(
        &mut self,
        e: &Event,
        final_goals: &[(GoalId, GoalState)],
        task_final: bool,
    ) -> Vec<(GoalId, GoalState)> {
        let project = e.project.clone();
        match &e.kind {
            EventKind::GoalStated {
                goal,
                text,
                from_issue,
                issues,
                after,
            } => {
                self.goal_versions.insert(e.id, goal.clone());
                if let Some(p) = project.as_ref().and_then(|id| self.projects.get_mut(id)) {
                    p.orchestrator_failures = (0, None);
                }
                // A goal stated again ("change course") is a new version of it;
                // its tasks stay, the old plan's unstarted ones cancelled.
                if let Some(g) = self.goals.get_mut(goal) {
                    g.text.clone_from(text);
                    g.after.clone_from(after);
                    g.from_issue = g.from_issue.or(*from_issue);
                    for issue in issues {
                        if !g.attached.iter().any(|a| a.number == issue.number) {
                            g.attached.push(issue.clone());
                        }
                    }
                    g.state = GoalState::Stated;
                    g.version = e.id;
                    g.planning_run = None;
                    g.planning_held = false;
                } else if let Some(project) = project.clone() {
                    self.goals.insert(
                        goal.clone(),
                        Goal {
                            after: after.clone(),
                            id: goal.clone(),
                            project,
                            text: text.clone(),
                            state: GoalState::Stated,
                            from_issue: *from_issue,
                            closed_issues: Vec::new(),
                            attached: issues.clone(),
                            version: e.id,
                            stated: e.id,
                            tasks: Vec::new(),
                            planned: None,
                            finished: None,
                            planning_run: None,
                            planning_held: false,
                            proposal: false,
                        },
                    );
                }
                // A next goal proposed by a run belongs to that run too; other
                // runs remain bound to the older goal and are refused as stale.
                if let Actor::Run(run) = &e.actor
                    && let Some(r) = self.runs.get_mut(run)
                    && Some(&r.project) == project.as_ref()
                    && r.assignment.task.is_none()
                {
                    r.goal_version = Some(e.id);
                }
                if let Some(project) = &project {
                    self.close_lapsed_questions(project);
                }
            }
            EventKind::PlanProposed { goal, tasks } => {
                let Some(project) = project else {
                    return Vec::new();
                };
                for t in tasks {
                    self.tasks.insert(
                        t.task.clone(),
                        Task {
                            summary: t.summary.clone(),
                            facts: {
                                let mut facts = dvb_core::state::TaskFacts::default();
                                facts.review_since = e.id;
                                facts
                            },
                            problem_at: None,
                            held: false,
                            merged_at: None,
                            view: TaskView {
                                after: t.after.clone(),
                                waiting_for: vec![],
                                outcome: Default::default(),
                                problem_superseded: false,
                                current_run: None,
                                id: t.task.clone(),
                                project: project.clone(),
                                goal: goal.clone(),
                                title: t.title.clone(),
                                role: t.role.clone(),
                                review: t.review,
                                state: TaskState::Planned,
                                reason: None,
                                pr: None,
                                head: None,
                                routine: t.routine,
                                round_model: None,
                            },
                            publication_pending: false,
                            branch: None,
                            recorded_verdicts: BTreeMap::new(),
                            retry_at: None,
                            retry_limit: None,
                            checks: None,
                            checks_waiting_for_slot: false,
                            pending_since: BTreeMap::new(),
                            stuck_checks: vec![],
                            ci_question: None,
                            ci_retry_runs: vec![],
                            updated_base: None,
                            base: None,
                            code_reviewed: Default::default(),
                            design_reviewed: Default::default(),
                            repairs: 0,
                            merged: None,
                            approved: self
                                .goals
                                .get(goal)
                                .is_some_and(|g| g.state == GoalState::Approved),
                            failed: None,
                            retried_at: None,
                            waiting: None,
                            routine_sent_back: 0,
                            sent_back: false,
                            scan_hit: false,
                        },
                    );
                }
                if let Some(g) = self.goals.get_mut(goal) {
                    g.tasks.extend(tasks.iter().map(|t| t.task.clone()));
                    g.planned = Some(e.id);
                }
            }
            EventKind::GoalIssueClosed { goal, issue } => {
                if let Some(g) = self.goals.get_mut(goal) {
                    g.closed_issues.push(*issue);
                }
            }
            EventKind::GoalApproved { goal } | EventKind::GoalDone { goal } => {
                let approved = matches!(e.kind, EventKind::GoalApproved { .. });
                if approved
                    && let Some(p) = project.as_ref().and_then(|id| self.projects.get_mut(id))
                {
                    p.approvals.insert(goal.clone(), e.id);
                }
                if !approved
                    && let Some(p) = project.as_ref().and_then(|id| self.projects.get_mut(id))
                {
                    p.last_done = Some(goal.clone());
                }
                if let Some(g) = self.goals.get_mut(goal) {
                    g.state = if approved {
                        GoalState::Approved
                    } else {
                        GoalState::Done
                    };
                    if !approved {
                        g.finished = Some(e.id);
                        g.planning_held = false;
                    }
                    // The yes covers the plan it was given for; a later plan
                    // (after "change course") waits for its own.
                    if approved {
                        for t in &g.tasks {
                            if let Some(t) = self.tasks.get_mut(t) {
                                t.approved = true;
                            }
                        }
                    }
                }
            }
            EventKind::ProposalRequested {} => {
                if let Some(project) = project.clone() {
                    let id = GoalId::new(format!("proposal-{}", e.id.0));
                    self.goal_versions.insert(e.id, id.clone());
                    self.goals.insert(
                        id.clone(),
                        Goal {
                            after: None,
                            id,
                            project,
                            text: PROPOSAL_TEXT.into(),
                            state: GoalState::Done,
                            from_issue: None,
                            closed_issues: Vec::new(),
                            attached: Vec::new(),
                            version: e.id,
                            stated: e.id,
                            tasks: Vec::new(),
                            planned: None,
                            finished: Some(e.id),
                            planning_run: None,
                            planning_held: false,
                            proposal: true,
                        },
                    );
                }
            }
            EventKind::GoalAfterSet { goal, after } => {
                if let Some(g) = self.goals.get_mut(goal) {
                    g.after.clone_from(after);
                }
            }
            EventKind::GoalDropped { goal } => {
                self.dropped_goals.insert(goal.clone());
                if let Some(g) = self.goals.get_mut(goal) {
                    g.state = GoalState::Dropped;
                    g.planning_held = false;
                    let version = g.version;
                    // Its open questions leave Needs you.
                    for q in self.questions.values_mut() {
                        if q.lapses_on == LapsesOn::GoalVersion(version) {
                            q.view.open = false;
                        }
                    }
                }
                for t in self.tasks.values_mut().filter(|t| &t.view.goal == goal) {
                    t.cancel_dropped_failure();
                }
            }
            _ => {}
        }
        let goal_states = self.goal_states(e.project.as_ref());
        let newly_final: Vec<GoalId> = goal_states
            .iter()
            .filter(|(id, state)| {
                goal_final(*state) && !final_goals.contains(&(id.clone(), *state))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for g in self
            .goals
            .values_mut()
            .filter(|g| Some(&g.project) == e.project.as_ref())
        {
            if task_final || newly_final.iter().any(|id| id != &g.id) {
                g.planning_held = false;
            }
        }
        goal_states
    }
}

impl Projections {
    /// The same approval snapshot serves run asks and recovered plan questions.
    pub(crate) fn plan_tasks(&self, goal: &Goal) -> Vec<dvb_core::event::PlannedTask> {
        goal.tasks
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .filter(|t| t.view.state == TaskState::Planned)
            .map(|t| dvb_core::event::PlannedTask {
                after: t.view.after.clone(),
                task: t.view.id.clone(),
                title: t.view.title.clone(),
                role: t.view.role.clone(),
                review: t.view.review,
                summary: t.summary.clone(),
                routine: t.view.routine,
            })
            .collect()
    }

    pub(crate) fn plan_wording(&self, goal: &Goal) -> String {
        let tasks = self.plan_tasks(goal);
        let mut text = String::from("\n\nPlan:\n");
        for (index, task) in tasks.iter().enumerate() {
            let role = self
                .projects
                .get(&goal.project)
                .and_then(|p| p.role(&task.role))
                .map_or(task.role.as_str(), |r| r.slug.as_str());
            let routine = if task.routine { " (routine)" } else { "" };
            text.push_str(&format!(
                "{}. {} — {}{routine}\n",
                index + 1,
                role,
                task.title
            ));
            for dep in &task.after {
                let predecessor = tasks
                    .iter()
                    .position(|t| t.task == *dep)
                    .map_or_else(|| dep.to_string(), |i| (i + 1).to_string());
                text.push_str(&format!(
                    "{} starts after {} merges\n",
                    index + 1,
                    predecessor
                ));
            }
        }
        text
    }
}
