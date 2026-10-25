//! The property test of the task state machine: random sequences of
//! work, stop, resume, cancel, pause, crash and retry through `decide` and
//! `apply`, checked after every step.
//!
//! The reference machine is the world the task lives in (paused or not, how many
//! other runs hold slots, CI or no CI). The system under test is a small
//! simulation of the engine around one task: its projection (head, round,
//! attempts, retries, verdict and CI evidence) and the outside world (the task's
//! unit, its `head` file, and the forge's branch, PR and merge). Effects follow
//! the per-task lock: an effect is sent in its state and its reply is
//! recorded before anything else takes the lock, except a stop (recorded first)
//! and a crash (the reply is lost; the forge may have acted).

use super::*;
use crate::event::ChecksStatus;
use crate::ids::{HireId, RoleId, RunId, VerdictId};
use crate::model::{Effort, Vendor};
use crate::refusal::RefusalCode as Code;
use crate::run_verbs::{Finding, Severity};
use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};
use std::collections::BTreeSet;

/// What happens next.
#[derive(Debug, Clone)]
pub enum Step {
    /// The next thing the engine or the task's run does in the task's state;
    /// `pick` chooses the outcome (1 is always progress). After an ended task
    /// it starts the next one.
    Work(u8),
    Stop,
    Resume,
    /// The owner retries the task (refused unless it failed).
    Retry,
    Cancel,
    /// The engine dies and starts again; on a `reboot` the units die too. An
    /// effect in flight loses its reply (without a reboot the forge did act).
    Crash {
        reboot: bool,
    },
    /// The task's unit dies on its own.
    UnitDies(u8),
    /// The effect in flight fails (a transient failure after the forge acted:
    /// a lost reply), or the unit being started does not start.
    EffectFails {
        rate_limited: bool,
        timed: bool,
        transient: bool,
    },
    Pause,
    Unpause,
    /// Other runs hold this many slots.
    OtherRuns(u64),
    MaxRuns(u64),
    PrerequisiteMerged,
    /// The owner replaces the saved project CI choice.
    Checks(CiMode),
    /// A command that is late or out of place: a stale timer, a duplicate,
    /// the orchestrator starting the task again, an old head.
    Stray(u8),
}

#[derive(Debug, Clone)]
pub struct World {
    paused: bool,
    other_runs: u64,
    max_runs: u64,
    ci: CiMode,
    design: bool,
    prerequisite_merged: bool,
}

pub struct WorldMachine;

impl ReferenceStateMachine for WorldMachine {
    type State = World;
    type Transition = Step;

    fn init_state() -> BoxedStrategy<World> {
        (
            prop_oneof![
                Just(CiMode::NoCi),
                Just(CiMode::Expected(vec!["rust".into()]))
            ],
            any::<bool>(),
        )
            .prop_map(|(ci, design)| World {
                paused: false,
                other_runs: 0,
                max_runs: 3,
                ci,
                design,
                prerequisite_merged: false,
            })
            .boxed()
    }

    fn transitions(_: &World) -> BoxedStrategy<Step> {
        prop_oneof![
            16 => any::<u8>().prop_map(Step::Work),
            2 => Just(Step::Stop),
            2 => Just(Step::Resume),
            1 => Just(Step::Retry),
            1 => Just(Step::Cancel),
            1 => any::<bool>().prop_map(|reboot| Step::Crash { reboot }),
            1 => any::<u8>().prop_map(Step::UnitDies),
            1 => (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(|(transient, rate_limited, timed)| Step::EffectFails { transient, rate_limited, timed }),
            1 => Just(Step::Pause),
            2 => Just(Step::Unpause),
            1 => (0u64..=6).prop_map(Step::OtherRuns),
            1 => (1u64..=6).prop_map(Step::MaxRuns),
            2 => Just(Step::PrerequisiteMerged),
            1 => prop_oneof![Just(CiMode::NoCi), Just(CiMode::Expected(vec!["rust".into()]))].prop_map(Step::Checks),
            2 => any::<u8>().prop_map(Step::Stray),
        ]
        .boxed()
    }

    fn apply(mut w: World, step: &Step) -> World {
        match step {
            Step::Pause => w.paused = true,
            Step::Unpause => w.paused = false,
            Step::OtherRuns(n) => w.other_runs = *n,
            Step::MaxRuns(n) => w.max_runs = *n,
            Step::PrerequisiteMerged => w.prerequisite_merged = true,
            Step::Checks(ci) => w.ci.clone_from(ci),
            Step::Work(_)
            | Step::Stop
            | Step::Resume
            | Step::Retry
            | Step::Cancel
            | Step::Crash { .. }
            | Step::UnitDies(_)
            | Step::EffectFails { .. }
            | Step::Stray(_) => {}
        }
        w
    }
}

/// The task's current unit: a builder or the head's reviewer.
#[derive(Debug, Clone)]
struct Unit {
    reviewer: bool,
    review_stage: Option<crate::roles::ReviewKind>,
    active: bool,
    /// The `head` file a builder's collect step wrote.
    head: Option<String>,
    /// A reviewer that gave its verdict: its end is not sent (engine rule).
    verdict_given: bool,
}

/// The forge, as the outside world.
#[derive(Debug, Clone, Default)]
struct Forge {
    /// The collected head the branch was last pushed from, and the pushed head.
    pushed: Option<(String, String)>,
    pr: Option<u64>,
    merged: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Sim {
    state: TaskState,
    world: World,
    facts: TaskFacts,
    event_id: u64,
    /// Every head that ever had a clean verdict (the merge gate).
    clean: BTreeSet<String>,
    checks_ok: BTreeSet<String>,
    /// Builder rounds the owner's retries granted past the review-round cap.
    grants: u8,
    collected: Option<String>,
    // The world.
    unit: Option<Unit>,
    forge: Forge,
    /// An effect sent under the task's lock whose reply is not recorded yet.
    flight: Option<Effect>,
    n: u32,
    now: i64,
}

const TASK: &str = "t";

/// The owner's retry of a failed task (the one command that moves it), or of
/// a stopped task on its head or kept work.
fn retried(cmd: &Command, before: &TaskState) -> bool {
    matches!(cmd, Command::Retry(_) | Command::RetryMerge)
        && matches!(before, TaskState::Failed { .. } | TaskState::Stopped { .. })
}

fn is_terminal(s: &TaskState) -> bool {
    matches!(
        s,
        TaskState::Done | TaskState::Cancelled | TaskState::Failed { .. }
    )
}

impl Sim {
    fn new(world: World) -> Self {
        Self {
            state: TaskState::Planned,
            world,
            facts: TaskFacts::default(),
            event_id: 0,
            clean: BTreeSet::new(),
            checks_ok: BTreeSet::new(),
            grants: 0,
            collected: None,
            unit: None,
            forge: Forge::default(),
            flight: None,
            n: 0,
            now: 1_000,
        }
    }

