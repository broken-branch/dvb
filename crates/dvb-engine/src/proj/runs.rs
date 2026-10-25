use super::Projections;
use crate::wake::Wake;
use dvb_core::api::{GoalState, RunView};
use dvb_core::event::{EndReason, LapsesOn, PreflightCode, ResourceWait};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Event, EventId, EventKind, ProjectId, RunAssignment, RunId, TaskId, TaskState, Timestamp,
};

#[derive(Debug, Default)]
pub(super) struct ResourceNotices {
    given: std::collections::BTreeSet<ResourceWait>,
    waiting: Option<ResourceWait>,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub project: ProjectId,
    pub assignment: RunAssignment,
    pub started: EventId,
    pub wake: Option<Wake>,
    pub started_at: Timestamp,
    pub ended: Option<EndReason>,
    /// The crashed run's exit code and cause (`run.ended`'s `detail`).
    pub ended_detail: Option<String>,
    /// For an orchestrator run: the goal version it started for, or the next
    /// goal it proposed. Its plan and question are for that version only.
    pub goal_version: Option<EventId>,
    /// Original completion delivery binding, before the run can propose another goal.
    pub(crate) delivered_goal_version: Option<EventId>,
    pub preflight_failed: Option<PreflightCode>,
    /// It reported its preflight, its first call: past it, it may not again.
    pub preflighted: bool,
    pub collected: Option<String>,
    pub summary: Option<String>,
    pub outcome: dvb_core::run_verbs::BuilderOutcome,
    pub outcome_at: Option<EventId>,
    /// Whether the run submitted a verdict.
    pub verdict_given: bool,
    /// Saved plan/question for the bound version (decisions/notes are telemetry).
    pub planning_work: bool,
    /// Startup has not confirmed this live attempt survived. Historical
    /// startup Completed ends without that confirmation remain interrupted.
    pub interrupted: bool,
    /// Durable explicit stop intent can race the watcher or span a restart.
    pub stop_requested: bool,
    /// When its unit was about to start (`run.launched`); absent before
    /// launch and in runs launched before the event existed.
    pub launched_at: Option<Timestamp>,
    /// The time of its newest event of its own (output, verdict, reports).
    pub active_at: Timestamp,
    /// The engine stops it at its time limit: however its unit ends, it
    /// ends `time_limit`.
    pub time_limited: bool,
}

impl Run {
    /// When its time limit runs out: counted from its launch (from its start
    /// for a run launched before `run.launched` existed).
    pub fn time_limit_at(&self) -> Timestamp {
        let limit = dvb_core::caps::time_limit(self.assignment.kind());
        self.launched_at
            .unwrap_or(self.started_at)
            .plus_millis(i64::try_from(limit.as_millis()).unwrap_or(i64::MAX))
    }

    /// A reviewer whose verdict is recorded and that has been quiet for the
    /// grace period has finished, even if its unit is still up.
    pub fn finished_review(&self, now: Timestamp) -> bool {
        let grace = i64::try_from(dvb_core::caps::REVIEWER_GRACE.as_millis()).unwrap_or(i64::MAX);
        self.verdict_given && self.ended.is_none() && now >= self.active_at.plus_millis(grace)
    }

    /// A builder run works on a task; it is not a reviewer.
    pub fn builds(&self) -> Option<&TaskId> {
        self.assignment
            .task
            .as_ref()
            .filter(|_| self.assignment.reviews.is_none())
    }
}

impl Projections {
    pub fn live_runs(&self) -> impl Iterator<Item = &Run> {
        self.runs.values().filter(|r| r.ended.is_none())
    }

    pub(crate) fn resource_notice_given(&self, run: &RunId, resource: ResourceWait) -> bool {
        self.resource_notices
            .get(run)
            .is_some_and(|notices| notices.given.contains(&resource))
    }

    pub(crate) fn resource_wait(&self, run: &RunId) -> Option<ResourceWait> {
        self.resource_notices
            .get(run)
            .and_then(|notices| notices.waiting)
    }

    /// Engine resource records update only the assigned task, keeping output replay bounded.
    pub(super) fn resource_note(
        &mut self,
        run: &RunId,
        resource: Option<ResourceWait>,
        text: &str,
    ) {
        let Some(resource) = resource.or_else(|| historical_resource(text)) else {
            return;
        };
        let notices = self.resource_notices.entry(run.clone()).or_default();
        let was_waiting = notices.waiting.is_some();
        notices.given.insert(resource);
        notices.waiting = (resource != ResourceWait::Available).then_some(resource);
        if (resource != ResourceWait::Available || was_waiting)
            && let Some(r) = self.runs.get(run).filter(|r| r.ended.is_none())
            && let Some(t) = r
                .assignment
                .task
                .as_ref()
                .and_then(|id| self.tasks.get_mut(id))
            && t.facts.run.as_ref() == Some(run)
            && matches!(t.view.state, TaskState::Preparing | TaskState::InReview)
        {
            t.view.reason = notices
                .waiting
                .map(|resource| dvb_core::wording::resource_words(resource).into())
                .or_else(|| {
                    r.assignment.reviews.as_ref().map(|_| {
                        dvb_core::wording::review_reason(
                            r.assignment.review_stage.unwrap_or(ReviewKind::Code),
                        )
                        .into()
                    })
                });
        }
    }

