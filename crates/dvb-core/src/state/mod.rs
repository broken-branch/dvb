//! Pure task decisions and event application. Every `(state, command)` pair
//! has a named match arm; clippy denies wildcard enum arms.
//!
//! Transition tests and the property model check work, stop, resume, pause,
//! crash and retry sequences against the state invariants.

mod facts;
use crate::wording::{effect_words, end_words};
pub use facts::{TaskFacts, Verdict};

use crate::caps;
use crate::clock::Timestamp;
use crate::event::{ChecksStatus, CiMode, EndReason, EventKind, RunAssignment, RunKind};
use crate::ids::TaskId;
use crate::launcher::UnitState;
use crate::refusal::{Cap, Refusal, RefusalCode};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// An external effect the engine performs, retried on its own schedule.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    StartUnit,
    StopUnit,
    Push,
    OpenPr,
    ReadChecks,
    Merge,
    CloseIssue,
    DeleteBranch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskState {
    Planned,
    Preparing,
    Running,
    Collecting,
    Publishing,
    InReview,
    AwaitingChecks,
    Merging,
    RetryPending {
        effect: Effect,
        attempt: u8,
    },
    /// Resumable; `was` is where it stopped.
    Stopped {
        #[schema(no_recursion)]
        was: Box<TaskState>,
    },
    /// Until observed again after a restart or a lost unit query.
    Unknown {
        #[schema(no_recursion)]
        last: Box<TaskState>,
    },
    Done,
    /// Terminal; from "change course", or a failure whose goal was dropped.
    Cancelled,
    /// Ended, unless the owner retries it (`Retry`).
    Failed {
        reason: String,
    },
}

/// An approved goal fails when all remaining tasks ended, with a stopped or failed task.
/// Cancelled tasks do not participate; an empty or fully Done plan has not failed.
pub fn goal_failed<'a>(approved: bool, states: impl IntoIterator<Item = &'a TaskState>) -> bool {
    let mut bad = false;
    for state in states {
        match state {
            TaskState::Failed { .. } | TaskState::Stopped { .. } => bad = true,
            TaskState::Done | TaskState::Cancelled => {}
            TaskState::Planned
            | TaskState::Preparing
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::InReview
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Unknown { .. } => return false,
        }
    }
    approved && bad
}

/// What reconcile observed for a task after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub unit: UnitState,
    /// The `head` file the run's collect step wrote, if any.
    pub head: Option<String>,
}

/// What happens to a task. Ids and run metadata are chosen by the engine before
/// `decide` is called (allocation stays outside the pure function).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start the task's run. The assignment's `task` is the task being decided.
    Start(RunAssignment),
    /// Engine repair of a conflicting publication, without a review round.
    RepairBranch(RunAssignment),
    UnitActive,
    HeadWritten,
    /// With a crashed run's `run.ended` detail and stderr, which a failure
    /// after the last attempt names.
    UnitEnded {
        reason: EndReason,
        detail: Option<String>,
        stderr: Option<String>,
    },
    Published,
    PrOpened,
    /// The builder collected its recorded base tree: nothing to publish.
    NoChanges,
    VerdictRecorded {
        head: String,
        open: u32,
    },
    ChecksObserved {
        head: String,
        status: ChecksStatus,
    },
    MergeDone,
    MergeRefused(String),
    /// An engine effect failed: `transient` (network, rate limit, 5xx, a D-Bus
    /// hiccup) retries after a backoff within `caps::RETRIES_PER_EFFECT`;
    /// otherwise the task fails with `detail`.
    EffectFailed {
        rate_limit: Option<crate::forge::RateLimit>,
        effect: Effect,
        transient: bool,
        detail: String,
    },
    /// Stop the task; the reason, when the engine stops it, is shown with it.
    Stop(Option<String>),
    /// Resume a stopped task. Carries the new run when the state it stopped in
    /// needs one (a stopped run restarts in its preserved workspace); `None` when
    /// it doesn't (e.g. stopped while in review).
    Resume(Option<RunAssignment>),
    /// Retry a failed task: a new run, the builder in its preserved workspace
    /// or on the head, or the head's reviewer, as a due run retry would start.
    /// A stopped task is retried only as a builder: on its head, or on its
    /// kept, unpublished work when it has no head.
    /// `None` when the engine chose no run (a pushed head with no PR goes back
    /// to publishing; a task that is not failed is refused).
    Retry(Option<RunAssignment>),
    /// Retry a failed task whose head every review approved and whose CI did
    /// not fail: no run; back to its checks and merge, which bring the branch
    /// up to date first.
    RetryMerge,
    Cancel,
    RetryDue,
    /// The engine started again (or lost a unit query): a task with a unit or an
    /// effect in flight is `Unknown` until observed again.
    Restarted,
    Reconcile(Observed),
}

