//! Owner API types for HTTP/1.1 and JSON over `/run/dvb/engine.sock`.
//! The engine router and these types generate `contracts/openapi.json` and
//! the app types through `scripts/gen-contracts.sh`. Errors carry Refusal JSON.

use crate::clock::Timestamp;
use crate::event::{Actor, Answer, CiMode, EndReason, OutputItem, QuestionFormat, QuestionOption};
use crate::ids::{
    EventId, GoalId, HireId, ProjectId, QuestionId, RoleId, RunId, TaskId, VerdictId,
};
use crate::model::{Effort, Vendor};
use crate::roles::{HireStatus, ReviewKind};
use crate::run_verbs::Finding;
use crate::state::TaskState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::{IntoParams, ToSchema};

/// The header every response carries.
pub const VERSION_HEADER: &str = "dvb-version";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VersionInfo {
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ProjectQuery {
    pub project: Option<ProjectId>,
}

/// `GET /update`: with `check=true` the engine reads the release list now;
/// without it, it answers from the last check and never touches the network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UpdateQuery {
    pub check: Option<bool>,
}

/// The `dvb status --bar` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Status {
    pub running: u32,
    pub needs_you: u32,
    /// Absent when the install has nothing to fix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<InstallWarning>,
    /// The names of the projects whose default branch has no dvb ruleset,
    /// because GitHub Free allows none on a private repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulesets_missing: Vec<String>,
    /// The failed or stopped tasks `needs_you` counts, one each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<NeedsYouTask>,
}

/// A failed or stopped task waiting on the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NeedsYouTask {
    pub id: TaskId,
    pub project: ProjectId,
    pub title: String,
    pub state: TaskState,
    /// Why it failed or stopped, as the task view's `reason` (or the
    /// failure's) says; absent when nothing says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Its open question that leaves it stopped; counted with the task, not
    /// on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<QuestionId>,
}

/// Something on the engine's machine only re-running install.sh fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallWarning {
    /// The run units are older than this engine (their version stamp is missing
    /// or differs): `dvb update` replaces the binary, never the unit files.
    CacheIsolationMissing,
}

impl InstallWarning {
    /// What the owner reads, in `dvb status` and the engine's log.
    pub fn text(self) -> &'static str {
        match self {
            Self::CacheIsolationMissing => {
                "the run units are older than this engine: re-run install.sh"
            }
        }
    }
}

/// `PUT /only`: exact selected model ids, in priority order, for 1–168 hours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetOnly {
    #[schema(min_items = 1)]
    pub models: Vec<String>,
    #[schema(minimum = 1, maximum = 168)]
    pub hours: u16,
}

/// `GET /events?after=<id>`; a `Last-Event-ID` header wins over `after`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AfterQuery {
    pub after: Option<EventId>,
}

/// Bounded, newest-first event reads; `before` is exclusive.
#[derive(Debug, Clone, Default, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventPageQuery {
    pub before: Option<EventId>,
    pub limit: Option<u32>,
    pub project: Option<ProjectId>,
    pub task: Option<TaskId>,
    /// Activity for a role, including its tasks and runs.
    pub role: Option<RoleId>,
    /// Comma-separated event kinds. Omitted means every kind.
    pub kinds: Option<String>,
    /// Latest evidence per project/kind/task/goal, rather than all observations.
    pub latest: Option<bool>,
    /// Include project ordering and attention counts across the entire log.
    pub summary: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProjectActivity {
    pub project: ProjectId,
    /// Goals in their original statement order, independent of generated ids.
    pub goals: Vec<GoalId>,
    pub latest: EventId,
    pub fresh: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EventPage {
    pub cursor: EventId,
    pub events: Vec<crate::Event>,
    pub before: Option<EventId>,
    pub activity: Vec<ProjectActivity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewProject {
    pub name: String,
    /// Explicitly create a new private repository before setup.
    #[serde(default)]
    pub create: bool,
    /// A GitHub `owner/name`, or a local path for the Fake forge in dev mode.
    pub repo: String,
    /// The repo's default branch when absent.
    pub default_branch: Option<String>,
    pub ci: CiMode,
}

/// The project's repository under its new name, after a rename or transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectRepo {
    /// A GitHub `owner/name`.
    pub repo: String,
}

/// A project removed from this install, and its files on this machine that
/// could not be freed, each as `<path>: <error>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectRemoval {
    pub project: ProjectView,
    pub not_freed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FinishedWork {
    pub text: String,
    pub pr: Option<u64>,
    pub at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub repo: String,
    pub default_branch: String,
    pub ci: CiMode,
    pub paused: bool,
    #[serde(default)]
    pub last_work_at: Option<Timestamp>,
    #[serde(default)]
    pub done_goals: u32,
    #[serde(default)]
    pub last_finished: Option<FinishedWork>,
    /// Open issues found when the project was added; absent when they could not be read.
    #[serde(default)]
    pub issues_found: Option<IssueCounts>,
    /// Only in the response to adding the project: the repository's most
    /// recent dvb commit names someone other than dvb's publishing identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_mismatch: Option<PublicationMismatch>,
}

/// A commit's author or committer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

/// `GET /settings/publication`: who dvb's published commits name as author
/// and committer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublishingIdentity {
    pub name: String,
    pub email: String,
    /// dvb's default, `dvb <dvb@dvb.invalid>`.
    pub is_default: bool,
    /// The owner chose to publish as themself: the name and email may be
    /// theirs as recorded at GitHub setup.
    pub as_owner: bool,
}