    fn refresh_current_run(&mut self, id: &RunId) {
        if let Some(r) = self.runs.get(id)
            && let Some(t) = r
                .assignment
                .task
                .as_ref()
                .and_then(|id| self.tasks.get_mut(id))
            && t.facts.run.as_ref() == Some(id)
        {
            if r.assignment.reviews.is_none() {
                t.view.round_model = Some(r.assignment.model.clone());
                t.view.routine |= r.assignment.routine;
            }
            t.view.current_run = Some(RunView {
                outcome: r.outcome.clone(),
                id: r.assignment.run.clone(),
                project: r.project.clone(),
                task: r.assignment.task.clone(),
                role: r.assignment.role.clone(),
                hire: r.assignment.hire.clone(),
                model: r.assignment.model.clone(),
                vendor: r.assignment.vendor,
                effort: r.assignment.effort,
                reason: r.assignment.reason.clone(),
                reviews: r.assignment.reviews.clone(),
                started_at: r.started_at,
                ended: r.ended.clone(),
            });
        }
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_runs(&mut self, e: &Event) {
        let project = e.project.clone();
        if let Actor::Run(run) = &e.actor
            && let Some(r) = self.runs.get_mut(run)
        {
            let saved = if let EventKind::PlanProposed { goal, .. } = &e.kind {
                self.goals
                    .get(goal)
                    .is_some_and(|g| Some(g.version) == r.goal_version)
            } else if let EventKind::QuestionOpened { lapses_on, .. } = &e.kind {
                r.goal_version
                    .is_some_and(|v| *lapses_on == LapsesOn::GoalVersion(v))
            } else {
                false
            };
            r.planning_work |= saved;
            r.active_at = e.at;
        }
        match &e.kind {
            EventKind::RunStarted(a) => {
                let orchestrator = a.is_orchestrator();
                let wake = Wake::from_reason(&a.reason);
                let delivered_goal_version = orchestrator
                    .then(|| {
                        a.goal_version.or_else(|| {
                            (wake == Some(Wake::GoalDone))
                                .then(|| {
                                    self.projects
                                        .get(project.as_ref()?)?
                                        .last_done
                                        .as_ref()
                                        .and_then(|id| self.goals.get(id))
                                        .map(|g| g.version)
                                })
                                .flatten()
                        })
                    })
                    .flatten();
                if orchestrator
                    && let Some(p) = project.as_ref().and_then(|id| self.projects.get_mut(id))
                {
                    p.orchestrator_start = e.id;
                }
                let goal_version = project.as_ref().filter(|_| a.task.is_none()).and_then(|p| {
                    a.goal_version.or_else(|| {
                        self.select_goal_for_wake(p, wake, true, &dvb_core::ModelPool::default())
                            .map(|g| g.version)
                    })
                });
                if a.is_orchestrator()
                    && let Some(g) = self.goals.values_mut().find(|g| {
                        Some(&g.project) == project.as_ref() && Some(g.version) == goal_version
                    })
                {
                    g.planning_run = Some(a.run.clone());
                    g.planning_held = false;
                }
                if let Some(t) = a.task.as_ref().and_then(|t| self.tasks.get_mut(t)) {
                    if a.reviews.is_some()
                        && let Some(stage) = a.review_stage
                    {
                        t.view.reason = Some(dvb_core::wording::review_reason(stage).into());
                    }
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                }
                if let Some(project) = project {
                    self.runs.insert(
                        a.run.clone(),
                        Run {
                            project,
                            assignment: a.clone(),
                            started: e.id,
                            wake,
                            started_at: e.at,
                            ended: None,
                            ended_detail: None,
                            goal_version,
                            delivered_goal_version,
                            preflight_failed: None,
                            preflighted: false,
                            collected: None,
                            summary: None,
                            outcome: Default::default(),
                            outcome_at: None,
                            verdict_given: false,
                            planning_work: false,
                            interrupted: false,
                            stop_requested: false,
                            launched_at: None,
                            active_at: e.at,
                            time_limited: false,
                        },
                    );
                }
            }
            EventKind::RunPreflight { run, checks, .. } => {
                // Historical disk holds had no release note. Preflight proves
                // the unit launched, so replay must not leave its wait visible.
                self.resource_note(run, Some(ResourceWait::Available), "");
                if let Some(r) = self.runs.get_mut(run) {
                    r.preflight_failed = checks.iter().find(|c| !c.ok).map(|c| c.code);
                    r.preflighted = true;
                }
            }
            EventKind::RunCollected {
                run,
                head,
                summary,
                outcome,
            } => {
                if let Some(r) = self.runs.get_mut(run) {
                    if let Some(head) = head {
                        r.collected = Some(head.clone());
                    }
                    if let Some(outcome) = outcome {
                        r.outcome = outcome.clone();
                        r.outcome_at = Some(e.id);
                    }
                    // Automatic collection/recovery supplies only a head.
                    if let Some(summary) = summary {
                        r.summary = Some(summary.clone());
                    }
                }
            }
            EventKind::RunStopRequested { run } => {
                if let Some(r) = self.runs.get_mut(run).filter(|r| r.ended.is_none()) {
                    r.stop_requested = true;
                }
            }
            EventKind::RunLaunched { run } => {
                if let Some(r) = self.runs.get_mut(run) {
                    r.launched_at = Some(e.at);
                }
            }
            EventKind::RunTimeLimitReached { run, .. } => {
                if let Some(r) = self.runs.get_mut(run).filter(|r| r.ended.is_none()) {
                    r.time_limited = true;
                }
            }
            EventKind::RunSurvivedRestart { run } => {
                if let Some(r) = self
                    .runs
                    .get_mut(run)
                    .filter(|r| r.ended.is_none() && !r.stop_requested)
                {
                    r.interrupted = false;
                }
            }
            EventKind::RunEnded {
                run,
                reason,
                detail,
            } => {
                if !matches!(reason, EndReason::Completed | EndReason::Stopped) {
                    self.failed_runs.insert(run.clone());
                }
                if let Some(r) = self
                    .runs
                    .get(run)
                    .filter(|r| r.assignment.is_orchestrator())
                    && let Some(p) = self.projects.get_mut(&r.project)
                {
                    if matches!(reason, EndReason::Completed | EndReason::Stopped) {
                        p.orchestrator_failures = (0, None);
                    } else {
                        p.orchestrator_failures.0 = p.orchestrator_failures.0.saturating_add(1);
                        p.orchestrator_failures.1 = Some(e.at);
                        p.orchestrator_start = self
                            .runs
                            .values()
                            .filter(|other| {
                                other.project == r.project
                                    && other.assignment.is_orchestrator()
                                    && other.assignment.run != *run
                                    && !self.failed_runs.contains(&other.assignment.run)
                            })
                            .map(|other| other.started)
                            .max()
                            .unwrap_or(EventId(0));
                    }
                }
                if let Some(r) = self.runs.get_mut(run) {
                    r.ended = Some(reason.clone());
                    r.ended_detail.clone_from(detail);
                }
                if let Some(r) = self.runs.get(run)
                    && let Some(g) = self.goals.values().find(|g| {
                        g.project == r.project
                            && Some(g.version) == r.goal_version
                            && g.planning_run.as_ref() == Some(run)
                    })
                {
                    // An older unapproved plan is not saved work by this
                    // attempt, including the planner receiving its reply,
                    // but the attempt did not plan nothing: the plan still
                    // waits for the owner's yes, which recovery asks for.
                    let held = g.state == GoalState::Stated
                        && *reason == EndReason::Completed
                        && !r.interrupted
                        && !r.stop_requested
                        && !r.planning_work
                        && !self.plan_unapproved(g);
                    let goal = g.id.clone();
                    if let Some(g) = self.goals.get_mut(&goal) {
                        g.planning_held = held;
                    }
                }
            }
            // `run.started` stays as recorded; the run now works on this.
            EventKind::RunRerouted {
                run,
                to_model,
                vendor,
                effort,
                ..
            } => {
                if let Some(r) = self.runs.get_mut(run) {
                    r.assignment.model.clone_from(to_model);
                    r.assignment.vendor = *vendor;
                    r.assignment.effort = *effort;
                }
            }
            EventKind::EngineStarted { .. } => {
                for r in self.runs.values_mut().filter(|r| r.ended.is_none()) {
                    r.interrupted = true;
                }
            }
            _ => {}
        }
        // Only assignment, reroute and ending events change the exposed run.
        if let EventKind::RunStarted(a) = &e.kind {
            self.refresh_current_run(&a.run);
        } else if let EventKind::RunRerouted { run, .. } | EventKind::RunEnded { run, .. } = &e.kind
        {
            self.refresh_current_run(run);
        }
    }
}

/// Compatibility for resource records written before typed metadata.
fn historical_resource(text: &str) -> Option<ResourceWait> {
    if text.starts_with("Waiting for memory:") || text == "Waiting for memory" {
        Some(ResourceWait::Memory)
    } else if text.starts_with("waiting for disk space:") || text == "Waiting for disk" {
        Some(ResourceWait::Disk)
    } else if text.starts_with("Waiting for host load:") || text == "Waiting for host load" {
        Some(ResourceWait::HostLoad)
    } else if matches!(
        text,
        "Resources available." | "Resources available; starting run."
    ) {
        Some(ResourceWait::Available)
    } else {
        None
    }
}