    fn review(&self) -> crate::roles::ReviewKind {
        if self.world.design {
            crate::roles::ReviewKind::Design
        } else {
            crate::roles::ReviewKind::Code
        }
    }

    fn id(&mut self, prefix: &str) -> String {
        self.n += 1;
        format!("{prefix}{}", self.n)
    }

    fn unit_active(&self) -> bool {
        self.unit.as_ref().is_some_and(|u| u.active)
    }

    fn assignment(&mut self, reviewer: bool, base: String) -> RunAssignment {
        RunAssignment {
            review_stage: reviewer.then_some(
                if self.world.design
                    && self.facts.next_review(crate::roles::ReviewKind::Design)
                        == crate::roles::ReviewKind::Design
                {
                    crate::roles::ReviewKind::Design
                } else {
                    crate::roles::ReviewKind::Code
                },
            ),
            goal_version: None,
            run: RunId::new(self.id("r")),
            task: Some(TaskId::new(TASK)),
            role: RoleId::new(if reviewer { "code-reviewer" } else { "backend" }),
            hire: HireId::new("h"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "model".into(),
            reviews: reviewer.then(|| base.clone()),
            base: Some(base),
            routine: false,
        }
    }

    /// A builder run as the engine assigns one: on the publication head once
    /// there is one, else where the last builder started.
    fn builder(&mut self) -> RunAssignment {
        let base = self.facts.head.clone().unwrap_or_else(|| {
            self.facts
                .builder_base
                .clone()
                .unwrap_or_else(|| "main".into())
        });
        self.assignment(false, base)
    }

    fn reviewer(&mut self) -> Option<RunAssignment> {
        let head = self.facts.head.clone()?;
        Some(self.assignment(true, head))
    }

    /// Whether the task's next run is the head's reviewer: once published, a
    /// builder runs only for a fix round (an open verdict on the head).
    fn needs_reviewer(&self) -> bool {
        self.facts.needs_reviewer()
    }

    /// The run a due retry, or a resume, starts.
    fn again(&mut self) -> Option<RunAssignment> {
        if self.needs_reviewer() {
            self.reviewer()
        } else {
            Some(self.builder())
        }
    }

    /// What the engine puts in the context for this command.
    fn context(&self, cmd: &Command) -> Context {
        Context {
            waiting_for: if self.world.prerequisite_merged {
                vec![]
            } else {
                vec![TaskId::new("prerequisite")]
            },
            task: TaskId::new(TASK),
            head: self.facts.head.clone(),
            goal_approved: true,
            project_paused: self.world.paused,
            max_runs: self.world.max_runs,
            caps: self
                .facts
                .caps(cmd, self.world.other_runs + u64::from(self.unit_active())),
            ci: self.world.ci.clone(),
            branch_repair: self.facts.branch_repair,
            retried: self.facts.retried,
            reviewed: self.facts.reviews_approved(self.review()),
            more_review: self.facts.more_review(cmd, self.review()),
            open_findings: self.facts.open_findings(),
            now: Timestamp(self.now),
        }
    }

    /// Folds an event into the projection, as the engine's projection does.
    fn fold(&mut self, ev: &EventKind) {
        self.event_id += 1;
        let actor = if let EventKind::VerdictRecorded { run, .. } = ev {
            crate::Actor::Run(run.clone())
        } else {
            crate::Actor::Engine
        };
        self.facts
            .fold(crate::EventId(self.event_id), &actor, ev, self.review());
        match ev {
            EventKind::VerdictRecorded { head, findings, .. } => {
                if crate::event::open_findings(findings) == 0 {
                    self.clean.insert(head.clone());
                }
            }
            EventKind::ChecksObserved { head, status, .. } => {
                if *status == ChecksStatus::Success {
                    self.checks_ok.insert(head.clone());
                }
            }
            EventKind::RunStarted(a) => {
                self.unit = Some(Unit {
                    reviewer: a.reviews.is_some(),
                    review_stage: a.review_stage,
                    active: true,
                    head: None,
                    verdict_given: false,
                });
            }
            EventKind::TaskStarted { .. }
            | EventKind::BranchPublished { .. }
            | EventKind::BranchUpdated { .. }
            | EventKind::MergeRefused { .. }
            | EventKind::ChecksPending { .. }
            | EventKind::ChecksStuck { .. }
            | EventKind::EffectRetry { .. }
            | EventKind::TaskStateChanged { .. }
            | EventKind::EngineStarted { .. }
            | EventKind::RepositoryCreated { .. }
            | EventKind::RepositorySetupPrepared { .. }
            | EventKind::GoalIssueClosed { .. }
            | EventKind::ProposalRequested { .. }
            | EventKind::ProjectIssuesFound(..)
            | EventKind::ProjectRulesetMissing {}
            | EventKind::ProjectRulesetInstalled {}
            | EventKind::ProjectRepoChanged { .. }
            | EventKind::ProjectRepoUnreachable { .. }
            | EventKind::ProjectRemoved { .. }
            | EventKind::GithubMirrorObserved { .. }
            | EventKind::EngineUpdated { .. }
            | EventKind::EngineUpdateFailed { .. }
            | EventKind::EngineReconciled { .. }
            | EventKind::ProjectAdded { .. }
            | EventKind::ProjectPaused {}
            | EventKind::ProjectResumed {}
            | EventKind::ProjectSeen {}
            | EventKind::ProjectBriefSet { .. }
            | EventKind::RoleAdded(_)
            | EventKind::RoleChanged { .. }
            | EventKind::RoleRemoved(_)
            | EventKind::HireAdded(_)
            | EventKind::HireChanged { .. }
            | EventKind::HireRetired(_)
            | EventKind::SettingsChanged { .. }
            | EventKind::GoalStated { .. }
            | EventKind::PlanProposed { .. }
            | EventKind::PlanCancelled { .. }
            | EventKind::GoalApproved { .. }
            | EventKind::GoalDone { .. }
            | EventKind::GoalDropped { .. }
            | EventKind::GoalAfterSet { .. }
            | EventKind::QuestionOpened { .. }
            | EventKind::QuestionAnswered { .. }
            | EventKind::QuestionClosed { .. }
            | EventKind::DecisionRecorded { .. }
            | EventKind::CommandRefused(_)
            | EventKind::TaskWaiting { .. }
            | EventKind::TaskStopped { .. }
            | EventKind::TaskResumed { .. }
            | EventKind::TaskRetryTextSet { .. }
            | EventKind::TaskReasonChanged { .. }
            | EventKind::TaskCancelled { .. }
            | EventKind::TaskDone { .. }
            | EventKind::TaskFailed { .. }
            | EventKind::RunPreflight { .. }
            | EventKind::RunModel { .. }
            | EventKind::RunOutput { .. }
            | EventKind::RunUsage { .. }
            | EventKind::RunCollected { .. }
            | EventKind::RunSurvivedRestart { .. }
            | EventKind::RunStopRequested { .. }
            | EventKind::RunLaunched { .. }
            | EventKind::RunTimeLimitReached { .. }
            | EventKind::RunEnded { .. }
            | EventKind::OnlyStarted(_)
            | EventKind::OnlyEnded { .. }
            | EventKind::RunRerouted { .. }
            | EventKind::VendorLimited { .. }
            | EventKind::VendorResumed { .. }
            | EventKind::ScanHit { .. }
            | EventKind::PrOpened { .. }
            | EventKind::PrMerged { .. } => {}
        }
    }

    /// Decides a command as the engine does: the effect's own events (`pre`) are
    /// recorded with the decision unless it is refused; then the postconditions.
    fn command(&mut self, cmd: &Command, pre: Vec<EventKind>) -> Result<(), Code> {
        let before = self.state.clone();
        let ctx = self.context(cmd);
        let events = match decide(&before, cmd.clone(), &ctx) {
            Ok(events) => events,
            Err(r) => {
                self.refused(&before, cmd, &r.code);
                return Err(r.code);
            }
        };
        for ev in pre {
            self.fold(&ev);
        }
        // At most one transition, first, from the state the task was in.
        let moves: Vec<&EventKind> = events
            .iter()
            .filter(|e| matches!(e, EventKind::TaskStateChanged { .. }))
            .collect();
        assert!(moves.len() <= 1, "{events:?}");
        if let Some(EventKind::TaskStateChanged { task, from, .. }) = moves.first() {
            assert_eq!(task.as_str(), TASK);
            assert_eq!(*from, before);
            assert!(matches!(events[0], EventKind::TaskStateChanged { .. }));
        }
        for ev in &events {
            self.fold(ev);
        }
        let after = events.iter().fold(before.clone(), apply);
        if retried(cmd, &before)
            && events
                .iter()
                .any(|e| matches!(e, EventKind::TaskStarted { .. }))
        {
            self.grants += 1;
        }
        self.accepted(&before, cmd, &ctx, &events, &after);
        self.state = after;
        Ok(())
    }

    /// The postconditions of a refusal.
    fn refused(&self, before: &TaskState, cmd: &Command, code: &Code) {
        if is_terminal(before) && !retried(cmd, before) {
            assert_eq!(*code, Code::AlreadyTerminal, "{cmd:?} in {before:?}");
        }
        if matches!(cmd, Command::Stop(_)) {
            let before = match before {
                TaskState::Unknown { last } => &**last,
                s @ (TaskState::Planned
                | TaskState::Preparing
                | TaskState::Running
                | TaskState::Collecting
                | TaskState::Publishing
                | TaskState::InReview
                | TaskState::AwaitingChecks
                | TaskState::Merging
                | TaskState::RetryPending { .. }
                | TaskState::Stopped { .. }
                | TaskState::Done
                | TaskState::Cancelled
                | TaskState::Failed { .. }) => s,
            };
            match before {
                TaskState::Stopped { .. } => assert_eq!(*code, Code::AlreadyStopped),
                TaskState::Done | TaskState::Cancelled | TaskState::Failed { .. } => {}
                s @ (TaskState::Planned
                | TaskState::Preparing
                | TaskState::Running
                | TaskState::Collecting
                | TaskState::Publishing
                | TaskState::InReview
                | TaskState::AwaitingChecks
                | TaskState::Merging
                | TaskState::RetryPending { .. }
                | TaskState::Unknown { .. }) => panic!("stop refused in {s:?}: {code:?}"),
            }
        }
    }

    /// The postconditions of an accepted command.
    fn accepted(
        &self,
        before: &TaskState,
        cmd: &Command,
        ctx: &Context,
        events: &[EventKind],
        after: &TaskState,
    ) {
        use TaskState as S;
        let moved = before != after;
        // An ended task never moves, except a failed one the owner retries.
        if is_terminal(before) && !retried(cmd, before) {
            assert!(events.is_empty(), "{cmd:?} moved {before:?}");
        }
        // A retry moves only a failed task, to a new run, or an approved head
        // back to its checks and merge; or a stopped task that started, to a
        // builder on its head or its kept work.
        if let Command::Retry(_) = cmd {
            let failed = matches!(before, S::Failed { .. })
                && matches!(after, S::Preparing | S::InReview | S::Publishing);
            let builder = matches!(before, S::Stopped { was } if **was != S::Planned)
                && *after == S::Preparing;
            assert!(failed || builder, "retry of {before:?} went to {after:?}");
        }
        if let Command::RetryMerge = cmd {
            assert!(matches!(before, S::Failed { .. }), "retry of {before:?}");
            assert!(ctx.reviewed, "retried an unapproved head into its merge");
            assert!(
                matches!(after, S::AwaitingChecks | S::Merging),
                "retry went to {after:?}"
            );
        }
        // Only resume, retry and cancel move a stopped task, and a restart, to unknown
        // until its unit is checked.
        if matches!(before, S::Stopped { .. }) && moved {
            let restarted = matches!(cmd, Command::Restarted)
                && *after
                    == S::Unknown {
                        last: Box::new(before.clone()),
                    };
            assert!(
                restarted
                    || matches!(
                        cmd,
                        Command::Resume(_) | Command::Retry(_) | Command::Cancel
                    ),
                "{cmd:?} moved {before:?} to {after:?}"
            );
        }
        // Receipts (a head, a verdict) keep a task unknown; a confirmed end,
        // reconcile or stop may settle it. Reconcile claims a running unit only
        // when it saw one, or a review once its fix/intermediate verdict came.
        if matches!(before, S::Unknown { .. }) && moved {
            assert!(matches!(
                cmd,
                Command::Reconcile(_)
                    | Command::Stop(_)
                    | Command::UnitEnded { .. }
                    | Command::HeadWritten
                    | Command::VerdictRecorded { .. }
            ));
            if matches!(cmd, Command::HeadWritten | Command::VerdictRecorded { .. }) {
                assert!(matches!(after, S::Unknown { .. }));
            }
            if let Command::Reconcile(seen) = cmd
                && matches!(
                    after,
                    S::Preparing | S::Running | S::Collecting | S::InReview
                )
                && !(*after == S::InReview && (ctx.open_findings > 0 || ctx.more_review))
            {
                assert_eq!(seen.unit, UnitState::Active);
            }
        }
        // A verdict stops the task only at the round cap, as it was.
        if let (Command::VerdictRecorded { .. }, S::Stopped { was }) = (cmd, after) {
            assert_eq!(**was, S::InReview);
            assert!(self.facts.round >= caps::REVIEW_ROUNDS_PER_PR);
        }
        if let Command::Stop(_) = cmd {
            let was = if let S::Unknown { last } = before {
                (**last).clone()
            } else {
                before.clone()
            };
            assert_eq!(*after, S::Stopped { was: Box::new(was) });
        }
        if let (Command::Resume(_), S::Stopped { was }) = (cmd, before) {
            let ok = match (&**was, after) {
                (
                    S::Preparing
                    | S::Running
                    | S::Collecting
                    | S::Publishing
                    | S::AwaitingChecks
                    | S::Merging,
                    S::Preparing,
                )
                | (S::InReview, S::InReview) => true,
                (S::RetryPending { effect, .. }, next) => match effect_state(*effect) {
                    Some(s) => s == *next,
                    None => matches!(next, S::Preparing | S::InReview),
                },
                (was, next) => was == next,
            };
            assert!(ok, "resume of {was:?} went to {after:?}");
        }
        if *after == S::Cancelled && moved {
            assert!(
                *before == S::Planned
                    || *before
                        == S::Stopped {
                            was: Box::new(S::Planned)
                        }
            );
        }
        // A run starts only when the goal is approved, the project is not
        // paused and a slot is free; an automatic start only within the cap.
        if events.iter().any(|e| matches!(e, EventKind::RunStarted(_))) {
            assert!(ctx.goal_approved && !ctx.project_paused && ctx.waiting_for.is_empty());
            assert!(ctx.caps.concurrent_runs < ctx.max_runs);
            assert!(matches!(
                cmd,
                Command::Start(_)
                    | Command::RepairBranch(_)
                    | Command::Resume(Some(_))
                    | Command::Retry(Some(_))
            ));
            if matches!(cmd, Command::Start(_)) {
                assert!(ctx.caps.attempts < caps::ATTEMPTS_PER_TASK);
            }
        }
        // A builder run after the first publication builds on the head.
        for ev in events {
            if let EventKind::RunStarted(a) = ev
                && a.reviews.is_none()
                && self.facts.head.is_some()
            {
                assert_eq!(a.base, self.facts.head);
            }
        }
        // The merge gate: a clean verdict on the head, CI success
        // on it unless the owner chose no CI, within the round cap and the
        // rounds the owner's retries granted.
        if *after == S::Merging && moved {
            let head = self.facts.head.clone().expect("a head to merge");
            assert!(self.clean.contains(&head), "{cmd:?}: {head} not reviewed");
            // Resume/reconcile/retry can restore a formerly ready state after the
            // owner changed CI. Dispatch observes checks again before merging.
            if self.world.ci != CiMode::NoCi
                && matches!(
                    cmd,
                    Command::VerdictRecorded { .. } | Command::ChecksObserved { .. }
                )
            {
                assert!(self.checks_ok.contains(&head), "{cmd:?}: no CI on {head}");
            }
            if self.world.design {
                assert!(
                    self.facts.reviews_approved(self.review()),
                    "{cmd:?}: both reviews required"
                );
            }
            assert!(self.facts.round <= caps::REVIEW_ROUNDS_PER_PR + self.grants);
        }
        // Done only on a merge the forge has.
        if *after == S::Done && moved {
            assert!(self.forge.merged.is_some(), "{cmd:?} from {before:?}");
        }
        // Retries are bounded.
        if let S::RetryPending { attempt, .. } = after {
            assert!(*attempt <= caps::RETRIES_PER_EFFECT.max(caps::ATTEMPTS_PER_TASK));
        }
    }

    fn published(&mut self, head: String) {
        let pre = vec![EventKind::BranchPublished {
            task: TaskId::new(TASK),
            round: self.facts.round + u8::from(!self.facts.branch_repair),
            branch: "dvb/t".into(),
            head,
        }];
        let _ = self.command(&Command::Published, pre);
    }

    /// Lock-taking steps first record the reply of the effect in flight.
    fn land(&mut self, pick: u8) {
        let Some(effect) = self.flight.take() else {
            return;
        };
        match effect {
            Effect::Push => {
                let head = self.id("p");
                self.forge.pushed =
                    Some((self.collected.clone().unwrap_or_default(), head.clone()));
                self.published(head);
            }
            Effect::OpenPr => {
                self.forge.pr = Some(1);
                let _ = self.command(&Command::PrOpened, vec![]);
            }
            Effect::Merge => {
                if pick.is_multiple_of(4) && self.forge.merged.is_none() {
                    // Pushed from outside dvb: the PR's head is a new one,
                    // recorded as the task's (`publication_moved`).
                    let moved = self.id("x");
                    let pre = vec![EventKind::BranchPublished {
                        task: TaskId::new(TASK),
                        round: self.facts.round,
                        branch: "dvb/t".into(),
                        head: moved,
                    }];
                    let _ = self.command(&Command::MergeRefused("head moved".into()), pre);
                } else {
                    let sha = self.forge.merged.clone().unwrap_or_else(|| self.id("m"));
                    self.forge.merged = Some(sha);
                    let _ = self.command(&Command::MergeDone, vec![]);
                }
            }
            Effect::StartUnit
            | Effect::StopUnit
            | Effect::ReadChecks
            | Effect::CloseIssue
            | Effect::DeleteBranch => {}
        }
    }

    /// The current unit ends; the engine tells `decide`, except for a reviewer
    /// that gave its verdict.
    fn unit_ended(&mut self, reason: EndReason) {
        if let Some(u) = &mut self.unit
            && u.active
        {
            u.active = false;
            if !u.verdict_given {
                let _ = self.command(
                    &Command::UnitEnded {
                        reason,
                        detail: None,
                        stderr: None,
                    },
                    vec![],
                );
            }
        }
    }

    fn verdict(&mut self, pick: u8) {
        let head = if pick.is_multiple_of(7) {
            "old".to_owned()
        } else {
            self.facts.head.clone().unwrap_or_default()
        };
        let open = u32::from(pick.is_multiple_of(2));
        let finding = Finding {
            severity: Severity::High,
            file: "f".into(),
            line: None,
            title: "t".into(),
            detail: "d".into(),
            open: true,
        };
        let pre = vec![EventKind::VerdictRecorded {
            review_stage: self.unit.as_ref().and_then(|u| u.review_stage),
            verdict: VerdictId::new("v"),
            task: TaskId::new(TASK),
            head: head.clone(),
            run: self.facts.run.clone().expect("current reviewer"),
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            findings: (0..open).map(|_| finding.clone()).collect(),
            summary: "s".into(),
        }];
        if self
            .command(&Command::VerdictRecorded { head, open }, pre)
            .is_ok()
            && let Some(u) = &mut self.unit
        {
            u.verdict_given = true;
        }
        self.unit_ended(EndReason::Completed);
    }

    fn work(&mut self, pick: u8) {
        use TaskState as S;
        self.now += 1_000;
        if self.flight.is_some() {
            return self.land(pick);
        }
        if is_terminal(&self.state) {
            // The next task of the plan: the walk goes on from a fresh one.
            let (n, now) = (self.n, self.now);
            *self = Sim {
                n,
                now,
                ..Sim::new(self.world.clone())
            };
            return;
        }
        let paused = self.world.paused;
        match self.state.clone() {
            S::Planned => {
                let a = self.builder();
                let _ = self.command(&Command::Start(a), vec![]);
            }
            S::Preparing => {
                let _ = self.command(&Command::UnitActive, vec![]);
            }
            S::Running => {
                if pick % 3 != 2 {
                    let head = self.id("c");
                    if let Some(u) = &mut self.unit {
                        u.head = Some(head.clone());
                    }
                    self.collected = Some(head);
                    let _ = self.command(&Command::HeadWritten, vec![]);
                }
            }
            S::Collecting => self.unit_ended(EndReason::Completed),
            S::Publishing if !paused => {
                let collected = self.collected.clone().unwrap_or_default();
                match self.forge.pushed.clone() {
                    // Pushed already (a reply was lost): the branch says so.
                    Some((from, head)) if from == collected => {
                        if self.facts.head.as_ref() != Some(&head) {
                            self.published(head);
                        } else if self.forge.pr.is_some() {
                            let _ = self.command(&Command::PrOpened, vec![]);
                        } else {
                            self.flight = Some(Effect::OpenPr);
                        }
                    }
                    Some(_) | None => self.flight = Some(Effect::Push),
                }
            }
            S::InReview if !paused => {
                let reviewing = self
                    .unit
                    .as_ref()
                    .is_some_and(|u| u.active && u.reviewer && !u.verdict_given);
                if reviewing {
                    self.verdict(pick);
                } else if !self.needs_reviewer() {
                    // Fix-first: the engine starts a builder run on the head.
                    let a = self.builder();
                    let _ = self.command(&Command::Start(a), vec![]);
                } else if self.world.other_runs < self.world.max_runs
                    && let Some(a) = self.reviewer()
                {
                    // The engine starts the head's reviewer (no decision).
                    self.fold(&EventKind::RunStarted(a));
                }
            }
            S::AwaitingChecks => {
                let status = match pick % 5 {
                    0 => ChecksStatus::Pending,
                    2 => ChecksStatus::Missing,
                    4 => ChecksStatus::Failed,
                    _ => ChecksStatus::Success,
                };
                let head = self.facts.head.clone().unwrap_or_default();
                let pre = vec![EventKind::ChecksObserved {
                    retry_question: None,
                    retry_runs: vec![],
                    task: TaskId::new(TASK),
                    head: head.clone(),
                    status,
                    names: vec!["rust".into()],
                }];
                let _ = self.command(&Command::ChecksObserved { head, status }, pre);
            }
            S::Merging if !paused => {
                if self.forge.merged.is_some() {
                    // The merge effect reads the PR first: merged already.
                    let _ = self.command(&Command::MergeDone, vec![]);
                } else {
                    // Like dispatch::merge, observe the current choice before
                    // sending a merge restored by resume/reconcile/retry.
                    let head = self.facts.head.clone().unwrap_or_default();
                    let status = if self.checks_ok.contains(&head) {
                        ChecksStatus::Success
                    } else {
                        ChecksStatus::Missing
                    };
                    let _ = self.command(&Command::ChecksObserved { head, status }, vec![]);
                    if self.state == S::Merging {
                        self.flight = Some(Effect::Merge);
                    }
                }
            }
            S::RetryPending { effect, .. } if !paused => {
                if effect == Effect::StartUnit {
                    if let Some(a) = self.again() {
                        let _ = self.command(&Command::Start(a), vec![]);
                    }
                } else {
                    let _ = self.command(&Command::RetryDue, vec![]);
                }
            }
            S::Unknown { .. } => {
                // A builder may report collection while its unit cannot be
                // queried. The receipt must not claim that it is active.
                if pick.is_multiple_of(3)
                    && self
                        .unit
                        .as_ref()
                        .is_some_and(|u| u.active && !u.reviewer && u.head.is_none())
                {
                    let head = self.id("c");
                    self.unit.as_mut().unwrap().head = Some(head.clone());
                    self.collected = Some(head);
                    let _ = self.command(&Command::HeadWritten, vec![]);
                    return;
                }
                let seen = Observed {
                    unit: if self.unit_active() {
                        UnitState::Active
                    } else {
                        UnitState::Inactive
                    },
                    head: self
                        .unit
                        .as_ref()
                        .filter(|u| !u.reviewer)
                        .and_then(|u| u.head.clone()),
                };
                let _ = self.command(&Command::Reconcile(seen), vec![]);
                // A unit that outlived a stop is stopped again.
                if matches!(self.state, S::Stopped { .. }) {
                    self.unit_ended(EndReason::Stopped);
                }
            }
            S::Publishing
            | S::InReview
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => {}
        }
    }

    fn resume(&mut self) {
        use TaskState as S;
        let run = match self.state.clone() {
            S::Stopped { was } => match *was {
                S::Preparing | S::Running | S::Collecting => Some(self.builder()),
                S::InReview => self.reviewer(),
                S::RetryPending {
                    effect: Effect::StartUnit,
                    ..
                } => self.again(),
                S::Planned
                | S::Publishing
                | S::AwaitingChecks
                | S::Merging
                | S::RetryPending { .. }
                | S::Stopped { .. }
                | S::Unknown { .. }
                | S::Done
                | S::Cancelled
                | S::Failed { .. } => None,
            },
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => None,
        };
        let _ = self.command(&Command::Resume(run), vec![]);
    }

    fn step(&mut self, step: &Step) {
        match *step {
            Step::Work(pick) => self.work(pick),
            Step::Stop => {
                // Recorded first; then the lock: the reply in flight lands, and
                // the task's unit is stopped.
                if self.command(&Command::Stop(None), vec![]).is_ok() {
                    self.land(1);
                    self.unit_ended(EndReason::Stopped);
                }
            }
            Step::Resume => {
                self.land(1);
                self.resume();
            }
            Step::Retry => {
                self.land(1);
                let failed = matches!(self.state, TaskState::Failed { .. });
                let cmd = if failed && self.facts.head.is_some() && self.forge.pr.is_none() {
                    // Pushed without a PR: publishing opens it first.
                    Command::Retry(None)
                } else if failed
                    && self.needs_reviewer()
                    && self.facts.reviews_approved(self.review())
                {
                    Command::RetryMerge
                } else if failed {
                    Command::Retry(self.again())
                } else {
                    Command::Retry(Some(self.builder()))
                };
                let _ = self.command(&cmd, vec![]);
            }
            Step::Cancel => {
                self.land(1);
                let _ = self.command(&Command::Cancel, vec![]);
            }
            Step::Crash { reboot } => {
                if let Some(effect) = self.flight.take()
                    && !reboot
                {
                    self.acted(effect);
                }
                if reboot && let Some(u) = &mut self.unit {
                    u.active = false;
                }
                let _ = self.command(&Command::Restarted, vec![]);
            }
            Step::UnitDies(pick) => {
                let reason = match pick % 8 {
                    0 => EndReason::Crashed,
                    1 => EndReason::Oom,
                    2 => EndReason::Interrupted,
                    3 => EndReason::Stopped,
                    4 => EndReason::Mismatch,
                    5 => EndReason::ParseError,
                    6 => EndReason::TimeLimit,
                    _ => EndReason::Preflight(crate::event::PreflightCode::PfLoginMissing),
                };
                self.unit_ended(reason);
            }
            Step::EffectFails {
                transient,
                rate_limited,
                timed,
            } => match self.flight.take() {
                Some(effect) => {
                    if transient {
                        self.acted(effect);
                    }
                    let _ = self.command(
                        &Command::EffectFailed {
                            rate_limit: rate_limited.then_some(crate::forge::RateLimit {
                                reset: timed.then_some(Timestamp(self.now).plus_millis(10_000)),
                            }),
                            effect,
                            transient,
                            detail: "502".into(),
                        },
                        vec![],
                    );
                }
                None if self.state == TaskState::Preparing => {
                    // The unit did not start.
                    if let Some(u) = &mut self.unit {
                        u.active = false;
                    }
                    let _ = self.command(
                        &Command::EffectFailed {
                            rate_limit: None,
                            effect: Effect::StartUnit,
                            transient,
                            detail: "dbus".into(),
                        },
                        vec![],
                    );
                }
                None => {}
            },
            Step::Pause
            | Step::Unpause
            | Step::OtherRuns(_)
            | Step::MaxRuns(_)
            | Step::Checks(_)
            | Step::PrerequisiteMerged => {}
            Step::Stray(pick) => {
                self.land(1);
                let cmd = match pick % 10 {
                    0 => Command::Start(self.assignment(false, "main".into())),
                    1 => Command::UnitActive,
                    2 => Command::RetryDue,
                    3 => Command::Published,
                    4 => Command::VerdictRecorded {
                        head: "old".into(),
                        open: 0,
                    },
                    5 => Command::ChecksObserved {
                        head: "old".into(),
                        status: ChecksStatus::Success,
                    },
                    6 => Command::Resume(None),
                    7 => Command::NoChanges,
                    8 => Command::RepairBranch(self.builder()),
                    _ => Command::Resume(Some(self.builder())),
                };
                let _ = self.command(&cmd, vec![]);
            }
        }
    }

    /// The forge acted although its reply was lost.
    fn acted(&mut self, effect: Effect) {
        match effect {
            Effect::Push => {
                let head = self.id("p");
                self.forge.pushed = Some((self.collected.clone().unwrap_or_default(), head));
            }
            Effect::OpenPr => self.forge.pr = Some(1),
            Effect::Merge => {
                let sha = self.id("m");
                self.forge.merged = Some(sha);
            }
            Effect::StartUnit
            | Effect::StopUnit
            | Effect::ReadChecks
            | Effect::CloseIssue
            | Effect::DeleteBranch => {}
        }
    }
}

impl StateMachineTest for Sim {
    type SystemUnderTest = Sim;
    type Reference = WorldMachine;