/// `PUT /settings/publication`: who dvb's next published commits name as
/// author and committer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicationChange {
    pub name: String,
    pub email: String,
    /// Publish as the owner: the name and email may be theirs as recorded at
    /// GitHub setup. Off keeps them out.
    #[serde(default)]
    pub as_owner: bool,
}

/// The publishing identity and the author of the repository's most recent
/// dvb commit, when they differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicationMismatch {
    pub publishes_as: CommitIdentity,
    pub recent: CommitIdentity,
    /// `recent` is the owner as recorded at GitHub setup.
    pub recent_is_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewGoal {
    /// May be empty when `issues` names at least one issue.
    pub text: String,
    /// Closed when the goal is Done; its text is not read.
    pub from_issue: Option<u64>,
    /// Issues of the project's repository the goal carries; each must be trusted.
    /// The goal's last PR closes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<u64>,
    /// An existing goal in this project; wait until it is Done or Dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<GoalId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Stated,
    Approved,
    /// A task is running, in review or in CI.
    InProgress,
    Done,
    /// Every task ended, and some failed or were stopped.
    Failed,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoalView {
    /// The predecessor still holding this goal's planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waits_for: Option<GoalId>,
    pub id: GoalId,
    pub project: ProjectId,
    pub text: String,
    pub state: GoalState,
    /// The owner answered its plan question with an option that does not
    /// approve the plan ("Not yet"): the plan is on hold, nothing asks or
    /// runs for it, until `POST /goals/{id}/resume` asks the question again.
    #[serde(default)]
    pub plan_parked: bool,
}

/// `GET /projects/{id}/issues`: an open issue of the project's repository. An
/// untrusted issue is shown with its title and link only, and never reaches a run's prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueListing {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    /// Any issue of a private repository; in a public or internal one, opened
    /// by the owner, a member or a collaborator.
    pub trusted: bool,
}

/// Counts of a repository's open issues under the trust rule, read from the
/// forge (`GET /projects/{id}/issues/counts`, and `project.issues_found` when
/// the project was added).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssueCounts {
    pub open: u32,
    /// Open issues dvb may propose and put into prompts.
    pub trusted: u32,
    /// Open issues per label, most used first, then by name.
    pub labels: Vec<LabelCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LabelCount {
    pub label: String,
    pub open: u32,
}

/// Optional missing information accompanying a retry; does not replace the task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RetryTask {
    /// At most 16 KiB of UTF-8, like owner role instructions. Blank means no text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Why the owner stops a task. A stop with a reason wakes no orchestrator to ask why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StopTask {
    /// Blank means no reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Byte bound shared by owner and attributed retries.
pub const RETRY_TEXT_MAX_BYTES: usize = 16 * 1024;

/// "Change course": the goal's new text; the old plan is cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Redirect {
    pub text: String,
}

/// The goal a stated goal waits for before planning; null clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoalAfter {
    pub after: Option<GoalId>,
}

