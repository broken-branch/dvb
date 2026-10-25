//! Task counters and current-publication review facts, shared by replay and simulation.
use super::{CapsUsed, Command, Effect, TaskState};
use crate::roles::ReviewKind;
use crate::{Actor, EventId, EventKind, RunId};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub head: String,
    pub open: u32,
    pub run: RunId,
}
impl Verdict {
    pub fn clean_on(&self, head: Option<&String>) -> bool {
        self.open == 0 && head == Some(&self.head)
    }
}

#[derive(Debug, Clone)]
pub struct TaskFacts {
    pub head: Option<String>,
    /// Published rounds; branch repair keeps the current round.
    pub round: u8,
    /// Builder starts since the last publication, including retries and resumes.
    pub attempts: u8,
    /// Reviewer starts on this head and ordered stage.
    pub reviews: u8,
    pub run: Option<RunId>,
    pub builder: Option<RunId>,
    /// The builder recorded when this head was published, even after later starts.
    pub published_by: Option<RunId>,
    pub builder_base: Option<String>,
    pub branch_repair: bool,
    /// A builder started by retrying the task (failed, or stopped on its
    /// head), until it publishes: the retry grants it one round past the
    /// review-round cap.
    pub retried: bool,
    pub verdict: Option<Verdict>,
    pub code_verdict: Option<Verdict>,
    pub design_verdict: Option<Verdict>,
    /// Reviews must start after this publication or invalidation boundary.
    pub review_since: EventId,
    pub retries: BTreeMap<Effect, u8>,
    /// Untimed budget waits advance backoff without spending failure retries.
    pub rate_limit_retries: BTreeMap<Effect, u8>,
    review_runs: BTreeMap<RunId, (Option<ReviewKind>, EventId, bool)>,
}

impl Default for TaskFacts {
    fn default() -> Self {
        Self {
            head: None,
            round: 0,
            attempts: 0,
            reviews: 0,
            run: None,
            builder: None,
            published_by: None,
            builder_base: None,
            branch_repair: false,
            retried: false,
            verdict: None,
            code_verdict: None,
            design_verdict: None,
            review_since: EventId(0),
            retries: BTreeMap::new(),
            rate_limit_retries: BTreeMap::new(),
            review_runs: BTreeMap::new(),
        }
    }
}

impl TaskFacts {
    pub fn next_review(&self, review: ReviewKind) -> ReviewKind {
        if review == ReviewKind::Design
            && self
                .code_verdict
                .as_ref()
                .is_some_and(|v| v.clean_on(self.head.as_ref()))
        {
            ReviewKind::Design
        } else {
            ReviewKind::Code
        }
    }

    pub fn reviews_approved(&self, review: ReviewKind) -> bool {
        let clean =
            |v: &Option<Verdict>| v.as_ref().is_some_and(|v| v.clean_on(self.head.as_ref()));
        match review {
            ReviewKind::Code => clean(&self.verdict),
            ReviewKind::Design => {
                clean(&self.code_verdict) && clean(&self.design_verdict) && clean(&self.verdict)
            }
        }
    }

    pub fn open_findings(&self) -> u32 {
        self.verdict
            .as_ref()
            .filter(|v| self.head.as_ref() == Some(&v.head))
            .map_or(0, |v| v.open)
    }

    pub fn needs_reviewer(&self) -> bool {
        let collected_fix = self.builder_base.is_some()
            && self.builder_base == self.head
            && self.published_by != self.builder;
        let fix = (self.open_findings() > 0 || collected_fix)
            && (self.branch_repair
                || self.retried
                || self.round < crate::caps::REVIEW_ROUNDS_PER_PR);
        self.head.is_some() && !fix
    }

