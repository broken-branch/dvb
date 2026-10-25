use super::{Goal, Project, Projections};
use dvb_core::api::{GoalState, TaskView};
use dvb_core::event::{ChecksStatus, open_findings};
use dvb_core::roles::{ReviewKind, ReviewSubject, accountable_reviewer};
use dvb_core::run_verbs::{BuilderOutcome, PartialReason};
use dvb_core::{Actor, Event, EventId, EventKind, GoalId, RoleId, TaskState, Timestamp};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Task {
    pub summary: String,
    pub view: TaskView,
    pub facts: dvb_core::state::TaskFacts,
    pub problem_at: Option<EventId>,
    /// Stopped by the owner with a reason: they know why, so the stop wakes
    /// no orchestrator. Cleared when the task moves on.
    pub held: bool,
    pub merged_at: Option<EventId>,
    /// The branch was published, but its PR body has not reached review yet.
    /// Kept through effect retries and terminal failure, and derived on replay.
    pub publication_pending: bool,
    pub branch: Option<String>,
    /// Latest recorded evidence for each head; late old-head verdicts cannot hide current evidence.
    pub recorded_verdicts: BTreeMap<String, dvb_core::api::VerdictView>,
    /// When the pending retry is due.
    pub retry_at: Option<Timestamp>,
    pub retry_limit: Option<dvb_core::forge::RateLimit>,
    /// The CI status last recorded, and its head.
    pub checks: Option<(String, ChecksStatus)>,
    /// Transient dispatch hold; startup rediscovers it from the forge.
    pub(crate) checks_waiting_for_slot: bool,
    /// When each required check of a head was first seen Pending, restored
    /// from checks.pending event timestamps on replay.
    pub(crate) pending_since: BTreeMap<(String, String), Timestamp>,
    /// The (head, check) pairs the owner was told are stuck (`checks.stuck`).
    pub stuck_checks: Vec<(String, String)>,
    pub ci_question: Option<dvb_core::QuestionId>,
    pub ci_retry_runs: Vec<u64>,
    /// The base the branch was last brought up to date with (`branch.updated`),
    /// until the next round is published: the head's diff is read against it.
    pub updated_base: Option<String>,
    /// The base the first publication was built on: a stage with no verdict
    /// yet reviews the whole change against it.
    pub base: Option<String>,
    /// The heads the code and design review stages gave verdicts on.
    pub code_reviewed: Reviewed,
    pub design_reviewed: Reviewed,
    /// Branch repair rounds started (`task.started` with `branch_repair`),
    /// reset by a retry of the failed task (its `task.retry_text_set`). The
    /// engine's own updates (`branch.updated`) are not counted: each one
    /// needs another merge on the base.
    pub repairs: u8,
    /// The merge commit, once `pr.merged` is recorded.
    pub merged: Option<String>,
    /// The owner said yes to the plan this task is part of.
    pub approved: bool,
    /// The id of the event that last moved it to failed.
    pub failed: Option<EventId>,
    /// The id of its last retry's `task.retry_text_set`: an owner's yes to
    /// retry it allows one retry.
    pub retried_at: Option<EventId>,
    /// A deferred start (`task.waiting`), until the task starts.
    pub waiting: Option<Waiting>,
    /// Publications built on the role's routine hire that review findings or
    /// failed CI sent back (`dispatch::round_hire`).
    pub routine_sent_back: u8,
    /// The current publication was sent back: it counts once.
    pub(crate) sent_back: bool,
    /// A scan hit (`scan.hit`) since the last publication or retry text: a
    /// hit in the builder's change gets one fix round only while this is unset.
    pub scan_hit: bool,
}

/// A deferred orchestrator or automatic start (`task.waiting`).
#[derive(Debug, Clone)]
pub struct Waiting {
    /// The `task.waiting` event's id: the oldest waits least.
    pub since: EventId,
    /// Selected at the durable waiting event, recovered against the role/hire
    /// history at that position. Old capacity waits without a model stay unpinned.
    pub hire: Option<dvb_core::HireId>,
    /// Engine-authored admissions retain the approved goal version from the
    /// event's position in the log, including after replay.
    pub approved_version: Option<EventId>,
    pub model: Option<String>,
    pub effort: Option<dvb_core::Effort>,
    pub reason: String,
    pub routine: bool,
}

