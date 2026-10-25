//! The event and its catalogue. `contracts/events.md`
//! lists the same kinds, one feed wording each; a test keeps the two equal.
//!
//! Payloads never carry tokens, logins or the owner's identity.

use crate::clock::Timestamp;
use crate::ids::{
    EventId, GoalId, HireId, ProjectId, QuestionId, RoleId, RunId, TaskId, VerdictId,
};
use crate::model::{Effort, Vendor};
use crate::refusal::Refusal;
use crate::roles::{HireRecord, ReviewKind, RoleRecord};
use crate::run_verbs::Finding;
use crate::state::{Effect, TaskState};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The most a `run.output` item carries.
pub const RUN_OUTPUT_MAX_BYTES: usize = 4096;

/// Who caused an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Owner,
    Engine,
    Run(RunId),
}

/// What an event rests on, where it exists.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictId>,
}

/// One row of the `events` table. On the wire `kind` and `payload` sit at the top
/// level, as in the table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Event {
    pub id: EventId,
    pub at: Timestamp,
    pub actor: Actor,
    pub project: Option<ProjectId>,
    #[serde(flatten)]
    pub kind: EventKind,
    pub evidence: Option<Evidence>,
}

/// How a project's merges are gated on CI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CiMode {
    /// These check names must report success on the head.
    Expected(Vec<String>),
    /// The owner explicitly chose "no CI"; the verdict states the local gates ran.
    NoCi,
}

/// An issue a goal carries: its fenced, secret-scanned content as read when the
/// goal was stated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoalIssue {
    pub number: u64,
    pub url: String,
    pub text: String,
}

/// One task in a proposed plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlannedTask {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<TaskId>,
    pub task: TaskId,
    pub title: String,
    pub role: RoleId,
    pub review: ReviewKind,
    pub summary: String,
    /// Routine work for the role's routine hire (fixes, tests, docs, chores).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub routine: bool,
}

/// How a question is laid out; the owner's setting is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionFormat {
    Simple,
    QuickCompare,
    Swot,
}

/// Why the engine opened a question; omitted metadata identifies historical records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    Ordinary,
    OrchestratorFailed,
    PlanningHold {
        attempt: RunId,
        fetch: bool,
    },
    /// One completion decision for this exact builder result.
    Completion {
        task: TaskId,
        run: RunId,
        result: EventId,
    },
}

/// A resource admission hold, or its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceWait {
    Memory,
    Disk,
    HostLoad,
    Available,
}

/// One option of a question. `id` is its 1-based position (`dvb answer <q> 1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuestionOption {
    pub id: u32,
    pub text: String,
    /// What choosing it does, in the owner's words.
    pub effect: String,
    /// Choosing it approves the goal's proposed plan (`goal.approved`).
    #[serde(default)]
    pub approves_plan: bool,
    /// Choosing it changes nothing (the team waits): the answer closes the
    /// question without waking the orchestrator.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_effect: bool,
    /// Choosing it is the owner's yes to retrying this task (failed, or
    /// stopped or in review with a PR): the orchestrator may then `dvb run
    /// task retry` it, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries_task: Option<TaskId>,
}

/// What a question lapses on: the version (the id of the event that last
/// set it) of the goal or value it asks about. A newer version refuses the answer
/// with `QUESTION_LAPSED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LapsesOn {
    GoalVersion(EventId),
    ValueVersion(EventId),
}

/// The owner's answer: one of the options, or a reply in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Choice(u32),
    Reply(String),
}

/// A refused command (`command.refused`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RefusedCommand {
    pub command: String,
    #[serde(flatten)]
    pub refusal: RecordedRefusal,
}

/// Durable refusals retain retired wire codes; current API refusals use `Refusal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum RecordedRefusal {
    Current(Refusal),
    Retired(RetiredRefusal),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "code")]
pub enum RetiredRefusal {
    #[serde(rename = "NOT_IMPLEMENTED")]
    NotImplemented { detail: String },
    #[serde(rename = "GITHUB_ACTIVITY_OFF")]
    GithubActivityOff { detail: String },
}

impl From<Refusal> for RecordedRefusal {
    fn from(refusal: Refusal) -> Self {
        Self::Current(refusal)
    }
}
impl RecordedRefusal {
    pub fn as_str(&self) -> String {
        match self {
            Self::Current(refusal) => refusal.code.as_str(),
            Self::Retired(RetiredRefusal::GithubActivityOff { .. }) => "GITHUB_ACTIVITY_OFF".into(),
            Self::Retired(RetiredRefusal::NotImplemented { .. }) => "NOT_IMPLEMENTED".into(),
        }
    }
    pub fn current(&self) -> Option<&Refusal> {
        match self {
            Self::Current(refusal) => Some(refusal),
            Self::Retired(
                RetiredRefusal::GithubActivityOff { .. } | RetiredRefusal::NotImplemented { .. },
            ) => None,
        }
    }
}