/// A project's brief: private context for its runs, kept only in dvb's event
/// store. An empty text clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Brief {
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct QuestionsQuery {
    /// Only open questions when true.
    pub open: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuestionView {
    /// Typed engine actions; completion choices 1=merge anyway, 2=retry check,
    /// 3=leave stopped. Bound to one result, never a general merge permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<crate::event::QuestionKind>,
    /// The persisted tasks presented for plan approval.
    #[serde(default)]
    pub plan: Vec<crate::event::PlannedTask>,
    /// Attributed task, including compatibility with historical failure questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    /// The goal it asks about: its task's goal, or the goal whose version it
    /// lapses on. Absent for a question about a setting or the install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalId>,
    pub id: QuestionId,
    pub project: ProjectId,
    pub subject: String,
    #[serde(default)]
    pub opened_at: Option<Timestamp>,
    pub format: QuestionFormat,
    pub options: Vec<QuestionOption>,
    pub recommended: u32,
    pub open: bool,
    pub answer: Option<Answer>,
}

impl QuestionView {
    /// The engine's round-cap choice also occurs in historical questions.
    pub fn leaves_task_stopped(&self) -> bool {
        self.options.iter().any(|o| {
            o.id == self.recommended
                && o.text == "Leave it stopped"
                && !o.approves_plan
                && o.retries_task.is_none()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TaskView {
    /// Enforced PR-merge dependencies, including ones already satisfied.
    #[serde(default)]
    pub after: Vec<TaskId>,
    /// Unmerged prerequisites for a never-started task. State remains Planned.
    #[serde(default)]
    pub waiting_for: Vec<TaskId>,
    #[serde(default)]
    pub outcome: crate::run_verbs::BuilderOutcome,
    /// A later merged task of this goal resolved this failed or stopped work.
    #[serde(default)]
    pub problem_superseded: bool,
    #[serde(default)]
    pub current_run: Option<RunView>,
    pub id: TaskId,
    pub project: ProjectId,
    pub goal: GoalId,
    pub title: String,
    pub role: RoleId,
    pub review: ReviewKind,
    pub state: TaskState,
    /// Derived display reason, including failures cancelled by a dropped goal.
    /// Older task views omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub pr: Option<u64>,
    pub head: Option<String>,
    /// The plan or the orchestrator's start marked the task routine: its
    /// builder rounds run on the role's routine hire when it has one.
    #[serde(default)]
    pub routine: bool,
    /// The model the task's latest builder round runs or ran on (after any
    /// reroute); absent before its first round.
    #[serde(default)]
    pub round_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunView {
    #[serde(default)]
    pub outcome: crate::run_verbs::BuilderOutcome,
    pub id: RunId,
    pub project: ProjectId,
    pub task: Option<TaskId>,
    pub role: RoleId,
    pub hire: HireId,
    pub model: String,
    pub vendor: Vendor,
    pub effort: Effort,
    pub reason: String,
    /// For a reviewer run: the head it reviews.
    pub reviews: Option<String>,
    pub started_at: Timestamp,
    /// Absent while the run is live.
    pub ended: Option<EndReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunOutputLine {
    pub id: EventId,
    pub at: Timestamp,
    pub item: OutputItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiffView {
    pub task: TaskId,
    pub base: String,
    pub head: String,
    /// Unified diff of the publication head against its base.
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VerdictView {
    pub verdict: VerdictId,
    pub task: TaskId,
    pub head: String,
    pub run: RunId,
    pub role: RoleId,
    pub model: String,
    pub vendor: Vendor,
    /// Derived from `findings`.
    pub open_findings: u32,
    pub summary: String,
    pub findings: Vec<Finding>,
}

/// A role's complete editable value for creation and history; the engine makes the slug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RoleFields {
    pub name: String,
    pub job: String,
    pub lane: String,
    pub lead: bool,
    pub reports_to: Option<RoleId>,
    pub reviewed_by: Option<RoleId>,
    pub review_instructions: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub order: u32,
}

/// Fields to change on a role. Omission preserves the current value; null clears an assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RoleChanges {
    pub name: Option<String>,
    pub job: Option<String>,
    pub lane: Option<String>,
    pub lead: Option<bool>,
    #[serde(
        default,
        deserialize_with = "nullable_change",
        skip_serializing_if = "Option::is_none"
    )]
    pub reports_to: Option<Option<RoleId>>,
    #[serde(
        default,
        deserialize_with = "nullable_change",
        skip_serializing_if = "Option::is_none"
    )]
    pub reviewed_by: Option<Option<RoleId>>,
    pub review_instructions: Option<String>,
    #[serde(
        default,
        deserialize_with = "nullable_change",
        skip_serializing_if = "Option::is_none"
    )]
    pub group: Option<Option<String>>,
    pub order: Option<u32>,
}

// A present null clears a field; serde's usual Option would also treat it as omitted.
fn nullable_change<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewHire {
    pub role: RoleId,
    pub model: String,
    pub effort: Effort,
    /// Hire the role's routine hire, replacing only its current routine hire.
    #[serde(default)]
    pub routine: bool,
}

/// Owner hiring receipt. Existing hire fields stay at the top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HireCreated {
    #[serde(flatten)]
    pub hire: crate::roles::HireRecord,
    /// Hires retired by this transaction, in their prior selection order.
    #[serde(default)]
    pub retired: Vec<crate::roles::HireRecord>,
}