/// The last two distinct heads a review stage gave a verdict on, newest first.
#[derive(Debug, Clone, Default)]
pub struct Reviewed {
    last: Option<String>,
    before: Option<String>,
}

impl Reviewed {
    fn record(&mut self, head: &str) {
        if self.last.as_deref() != Some(head) {
            self.before = self.last.replace(head.to_owned());
        }
    }

    /// The last head reviewed before `head`: a review of `head` covers everything since.
    pub fn before(&self, head: &str) -> Option<&String> {
        if self.last.as_deref() == Some(head) {
            self.before.as_ref()
        } else {
            self.last.as_ref()
        }
    }
}

impl Task {
    pub fn reviewed(&self, stage: ReviewKind) -> &Reviewed {
        match stage {
            ReviewKind::Code => &self.code_reviewed,
            ReviewKind::Design => &self.design_reviewed,
        }
    }

    /// Code routing is shared by both task kinds; design always uses its default role.
    pub fn reviewer(&self, project: &Project, stage: ReviewKind) -> Option<RoleId> {
        match stage {
            ReviewKind::Code => accountable_reviewer(
                &ReviewSubject {
                    review: ReviewKind::Code,
                    built_by: std::slice::from_ref(&self.view.role),
                },
                &self.view.role,
                &project.roles,
            ),
            ReviewKind::Design => project
                .role_by_slug(dvb_core::roles::DESIGN_REVIEWER)
                .map(|r| r.id.clone()),
        }
    }

    /// A current code approval releases the second stage of a Design task.
    pub fn next_review(&self) -> ReviewKind {
        self.facts.next_review(self.view.review)
    }

    /// Both ordered Design verdicts must approve this publication.
    pub fn reviews_approved(&self) -> bool {
        self.facts.reviews_approved(self.view.review)
    }

    /// Display and controls follow the dropped goal without rewriting its
    /// durable failure, run, branch or evidence records.
    pub(super) fn cancel_dropped_failure(&mut self) {
        if matches!(self.view.state, TaskState::Failed { .. }) {
            self.view.state = TaskState::Cancelled;
            self.view.reason = Some("its goal was dropped".into());
        }
    }

    /// Whether the task's next run is the head's reviewer: once published, a
    /// builder runs only for a fix round (an open verdict on the head, or a
    /// builder already started on the head and not yet published, such as a
    /// conflict fix round), below the round cap. A fix round that changed
    /// nothing publishes the head it started on, which is then reviewed again.
    pub fn needs_reviewer(&self) -> bool {
        self.facts.needs_reviewer()
    }

    /// CI failed on the current head, as last recorded.
    pub fn checks_failed(&self) -> bool {
        self.checks.as_ref().is_some_and(|(head, status)| {
            self.view.head.as_ref() == Some(head) && *status == ChecksStatus::Failed
        })
    }

    /// Stopped on a `blocked` or `partial: other` result: the orchestrator is
    /// woken once with the builder's reason.
    pub fn stopped_blocked(&self) -> bool {
        matches!(self.view.state, TaskState::Stopped { .. })
            && matches!(
                self.view.outcome,
                BuilderOutcome::Blocked { .. }
                    | BuilderOutcome::Partial {
                        reason: PartialReason::Other,
                        ..
                    }
            )
    }
}

fn rate_limit_reason(limit: dvb_core::forge::RateLimit) -> String {
    match limit.reset {
        Some(reset) => format!(
            "Waiting for GitHub rate limit; reset at {}",
            chrono::DateTime::from_timestamp_millis(reset.millis()).map_or_else(
                || reset.millis().to_string(),
                |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
            )
        ),
        None => "Waiting for GitHub rate limit; reset not supplied".into(),
    }
}

pub(super) fn task_final(state: &TaskState) -> bool {
    matches!(
        state,
        TaskState::Done | TaskState::Failed { .. } | TaskState::Cancelled
    )
}