/// Who runs, on what: the start record every run has (`run.started`), written
/// before its unit starts. The engine chooses all of it; the runs projection, usage
/// attribution and reviewer evidence are rebuilt from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunAssignment {
    /// Engine-selected review stage; absent in historical runs and builders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_stage: Option<ReviewKind>,
    /// The goal version selected for an orchestrator wake. New wakes record it
    /// at admission; absent in task runs and historical logs, whose binding is
    /// rebuilt with the legacy selection rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_version: Option<EventId>,
    pub run: RunId,
    /// Absent for an orchestrator wake that is not working on a task.
    pub task: Option<TaskId>,
    pub role: RoleId,
    pub hire: HireId,
    pub model: String,
    pub vendor: Vendor,
    pub effort: Effort,
    pub reason: String,
    /// For a reviewer run: the publication head it reviews, the only head its
    /// verdict may name.
    pub reviews: Option<String>,
    /// The commit the run's workspace starts from, chosen by the engine. A
    /// builder's publication commit is built on it, never on a parent
    /// the run reports. Absent only in logs written before it existed.
    #[serde(default)]
    pub base: Option<String>,
    /// A builder round of a routine task: the plan or this start marked it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub routine: bool,
}

impl RunAssignment {
    pub fn is_orchestrator(&self) -> bool {
        self.task.is_none() && self.reviews.is_none()
    }

    pub fn kind(&self) -> RunKind {
        match (&self.task, &self.reviews) {
            (_, Some(_)) => RunKind::Reviewer,
            (Some(_), None) => RunKind::Builder,
            (None, None) => RunKind::Orchestrator,
        }
    }
}

/// What a run does: plan (no task), build a task (any building role,
/// the clerk's included), or review a head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Orchestrator,
    Builder,
    Reviewer,
}

impl QuestionOption {
    /// An engine-written owner choice. Approval and retry authorization are explicit at the call site.
    pub fn owner(id: u32, text: impl Into<String>, effect: impl Into<String>) -> Self {
        Self {
            id,
            text: text.into(),
            effect: effect.into(),
            approves_plan: false,
            no_effect: false,
            retries_task: None,
        }
    }
}

crate::text::text_enum! {
    /// Preflight check codes; each has a fix hint in `prompts/preflight.md`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
    pub enum PreflightCode {
        PfUid => "PF_UID",
        PfHome => "PF_HOME",
        PfCredVisible => "PF_CRED_VISIBLE",
        PfStoreVisible => "PF_STORE_VISIBLE",
        PfWsBase => "PF_WS_BASE",
        // No longer checked; kept so stored `run.preflight` events still load.
        PfGitIdentity => "PF_GIT_IDENTITY",
        PfCliMissing => "PF_CLI_MISSING",
        // No longer checked; kept so stored `run.preflight` events still load.
        PfCliSandbox => "PF_CLI_SANDBOX",
        PfLoginMissing => "PF_LOGIN_MISSING",
        PfModelUnknown => "PF_MODEL_UNKNOWN",
        PfMemoryCap => "PF_MEMORY_CAP",
        PfMemoryFeature => "PF_MEMORY_FEATURE",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PreflightCheck {
    pub code: PreflightCode,
    pub ok: bool,
    pub detail: String,
}

/// The served-model evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServedModel {
    Confirmed(String),
    Unconfirmed,
    Mismatch(String),
}

/// One live line of a run (bounded by `RUN_OUTPUT_MAX_BYTES`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputItem {
    Text(String),
    ToolCall { name: String },
    Note(String),
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Completed,
    Stopped,
    Crashed,
    Oom,
    Interrupted,
    Mismatch,
    Preflight(PreflightCode),
    ParseError,
    /// The engine stopped the run at its kind's time limit (`caps::time_limit`).
    TimeLimit,
}

/// CI evidence on a head, per expected name and overall. Only
/// `success` on every expected name counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChecksStatus {
    Missing,
    Pending,
    Unreadable,
    Failed,
    Success,
}