    pub fn caps(&self, cmd: &Command, concurrent_runs: u64) -> CapsUsed {
        CapsUsed {
            concurrent_runs,
            attempts: if cmd.reviewer_attempt(self.needs_reviewer()) {
                self.reviews
            } else {
                self.attempts
            },
            review_rounds: self.round,
            retries: cmd
                .retry_effect()
                .and_then(|effect| self.retries.get(&effect))
                .copied()
                .unwrap_or(0),
            rate_limit_retries: if let Command::EffectFailed { effect, .. } = cmd {
                self.rate_limit_retries.get(effect).copied().unwrap_or(0)
            } else {
                0
            },
        }
    }

    pub fn more_review(&self, cmd: &Command, review: ReviewKind) -> bool {
        review == ReviewKind::Design
            && if matches!(cmd, Command::Reconcile(_)) {
                self.code_verdict.as_ref().is_some_and(|v| {
                    v.clean_on(self.head.as_ref()) && self.run.as_ref() == Some(&v.run)
                }) && self.design_verdict.is_none()
            } else {
                self.next_review(review) == ReviewKind::Code
            }
    }

    fn invalidate(&mut self, id: EventId) {
        self.verdict = None;
        self.code_verdict = None;
        self.design_verdict = None;
        self.review_since = id;
    }