impl Projections {
    /// An approved goal with approved tasks the orchestrator has not started.
    /// Usable deferred starts belong to the engine; unchosen Auto starts and
    /// missing hires still need the orchestrator, including deferred requests.
    pub fn has_waiting_tasks(&self, g: &Goal, pool: &dvb_core::ModelPool) -> bool {
        g.state == GoalState::Approved
            && g.tasks.iter().filter_map(|t| self.tasks.get(t)).any(|t| {
                t.approved
                    && t.facts.run.is_none()
                    && t.view.state == TaskState::Planned
                    && t.waiting.as_ref().is_none_or(|w| {
                        w.approved_version.is_none_or(|v| v == g.version)
                            && self.needs_orchestrator(t, pool)
                    })
            })
    }

    /// A never-started task needs a model choice or an owner hire question.
    pub fn needs_orchestrator(&self, t: &Task, pool: &dvb_core::ModelPool) -> bool {
        let choice = t.waiting.as_ref();
        if let Some(hire) = choice.and_then(|w| w.hire.as_ref())
            && !self.projects.get(&t.view.project).is_some_and(|p| {
                p.hires
                    .iter()
                    .any(|h| &h.id == hire && h.status == dvb_core::roles::HireStatus::Active)
            })
        {
            return true;
        }
        if (self.settings.model_mode() == dvb_core::api::ModelMode::Auto)
            && choice.is_none_or(|w| w.model.is_none() && w.effort.is_none())
        {
            return true;
        }
        let model = choice.and_then(|w| w.model.as_deref());
        // An explicit Auto request may hire its named model atomically with
        // the start. Engine-authored requests use an existing hire only.
        if (self.settings.model_mode() == dvb_core::api::ModelMode::Auto)
            && choice.is_some_and(|w| w.approved_version.is_none())
            && model.is_some_and(|m| self.model_selected(m) && pool.find(m).is_some())
        {
            return false;
        }
        !self.projects.get(&t.view.project).is_some_and(|p| {
            let routine = t.view.routine || choice.is_some_and(|w| w.routine);
            self.usable_hire(p, &t.view.role, model, routine, pool)
                .is_some()
        })
    }

    /// Counts the current publication once when open findings or failed CI
    /// send it back, in `routine_sent_back` when the role's routine hire built it.
    fn count_send_back(&mut self, task: &dvb_core::TaskId) {
        let Some(t) = self.tasks.get(task) else {
            return;
        };
        if t.sent_back || (t.facts.open_findings() == 0 && !t.checks_failed()) {
            return;
        }
        let routine = (t.facts.published_by.as_ref())
            .and_then(|run| self.runs.get(run))
            .zip(self.projects.get(&t.view.project))
            .is_some_and(|(r, p)| {
                p.hires
                    .iter()
                    .any(|h| h.id == r.assignment.hire && h.routine)
            });
        if let Some(t) = self.tasks.get_mut(task) {
            t.sent_back = true;
            if routine {
                t.routine_sent_back = t.routine_sent_back.saturating_add(1);
            }
        }
    }