// Capture name tokens directly so utoipa sees the same string literals as serde.
macro_rules! event_kinds {
    ($(#[$meta:meta])* pub enum $name:ident {
        $( $(#[$variant_meta:meta])* $variant:ident => $text:tt
            $(($tuple:ty))?
            $({ $( $(#[$field_meta:meta])* $field:ident: $ty:ty ),* $(,)? })?,
        )*
    }) => {
        $(#[$meta])*
        pub enum $name {
            $( $(#[$variant_meta])* #[serde(rename = $text)]
                $variant $(($tuple))?
                $({ $( $(#[$field_meta])* $field: $ty, )* })?,
            )*
        }
        impl $name {
            /// Every dotted kind, in declaration order.
            pub const ALL_KINDS: &'static [&'static str] = &[$($text,)*];

            /// The dotted kind, as stored in the `kind` column.
            pub fn kind(&self) -> &'static str {
                match self {
                    $(Self::$variant $( (event_kinds!(@ignore $tuple)) )?
                        $( { $($field: _,)* } )? => $text,)*
                }
            }
        }
    };
    (@ignore $ty:ty) => { _ };
}

event_kinds! {
    /// One variant per line of `contracts/events.md`. On the wire: `kind` is the
    /// dotted name and `payload` the fields.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
    #[serde(tag = "kind", content = "payload")]
    pub enum EventKind {
        EngineStarted => "engine.started" { version: String },
        EngineReconciled => "engine.reconciled" { task: TaskId, state: TaskState },
        /// An update finished: the target binary is current. `snapshot` names the
        /// backup taken before it.
        EngineUpdated => "engine.updated" {
            from: String,
            to: String,
            snapshot: String,
        },
        /// An update did not take: the database is back as it was and the old binary
        /// carries on.
        EngineUpdateFailed => "engine.update_failed" {
            from: String,
            to: String,
            reason: String,
        },

        ProjectAdded => "project.added" {
            id: ProjectId,
            name: String,
            repo: String,
            default_branch: String,
            ci: CiMode,
        },
        /// Successful creation, with the immutable forge identity for setup recovery.
        RepositoryCreated => "repository.created" {
            name: String,
            repo: String,
            repository_id: u64,
        },
        /// The exact root commit, recorded before the empty-ref setup push.
        RepositorySetupPrepared => "repository.setup_prepared" {
            repo: String,
            branch: String,
            head: String,
            ci: CiMode,
        },
        ProjectPaused => "project.paused" {},
        ProjectResumed => "project.resumed" {},
        /// The owner opened the project: Home's "last looked" mark. The
        /// project's events after its latest `project.seen` are new to the owner.
        ProjectSeen => "project.seen" {},
        /// The owner set the project's brief: private context for the project's
        /// runs, kept only in this log (never sent to the forge).
        ProjectBriefSet => "project.brief_set" { brief: String },
        /// What dvb found in the repository's open issues when the project was added.
        ProjectIssuesFound => "project.issues_found"(crate::api::IssueCounts),
        /// GitHub allows no ruleset on this repository (a private repository on
        /// GitHub Free), so its default branch has none of dvb's; dvb goes on
        /// without it. Recorded when it starts, not again while it lasts.
        ProjectRulesetMissing => "project.ruleset_missing" {},
        /// A project that had no ruleset got dvb's when its checks were set.
        ProjectRulesetInstalled => "project.ruleset_installed" {},

        RoleAdded => "role.added"(RoleRecord),
        /// `restored_from`: the earlier event whose value this restores.
        RoleChanged => "role.changed" {
            #[serde(flatten)]
            role: RoleRecord,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            restored_from: Option<EventId>,
        },
        RoleRemoved => "role.removed"(RoleRecord),
        HireAdded => "hire.added"(HireRecord),
        /// `restored_from`: the earlier event whose value this restores.
        HireChanged => "hire.changed" {
            #[serde(flatten)]
            hire: HireRecord,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            restored_from: Option<EventId>,
        },
        HireRetired => "hire.retired"(HireRecord),
        SettingsChanged => "settings.changed" {
            key: String,
            old: serde_json::Value,
            new: serde_json::Value,
            /// The earlier event whose value this restores.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            restored_from: Option<EventId>,
        },

        GoalStated => "goal.stated" {
            goal: GoalId,
            text: String,
            from_issue: Option<u64>,
            /// Trusted issues the goal carries, read when it was stated; closed when it is Done.
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            issues: Vec<GoalIssue>,
            /// Planning waits until this goal is Done or Dropped.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            after: Option<GoalId>,
        },
        PlanProposed => "plan.proposed" {
            goal: GoalId,
            tasks: Vec<PlannedTask>,
        },
        PlanCancelled => "plan.cancelled" { goal: GoalId, tasks: Vec<TaskId> },
        GoalApproved => "goal.approved" { goal: GoalId },
        GoalDone => "goal.done" { goal: GoalId },
        /// The owner dropped the goal: it is never planned, started or woken for again.
        GoalDropped => "goal.dropped" { goal: GoalId },
        /// The owner changed the goal's `after`; null clears it.
        GoalAfterSet => "goal.after_set" { goal: GoalId, after: Option<GoalId> },

        GoalIssueClosed => "goal.issue_closed" { goal: GoalId, issue: u64 },
        /// The owner asked the orchestrator to propose the next goal from the
        /// project's open issues. No goal exists until the owner approves one.
        ProposalRequested => "proposal.requested" {},
        /// Mirror availability only; it does not change the goal or task state.
        GithubMirrorObserved => "github.mirror_observed" {
            goal: GoalId,
            task: Option<TaskId>,
            mirrored: bool,
            detail: Option<String>,
        },

        QuestionOpened => "question.opened" {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            kind: Option<QuestionKind>,
            /// The task this failure question concerns; absent in historical events.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            task: Option<TaskId>,
            question: QuestionId,
            subject: String,
            format: QuestionFormat,
            options: Vec<QuestionOption>,
            /// The recommended option's id.
            recommended: u32,
            lapses_on: LapsesOn,
            /// Project-wide main CI hold; absent in historical questions.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            main_ci: Option<Vec<crate::forge::CheckObservation>>,
        },
        QuestionAnswered => "question.answered" {
            question: QuestionId,
            answer: Answer,
        },
        /// The question no longer needs an owner answer.
        QuestionClosed => "question.closed" { question: QuestionId, task: TaskId },
        DecisionRecorded => "decision.recorded" {
            by: RoleId,
            title: String,
            options: Option<Vec<String>>,
            swot: Option<String>,
            choice: String,
            reason: String,
        },
        CommandRefused => "command.refused"(RefusedCommand),

        /// The task's run (its `run.started` has the role, hire and model).
        TaskStarted => "task.started" {
            task: TaskId,
            run: RunId,
            resume: bool,
            /// Bringing a conflicting branch up to date costs no review round.
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            branch_repair: bool,
            /// A builder round a retry granted: it may build past the
            /// review-round cap until it publishes. Older retries of a failed
            /// task are read from their `task.state` instead.
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            retried: bool,
        },
        /// The orchestrator's start of an approved task was refused for the cap on
        /// concurrent runs: the engine starts it as asked (the same model,
        /// effort and reason; none means the role's active hire) when a run ends.
        TaskWaiting => "task.waiting" {
            task: TaskId,
            model: Option<String>,
            effort: Option<Effort>,
            reason: String,
            /// The start marked the task routine.
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            routine: bool,
        },
        TaskStopped => "task.stopped" {
            task: TaskId,
            reason: Option<String>,
        },
        TaskResumed => "task.resumed" {
            task: TaskId,
            reason: Option<String>,
        },
        TaskCancelled => "task.cancelled" {
            task: TaskId,
            reason: Option<String>,
        },
        TaskDone => "task.done" {
            task: TaskId,
            reason: Option<String>,
        },
        TaskFailed => "task.failed" {
            task: TaskId,
            reason: Option<String>,
        },
        /// Private builder context, appended atomically with an accepted owner retry.
        /// None clears the previous retry's text. Historical retries have no event.
        /// Texts not set by the engine also go to the task's reviewers.
        TaskRetryTextSet => "task.retry_text_set" { task: TaskId, text: Option<String> },
        /// Not shown in the feed; refreshes the current run's task status.
        TaskReasonChanged => "task.reason" {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            resource: Option<ResourceWait>,
            task: TaskId,
            run: RunId,
            reason: Option<String>,
        },
        /// Not shown in the feed; the source of the `tasks` projection's state.
        TaskStateChanged => "task.state" {
            task: TaskId,
            from: TaskState,
            to: TaskState,
        },

        RunStarted => "run.started"(RunAssignment),
        RunPreflight => "run.preflight" {
            run: RunId,
            checks: Vec<PreflightCheck>,
            cli_version: Option<String>,
            /// The unit and user the run is confined to, as checked.
            boundary: String,
        },
        RunModel => "run.model" {
            run: RunId,
            requested: String,
            served: ServedModel,
        },
        RunOutput => "run.output" {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            resource: Option<ResourceWait>, run: RunId, item: OutputItem },
        RunUsage => "run.usage" {
            run: RunId,
            uncached_in: u64,
            cached_in: u64,
            out: u64,
            /// Estimated cost in US dollars, from the release's price table.
            est_cost: f64,
        },
        RunCollected => "run.collected" {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            outcome: Option<crate::run_verbs::BuilderOutcome>,
            run: RunId,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            head: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            summary: Option<String>,
        },
        /// A taskless run must stop even if the launcher acknowledgement is lost
        /// and the engine hands over before its end is confirmed.
        RunStopRequested => "run.stop_requested" { run: RunId },
        /// Startup confirmed this run was still active; its eventual normal end
        /// must remain a completion during replay.
        RunSurvivedRestart => "run.survived_restart" { run: RunId },
        RunEnded => "run.ended" {
            run: RunId,
            reason: EndReason,
            /// For a crashed run: the worker's exit code and one-line cause,
            /// when it left one. Absent otherwise and in older events.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            detail: Option<String>,
            /// For a crashed run: the last lines its session's CLI wrote to
            /// stderr, secret-scrubbed, at most 400 bytes. Absent otherwise
            /// and in older events.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            stderr: Option<String>,
        },
        /// The run's unit is about to start: its time limit counts from here,
        /// not from `run.started` (a queued run has no unit yet).
        RunLaunched => "run.launched" { run: RunId },
        /// The run outlived its kind's time limit; the engine stops its unit and
        /// it ends `time_limit`.
        RunTimeLimitReached => "run.time_limit_reached" {
            run: RunId,
            task: Option<TaskId>,
            kind: RunKind,
            limit_minutes: u64,
        },
        /// Before launch the engine moved this run under a timed switch, or to the
        /// orchestrator's vendor at a usage limit. Its
        /// `run.started` stays as recorded; `to_model`, `vendor` and `effort` are what
        /// it runs on now.
        RunRerouted => "run.rerouted" {
            run: RunId,
            task: Option<TaskId>,
            role: RoleId,
            from_model: String,
            to_model: String,
            vendor: Vendor,
            effort: Effort,
            /// The vendor at its limit; absent for a timed model switch.
            limited: Option<Vendor>,
            resets_at: Option<Timestamp>,
        },

        /// Starts (or replaces) the one install-wide model switch.
        OnlyStarted => "only.started"(crate::model::OnlyModels),
        /// Ends only the switch identified by its start event; stale timers are inert.
        OnlyEnded => "only.ended" { started: EventId },

        VendorLimited => "vendor.limited" {
            vendor: Vendor,
            resets_at: Option<Timestamp>,
        },
        VendorResumed => "vendor.resumed" { vendor: Vendor },

        BranchPublished => "branch.published" {
            task: TaskId,
            round: u8,
            branch: String,
            head: String,
        },
        /// The engine brought the task's branch up to date with the base branch,
        /// once the base moved while the task awaited checks or after a merge
        /// refused it as behind: its own merge commit of `base` into
        /// `from`, neutral author and committer. `same_change`: the PR's own patch
        /// (`git patch-id --stable`) is unchanged, so the clean verdict on `from`
        /// is carried over to `head` (a `verdict.recorded` copy follows); otherwise
        /// review runs again. CI always runs again on `head`.
        BranchUpdated => "branch.updated" {
            task: TaskId,
            branch: String,
            base: String,
            from: String,
            head: String,
            same_change: bool,
        },
        ScanHit => "scan.hit" {
            task: TaskId,
            file: String,
            line: u32,
            rule: String,
        },
        PrOpened => "pr.opened" { task: TaskId, pr: u64, url: String },
        /// The whole verdict, attributed from the engine's record of the reviewer run
        /// (`run.started` with `reviews: head`), never from its body.
        VerdictRecorded => "verdict.recorded" {
            /// Engine-selected stage, absent in historical verdicts.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            review_stage: Option<ReviewKind>,
            verdict: VerdictId,
            task: TaskId,
            head: String,
            run: RunId,
            role: RoleId,
            model: String,
            vendor: Vendor,
            findings: Vec<Finding>,
            summary: String,
        },
        ChecksObserved => "checks.observed" {
            task: TaskId,
            head: String,
            status: ChecksStatus,
            names: Vec<String>,
            /// The shared main-CI question this head waited for.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            retry_question: Option<QuestionId>,
            /// Check ids authorized for one recovery rerun after that hold.
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            retry_runs: Vec<u64>,
        },
        /// A changed set of required checks observed Pending on this head.
        /// The event timestamp starts newly Pending checks' durable timers.
        ChecksPending => "checks.pending" {
            task: TaskId,
            head: String,
            checks: Vec<String>,
        },
        /// A required check on the task's head has been Pending past
        /// `caps::CHECK_PENDING_LIMIT`: told to the owner once per check and
        /// head; the task keeps waiting.
        ChecksStuck => "checks.stuck" {
            task: TaskId,
            head: String,
            check: String,
            url: Option<String>,
            limit_minutes: u64,
        },
        MergeRefused => "merge.refused" { task: TaskId, reason: String },
        PrMerged => "pr.merged" { task: TaskId, pr: u64, sha: String },
        EffectRetry => "effect.retry" {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            rate_limit: Option<crate::forge::RateLimit>,
            task: TaskId,
            effect: Effect,
            attempt: u8,
            at: Timestamp,
        },
    }
}

/// Findings still blocking the merge.
pub fn open_findings(findings: &[Finding]) -> u32 {
    u32::try_from(findings.iter().filter(|f| f.open).count()).unwrap_or(u32::MAX)
}

impl EventKind {
    /// Whether this event advances project work.
    pub fn is_work(&self) -> bool {
        match self {
            Self::EngineReconciled { .. }
            | Self::GoalStated { .. }
            | Self::PlanProposed { .. }
            | Self::PlanCancelled { .. }
            | Self::GoalApproved { .. }
            | Self::GoalDone { .. }
            | Self::GoalDropped { .. }
            | Self::GoalAfterSet { .. }
            | Self::GoalIssueClosed { .. }
            | Self::ProposalRequested { .. }
            | Self::QuestionOpened { .. }
            | Self::QuestionAnswered { .. }
            | Self::QuestionClosed { .. }
            | Self::TaskStarted { .. }
            | Self::TaskWaiting { .. }
            | Self::TaskStopped { .. }
            | Self::TaskResumed { .. }
            | Self::TaskCancelled { .. }
            | Self::TaskDone { .. }
            | Self::TaskFailed { .. }
            | Self::TaskRetryTextSet { .. }
            | Self::TaskReasonChanged { .. }
            | Self::TaskStateChanged { .. }
            | Self::RunStarted(..)
            | Self::RunPreflight { .. }
            | Self::RunModel { .. }
            | Self::RunOutput { .. }
            | Self::RunUsage { .. }
            | Self::RunCollected { .. }
            | Self::RunStopRequested { .. }
            | Self::RunLaunched { .. }
            | Self::RunTimeLimitReached { .. }
            | Self::RunSurvivedRestart { .. }
            | Self::RunEnded { .. }
            | Self::RunRerouted { .. }
            | Self::BranchPublished { .. }
            | Self::BranchUpdated { .. }
            | Self::PrOpened { .. }
            | Self::VerdictRecorded { .. }
            | Self::ChecksObserved { .. }
            | Self::MergeRefused { .. }
            | Self::PrMerged { .. }
            | Self::EffectRetry { .. } => true,

            Self::EngineStarted { .. }
            | Self::EngineUpdated { .. }
            | Self::EngineUpdateFailed { .. }
            | Self::ProjectAdded { .. }
            | Self::RepositoryCreated { .. }
            | Self::RepositorySetupPrepared { .. }
            | Self::ProjectPaused { .. }
            | Self::ProjectResumed { .. }
            | Self::ProjectSeen { .. }
            | Self::ProjectBriefSet { .. }
            | Self::ProjectIssuesFound(..)
            | Self::ProjectRulesetMissing { .. }
            | Self::ProjectRulesetInstalled { .. }
            | Self::RoleAdded(..)
            | Self::RoleChanged { .. }
            | Self::RoleRemoved(..)
            | Self::HireAdded(..)
            | Self::HireChanged { .. }
            | Self::HireRetired(..)
            | Self::SettingsChanged { .. }
            | Self::GithubMirrorObserved { .. }
            | Self::DecisionRecorded { .. }
            | Self::CommandRefused(..)
            | Self::OnlyStarted(..)
            | Self::OnlyEnded { .. }
            | Self::VendorLimited { .. }
            | Self::VendorResumed { .. }
            | Self::ScanHit { .. }
            | Self::ChecksPending { .. }
            | Self::ChecksStuck { .. } => false,
        }
    }
    /// Whether this event is omitted from orchestrator activity context.
    pub fn is_prompt_chatter(&self) -> bool {
        match self {
            Self::EngineReconciled { .. }
            | Self::ProjectBriefSet { .. }
            | Self::TaskStateChanged { .. }
            | Self::RunStarted(..)
            | Self::RunPreflight { .. }
            | Self::RunModel { .. }
            | Self::RunOutput { .. }
            | Self::RunUsage { .. }
            | Self::RunCollected { .. }
            | Self::RunStopRequested { .. }
            | Self::RunLaunched { .. }
            | Self::RunSurvivedRestart { .. }
            | Self::RunEnded { .. }
            | Self::RunRerouted { .. }
            | Self::ChecksPending { .. } => true,

            Self::EngineStarted { .. }
            | Self::EngineUpdated { .. }
            | Self::EngineUpdateFailed { .. }
            | Self::ProjectAdded { .. }
            | Self::RepositoryCreated { .. }
            | Self::RepositorySetupPrepared { .. }
            | Self::ProjectPaused { .. }
            | Self::ProjectResumed { .. }
            | Self::ProjectSeen { .. }
            | Self::ProjectIssuesFound(..)
            | Self::ProjectRulesetMissing { .. }
            | Self::ProjectRulesetInstalled { .. }
            | Self::RoleAdded(..)
            | Self::RoleChanged { .. }
            | Self::RoleRemoved(..)
            | Self::HireAdded(..)
            | Self::HireChanged { .. }
            | Self::HireRetired(..)
            | Self::SettingsChanged { .. }
            | Self::GoalStated { .. }
            | Self::ChecksStuck { .. }
            | Self::ProposalRequested { .. }
            | Self::PlanProposed { .. }
            | Self::PlanCancelled { .. }
            | Self::GoalApproved { .. }
            | Self::GoalDone { .. }
            | Self::GoalDropped { .. }
            | Self::GoalAfterSet { .. }
            | Self::GoalIssueClosed { .. }
            | Self::GithubMirrorObserved { .. }
            | Self::QuestionOpened { .. }
            | Self::QuestionAnswered { .. }
            | Self::QuestionClosed { .. }
            | Self::DecisionRecorded { .. }
            | Self::CommandRefused(..)
            | Self::TaskStarted { .. }
            | Self::TaskWaiting { .. }
            | Self::TaskStopped { .. }
            | Self::TaskResumed { .. }
            | Self::TaskCancelled { .. }
            | Self::TaskDone { .. }
            | Self::TaskFailed { .. }
            | Self::TaskRetryTextSet { .. }
            | Self::TaskReasonChanged { .. }
            | Self::OnlyStarted(..)
            | Self::OnlyEnded { .. }
            | Self::VendorLimited { .. }
            | Self::VendorResumed { .. }
            | Self::BranchPublished { .. }
            | Self::BranchUpdated { .. }
            | Self::ScanHit { .. }
            | Self::PrOpened { .. }
            | Self::VerdictRecorded { .. }
            | Self::ChecksObserved { .. }
            | Self::MergeRefused { .. }
            | Self::PrMerged { .. }
            | Self::RunTimeLimitReached { .. }
            | Self::EffectRetry { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_refusals_decode_in_literal_events_and_keep_their_wording() {
        for code in ["NOT_IMPLEMENTED", "GITHUB_ACTIVITY_OFF"] {
            let json = serde_json::json!({"kind":"command.refused", "payload":{
                "command":"historical", "code":code, "detail":"historical refusal"
            }});
            let event: EventKind = serde_json::from_value(json.clone()).unwrap();
            let EventKind::CommandRefused(ref command) = event else {
                panic!("command refusal");
            };
            assert!(command.refusal.current().is_none());
            assert_eq!(command.refusal.as_str(), code);
            assert_eq!(serde_json::to_value(&event).unwrap(), json);
            assert!(
                crate::wording::event_text(&event, |_| String::new())
                    .unwrap()
                    .contains(code)
            );
        }
    }

    #[test]
    fn resource_and_question_metadata_round_trip_with_opaque_identifiers() {
        for json in [
            serde_json::json!({"kind":"run.output","payload":{
                "run":"opaque", "resource":"memory", "item":{"note":"available bytes"}
            }}),
            serde_json::json!({"kind":"task.reason","payload":{
                "run":"opaque", "task":"task", "resource":"available", "reason":null
            }}),
            serde_json::json!({"kind":"question.opened","payload":{
                "kind":{"planning_hold":{"attempt":"opaque-attempt","fetch":true}},
                "question":"opaque", "subject":"localized", "format":"simple",
                "options":[], "recommended":1, "lapses_on":{"goal_version":2}
            }}),
        ] {
            let event: EventKind = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), json);
        }
    }

    #[test]
    fn historical_effect_retries_round_trip_without_budget_fields() {
        let json = serde_json::json!({
            "kind": "effect.retry", "payload": {"task": "t", "effect": "push", "attempt": 2, "at": 12000}
        });
        let event: EventKind = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(
            event,
            EventKind::EffectRetry {
                rate_limit: None,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(event).unwrap(), json);
    }

    #[test]
    fn event_wire_shape_matches_the_table() {
        let ev = Event {
            id: EventId(7),
            at: Timestamp(1),
            actor: Actor::Run(RunId::new("r1")),
            project: Some(ProjectId::new("p1")),
            kind: EventKind::GoalApproved {
                goal: GoalId::new("g1"),
            },
            evidence: None,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": 7, "at": 1, "actor": {"run": "r1"}, "project": "p1",
                "kind": "goal.approved", "payload": {"goal": "g1"}, "evidence": null
            })
        );
        assert_eq!(serde_json::from_value::<Event>(json).unwrap(), ev);
    }

    #[test]
    fn changed_payloads_stay_flat_and_restored_from_is_optional() {
        let hire = HireRecord {
            id: HireId::new("h1"),
            role: RoleId::new("r1"),
            model: "fake-1".into(),
            effort: Effort::Medium,
            status: crate::roles::HireStatus::Active,
            routine: false,
        };
        let plain = EventKind::HireChanged {
            hire: hire.clone(),
            restored_from: None,
        };
        let json = serde_json::to_value(&plain).unwrap();
        // Historical logs have exactly the record as the payload.
        assert_eq!(json["payload"], serde_json::to_value(&hire).unwrap());
        assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), plain);

        let restored = EventKind::HireChanged {
            hire,
            restored_from: Some(EventId(4)),
        };
        let json = serde_json::to_value(&restored).unwrap();
        assert_eq!(json["payload"]["restored_from"], 4);
        assert_eq!(json["payload"]["model"], "fake-1");
        assert_eq!(serde_json::from_value::<EventKind>(json).unwrap(), restored);

        let old = serde_json::json!({"kind": "settings.changed",
            "payload": {"key": "theme", "old": "auto", "new": "dark"}});
        assert_eq!(
            serde_json::from_value::<EventKind>(old).unwrap(),
            EventKind::SettingsChanged {
                key: "theme".into(),
                old: "auto".into(),
                new: "dark".into(),
                restored_from: None,
            }
        );
    }

    #[test]
    fn historical_usage_reroutes_keep_their_wire_shape() {
        let historical = serde_json::json!({
            "kind":"run.rerouted", "payload": {
                "run":"old-run", "task":"old-task", "role":"backend",
                "from_model":"gpt-6.1-sol", "to_model":"claude-sonnet-5-5",
                "vendor":"claude", "effort":"high", "limited":"codex", "resets_at":null
            }
        });
        let event: EventKind = serde_json::from_value(historical.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), historical);
    }

    #[test]
    fn historical_review_events_keep_their_wire_shape() {
        for historical in [
            serde_json::json!({"kind": "run.started", "payload": {
                "run": "review", "task": "task", "role": "design-reviewer", "hire": "hire",
                "model": "fake-1", "vendor": "fake", "effort": "medium", "reason": "Review head.",
                "reviews": "head", "base": "head"
            }}),
            serde_json::json!({"kind": "verdict.recorded", "payload": {
                "verdict": "verdict", "run": "review", "task": "task", "head": "head",
                "role": "design-reviewer", "model": "fake-1", "vendor": "fake",
                "findings": [], "summary": "Reviewed"
            }}),
        ] {
            let event: EventKind = serde_json::from_value(historical.clone()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), historical);
        }
    }

    #[test]
    fn historical_issues_found_with_the_removed_collaborator_flag_still_reads() {
        let historical = serde_json::json!({"kind": "project.issues_found", "payload": {
            "open": 4, "trusted": 1, "labels": [{"label": "dvb", "open": 1}],
            "collaborator_check_refused": true
        }});
        let event: EventKind = serde_json::from_value(historical).unwrap();
        assert_eq!(
            event,
            EventKind::ProjectIssuesFound(crate::api::IssueCounts {
                open: 4,
                trusted: 1,
                labels: vec![crate::api::LabelCount {
                    label: "dvb".into(),
                    open: 1
                }],
            })
        );
    }

    #[test]
    fn every_kind_matches_its_historical_serde_tag_and_payload() {
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/event-kinds.json")).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for json in fixtures {
            let kind: EventKind = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(json["kind"], kind.kind());
            assert!(seen.insert(kind.kind()), "duplicate {}", kind.kind());
            assert_eq!(
                serde_json::to_value(&kind).unwrap(),
                json,
                "{}",
                kind.kind()
            );
        }
        assert_eq!(seen.len(), EventKind::ALL_KINDS.len());
        assert_eq!(seen, EventKind::ALL_KINDS.iter().copied().collect());
    }
}