/// `GET /hires`: a stored hire and the engine's default hire for its role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HireView {
    #[serde(flatten)]
    pub hire: crate::roles::HireRecord,
    pub default_hire: Option<HireId>,
}

/// Atomic chart operations. Group members must share a reporting line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChartMove {
    Report {
        role: RoleId,
        parent: Option<RoleId>,
    },
    Group {
        roles: Vec<RoleId>,
        group: Option<String>,
    },
    Reorder {
        role: RoleId,
        /// None places the role last on its current reporting line.
        before: Option<RoleId>,
    },
    /// Exchange a direct report and its lead, including their other reports.
    Swap { role: RoleId, lead: RoleId },
}

crate::text::text_enum! {
    /// Supported settings names. The string wire shape remains compatible with
    /// existing clients and settings.changed events.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
    pub enum SettingsKey {
        /// Install-wide run limit: whole number, minimum 1, default 3; no policy maximum.
        MaxRuns => "max_runs",
        /// Live dvb.slice MemoryMax in whole GiB (null if unlimited, absent without
        /// installed units); never stored as a preference. Saves accept 2 through PC RAM.
        RunsMemoryGb => "runs_memory_gb",
        ResponseStyle => "response_style",
        QuestionFormat => "question_format",
        NotifyFaults => "notify_faults",
        NotifyCompleted => "notify_completed",
        ModelPool => "model_pool",
        ModelMode => "model_mode",
        Theme => "theme",
        Font => "font",
        /// Written goal/task text size: S, M, L (default), or XL.
        TextSize => "text_size",
        Motion => "motion",
        TaskPanels => "task_panels",
        TaskFilesRight => "task_files_right",
        /// Move a limited vendor's runs to the orchestrator's vendor (bool, default true).
        VendorFallback => "vendor_fallback",
        /// Names of the projects where dvb takes the recommended option of a
        /// plan question, and of a what-next proposal whose issues are all
        /// trusted, by itself (default empty: the owner answers every question).
        AutonomousProjects => "autonomous_projects",
    }
}

impl Ord for SettingsKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}
impl PartialOrd for SettingsKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::borrow::Borrow<str> for SettingsKey {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum ResponseStyle {
    Simple,
    Clear,
    Technical,
    Expert,
}

crate::text::text_enum! {
    /// How the engine selects models for new work.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
    pub enum ModelMode {
        Custom => "Custom",
        Auto => "Auto",
    }
}

/// Settings by key. `PUT /settings` takes only the keys to change.
/// `max_runs` is an install-wide whole number, minimum 1, default 3, with no policy upper limit.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
pub struct Settings {
    pub values: BTreeMap<SettingsKey, serde_json::Value>,
    /// Read-only total PC RAM, rounded down to whole GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(read_only)]
    pub ram_gb: Option<u64>,
    /// Read-only warning threshold: half the PC's RAM in whole GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(read_only)]
    pub half_ram_gb: Option<u64>,
}

impl Settings {
    /// Install defaults; an absent pool means every installed model is selected.
    pub fn defaults() -> Self {
        use SettingsKey::*;
        use serde_json::json;
        Self {
            values: [
                (ResponseStyle, json!("Clear")),
                (QuestionFormat, json!("quick_compare")),
                (NotifyFaults, json!(true)),
                (NotifyCompleted, json!(false)),
                (MaxRuns, json!(3)),
                (ModelMode, json!(crate::api::ModelMode::Custom)),
                (Theme, json!("auto")),
                (Font, json!("Auto")),
                (TextSize, json!("L")),
                (Motion, json!("Follow system")),
                (TaskPanels, json!({"review": true, "checks": true})),
                (TaskFilesRight, json!(false)),
                (VendorFallback, json!(true)),
                (AutonomousProjects, json!([])),
            ]
            .into(),
            ram_gb: None,
            half_ram_gb: None,
        }
    }

    pub fn runs_memory_warning(&self) -> Option<&'static str> {
        let limit = self.values.get(&SettingsKey::RunsMemoryGb)?.as_u64()?;
        (limit > self.half_ram_gb?).then_some(
            "Several runs building at once can use this much memory and freeze the desktop.",
        )
    }