    fn latest_goal_merge(&self, goal: &GoalId) -> Option<EventId> {
        self.goals
            .get(goal)?
            .tasks
            .iter()
            .filter_map(|id| self.tasks.get(id)?.merged_at)
            .max()
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "only the kinds this fold reads have an arm"
    )]
    pub(super) fn fold_tasks(&mut self, e: &Event) {
        match &e.kind {
            EventKind::TaskStopped { task, reason } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.view.reason.clone_from(reason);
                    t.held = e.actor == Actor::Owner && reason.is_some();
                }
            }
            EventKind::TaskReasonChanged {
                task,
                run,
                reason,
                resource,
            } => {
                let current =
                    self.runs.get(run).is_some_and(|r| {
                        r.ended.is_none() && r.assignment.task.as_ref() == Some(task)
                    }) && self.tasks.get(task).is_some_and(|t| {
                        t.facts.run.as_ref() == Some(run)
                            && matches!(t.view.state, TaskState::Preparing | TaskState::InReview)
                    });
                if current {
                    self.resource_note(
                        run,
                        resource.or_else(|| {
                            reason
                                .is_none()
                                .then_some(dvb_core::event::ResourceWait::Available)
                        }),
                        reason.as_deref().unwrap_or_default(),
                    );
                    if resource.is_none()
                        && let Some(t) = self.tasks.get_mut(task)
                    {
                        t.view.reason.clone_from(reason);
                    }
                }
            }
            EventKind::TaskStateChanged { task, to, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    // A reasoned hold, and its reason, survive a restart's
                    // reconciliation: to `Unknown` and back to stopped.
                    let unknown = |s: &TaskState| matches!(s, TaskState::Unknown { .. });
                    t.held = t.held
                        && (unknown(to)
                            || matches!(to, TaskState::Stopped { .. }) && unknown(&t.view.state));
                    let held_reason = t.view.reason.clone().filter(|_| t.held);
                    if *to == TaskState::InReview {
                        t.publication_pending = false;
                    }
                    if matches!(to, TaskState::Failed { .. } | TaskState::Stopped { .. })
                        && t.view.state != *to
                    {
                        t.problem_at = Some(e.id);
                    }
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    t.view.state = dvb_core::apply(t.view.state.clone(), &e.kind);
                    t.view.reason = if *to == TaskState::InReview {
                        Some(dvb_core::wording::review_reason(t.next_review()).into())
                    } else if matches!(to, TaskState::RetryPending { .. }) {
                        t.retry_limit.map(rate_limit_reason)
                    } else {
                        held_reason
                    };
                    if matches!(to, TaskState::Failed { .. }) {
                        t.failed = Some(e.id);
                    }
                    if self
                        .goals
                        .get(&t.view.goal)
                        .is_some_and(|g| g.state == GoalState::Dropped)
                    {
                        t.cancel_dropped_failure();
                    }
                }
            }
            EventKind::TaskStarted {
                task,
                branch_repair,
                ..
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    if *branch_repair && !t.facts.branch_repair {
                        t.repairs = t.repairs.saturating_add(1);
                    }
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    t.waiting = None;
                }
            }
            EventKind::TaskWaiting {
                task,
                model,
                effort,
                reason,
                routine,
            } => {
                let hire = self
                    .tasks
                    .get(task)
                    .and_then(|t| {
                        self.projects.get(&t.view.project).and_then(|p| {
                            model.as_deref().and_then(|m| {
                                p.hire_for(&t.view.role, Some(m), *routine || t.view.routine)
                            })
                        })
                    })
                    .map(|h| h.id.clone());
                if let Some(t) = self.tasks.get_mut(task) {
                    t.view.routine |= *routine;
                    t.waiting = Some(Waiting {
                        hire,
                        since: e.id,
                        approved_version: (e.actor == Actor::Engine)
                            .then(|| self.goals.get(&t.view.goal).map(|g| g.version))
                            .flatten(),
                        model: model.clone(),
                        effort: *effort,
                        reason: reason.clone(),
                        routine: *routine,
                    });
                }
            }
            EventKind::BranchPublished {
                task,
                round,
                branch,
                ..
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    if *round > t.facts.round || t.facts.branch_repair {
                        t.publication_pending = true;
                    }
                    if t.facts.head.is_none() {
                        t.base.clone_from(&t.facts.builder_base);
                    }
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    t.view.head.clone_from(&t.facts.head);
                    t.branch = Some(branch.clone());
                    t.checks = None;
                    t.ci_question = None;
                    t.ci_retry_runs.clear();
                    t.updated_base = None;
                    t.sent_back = false;
                    t.scan_hit = false;
                }
            }
            // A carried-over verdict follows as its own `verdict.recorded`.
            EventKind::BranchUpdated { task, base, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.updated_base = Some(base.clone());
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    t.view.head.clone_from(&t.facts.head);
                    t.checks = None;
                    t.ci_question = None;
                    t.ci_retry_runs.clear();
                }
            }
            EventKind::VerdictRecorded {
                task,
                head,
                run,
                findings,
                review_stage,
                verdict,
                role,
                model,
                vendor,
                summary,
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.recorded_verdicts.insert(
                        head.clone(),
                        dvb_core::api::VerdictView {
                            verdict: verdict.clone(),
                            task: task.clone(),
                            head: head.clone(),
                            run: run.clone(),
                            role: role.clone(),
                            model: model.clone(),
                            vendor: *vendor,
                            open_findings: open_findings(findings),
                            summary: summary.clone(),
                            findings: findings.clone(),
                        },
                    );
                    let accepted = t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    if accepted {
                        match review_stage.unwrap_or(ReviewKind::Code) {
                            ReviewKind::Code => t.code_reviewed.record(head),
                            ReviewKind::Design => t.design_reviewed.record(head),
                        }
                    }
                    if accepted
                        && t.view.review == ReviewKind::Design
                        && *review_stage == Some(ReviewKind::Code)
                        && open_findings(findings) == 0
                    {
                        t.view.reason =
                            Some(dvb_core::wording::review_reason(ReviewKind::Design).into());
                    }
                }
                if let Some(r) = self.runs.get_mut(run) {
                    r.verdict_given = true;
                }
                self.count_send_back(task);
            }
            EventKind::MergeRefused { task, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    t.checks = None;
                    t.ci_question = None;
                    t.ci_retry_runs.clear();
                }
            }
            EventKind::EffectRetry {
                task,
                at,
                rate_limit,
                attempt,
                effect,
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.facts.fold(e.id, &e.actor, &e.kind, t.view.review);
                    if matches!(t.view.state, TaskState::RetryPending { .. }) {
                        // An automatic retry says so, and a run's again how
                        // the last one ended.
                        let ended = t
                            .facts
                            .run
                            .as_ref()
                            .filter(|_| *effect == dvb_core::state::Effect::StartUnit)
                            .and_then(|run| self.runs.get(run))
                            .and_then(|r| {
                                let reason = r.ended.as_ref()?;
                                Some(dvb_core::wording::ended_words(
                                    reason,
                                    r.ended_detail.as_deref(),
                                ))
                            });
                        t.view.reason = Some(match (rate_limit, ended) {
                            (Some(limit), _) => rate_limit_reason(*limit),
                            (None, Some(reason)) => dvb_core::wording::render(
                                "task.retrying-after",
                                &serde_json::json!({"reason": reason, "attempt": attempt}),
                            ),
                            (None, None) => dvb_core::wording::render(
                                "task.retrying",
                                &serde_json::json!({"attempt": attempt}),
                            ),
                        });
                    }
                    t.retry_at = Some(*at);
                    t.retry_limit = *rate_limit;
                }
            }
            EventKind::ChecksObserved {
                task,
                head,
                status,
                retry_question,
                retry_runs,
                ..
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.checks = Some((head.clone(), *status));
                    t.ci_question.clone_from(retry_question);
                    t.ci_retry_runs.clone_from(retry_runs);
                }
                self.count_send_back(task);
            }
            // Only a retry of the failed task sets its text while it is failed.
            EventKind::TaskRetryTextSet { task, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.retried_at = Some(e.id);
                    t.scan_hit = false;
                    if matches!(t.view.state, TaskState::Failed { .. }) {
                        t.repairs = 0;
                    }
                }
            }
            EventKind::ChecksPending { task, head, checks } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.pending_since
                        .retain(|(h, name), _| h == head && checks.contains(name));
                    for name in checks {
                        t.pending_since
                            .entry((head.clone(), name.clone()))
                            .or_insert(e.at);
                    }
                }
            }
            EventKind::ChecksStuck {
                task, head, check, ..
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.stuck_checks.push((head.clone(), check.clone()));
                }
            }
            EventKind::PrOpened { task, pr, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.view.pr = Some(*pr);
                    t.publication_pending = false;
                }
            }
            EventKind::ScanHit { task, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.scan_hit = true;
                }
            }
            EventKind::PrMerged { task, sha, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.merged = Some(sha.clone());
                    t.merged_at = Some(e.id);
                }
            }
            _ => {}
        }
        if let EventKind::TaskStateChanged { task, .. } = &e.kind
            && let Some(t) = self.tasks.get(task)
        {
            let goal = t.view.goal.clone();
            let merged = self.latest_goal_merge(&goal);
            if let Some(t) = self.tasks.get_mut(task) {
                t.view.problem_superseded = t
                    .problem_at
                    .is_some_and(|at| merged.is_some_and(|m| m > at));
            }
        } else if let EventKind::PrMerged { task, .. } = &e.kind
            && let Some(t) = self.tasks.get(task)
        {
            let goal = t.view.goal.clone();
            let merged = self.latest_goal_merge(&goal);
            if let Some(g) = self.goals.get(&goal) {
                for task in &g.tasks {
                    if let Some(t) = self.tasks.get_mut(task) {
                        t.view.problem_superseded = t
                            .problem_at
                            .is_some_and(|at| merged.is_some_and(|m| m > at));
                    }
                }
            }
        }
    }
}