    /// Fold an event scoped to this task, in durable order; return whether its verdict was accepted.
    pub fn fold(
        &mut self,
        id: EventId,
        actor: &Actor,
        kind: &EventKind,
        review: ReviewKind,
    ) -> bool {
        let mut accepted = false;
        match kind {
            EventKind::TaskStarted {
                branch_repair,
                retried,
                ..
            } => {
                self.attempts = self.attempts.saturating_add(1);
                self.branch_repair = *branch_repair;
                self.retried |= *retried;
            }
            EventKind::RunStarted(a) => {
                if a.reviews.is_some() {
                    if review == ReviewKind::Design
                        && self
                            .run
                            .as_ref()
                            .and_then(|r| self.review_runs.get(r))
                            .is_none_or(|(stage, started, _)| {
                                *stage != a.review_stage || *started <= self.review_since
                            })
                    {
                        self.reviews = 0;
                    }
                    self.reviews = self.reviews.saturating_add(1);
                    self.review_runs
                        .insert(a.run.clone(), (a.review_stage, id, false));
                } else {
                    self.builder_base.clone_from(&a.base);
                    self.builder = Some(a.run.clone());
                }
                self.run = Some(a.run.clone());
            }
            EventKind::BranchPublished { round, head, .. } => {
                self.published_by = self.builder.clone();
                self.round = *round;
                self.head = Some(head.clone());
                self.branch_repair = false;
                self.retried = false;
                self.attempts = 0;
                self.reviews = 0;
                self.retries.clear();
                self.rate_limit_retries.clear();
                self.invalidate(id);
            }
            EventKind::BranchUpdated {
                head, same_change, ..
            } => {
                self.head = Some(head.clone());
                self.verdict = None;
                if *same_change {
                    for verdict in self
                        .code_verdict
                        .iter_mut()
                        .chain(self.design_verdict.iter_mut())
                    {
                        verdict.head.clone_from(head);
                    }
                } else {
                    self.invalidate(id);
                }
            }
            EventKind::VerdictRecorded {
                head,
                run,
                findings,
                review_stage,
                ..
            } => {
                let evidence = Verdict {
                    head: head.clone(),
                    open: crate::event::open_findings(findings),
                    run: run.clone(),
                };
                if review == ReviewKind::Design {
                    if let (Some(stage), Some((run_stage, started, given))) =
                        (review_stage, self.review_runs.get(run))
                        && *run_stage == Some(*stage)
                        && *started > self.review_since
                        && self.head.as_ref() == Some(head)
                        && ((*stage == self.next_review(review)
                            && self.run.as_ref() == Some(run)
                            && !given)
                            || (*actor == Actor::Engine
                                && match stage {
                                    ReviewKind::Code => self.code_verdict.as_ref(),
                                    ReviewKind::Design => self.design_verdict.as_ref(),
                                }
                                .is_some_and(|v| &v.run == run)))
                    {
                        match stage {
                            ReviewKind::Code => self.code_verdict = Some(evidence.clone()),
                            ReviewKind::Design => self.design_verdict = Some(evidence.clone()),
                        }
                        self.verdict = Some(evidence);
                        accepted = true;
                    } else if review_stage.is_none() {
                        self.verdict = Some(evidence);
                        accepted = true;
                    }
                } else {
                    self.verdict = Some(evidence);
                    accepted = true;
                }
                if let Some((_, _, given)) = self.review_runs.get_mut(run) {
                    *given = true;
                }
            }
            EventKind::TaskStateChanged {
                from: TaskState::Failed { .. },
                to: TaskState::Preparing,
                ..
            } => self.retried = true,
            EventKind::MergeRefused { .. } => {
                *self.retries.entry(Effect::Merge).or_default() += 1;
                self.invalidate(id);
            }
            EventKind::EffectRetry {
                effect, rate_limit, ..
            } => {
                if rate_limit.is_none() {
                    *self.retries.entry(*effect).or_default() += 1;
                } else if rate_limit.is_some_and(|limit| limit.reset.is_none()) {
                    let waits = self.rate_limit_retries.entry(*effect).or_default();
                    *waits = waits.saturating_add(1);
                }
            }

            EventKind::EngineStarted { .. }
            | EventKind::EngineReconciled { .. }
            | EventKind::EngineUpdated { .. }
            | EventKind::EngineUpdateFailed { .. }
            | EventKind::ProjectAdded { .. }
            | EventKind::RepositoryCreated { .. }
            | EventKind::RepositorySetupPrepared { .. }
            | EventKind::ProjectPaused { .. }
            | EventKind::ProjectResumed { .. }
            | EventKind::ProjectSeen { .. }
            | EventKind::ProjectBriefSet { .. }
            | EventKind::RoleAdded(..)
            | EventKind::RoleChanged { .. }
            | EventKind::RoleRemoved(..)
            | EventKind::HireAdded(..)
            | EventKind::HireChanged { .. }
            | EventKind::HireRetired(..)
            | EventKind::SettingsChanged { .. }
            | EventKind::GoalStated { .. }
            | EventKind::PlanProposed { .. }
            | EventKind::PlanCancelled { .. }
            | EventKind::GoalApproved { .. }
            | EventKind::GoalDone { .. }
            | EventKind::GoalDropped { .. }
            | EventKind::GoalAfterSet { .. }
            | EventKind::GoalIssueClosed { .. }
            | EventKind::ProposalRequested { .. }
            | EventKind::ProjectIssuesFound(..)
            | EventKind::ProjectRulesetMissing {}
            | EventKind::ProjectRulesetInstalled {}
            | EventKind::GithubMirrorObserved { .. }
            | EventKind::QuestionOpened { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::QuestionClosed { .. }
            | EventKind::DecisionRecorded { .. }
            | EventKind::CommandRefused(..)
            | EventKind::TaskWaiting { .. }
            | EventKind::TaskStopped { .. }
            | EventKind::TaskResumed { .. }
            | EventKind::TaskCancelled { .. }
            | EventKind::TaskDone { .. }
            | EventKind::TaskFailed { .. }
            | EventKind::TaskRetryTextSet { .. }
            | EventKind::TaskReasonChanged { .. }
            | EventKind::TaskStateChanged { .. }
            | EventKind::ChecksPending { .. }
            | EventKind::ChecksStuck { .. }
            | EventKind::RunPreflight { .. }
            | EventKind::RunModel { .. }
            | EventKind::RunOutput { .. }
            | EventKind::RunUsage { .. }
            | EventKind::RunCollected { .. }
            | EventKind::RunStopRequested { .. }
            | EventKind::RunLaunched { .. }
            | EventKind::RunTimeLimitReached { .. }
            | EventKind::RunSurvivedRestart { .. }
            | EventKind::RunEnded { .. }
            | EventKind::RunRerouted { .. }
            | EventKind::OnlyStarted(..)
            | EventKind::OnlyEnded { .. }
            | EventKind::VendorLimited { .. }
            | EventKind::VendorResumed { .. }
            | EventKind::ScanHit { .. }
            | EventKind::PrOpened { .. }
            | EventKind::ChecksObserved { .. }
            | EventKind::PrMerged { .. } => {}
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EndReason;
    use crate::forge::RateLimit;
    use crate::launcher::UnitState;
    use crate::state::Observed;
    use crate::{Effort, HireId, RoleId, RunAssignment, TaskId, Vendor, VerdictId};

    fn start(run: &str, stage: Option<ReviewKind>, base: &str) -> EventKind {
        EventKind::RunStarted(RunAssignment {
            run: RunId::new(run),
            task: Some(TaskId::new("t")),
            role: RoleId::new("reviewer"),
            hire: HireId::new("h"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "test".into(),
            base: Some(base.into()),
            reviews: stage.map(|_| base.into()),
            review_stage: stage,
            goal_version: None,
            routine: false,
        })
    }
    fn published(head: &str) -> EventKind {
        EventKind::BranchPublished {
            task: TaskId::new("t"),
            round: 1,
            branch: "dvb/t".into(),
            head: head.into(),
        }
    }
    fn verdict(run: &str, stage: ReviewKind, head: &str) -> EventKind {
        EventKind::VerdictRecorded {
            task: TaskId::new("t"),
            verdict: VerdictId::new(run),
            run: RunId::new(run),
            role: RoleId::new("reviewer"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            head: head.into(),
            findings: vec![],
            summary: "clean".into(),
            review_stage: Some(stage),
        }
    }
    fn fold(facts: &mut TaskFacts, id: u64, kind: &EventKind) {
        facts.fold(EventId(id), &Actor::Engine, kind, ReviewKind::Design);
    }

    #[test]
    fn design_stages_keep_neutral_updates_and_reject_pre_invalidation_runs() {
        let mut facts = TaskFacts::default();
        fold(&mut facts, 1, &start("builder", None, "main"));
        fold(&mut facts, 2, &published("head"));
        fold(
            &mut facts,
            3,
            &start("code", Some(ReviewKind::Code), "head"),
        );
        fold(
            &mut facts,
            4,
            &start("code-retry", Some(ReviewKind::Code), "head"),
        );
        assert_eq!(facts.reviews, 2);
        fold(
            &mut facts,
            5,
            &verdict("code-retry", ReviewKind::Code, "head"),
        );
        let reconcile = Command::Reconcile(Observed {
            unit: UnitState::Inactive,
            head: None,
        });
        assert!(facts.more_review(&reconcile, ReviewKind::Design));
        assert!(!facts.more_review(
            &Command::UnitEnded(EndReason::Completed),
            ReviewKind::Design
        ));
        fold(
            &mut facts,
            6,
            &start("design", Some(ReviewKind::Design), "head"),
        );
        assert_eq!(facts.reviews, 1, "each stage has its own attempt cap");
        fold(
            &mut facts,
            7,
            &verdict("design", ReviewKind::Design, "head"),
        );
        assert!(facts.reviews_approved(ReviewKind::Design));
        let updated = |head: &str, same_change| EventKind::BranchUpdated {
            task: TaskId::new("t"),
            branch: "dvb/t".into(),
            from: "head".into(),
            base: "new-main".into(),
            head: head.into(),
            same_change,
        };
        fold(&mut facts, 8, &updated("neutral", true));
        assert_eq!(facts.review_since, EventId(2));
        assert!(facts.verdict.is_none());
        assert!(
            facts
                .code_verdict
                .as_ref()
                .unwrap()
                .clean_on(facts.head.as_ref())
        );
        fold(
            &mut facts,
            9,
            &verdict("code-retry", ReviewKind::Code, "neutral"),
        );
        fold(
            &mut facts,
            10,
            &verdict("design", ReviewKind::Design, "neutral"),
        );
        assert!(
            facts.reviews_approved(ReviewKind::Design),
            "engine-carried stage evidence survives"
        );
        fold(&mut facts, 11, &updated("changed", false));
        fold(
            &mut facts,
            12,
            &verdict("code-retry", ReviewKind::Code, "changed"),
        );
        assert!(
            facts.verdict.is_none(),
            "a prior run cannot approve a changed publication"
        );
        fold(
            &mut facts,
            13,
            &start("new-code", Some(ReviewKind::Code), "changed"),
        );
        assert_eq!(facts.reviews, 1);
        fold(
            &mut facts,
            14,
            &verdict("new-code", ReviewKind::Code, "changed"),
        );
        fold(
            &mut facts,
            15,
            &EventKind::MergeRefused {
                task: TaskId::new("t"),
                reason: "changed base".into(),
            },
        );
        fold(
            &mut facts,
            16,
            &verdict("new-code", ReviewKind::Code, "changed"),
        );
        assert!(facts.verdict.is_none());
        fold(
            &mut facts,
            17,
            &start("same-stage", Some(ReviewKind::Code), "changed"),
        );
        assert_eq!(facts.reviews, 1, "review_since resets even the same stage");
    }

    /// The builder a retry of the failed task started is a fix round past the
    /// review-round cap until it publishes.
    #[test]
    fn a_retried_builder_is_a_round_past_the_cap_until_it_publishes() {
        let mut facts = TaskFacts::default();
        fold(&mut facts, 1, &start("builder", None, "main"));
        fold(&mut facts, 2, &published("head"));
        facts.round = crate::caps::REVIEW_ROUNDS_PER_PR;
        assert!(facts.needs_reviewer());
        fold(
            &mut facts,
            3,
            &EventKind::TaskStateChanged {
                task: TaskId::new("t"),
                from: TaskState::Failed {
                    reason: "CI failed".into(),
                },
                to: TaskState::Preparing,
            },
        );
        fold(&mut facts, 4, &start("retried", None, "head"));
        assert!(facts.retried);
        assert!(!facts.needs_reviewer());
        fold(&mut facts, 5, &published("fixed"));
        assert!(!facts.retried);
        assert!(facts.needs_reviewer());
    }

    #[test]
    fn failure_retries_budget_waits_and_builder_repair_have_separate_caps() {
        let mut facts = TaskFacts::default();
        fold(&mut facts, 1, &start("builder", None, "main"));
        fold(&mut facts, 2, &published("head"));
        fold(&mut facts, 3, &start("repair", None, "head"));
        fold(
            &mut facts,
            4,
            &EventKind::TaskStarted {
                task: TaskId::new("t"),
                run: RunId::new("repair"),
                resume: false,
                branch_repair: true,
                retried: false,
            },
        );
        assert!(!facts.needs_reviewer());
        for (id, limit) in [
            (5, None),
            (6, Some(RateLimit { reset: None })),
            (
                7,
                Some(RateLimit {
                    reset: Some(crate::Timestamp(50)),
                }),
            ),
        ] {
            fold(
                &mut facts,
                id,
                &EventKind::EffectRetry {
                    task: TaskId::new("t"),
                    effect: Effect::Push,
                    attempt: 1,
                    at: crate::Timestamp(100),
                    rate_limit: limit,
                },
            );
        }
        let cmd = Command::EffectFailed {
            effect: Effect::Push,
            rate_limit: None,
            transient: true,
            detail: "offline".into(),
        };
        let caps = facts.caps(&cmd, 2);
        assert_eq!(
            caps,
            CapsUsed {
                concurrent_runs: 2,
                attempts: 1,
                review_rounds: 1,
                retries: 1,
                rate_limit_retries: 1
            }
        );
        assert_eq!(
            Command::MergeRefused("base".into()).retry_effect(),
            Some(Effect::Merge)
        );
        fold(&mut facts, 8, &published("repaired"));
        assert!(facts.retries.is_empty() && facts.rate_limit_retries.is_empty());
        assert!(!facts.branch_repair);
        assert!(facts.needs_reviewer());
        assert_eq!(facts.published_by, Some(RunId::new("repair")));
    }
}