impl Command {
    /// Whether this command consumes reviewer rather than builder attempts.
    pub fn reviewer_attempt(&self, needs_reviewer: bool) -> bool {
        match self {
            Command::Start(a)
            | Command::RepairBranch(a)
            | Command::Resume(Some(a))
            | Command::Retry(Some(a)) => a.reviews.is_some(),
            Command::UnitEnded { .. } | Command::Reconcile(_) => needs_reviewer,
            Command::UnitActive
            | Command::HeadWritten
            | Command::Published
            | Command::PrOpened
            | Command::NoChanges
            | Command::VerdictRecorded { .. }
            | Command::ChecksObserved { .. }
            | Command::MergeDone
            | Command::MergeRefused(_)
            | Command::EffectFailed { .. }
            | Command::Stop(_)
            | Command::Resume(None)
            | Command::Retry(None)
            | Command::RetryMerge
            | Command::Cancel
            | Command::RetryDue
            | Command::Restarted => false,
        }
    }
    /// The effect whose failure count this command reads.
    pub fn retry_effect(&self) -> Option<Effect> {
        match self {
            Command::EffectFailed { effect, .. } => Some(*effect),
            Command::MergeRefused(_) => Some(Effect::Merge),
            Command::Start(_)
            | Command::RepairBranch(_)
            | Command::UnitActive
            | Command::HeadWritten
            | Command::UnitEnded { .. }
            | Command::Published
            | Command::PrOpened
            | Command::NoChanges
            | Command::VerdictRecorded { .. }
            | Command::ChecksObserved { .. }
            | Command::MergeDone
            | Command::Stop(_)
            | Command::Resume(_)
            | Command::Retry(_)
            | Command::RetryMerge
            | Command::Cancel
            | Command::RetryDue
            | Command::Restarted
            | Command::Reconcile(_) => None,
        }
    }
}

/// How much of each cap is used (owner run limit and `dvb_core::caps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapsUsed {
    pub concurrent_runs: u64,
    pub attempts: u8,
    pub review_rounds: u8,
    pub retries: u8,
    /// Untimed GitHub waits advance backoff without consuming failure retries.
    pub rate_limit_retries: u8,
}

/// What `decide` may read besides the state; built by the engine under the
/// projection lock, with one clock read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// Unmerged task prerequisites. All worker admissions enforce these.
    pub waiting_for: Vec<TaskId>,
    /// The task being decided; every `task.*` event `decide` writes names it.
    pub task: TaskId,
    /// The task's current publication head, once published. Head-bearing commands
    /// (`VerdictRecorded`, `ChecksObserved`) for another head are
    /// stale and refused with `HEAD_MISMATCH`.
    pub head: Option<String>,
    pub goal_approved: bool,
    pub project_paused: bool,
    pub caps: CapsUsed,
    pub max_runs: u64,
    pub ci: CiMode,
    pub branch_repair: bool,
    /// The builder the owner's retry of the failed task started, until it
    /// publishes (`TaskFacts::retried`): it may build past the review-round cap.
    pub retried: bool,
    /// Every review stage approved the current head (`TaskFacts::reviews_approved`).
    pub reviewed: bool,
    /// A clean intermediate verdict waits for another stage. During reconcile,
    /// the current reviewer already approved and the next stage is still due.
    pub more_review: bool,
    /// Findings still open in the verdict on the current head (0: none, or no
    /// verdict on it). With the round cap used up, a task stopped in review
    /// by this verdict has no review left to resume.
    pub open_findings: u32,
    pub now: Timestamp,
}