    fn init_test(world: &World) -> Sim {
        Sim::new(world.clone())
    }

    fn apply(mut sim: Sim, world: &World, step: Step) -> Sim {
        sim.world = world.clone();
        sim.step(&step);
        sim
    }

    fn check_invariants(sim: &Sim, _: &World) {
        use TaskState as S;
        // A builder state has its builder unit.
        if matches!(sim.state, S::Preparing | S::Running | S::Collecting) {
            assert!(sim.unit.as_ref().is_some_and(|u| !u.reviewer), "{sim:?}");
        }
        // An effect is in flight only in its own state, or after a stop
        // recorded while it was.
        if let Some(effect) = sim.flight
            && let Some(state) = effect_state(effect)
        {
            assert!(
                sim.state == state || matches!(sim.state, S::Stopped { .. }),
                "{effect:?} in flight in {:?}",
                sim.state
            );
        }
    }

    /// Liveness: left alone (unpaused, slots free, good outcomes), the task
    /// ends; a stopped task carries on once resumed.
    fn teardown(mut sim: Sim, _: World) {
        sim.world.paused = false;
        sim.world.other_runs = 0;
        sim.world.prerequisite_merged = true;
        for _ in 0..300 {
            if is_terminal(&sim.state) {
                return;
            }
            // The round cap's stop is final for resume: the owner
            // stops the task or redirects the orchestrator.
            let cap_stop = sim.state
                == TaskState::Stopped {
                    was: Box::new(TaskState::InReview),
                }
                && sim.facts.round >= caps::REVIEW_ROUNDS_PER_PR
                && sim.facts.open_findings() > 0;
            if cap_stop {
                return;
            }
            if matches!(sim.state, TaskState::Stopped { .. }) {
                sim.step(&Step::Resume);
            } else {
                sim.step(&Step::Work(1));
            }
        }
        panic!("stuck in {:?}: {sim:?}", sim.state);
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn work_stop_resume_pause_crash_and_retry(sequential 1..200 => Sim);
}