impl Projections {
    pub fn unmet_dependencies(&self, task: &Task) -> Vec<dvb_core::TaskId> {
        task.view
            .after
            .iter()
            .filter(|id| self.tasks.get(*id).is_none_or(|t| t.merged.is_none()))
            .cloned()
            .collect()
    }

    pub(super) fn refresh_task_results(&mut self) {
        let values: Vec<_> = self
            .tasks
            .values()
            .map(|t| {
                let waiting = if t.facts.builder.is_none() && t.view.state == TaskState::Planned {
                    self.unmet_dependencies(t)
                } else {
                    vec![]
                };
                let outcome = t
                    .facts
                    .builder
                    .as_ref()
                    .and_then(|id| self.runs.get(id))
                    .map(|r| r.outcome.clone())
                    .unwrap_or_default();
                let reason = (!waiting.is_empty()).then(|| {
                    let tasks = waiting
                        .iter()
                        .map(|id| {
                            self.tasks
                                .get(id)
                                .map_or(id.as_str(), |t| t.view.title.as_str())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    dvb_core::wording::render(
                        "task.waiting-for",
                        &serde_json::json!({"tasks":tasks}),
                    )
                });
                (t.view.id.clone(), waiting, outcome, reason)
            })
            .collect();
        for (id, waiting, outcome, reason) in values {
            if let Some(t) = self.tasks.get_mut(&id) {
                t.view.waiting_for = waiting;
                t.view.outcome = outcome;
                if t.facts.run.is_none() && t.view.state == TaskState::Planned {
                    t.view.reason = reason;
                }
            }
        }
    }
}

impl Projections {
    /// A structured scope result can extend the already approved goal; it
    /// cannot rewrite its text or silently approve a larger goal.
    pub fn can_extend_plan(&self, g: &Goal) -> bool {
        g.state == GoalState::Approved
            && g.tasks.iter().filter_map(|id| self.tasks.get(id)).any(|t| {
                matches!(t.view.state, TaskState::Stopped { .. })
                    && matches!(
                        t.view.outcome,
                        dvb_core::run_verbs::BuilderOutcome::Partial {
                            reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                            ..
                        }
                    )
                    && t.facts
                        .builder
                        .as_ref()
                        .and_then(|id| self.runs.get(id))
                        .and_then(|r| r.outcome_at)
                        .is_some_and(|at| g.planned.is_none_or(|plan| at > plan))
            })
    }

    /// The first goal with a stop the orchestrator has not been woken for,
    /// among goals that can take a wake now: a held goal, or one waiting for
    /// an answer, keeps its recovery without hiding other goals' work.
    pub(crate) fn pending_task_recovery(&self, project: &dvb_core::ProjectId) -> Option<&Goal> {
        self.goals_in_order(project).into_iter().find(|g| {
            g.state == GoalState::Approved
                && self.waits_for(g).is_none()
                && !g.planning_held
                && !self.orchestrator_question_open(g)
                && self.task_recovery_reason(g).is_some()
        })
    }

    pub(crate) fn task_recovery_reason(&self, g: &Goal) -> Option<String> {
        let since = self
            .runs
            .values()
            .filter(|r| {
                r.assignment.is_orchestrator()
                    && r.project == g.project
                    && r.goal_version == Some(g.version)
            })
            .map(|r| r.started)
            .max()
            .unwrap_or(EventId(0));
        g.tasks
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .find_map(|t| {
                // A scope stop wakes once for the stop, and once for each
                // later merge of the goal's work: the moment to retry it.
                let result_at = t
                    .facts
                    .builder
                    .as_ref()
                    .and_then(|id| self.runs.get(id))
                    .and_then(|r| r.outcome_at);
                let scope = matches!(
                    t.view.outcome,
                    BuilderOutcome::Partial {
                        reason: PartialReason::OutOfRole,
                        ..
                    }
                ) && matches!(t.view.state, TaskState::Stopped { .. })
                    && (t.problem_at.is_some_and(|at| at > since)
                        || g.tasks.iter().filter_map(|id| self.tasks.get(id)).any(|m| {
                            m.merged_at
                                .is_some_and(|at| at > since && Some(at) > result_at)
                        }));
                // A blocked or other incomplete stop wakes once, for the stop.
                let blocked = t.stopped_blocked() && t.problem_at.is_some_and(|at| at > since);
                if scope || blocked {
                    let problem = match &t.view.outcome {
                        BuilderOutcome::Blocked { prerequisite } => {
                            format!("blocked on {prerequisite}")
                        }
                        BuilderOutcome::Partial {
                            reason: PartialReason::OutOfRole,
                            ..
                        } => "out of its role".into(),
                        BuilderOutcome::Partial {
                            reason: PartialReason::Other,
                            detail,
                        } => {
                            let mut chars = detail.chars();
                            let detail: String = chars.by_ref().take(160).collect();
                            let suffix = if chars.next().is_some() { "…" } else { "" };
                            format!("partial ({detail}{suffix})")
                        }
                        BuilderOutcome::Complete
                        | BuilderOutcome::Partial {
                            reason: PartialReason::CompletionUnmet,
                            ..
                        } => return None,
                    };
                    return Some(format!("{} stopped: {problem}", t.view.title));
                }
                if t.facts.run.is_some() || t.view.state != TaskState::Planned {
                    return None;
                }
                t.view
                    .after
                    .iter()
                    .filter_map(|id| self.tasks.get(id))
                    .find(|prerequisite| {
                        prerequisite.merged.is_none()
                            && matches!(
                                prerequisite.view.state,
                                TaskState::Failed { .. }
                                    | TaskState::Stopped { .. }
                                    | TaskState::Cancelled
                            )
                            && prerequisite.problem_at.is_some_and(|at| at > since)
                            && !prerequisite.held
                    })
                    .map(|prerequisite| {
                        let status = if matches!(prerequisite.view.state, TaskState::Failed { .. })
                        {
                            "failed"
                        } else {
                            "stopped"
                        };
                        format!(
                            "{} is waiting: its prerequisite {} {status}",
                            t.view.title, prerequisite.view.title
                        )
                    })
            })
    }

    /// The task a `blocked` result names, by id or else by its unique title,
    /// in the same project. Its merge after the result resumes the task.
    pub fn blocking_task(&self, t: &Task) -> Option<&Task> {
        let BuilderOutcome::Blocked { prerequisite } = &t.view.outcome else {
            return None;
        };
        let name = prerequisite.trim();
        let others = || {
            self.tasks
                .values()
                .filter(|o| o.view.project == t.view.project && o.view.id != t.view.id)
        };
        others().find(|o| o.view.id.as_str() == name).or_else(|| {
            let mut titled = others().filter(|o| o.view.title.trim() == name);
            titled.next().filter(|_| titled.next().is_none())
        })
    }

    /// The task a stopped `blocked` result names, once it merged after that
    /// result: the moment the stopped task resumes by itself.
    pub fn merged_blocker(&self, t: &Task) -> Option<&Task> {
        if !matches!(t.view.state, TaskState::Stopped { .. }) {
            return None;
        }
        let result_at = t
            .facts
            .builder
            .as_ref()
            .and_then(|id| self.runs.get(id))
            .and_then(|r| r.outcome_at)?;
        self.blocking_task(t)
            .filter(|b| b.merged_at.is_some_and(|at| at > result_at))
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::{Config, Engine};
    use dvb_core::{GoalId, ProjectId, RunId, TaskId};

    #[test]
    fn prerequisite_problems_notify_once_and_consumption_survives_replay() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "unused".into()), Default::default()).unwrap();
        let (p, g, a, b) = (
            ProjectId::new("p"),
            GoalId::new("g"),
            TaskId::new("api"),
            TaskId::new("caller"),
        );
        let task = |id: TaskId, after| dvb_core::event::PlannedTask {
            task: id.clone(),
            title: id.to_string(),
            role: RoleId::new("backend"),
            review: ReviewKind::Code,
            summary: "work".into(),
            after,
            routine: false,
        };
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::ProjectAdded {
                    id: p.clone(),
                    name: "p".into(),
                    repo: "local".into(),
                    default_branch: "main".into(),
                    ci: dvb_core::event::CiMode::NoCi,
                },
                EventKind::GoalStated {
                    goal: g.clone(),
                    text: "goal".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal: g.clone(),
                    tasks: vec![task(a.clone(), vec![]), task(b.clone(), vec![a.clone()])],
                },
                EventKind::GoalApproved { goal: g.clone() },
                EventKind::TaskStateChanged {
                    task: a.clone(),
                    from: TaskState::Planned,
                    to: TaskState::Failed {
                        reason: "CI failed".into(),
                    },
                },
            ])
        })
        .unwrap();
        assert_eq!(
            e.proj().pending_task_recovery(&p).map(|g| g.id.clone()),
            Some(g.clone())
        );
        let version = e.proj().goals[&g].version;
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(RunId::new("notice"))
                    .role(RoleId::new("orchestrator"))
                    .goal_version(Some(version))
                    .build(),
            )])
        })
        .unwrap();
        let replay = Projections::rebuild(&crate::test_support::events::all(&e));
        assert!(replay.pending_task_recovery(&p).is_none());
        assert_eq!(
            replay.tasks[&b].view.reason.as_deref(),
            Some("Waiting for api")
        );
        assert!(replay.tasks[&b].facts.run.is_none());
    }

    #[test]
    fn scope_stopped_original_wakes_once_when_later_goal_work_merges() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "unused".into()), Default::default()).unwrap();
        let (p, g) = (ProjectId::new("p"), GoalId::new("g"));
        let (ui, early, api) = (TaskId::new("ui"), TaskId::new("early"), TaskId::new("api"));
        let task = |id: &TaskId| dvb_core::event::PlannedTask {
            task: id.clone(),
            title: id.to_string(),
            role: RoleId::new("backend"),
            review: ReviewKind::Code,
            summary: "work".into(),
            after: vec![],
            routine: false,
        };
        let merged = |task: &TaskId| EventKind::PrMerged {
            task: task.clone(),
            pr: 1,
            sha: format!("{task}-sha"),
        };
        let orchestrator = |run: &str| {
            let version = e.proj().goals[&g].version;
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunStarted(
                    crate::test_support::assignment::Assignment::new(RunId::new(run))
                        .role(RoleId::new("orchestrator"))
                        .goal_version(Some(version))
                        .build(),
                )])
            })
            .unwrap();
        };
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::ProjectAdded {
                    id: p.clone(),
                    name: "p".into(),
                    repo: "local".into(),
                    default_branch: "main".into(),
                    ci: dvb_core::event::CiMode::NoCi,
                },
                EventKind::GoalStated {
                    goal: g.clone(),
                    text: "goal".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal: g.clone(),
                    tasks: vec![task(&ui), task(&early)],
                },
                EventKind::GoalApproved { goal: g.clone() },
                merged(&early),
                EventKind::RunStarted(
                    crate::test_support::assignment::Assignment::new(RunId::new("builder"))
                        .task(Some(ui.clone()))
                        .build(),
                ),
                EventKind::RunCollected {
                    run: RunId::new("builder"),
                    head: None,
                    summary: None,
                    outcome: Some(dvb_core::run_verbs::BuilderOutcome::Partial {
                        reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                        detail: "Needs engine API".into(),
                    }),
                },
                EventKind::TaskStateChanged {
                    task: ui.clone(),
                    from: TaskState::Running,
                    to: TaskState::Stopped {
                        was: Box::new(TaskState::Running),
                    },
                },
            ])
        })
        .unwrap();
        assert!(e.proj().pending_task_recovery(&p).is_some());
        // Planning the missing task consumes the stop; the merge before the
        // result is not news.
        orchestrator("plan-recovery");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::PlanProposed {
                goal: g.clone(),
                tasks: vec![task(&api)],
            }])
        })
        .unwrap();
        assert!(e.proj().pending_task_recovery(&p).is_none());
        e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(vec![merged(&api)]))
            .unwrap();
        let replay = Projections::rebuild(&crate::test_support::events::all(&e));
        assert_eq!(
            replay.pending_task_recovery(&p).map(|g| g.id.clone()),
            Some(g.clone())
        );
        orchestrator("retry-original");
        let replay = Projections::rebuild(&crate::test_support::events::all(&e));
        assert!(replay.pending_task_recovery(&p).is_none());
    }
}