    pub fn model_mode(&self) -> ModelMode {
        self.values
            .get(&SettingsKey::ModelMode)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(ModelMode::Custom)
    }

    pub fn model_selected(&self, model: &str) -> bool {
        self.values.get(&SettingsKey::ModelPool).is_none_or(|v| {
            v.as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(model)))
        })
    }

    pub fn question_format(&self) -> QuestionFormat {
        self.values
            .get(&SettingsKey::QuestionFormat)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(QuestionFormat::QuickCompare)
    }

    pub fn response_style(&self) -> &str {
        self.values
            .get(&SettingsKey::ResponseStyle)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Clear")
    }

    /// Whether the project named `name` is in `autonomous_projects`.
    pub fn autonomous(&self, name: &str) -> bool {
        self.values
            .get(&SettingsKey::AutonomousProjects)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(name)))
    }

    pub fn vendor_fallback(&self) -> bool {
        self.values
            .get(&SettingsKey::VendorFallback)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UsageQuery {
    pub project: Option<ProjectId>,
    pub since: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UsageRow {
    pub project: ProjectId,
    pub role: RoleId,
    pub model: String,
    pub vendor: Vendor,
    pub uncached_in: u64,
    pub cached_in: u64,
    pub out: u64,
    /// US dollars, estimated from the release's price table.
    pub est_cost: f64,
}

/// A vendor's usage window, read at no cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct VendorWindow {
    pub vendor: Vendor,
    pub used_pct: f64,
    pub window: String,
    pub resets_at: Option<Timestamp>,
    pub limited: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    pub rows: Vec<UsageRow>,
    pub windows: Vec<VendorWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateInfo {
    pub current: String,
    pub available: Option<String>,
    /// Present while an update is under way.
    pub updating: Option<UpdateStatus>,
    /// The latest handover's interrupted task runs and their recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<UpdateCompletion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateCompletion {
    pub to: String,
    pub stopped: u32,
    pub resumed: u32,
    /// Update stops still waiting to resume (including startup reconciliation).
    pub pending: u32,
}

/// An update that has started: waiting for runs to finish, then switching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateStatus {
    pub to: String,
    /// Runs still going; drain waits up to 30 seconds, then stops them.
    pub runs_active: u32,
}

/// `POST /update`: install the newest release, or `to` (a version the database
/// schema of this one can read).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StartUpdate {
    pub to: Option<String>,
}

/// The optional body of `POST /logins/{vendor}`: the second call, once the owner
/// has the URL. `code` is what the vendor's page showed (Claude: pasted back into
/// the login run); an empty `code` just waits for the login to finish (Codex: the
/// page took the device code). The reply's `prompt` then says how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LoginCode {
    pub run: RunId,
    pub code: String,
}

/// Login progress, independent of the human-readable prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoginStatus {
    Pending,
    LoggedIn,
    Failed,
}

/// A login run inside the vendor's unit: its progress, URL and instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LoginStarted {
    pub status: LoginStatus,
    pub run: RunId,
    pub url: String,
    pub prompt: String,
}

/// One past value of a role, hire or setting: the event that set it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Version<T> {
    pub event: EventId,
    pub at: Timestamp,
    pub actor: Actor,
    pub value: T,
}

/// `GET /projects/{id}/checks/history`: newest first, including project addition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectChecksHistory {
    pub project: ProjectId,
    pub versions: Vec<Version<CiMode>>,
}

/// `GET /roles/{id}/history`: newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RoleHistory {
    pub role: RoleId,
    pub versions: Vec<Version<RoleFields>>,
}

/// A hire's recorded fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HireFields {
    pub role: RoleId,
    pub model: String,
    pub effort: Effort,
    pub status: HireStatus,
    #[serde(default)]
    pub routine: bool,
}

/// `GET /hires/{id}/history`: newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct HireHistory {
    pub hire: HireId,
    pub versions: Vec<Version<HireFields>>,
}

/// `GET /settings/history?key=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SettingsHistoryQuery {
    pub key: SettingsKey,
}

/// `GET /settings/history?key=`: newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SettingsHistory {
    pub key: SettingsKey,
    pub versions: Vec<Version<serde_json::Value>>,
}

/// `POST /roles/{id}/restore` and `/hires/{id}/restore`: bring back the value set
/// by `event`. `current` is the latest event the owner was looking at; if the
/// record changed since, the restore is refused with `VALUE_CHANGED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Restore {
    pub event: EventId,
    pub current: EventId,
}