/// Decide what a command does to a task in a state: the events to append, or a
/// coded refusal. Every transition writes one `task.state { task: ctx.task, from,
/// to }` first; a run start also writes `run.started` with the assignment, and a
/// builder run `task.started { task, run, resume }`.
///
/// Task commands and project pause follow these rules:
///
/// - **Owner and run commands** (`Start`, `Stop`, `Resume`, `Retry`,
///   `RetryMerge`, `Cancel`, `VerdictRecorded`) are refused with a code when
///   they don't apply: `ALREADY_STOPPED`, `ALREADY_TERMINAL`, `HEAD_MISMATCH`,
///   or `VALUE_CHANGED` when the task has moved on (or is unknown until
///   reconciled).
/// - **Observations** of an effect or a unit (`UnitActive`, `HeadWritten`,
///   `UnitEnded`, `Published`, `PrOpened`, `ChecksObserved`, `MergeDone`,
///   `MergeRefused`, `EffectFailed`, `RetryDue`, `Reconcile`, `Restarted`) change
///   the state only in the state they belong to. Anywhere else they are stale (an
///   effect that completed after a stop was recorded, a timer after a resume) and
///   decide nothing; the engine still records the effect's own event as evidence.
///   A stopped task is never moved by an observation: resume re-enters the
///   state it stopped in, and that state's effect looks at the forge first.
/// - **Pause and unpause** are a project's flag (`project.paused`), read here as
///   `ctx.project_paused`; they move no task.
///
/// The engine writes an effect's own event (`run.ended`, `branch.published`,
/// `pr.opened`, `verdict.recorded`, `pr.merged`, `checks.observed`,
/// `merge.refused`); `decide` writes the state, `task.*`, `effect.retry` and
/// `engine.reconciled`.
pub fn decide(state: &TaskState, cmd: Command, ctx: &Context) -> Result<Vec<EventKind>, Refusal> {
    use TaskState as S;
    match cmd {
        Command::Start(run) => match state {
            S::Planned => start_builder(state, run, ctx, false, true),
            // A due run retry past the cap (resumes count too) ends the task
            // rather than leave it waiting for a start that is never allowed.
            S::RetryPending {
                effect: Effect::StartUnit,
                ..
            } if ctx.caps.attempts >= caps::ATTEMPTS_PER_TASK => Ok(failed(
                ctx,
                state,
                &format!("{} attempts made", ctx.caps.attempts),
            )),
            S::RetryPending {
                effect: Effect::StartUnit,
                ..
            } => retry_run(state, run, ctx, true),
            S::InReview => fix_round(state, run, ctx),
            S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. } => Err(changed("the task is already started")),
            S::Stopped { .. } => Err(already_stopped()),
            S::Unknown { .. } => Err(unknown()),
            S::Done | S::Cancelled | S::Failed { .. } => Err(terminal()),
        },
        Command::RepairBranch(run) => match state {
            S::InReview | S::AwaitingChecks | S::Merging => {
                if ctx.head.is_none() {
                    return Err(changed("no branch to repair"));
                }
                let mut repair = ctx.clone();
                repair.branch_repair = true;
                start_builder(state, run, &repair, false, true)
            }
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::UnitActive => match state {
            S::Preparing => Ok(vec![moved(ctx, state, S::Running)]),
            S::Planned
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::HeadWritten => match state {
            // A fast run can collect before its start is seen as active.
            S::Preparing | S::Running => Ok(vec![moved(ctx, state, S::Collecting)]),
            // A receipt during a lost link is evidence of collection, not of
            // liveness. Remember it without claiming the unit's state.
            S::Unknown { last } if matches!(**last, S::Preparing | S::Running) => Ok(vec![moved(
                ctx,
                state,
                S::Unknown {
                    last: Box::new(S::Collecting),
                },
            )]),
            S::Planned
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::UnitEnded {
            reason,
            detail,
            stderr,
        } => match state {
            S::Preparing | S::Running | S::Collecting => Ok(builder_ended(
                state,
                &reason,
                (detail.as_deref(), stderr.as_deref()),
                ctx,
            )),
            // The reviewer's unit ended without a verdict (a verdict moves the task).
            S::InReview => Ok(reviewer_ended(
                state,
                &reason,
                (detail.as_deref(), stderr.as_deref()),
                ctx,
            )),
            // A confirmed unit end settles uncertainty with its actual reason,
            // including driver/preflight failures and collected work, and is
            // recorded as the check (`engine.reconciled`) with the state it settled.
            S::Unknown { last } => {
                let mut events = decide(
                    last,
                    Command::UnitEnded {
                        reason,
                        detail,
                        stderr,
                    },
                    ctx,
                )?;
                if events.is_empty() {
                    events.push(moved(ctx, state, (**last).clone()));
                }
                for event in &mut events {
                    if let EventKind::TaskStateChanged { from, .. } = event {
                        *from = state.clone();
                    }
                }
                checked(ctx, last, &mut events);
                Ok(events)
            }
            S::Planned
            | S::Publishing
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        // The engine records `branch.published`; the task waits for its PR.
        Command::Published => match state {
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::PrOpened => match state {
            S::Publishing => Ok(vec![moved(ctx, state, S::InReview)]),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::VerdictRecorded { head, open } => match state {
            S::InReview => verdict(state, &head, open, ctx),
            // The current reviewer's verdict during a lost link is kept; what it
            // decides stays unknown until the unit is observed again.
            S::Unknown { last } if **last == S::InReview => {
                let mut events = verdict(last, &head, open, ctx)?;
                for event in &mut events {
                    if let EventKind::TaskStateChanged { from, to, .. } = event {
                        *from = state.clone();
                        *to = S::Unknown {
                            last: Box::new(to.clone()),
                        };
                    }
                }
                Ok(events)
            }
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. } => Err(changed("the task is not in review")),
            S::Stopped { .. } => Err(already_stopped()),
            S::Unknown { .. } => Err(unknown()),
            S::Done | S::Cancelled | S::Failed { .. } => Err(terminal()),
        },
        Command::ChecksObserved { head, status } => match state {
            S::AwaitingChecks | S::Merging => checks(state, &head, status, ctx),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::MergeDone => match state {
            S::Merging => Ok(done(ctx, state)),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        // the head moved (while merging or while waiting for
        // CI), so review re-enters, recorded as `merge.refused` (bounded, since
        // a refusal for another reason would loop).
        Command::MergeRefused(reason) => match state {
            S::AwaitingChecks | S::Merging if ctx.caps.retries < caps::RETRIES_PER_EFFECT => {
                Ok(vec![
                    moved(ctx, state, S::InReview),
                    EventKind::MergeRefused {
                        task: ctx.task.clone(),
                        reason,
                    },
                ])
            }
            S::AwaitingChecks | S::Merging => Ok(failed(
                ctx,
                state,
                &format!("the merge was refused: {reason}"),
            )),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::NoChanges => match state {
            S::Publishing => Ok(failed(ctx, state, "no changes: the run changed nothing")),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        Command::EffectFailed {
            effect,
            transient,
            detail,
            rate_limit,
        } => Ok(effect_failed(
            state, effect, transient, &detail, rate_limit, ctx,
        )),
        Command::Stop(reason) => stop(state, reason.as_deref(), ctx),
        Command::Resume(run) => match state {
            S::Stopped { was } => resume(state, was, run, ctx),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. } => Err(changed("the task is not stopped")),
            S::Unknown { .. } => Err(unknown()),
            S::Done | S::Cancelled | S::Failed { .. } => Err(terminal()),
        },
        // A stopped task with a head: one builder round on it, a round the
        // retry grants even past the review-round cap. Its old head is never
        // merged as it was. A stopped task with no head: one builder round on
        // its kept, unpublished work; one that never started has none.
        Command::Retry(Some(run))
            if matches!(state, S::Stopped { was } if **was != S::Planned)
                && run.reviews.is_none() =>
        {
            let granted = Context {
                retried: true,
                ..ctx.clone()
            };
            let reason = if ctx.head.is_some() {
                "retried on its head"
            } else {
                "retried on its kept work"
            };
            let mut events = start_builder(state, run, &granted, true, false)?;
            events.push(EventKind::TaskResumed {
                task: ctx.task.clone(),
                reason: Some(reason.into()),
            });
            Ok(events)
        }
        Command::Retry(run) => retry(state, ctx, |ctx| match run {
            Some(run) => {
                let granted = Context {
                    retried: true,
                    ..ctx.clone()
                };
                retry_run(state, run, &granted, false)
            }
            None if ctx.head.is_some() => Ok(vec![moved(ctx, state, TaskState::Publishing)]),
            None => Err(changed("retrying this failed task starts a run")),
        }),
        Command::RetryMerge => retry(state, ctx, |ctx| {
            if ctx.head.is_some() && ctx.reviewed {
                Ok(vec![moved(ctx, state, reviewed(ctx))])
            } else {
                Err(changed(
                    "the head is not approved: retrying it starts a run",
                ))
            }
        }),
        // "Change course" drops the unstarted tasks of the old plan.
        Command::Cancel => match state {
            S::Planned => Ok(cancelled(ctx, state)),
            S::Stopped { was } if **was == S::Planned => Ok(cancelled(ctx, state)),
            S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. } => Err(changed("only a task that has not started is cancelled")),
            S::Unknown { .. } => Err(unknown()),
            S::Done | S::Cancelled | S::Failed { .. } => Err(terminal()),
        },
        Command::RetryDue => match state {
            S::RetryPending { effect, .. } => match effect_state(*effect) {
                Some(next) => Ok(vec![moved(ctx, state, next)]),
                None => Err(changed(
                    "a run is retried with Start, which carries the new run",
                )),
            },
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
        // A task that may have a unit or an effect in flight is unknown until
        // reconciled, a stopped one too (its unit may have
        // outlived the stop); the others are the engine's own record.
        Command::Restarted if in_flight(state) => Ok(vec![moved(
            ctx,
            state,
            S::Unknown {
                last: Box::new(state.clone()),
            },
        )]),
        Command::Restarted => Ok(vec![]),
        Command::Reconcile(observed) => match state {
            S::Unknown { last } => Ok(reconciled(state, last, &observed, ctx)),
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. } => Ok(vec![]),
        },
    }
}

/// Whether a unit or an effect may be in flight in this state, so that after a
/// restart only an observation can say where the task is.
fn in_flight(state: &TaskState) -> bool {
    use TaskState as S;
    match state {
        S::Preparing | S::Running | S::Collecting | S::Publishing | S::InReview | S::Merging => {
            true
        }
        S::Stopped { was } => in_flight(was),
        S::Planned
        | S::AwaitingChecks
        | S::RetryPending { .. }
        | S::Unknown { .. }
        | S::Done
        | S::Cancelled
        | S::Failed { .. } => false,
    }
}

/// The state an engine effect is performed in, where a due retry goes back to.
/// `StartUnit` needs a new run (`Start`); the others are not a task's state.
fn effect_state(effect: Effect) -> Option<TaskState> {
    match effect {
        Effect::Push | Effect::OpenPr => Some(TaskState::Publishing),
        Effect::ReadChecks => Some(TaskState::AwaitingChecks),
        Effect::Merge => Some(TaskState::Merging),
        Effect::StartUnit | Effect::StopUnit | Effect::CloseIssue | Effect::DeleteBranch => None,
    }
}

/// The wait before retry `attempt` (1-based), doubling within this policy's bounds.
pub fn backoff_millis(attempt: u8, initial: i64, maximum: i64) -> i64 {
    let doublings = u32::from(attempt.saturating_sub(1)).min(62);
    initial.saturating_mul(1i64 << doublings).min(maximum)
}

/// Goal approved, project not paused, a run slot free.
fn admit(ctx: &Context) -> Result<(), Refusal> {
    if let Some(task) = ctx.waiting_for.first() {
        return Err(Refusal::new(
            RefusalCode::DependencyNotMerged { task: task.clone() },
            format!("Waiting for {task}"),
        ));
    }
    if !ctx.goal_approved {
        return Err(Refusal::new(
            RefusalCode::GoalNotApproved,
            "the owner has not approved this goal's plan yet",
        ));
    }
    if ctx.project_paused {
        return Err(Refusal::new(
            RefusalCode::ProjectPaused,
            "the project is paused",
        ));
    }
    if ctx.caps.concurrent_runs >= ctx.max_runs {
        return Err(Refusal::new(
            RefusalCode::CapReached {
                which: Cap::ConcurrentRuns,
            },
            format!(
                "{} runs are going; maximum is {}",
                ctx.caps.concurrent_runs, ctx.max_runs
            ),
        ));
    }
    Ok(())
}

fn attempts_left(ctx: &Context) -> Result<(), Refusal> {
    if ctx.caps.attempts >= caps::ATTEMPTS_PER_TASK {
        return Err(Refusal::new(
            RefusalCode::CapReached {
                which: Cap::AttemptsPerTask,
            },
            format!("{} attempts made", ctx.caps.attempts),
        ));
    }
    Ok(())
}

fn for_this_task(run: &RunAssignment, ctx: &Context) -> Result<(), Refusal> {
    if run.task.as_ref() != Some(&ctx.task) {
        return Err(changed("the run is for another task"));
    }
    Ok(())
}

/// A builder run of the task: `Preparing`, `run.started`, `task.started`.
fn start_builder(
    from: &TaskState,
    run: RunAssignment,
    ctx: &Context,
    resume: bool,
    counted: bool,
) -> Result<Vec<EventKind>, Refusal> {
    for_this_task(&run, ctx)?;
    if run.reviews.is_some() {
        return Err(changed("a reviewer run does not build the task"));
    }
    // After the first publication every builder run builds on the publication
    // head, never again on the default branch.
    if ctx.head.is_some() && run.base != ctx.head {
        return Err(changed("the task is already published"));
    }
    // Conflict repairs and the owner's retry must remain possible even at the
    // review-round cap.
    if ctx.head.is_some()
        && !ctx.branch_repair
        && !ctx.retried
        && ctx.caps.review_rounds >= caps::REVIEW_ROUNDS_PER_PR
    {
        return Err(Refusal::new(
            RefusalCode::CapReached {
                which: Cap::ReviewRoundsPerPr,
            },
            format!("{} review rounds", ctx.caps.review_rounds),
        ));
    }
    admit(ctx)?;
    if counted {
        attempts_left(ctx)?;
    }
    let started = EventKind::TaskStarted {
        branch_repair: ctx.branch_repair,
        retried: ctx.retried,
        task: ctx.task.clone(),
        run: run.run.clone(),
        resume,
    };
    Ok(vec![
        moved(ctx, from, TaskState::Preparing),
        EventKind::RunStarted(run),
        started,
    ])
}

/// The head's reviewer run: `InReview`, `run.started`.
fn start_reviewer(
    from: &TaskState,
    run: RunAssignment,
    ctx: &Context,
    counted: bool,
) -> Result<Vec<EventKind>, Refusal> {
    for_this_task(&run, ctx)?;
    if run.reviews.is_none() || run.reviews != ctx.head {
        return Err(Refusal::new(
            RefusalCode::HeadMismatch,
            format!(
                "the reviewer run is for {:?}, the task's head is {:?}",
                run.reviews, ctx.head
            ),
        ));
    }
    admit(ctx)?;
    if counted {
        attempts_left(ctx)?;
    }
    Ok(vec![
        moved(ctx, from, TaskState::InReview),
        EventKind::RunStarted(run),
    ])
}

/// A due run retry: the builder again in its preserved workspace, or the head's
/// reviewer again.
fn retry_run(
    from: &TaskState,
    run: RunAssignment,
    ctx: &Context,
    counted: bool,
) -> Result<Vec<EventKind>, Refusal> {
    if run.reviews.is_some() {
        start_reviewer(from, run, ctx, counted)
    } else {
        start_builder(from, run, ctx, true, counted)
    }
}

/// After a fix-first verdict the engine starts a builder run on the publication
/// head; a run on any other base is not a fix
/// round, so the orchestrator cannot restart a task under review.
fn fix_round(
    from: &TaskState,
    run: RunAssignment,
    ctx: &Context,
) -> Result<Vec<EventKind>, Refusal> {
    if run.reviews.is_some() || ctx.head.is_none() || run.base != ctx.head {
        return Err(changed("the task is in review"));
    }
    if ctx.caps.attempts >= caps::ATTEMPTS_PER_TASK {
        return Ok(failed(
            ctx,
            from,
            &format!("{} attempts made", ctx.caps.attempts),
        ));
    }
    start_builder(from, run, ctx, false, true)
}

fn verdict(
    state: &TaskState,
    head: &str,
    open: u32,
    ctx: &Context,
) -> Result<Vec<EventKind>, Refusal> {
    if ctx.head.as_deref() != Some(head) {
        return Err(Refusal::new(
            RefusalCode::HeadMismatch,
            format!(
                "the verdict names {head}, the task's head is {:?}",
                ctx.head
            ),
        ));
    }
    if open > 0 {
        // Fix-first: the engine starts the fix round (`Start` on the head). At
        // the cap the task stops with its PR and findings kept, and the engine
        // asks the owner: stop it, or change course. No builder
        // starts past the cap, so a resume reviews the same head again.
        if ctx.caps.review_rounds >= caps::REVIEW_ROUNDS_PER_PR {
            let why = format!(
                "{} review rounds and {open} findings are still open",
                ctx.caps.review_rounds
            );
            return Ok(stopped(ctx, state, state, Some(&why)));
        }
        return Ok(vec![]);
    }
    if ctx.more_review {
        return Ok(vec![]);
    }
    Ok(vec![moved(ctx, state, reviewed(ctx))])
}

/// Where an approved head goes: its checks, or its merge without CI.
fn reviewed(ctx: &Context) -> TaskState {
    match ctx.ci {
        CiMode::NoCi => TaskState::Merging,
        CiMode::Expected(_) => TaskState::AwaitingChecks,
    }
}

/// only success on every expected name of the head merges
/// (an empty expected set never does); anything else keeps the task waiting,
/// and seen again just before the merge it sends the task back to waiting.
/// An explicit switch to NoCi releases an already waiting task.
fn checks(
    state: &TaskState,
    head: &str,
    status: ChecksStatus,
    ctx: &Context,
) -> Result<Vec<EventKind>, Refusal> {
    if ctx.head.as_deref() != Some(head) {
        return Err(Refusal::new(
            RefusalCode::HeadMismatch,
            format!(
                "the checks are for {head}, the task's head is {:?}",
                ctx.head
            ),
        ));
    }
    let green = match &ctx.ci {
        CiMode::NoCi => true,
        CiMode::Expected(names) => !names.is_empty() && status == ChecksStatus::Success,
    };
    Ok(match (state, green) {
        (TaskState::AwaitingChecks, true) => vec![moved(ctx, state, TaskState::Merging)],
        (TaskState::Merging, false) => vec![moved(ctx, state, TaskState::AwaitingChecks)],
        (
            TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::Planned
            | TaskState::Preparing
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::InReview
            | TaskState::RetryPending { .. }
            | TaskState::Stopped { .. }
            | TaskState::Unknown { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. },
            _,
        ) => vec![],
    })
}

/// A builder run ended: collected work publishes; a transient end retries
/// within the attempts cap; a stop from outside stops the task; anything else
/// fails it.
fn builder_ended(
    state: &TaskState,
    reason: &EndReason,
    words: EndWords<'_>,
    ctx: &Context,
) -> Vec<EventKind> {
    match reason {
        EndReason::Completed if *state == TaskState::Collecting => {
            vec![moved(ctx, state, TaskState::Publishing)]
        }
        EndReason::Completed => failed(ctx, state, "the run ended without collecting its work"),
        EndReason::Stopped => stopped(ctx, state, state, Some("the run was stopped")),
        EndReason::Crashed | EndReason::Oom | EndReason::Interrupted | EndReason::TimeLimit => {
            run_retry(ctx, state, reason, words, RunKind::Builder)
        }
        EndReason::Mismatch | EndReason::Preflight(_) | EndReason::ParseError => {
            failed(ctx, state, &format!("the run ended: {}", end_words(reason)))
        }
    }
}

/// The head's reviewer ended without a verdict (the engine sends a reviewer's
/// end only then): again, within the attempts cap (`ctx.caps.attempts` counts
/// the reviewer runs on this head).
fn reviewer_ended(
    state: &TaskState,
    reason: &EndReason,
    words: EndWords<'_>,
    ctx: &Context,
) -> Vec<EventKind> {
    match reason {
        EndReason::Stopped => stopped(ctx, state, state, Some("the review was stopped")),
        EndReason::Completed
        | EndReason::Crashed
        | EndReason::Oom
        | EndReason::Interrupted
        | EndReason::TimeLimit => run_retry(ctx, state, reason, words, RunKind::Reviewer),
        EndReason::Mismatch | EndReason::Preflight(_) | EndReason::ParseError => failed(
            ctx,
            state,
            &format!("the review ended: {}", end_words(reason)),
        ),
    }
}

/// A run's `run.ended` detail and stderr.
type EndWords<'a> = (Option<&'a str>, Option<&'a str>);

/// Another run, after a backoff, while attempts are left; else the task fails.
/// The failure names the last run's detail and stderr, and a time limit the
/// limit of the run's `kind`.
fn run_retry(
    ctx: &Context,
    from: &TaskState,
    reason: &EndReason,
    (detail, stderr): EndWords<'_>,
    kind: RunKind,
) -> Vec<EventKind> {
    if ctx.caps.attempts >= caps::ATTEMPTS_PER_TASK {
        let mut last = crate::wording::ended_words(reason, detail);
        if *reason == EndReason::TimeLimit {
            let minutes = caps::time_limit(kind).as_secs() / 60;
            last = format!("{last} ({minutes} min)");
        }
        if let Some(stderr) = stderr {
            let lines: Vec<_> = stderr.lines().collect();
            last = format!("{last}; its stderr ended: {}", lines.join(" | "));
        }
        return failed(
            ctx,
            from,
            &format!("{} attempts; the last ended: {last}", ctx.caps.attempts),
        );
    }
    retry_pending(
        ctx,
        from,
        Effect::StartUnit,
        ctx.caps.attempts.saturating_add(1),
    )
}

fn retry_pending(ctx: &Context, from: &TaskState, effect: Effect, attempt: u8) -> Vec<EventKind> {
    vec![
        moved(ctx, from, TaskState::RetryPending { effect, attempt }),
        EventKind::EffectRetry {
            rate_limit: None,
            task: ctx.task.clone(),
            effect,
            attempt,
            at: ctx.now.plus_millis(backoff_millis(attempt, 2_000, 300_000)),
        },
    ]
}

/// An engine effect failed in the state it belongs to: transient errors retry
/// within `RETRIES_PER_EFFECT` (`ctx.caps.retries` is how many were made for
/// this effect); anything else fails the task with the detail.
fn effect_failed(
    state: &TaskState,
    effect: Effect,
    transient: bool,
    detail: &str,
    rate_limit: Option<crate::forge::RateLimit>,
    ctx: &Context,
) -> Vec<EventKind> {
    use TaskState as S;
    let applies = match (effect, state) {
        (Effect::Push | Effect::OpenPr, S::Publishing)
        | (Effect::ReadChecks, S::AwaitingChecks)
        | (Effect::Merge, S::Merging)
        | (Effect::StartUnit, S::Preparing | S::InReview) => true,
        (
            Effect::StartUnit
            | Effect::StopUnit
            | Effect::Push
            | Effect::OpenPr
            | Effect::ReadChecks
            | Effect::Merge
            | Effect::CloseIssue
            | Effect::DeleteBranch,
            S::Planned
            | S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::InReview
            | S::AwaitingChecks
            | S::Merging
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. },
        ) => false,
    };
    if !applies {
        return vec![];
    }
    if let Some(limit) = rate_limit {
        let attempt = ctx.caps.retries;
        return vec![
            moved(ctx, state, TaskState::RetryPending { effect, attempt }),
            EventKind::EffectRetry {
                task: ctx.task.clone(),
                effect,
                attempt,
                at: limit.not_before(
                    ctx.now,
                    backoff_millis(
                        attempt
                            .saturating_add(ctx.caps.rate_limit_retries)
                            .saturating_add(1),
                        2_000,
                        300_000,
                    ),
                ),
                rate_limit: Some(limit),
            },
        ];
    }
    if transient && ctx.caps.retries < caps::RETRIES_PER_EFFECT {
        return retry_pending(ctx, state, effect, ctx.caps.retries.saturating_add(1));
    }
    let tries = if transient {
        format!(" ({} retries)", ctx.caps.retries)
    } else {
        String::new()
    };
    failed(
        ctx,
        state,
        &format!("{} failed{tries}: {detail}", effect_words(effect)),
    )
}

/// The state a stop keeps as `was`, or why there is nothing to stop.
fn stoppable(state: &TaskState) -> Result<TaskState, Refusal> {
    use TaskState as S;
    match state {
        S::Planned
        | S::Preparing
        | S::Running
        | S::Collecting
        | S::Publishing
        | S::InReview
        | S::AwaitingChecks
        | S::Merging
        | S::RetryPending { .. } => Ok(state.clone()),
        S::Stopped { .. } => Err(already_stopped()),
        // Stopped as what it was; the engine stops whatever unit it had.
        S::Unknown { last } => stoppable(last),
        S::Done | S::Cancelled | S::Failed { .. } => Err(terminal()),
    }
}

/// Stop has an outcome in every state. The engine then takes the task's
/// lock, so an effect in flight completes and is recorded before stop returns,
/// and stops the task's unit if it has one.
fn stop(state: &TaskState, reason: Option<&str>, ctx: &Context) -> Result<Vec<EventKind>, Refusal> {
    let was = stoppable(state)?;
    Ok(stopped(ctx, state, &was, reason))
}

fn stopped(
    ctx: &Context,
    from: &TaskState,
    was: &TaskState,
    reason: Option<&str>,
) -> Vec<EventKind> {
    vec![
        moved(
            ctx,
            from,
            TaskState::Stopped {
                was: Box::new(was.clone()),
            },
        ),
        EventKind::TaskStopped {
            update: false,
            task: ctx.task.clone(),
            reason: reason.map(str::to_owned),
        },
    ]
}

/// Resumes by the state the task stopped in. A stopped run resumes
/// as a new run (the builder in its preserved workspace, the reviewer on the
/// same head), so `run` is the new run exactly when `was` had one.
fn resume(
    state: &TaskState,
    was: &TaskState,
    run: Option<RunAssignment>,
    ctx: &Context,
) -> Result<Vec<EventKind>, Refusal> {
    use TaskState as S;
    if *was == S::InReview
        && ctx.open_findings > 0
        && ctx.caps.review_rounds >= caps::REVIEW_ROUNDS_PER_PR
    {
        return Err(changed(
            "the review rounds are used up with findings open: stop the task or change course",
        ));
    }
    let mut events = match (was, run) {
        (S::Planned | S::Publishing | S::AwaitingChecks | S::Merging, None) => {
            vec![moved(ctx, state, was.clone())]
        }
        // The retry happens on resume.
        (S::RetryPending { effect, .. }, None) => match effect_state(*effect) {
            Some(next) => vec![moved(ctx, state, next)],
            None => return Err(changed("resuming this task starts a run")),
        },
        (
            S::RetryPending {
                effect: Effect::StartUnit,
                ..
            },
            Some(run),
        ) => retry_run(state, run, ctx, false)?,
        (
            S::Preparing
            | S::Running
            | S::Collecting
            | S::Publishing
            | S::AwaitingChecks
            | S::Merging,
            Some(run),
        ) => start_builder(state, run, ctx, true, false)?,
        (S::InReview, Some(run)) => start_reviewer(state, run, ctx, false)?,
        (
            S::Preparing
            | S::Running
            | S::Collecting
            | S::InReview
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. },
            None,
        )
        | (
            S::Planned
            | S::RetryPending { .. }
            | S::Stopped { .. }
            | S::Unknown { .. }
            | S::Done
            | S::Cancelled
            | S::Failed { .. },
            Some(_),
        ) => return Err(changed("this task resumes another way")),
    };
    events.push(EventKind::TaskResumed {
        task: ctx.task.clone(),
        reason: None,
    });
    Ok(events)
}

/// The owner retries a failed task (`failed` decides how): one more run,
/// admitted like a resume (goal approved, not paused, a slot free) but not
/// refused for the attempts it already made; its own end counts as usual. A
/// builder may start past the review-round cap: the owner's retry grants it
/// one more round, whose review and CI then end the task again at the cap.
/// Without a run, a pushed head with no PR goes back to publishing, which
/// opens it, and an approved head back to its checks and merge.
fn retry(
    state: &TaskState,
    ctx: &Context,
    failed: impl FnOnce(&Context) -> Result<Vec<EventKind>, Refusal>,
) -> Result<Vec<EventKind>, Refusal> {
    use TaskState as S;
    let not_failed = || changed("only a failed task is retried");
    let mut events = match state {
        S::Failed { .. } => failed(ctx)?,
        S::Stopped { .. } => {
            return Err(Refusal::new(
                RefusalCode::AlreadyStopped,
                "the task is stopped with no published head: resume it (retry text is used only when the task has kept work)",
            ));
        }
        S::Unknown { .. } => return Err(unknown()),
        S::Done | S::Cancelled => {
            return Err(Refusal::new(
                RefusalCode::AlreadyTerminal,
                "only a failed task is retried; this one is done or cancelled",
            ));
        }
        S::Planned
        | S::Preparing
        | S::Running
        | S::Collecting
        | S::Publishing
        | S::InReview
        | S::AwaitingChecks
        | S::Merging
        | S::RetryPending { .. } => return Err(not_failed()),
    };
    events.push(EventKind::TaskResumed {
        task: ctx.task.clone(),
        reason: Some("retried after it failed".into()),
    });
    Ok(events)
}

/// After a restart: what the unit, the `head` file and the forge say decides the
/// state. Nothing is claimed without its observation: a run is running only
/// if its unit is active. A merging task stays merging: the merge step reads the
/// PR (merged or not) and finishes the merge's effects (cleanup, goal, wake).
fn reconciled(
    state: &TaskState,
    last: &TaskState,
    seen: &Observed,
    ctx: &Context,
) -> Vec<EventKind> {
    use TaskState as S;
    let mut events = match last {
        S::Preparing | S::Running | S::Collecting => match (seen.unit, &seen.head) {
            // Its head came while the engine checked: collected, still exiting.
            (UnitState::Active, Some(_)) => vec![moved(ctx, state, S::Collecting)],
            (UnitState::Active, None) => vec![moved(ctx, state, last.clone())],
            (UnitState::Inactive | UnitState::Failed, Some(_)) => {
                vec![moved(ctx, state, S::Publishing)]
            }
            (UnitState::Inactive | UnitState::Failed, None) => run_retry(
                ctx,
                state,
                &EndReason::Interrupted,
                (None, None),
                RunKind::Builder,
            ),
        },
        // The reviewer ended while unseen: without its verdict, again.
        S::InReview => match seen.unit {
            UnitState::Active => vec![moved(ctx, state, S::InReview)],
            // Its verdict came: a fix round or the next review stage is due.
            UnitState::Inactive | UnitState::Failed if ctx.open_findings > 0 || ctx.more_review => {
                vec![moved(ctx, state, S::InReview)]
            }
            UnitState::Inactive | UnitState::Failed => run_retry(
                ctx,
                state,
                &EndReason::Interrupted,
                (None, None),
                RunKind::Reviewer,
            ),
        },
        S::Planned
        | S::Publishing
        | S::AwaitingChecks
        | S::Merging
        | S::RetryPending { .. }
        | S::Stopped { .. }
        | S::Unknown { .. }
        | S::Done
        | S::Cancelled
        | S::Failed { .. } => vec![moved(ctx, state, last.clone())],
    };
    checked(ctx, last, &mut events);
    events
}

/// Records the check of an unknown task: `engine.reconciled` with the state
/// the observation settled it in.
fn checked(ctx: &Context, last: &TaskState, events: &mut Vec<EventKind>) {
    let now = events
        .iter()
        .find_map(|e| {
            if let EventKind::TaskStateChanged { to, .. } = e {
                Some(to.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| last.clone());
    events.push(EventKind::EngineReconciled {
        task: ctx.task.clone(),
        state: now,
    });
}

fn moved(ctx: &Context, from: &TaskState, to: TaskState) -> EventKind {
    EventKind::TaskStateChanged {
        task: ctx.task.clone(),
        from: from.clone(),
        to,
    }
}

fn done(ctx: &Context, from: &TaskState) -> Vec<EventKind> {
    vec![
        moved(ctx, from, TaskState::Done),
        EventKind::TaskDone {
            task: ctx.task.clone(),
            reason: None,
        },
    ]
}

fn cancelled(ctx: &Context, from: &TaskState) -> Vec<EventKind> {
    vec![
        moved(ctx, from, TaskState::Cancelled),
        EventKind::TaskCancelled {
            task: ctx.task.clone(),
            reason: Some("the plan changed".into()),
        },
    ]
}

fn failed(ctx: &Context, from: &TaskState, reason: &str) -> Vec<EventKind> {
    vec![
        moved(
            ctx,
            from,
            TaskState::Failed {
                reason: reason.to_owned(),
            },
        ),
        EventKind::TaskFailed {
            task: ctx.task.clone(),
            reason: Some(reason.to_owned()),
        },
    ]
}

fn changed(detail: &str) -> Refusal {
    Refusal::new(RefusalCode::ValueChanged, detail)
}

fn already_stopped() -> Refusal {
    Refusal::new(RefusalCode::AlreadyStopped, "the task is already stopped")
}

fn unknown() -> Refusal {
    Refusal::new(
        RefusalCode::ValueChanged,
        "the task is being checked after a restart",
    )
}

fn terminal() -> Refusal {
    Refusal::new(RefusalCode::AlreadyTerminal, "the task has ended")
}

/// Fold one of the task's events into its state. `task.state` is the only event
/// that moves it (`decide` writes one with every transition); every other kind
/// leaves the state as it is.
pub fn apply(state: TaskState, ev: &EventKind) -> TaskState {
    if let EventKind::TaskStateChanged { to, .. } = ev {
        to.clone()
    } else {
        state
    }
}

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;