/// `POST /settings/restore`: `Restore` for one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SettingsRestore {
    pub key: SettingsKey,
    pub event: EventId,
    pub current: EventId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Bug,
    Feature,
}

/// `POST /report`: what the owner wrote. Nothing is filed or recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Report {
    pub kind: ReportKind,
    pub title: String,
    pub text: String,
}

/// The issue exactly as it would be filed, and the GitHub new-issue URL that
/// opens it prefilled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportPreview {
    pub title: String,
    pub body: String,
    pub url: String,
}

/// `GET /logins`: which vendors' login files are present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Logins {
    pub claude: bool,
    pub codex: bool,
    /// The vendors a new project's default hires will use, so their sign-in is
    /// needed; empty when no model is selected. Fake needs no sign-in.
    #[serde(default)]
    pub needed: Vec<Vendor>,
}

#[cfg(test)]
mod settings_contract_tests {
    use super::*;
    #[test]
    fn typed_keys_keep_the_existing_wire_shape_and_string_lookup() {
        let settings: Settings = serde_json::from_str(
            r#"{"values":{"response_style":"Expert","font":"Auto","notify_faults":true}}"#,
        )
        .unwrap();
        assert_eq!(settings.values["response_style"], "Expert");
        assert_eq!(settings.values["font"], "Auto");
        assert_eq!(settings.values["notify_faults"], true);
        assert!(serde_json::from_str::<Settings>(r#"{"values":{"unknown":true}}"#).is_err());
        for style in ["Simple", "Clear", "Technical", "Expert"] {
            assert!(serde_json::from_value::<ResponseStyle>(serde_json::json!(style)).is_ok());
        }
    }
}

/// Starts the private App manifest flow. Identity remains local to the engine.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubManifestRequest {
    pub organization: String,
    pub redirect_url: String,
    pub state: String,
    pub owner_name: String,
    pub owner_email: String,
}
/// `GET /github/prefill`: suggestions for Create App's fields, each null when
/// unknown. The engine gives what an earlier App stored; the desk's route of the
/// same name adds the owner's git `user.name` and `user.email`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GithubPrefill {
    pub organization: Option<String>,
    pub owner_name: Option<String>,
    pub owner_email: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubManifest {
    pub create_url: String,
    pub manifest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubCallback {
    pub state: String,
    pub code: Option<String>,
    pub installation_id: Option<u64>,
    pub error: Option<GithubCallbackError>,
}
/// The kind of GitHub account the App is created under; its login is the
/// `organization` field. Apps set up before personal accounts were supported are organizations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GithubAccountType {
    #[default]
    Organization,
    User,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubConnection {
    pub organization: Option<String>,
    /// The type of the `organization` account, null while no App is pending or installed.
    #[serde(default)]
    pub account_type: Option<GithubAccountType>,
    pub install_url: Option<String>,
    pub connected: bool,
    pub repository_access_url: Option<String>,
    pub create_app: GithubSetupStep,
    pub install_app: GithubSetupStep,
}

/// Safe, actionable setup/discovery failure. Never contains GitHub response bodies.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubFailure {
    pub code: GithubFailureCode,
    pub message: String,
    pub retryable: bool,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GithubFailureCode {
    NotConnected,
    RepositoryInaccessible,
    DiscoveryFailed,
    AuthorizationDenied,
    CallbackExpired,
    CreateAppFailed,
    InstallAppFailed,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GithubSetupStep {
    #[default]
    NotStarted,
    AwaitingCallback,
    Succeeded,
    Failed {
        failure: GithubFailure,
    },
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct GithubSetupProgress {
    pub create_app: GithubSetupStep,
    pub install_app: GithubSetupStep,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GithubCallbackError {
    Denied,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubInstallRequest {
    pub state: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubRepository {
    pub repo: String,
    pub private: bool,
    pub default_branch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GithubRepositories {
    pub organization: String,
    pub repository_access_url: String,
    pub repositories: Vec<GithubRepository>,
}
/// Only `discovered` with an empty checks list confirms absence on this head.
/// Pending or unreadable observations are still checks, never absence.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GithubCheckDiscovery {
    Discovered {
        default_branch: String,
        head: String,
        checks: Vec<crate::forge::CheckObservation>,
    },
    Inaccessible {
        failure: GithubFailure,
    },
    Failed {
        failure: GithubFailure,
    },
}

/// Default-branch check discovery for a repository the installation can reach.
#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GithubChecksQuery {
    pub repo: String,
}
