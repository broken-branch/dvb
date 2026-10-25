//! Forge operations implemented by GitHub and the offline Fake forge.
//! GitHub mints scoped tokens; Fake records PRs and checks in memory and can use
//! a local bare repository. Milestones and boards mirror engine state.
//!
//! Read-only `pr` and `find_pr` recover lost publication and merge replies.
//! Issue reads report facts (author associations, labels and comments); the
//! engine applies the trust rule before any issue text reaches a prompt.

use crate::clock::Timestamp;
use crate::event::ChecksStatus;
use std::path::Path;

/// A repository: `owner/name` on GitHub; the bare repo's path for the Fake forge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Repo(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// A PR as the forge reports it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrObservation {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    /// The PR's head branch and its current head commit.
    pub branch: String,
    pub head: String,
    /// The branch it merges into.
    pub base: String,
    /// Set once merged.
    pub merge_commit: Option<String>,
    /// Whether the head conflicts with the base; None while the forge computes it.
    pub conflicting: Option<bool>,
}

/// One check run or commit status on a head, by name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct CheckObservation {
    pub name: String,
    pub status: ChecksStatus,
    /// Failed Actions check run id; workflow attempts bound automatic reruns.
    pub run: Option<u64>,
    pub conclusion: Option<String>,
    pub url: Option<String>,
}

/// What the repo's ruleset enforces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExpectedChecks {
    pub pr_required: bool,
    pub required_checks: Vec<String>,
    pub no_force_push: bool,
}

/// A GitHub budget wait, independent of failure attempts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct RateLimit {
    pub reset: Option<Timestamp>,
}

impl RateLimit {
    pub fn not_before(self, now: Timestamp, backoff_ms: i64) -> Timestamp {
        self.reset.map_or_else(
            || now.plus_millis(backoff_ms),
            |reset| reset.plus_millis(1_000).max(now.plus_millis(1_000)),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeError {
    pub rate_limit: Option<RateLimit>,
    /// Worth retrying (network, rate limit, 5xx), as opposed to refused.
    pub retryable: bool,
    /// GitHub refused a token for the repository by its saved name (HTTP 422):
    /// it was renamed or moved, or the App's installation no longer has it.
    pub unreachable: bool,
    pub detail: String,
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ForgeError {}

/// A pull request number is invalid issue input, not a forge failure to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IssueError {
    #[error("#{number} is a pull request, not an issue")]
    PullRequest { number: u64 },
    #[error(transparent)]
    Forge(#[from] ForgeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    /// The PR's head is not the `sha` asked for; review re-enters for the new head.
    HeadMoved,
    /// The forge refused for another reason.
    Refused(String),
    Forge(ForgeError),
}

/// Engine-owned goal text; `key` identifies it across retries and restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalMirror {
    pub key: String,
    pub title: String,
    pub description: String,
    pub done: bool,
}

/// The board has four active/completed columns, plus honest inactive states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardState {
    Planned,
    Working,
    InReview,
    ReadyToMerge,
    Done,
    Stopped,
    Cancelled,
    Failed,
    Unknown,
}
impl BoardState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Planned => "Planned",
            Self::Working => "Working",
            Self::InReview => "In review",
            Self::ReadyToMerge => "Ready to merge",
            Self::Done => "Done",
            Self::Stopped => "Stopped",
            Self::Cancelled => "Cancelled",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMirror {
    pub key: String,
    pub title: String,
    pub role: String,
    pub model: String,
    pub state: BoardState,
    pub pr: Option<u64>,
    pub milestone: u64,
}

/// GitHub's `author_association`: the author's standing in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Association {
    Owner,
    Member,
    Collaborator,
    Contributor,
    FirstTimeContributor,
    FirstTimer,
    Mannequin,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    #[default]
    Open,
    Closed,
}

/// Deserialized from the Fake forge's dev-mode issues file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueComment {
    pub author: String,
    #[serde(rename = "author_association")]
    pub association: Association,
    pub body: String,
}

/// Whether a repository is private: only people with access to it can open
/// or comment on its issues. An Enterprise internal repository, which every
/// member of the enterprise can read, is Public.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

/// One issue as the forge reports it now; never a pull request. The Fake
/// forge's dev-mode issues file holds these without `url` and `state`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub number: u64,
    #[serde(skip)]
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(skip)]
    pub state: IssueState,
    #[serde(default)]
    pub labels: Vec<String>,
    pub author: String,
    #[serde(rename = "author_association")]
    pub association: Association,
    #[serde(default)]
    pub comments: Vec<IssueComment>,
}

/// An open issue in a list, before the engine applies the trust rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSummary {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub labels: Vec<String>,
    pub author: String,
    pub association: Association,
    pub opened: Timestamp,
}

#[async_trait::async_trait]
pub trait Forge: Send + Sync {
    /// Read the installed App's actual repository scope, without narrowing its token.
    async fn accessible_repositories(&self) -> Result<crate::api::GithubRepositories, ForgeError>;
    /// Discover latest check runs on the immutable current default-branch head.
    async fn discover_checks(&self, repo: &Repo) -> crate::api::GithubCheckDiscovery;

    /// The repository's id, read with the installation's token: GitHub follows
    /// a renamed or moved repository's old name. None when no repository has the name.
    async fn repository_id(&self, repo: &Repo) -> Result<Option<u64>, ForgeError>;
    /// Move the mirror's receipts of `from` to `to`, the same repository under
    /// its new name, so the next pass updates its milestones and board items.
    fn repoint_mirror(&self, from: &Repo, to: &Repo) -> Result<(), ForgeError>;
    /// Drop the mirror's local receipts of `repo` (its board, milestones and
    /// cards) when its project is removed. Writes nothing on GitHub.
    fn forget_mirror(&self, repo: &Repo) -> Result<(), ForgeError>;

    /// Prepare the private organization board for this project, even before its first goal.
    async fn mirror_project(&self, repo: &Repo) -> Result<(), ForgeError>;
    /// Upsert by an engine marker. Reads serve effect recovery only, never plan input.
    async fn mirror_goal(&self, repo: &Repo, goal: &GoalMirror) -> Result<u64, ForgeError>;
    /// Upsert a draft/PR board card and attach the PR to its goal milestone. No issue creation.
    async fn mirror_task(&self, repo: &Repo, task: &TaskMirror) -> Result<(), ForgeError>;

    /// Clone an authenticated bare mirror; returns the repository's default branch.
    async fn clone_repo(&self, repo: &Repo, mirror: &Path) -> Result<String, ForgeError>;
    /// Read the Git author of a publication by its full SHA, including commits
    /// retained by a pull request after its head branch has been deleted.
    async fn commit_author(
        &self,
        repo: &Repo,
        sha: &str,
    ) -> Result<crate::api::CommitIdentity, ForgeError>;
    /// Fetch remote refs using a read token, returning the requested branch head.
    async fn fetch(&self, repo: &Repo, mirror: &Path, branch: &str) -> Result<String, ForgeError>;
    /// Install the no-bypass backstop after the setup commit/PR is published.
    async fn install_ruleset(
        &self,
        repo: &Repo,
        branch: &str,
        checks: &[String],
    ) -> Result<(), ForgeError>;
    /// The sole default-branch push exception: an empty repository just created by this forge.
    async fn push_setup(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError>;
    /// Whether `create_repo` can work: an App installation creates repositories only
    /// in an organization, never in a personal account.
    fn can_create_repo(&self) -> bool;
    /// Always private.
    async fn create_repo(&self, org: &str, name: &str) -> Result<Repo, ForgeError>;
    /// Immutable identity from a successful creation response, before any other effect.
    fn created_repo_id(&self, repo: &Repo) -> Option<u64>;
    /// Restore the setup exception only for the same repository recorded at creation.
    async fn resume_setup(&self, repo: &Repo, repository_id: u64) -> Result<(), ForgeError>;
    async fn push(
        &self,
        repo: &Repo,
        mirror: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(), ForgeError>;
    async fn open_pr(
        &self,
        repo: &Repo,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<Pr, ForgeError>;
    /// Replace the description of the task's existing PR (idempotent on retry).
    async fn edit_pr_body(&self, repo: &Repo, pr: u64, body: &str) -> Result<(), ForgeError>;
    /// Check runs and commit statuses on `head`, from both APIs.
    async fn checks(&self, repo: &Repo, head: &str) -> Result<Vec<CheckObservation>, ForgeError>;
    /// Latest completed Actions workflow jobs on the default branch's head
    /// commit, by name, from push and pull_request runs only; empty when that
    /// commit has none (main's state is unknown, not failing). Links identify
    /// the workflow run.
    async fn main_checks(
        &self,
        repo: &Repo,
        branch: &str,
    ) -> Result<Vec<CheckObservation>, ForgeError>;
    /// Rerun failed jobs once. `recovery` authorizes one more attempt of these
    /// exact check ids after a main-CI hold. Later workflow attempts deduplicate
    /// both normal and recovery requests across polling and engine restarts.
    async fn rerun_checks(
        &self,
        repo: &Repo,
        head: &str,
        runs: &[u64],
        recovery: bool,
    ) -> Result<(), ForgeError>;
    /// Cancel the Actions workflow runs still queued or running on `head`,
    /// whose result no longer counts. Callers log a failure and go on.
    async fn cancel_checks(&self, repo: &Repo, head: &str) -> Result<(), ForgeError>;
    /// A few lines from the log of failed check run `run`, where it failed,
    /// bounded and with no line the outbound scan matches.
    async fn check_excerpt(&self, repo: &Repo, run: u64) -> Result<String, ForgeError>;
    /// Merges only if the PR's head is `sha`, as one commit titled
    /// `<title> (#<pr>)`; returns the merge commit.
    async fn merge(
        &self,
        repo: &Repo,
        pr: u64,
        sha: &str,
        title: &str,
    ) -> Result<String, MergeError>;
    async fn close_issue(&self, repo: &Repo, number: u64) -> Result<(), ForgeError>;
    /// Read one issue with its labels and comments, using an
    /// `issues: read`, `pull_requests: read`, `metadata: read` token for this
    /// repository only. None when it does not exist; `IssueError::PullRequest`
    /// when it is a pull request, without reading its comments.
    async fn issue(&self, repo: &Repo, number: u64) -> Result<Option<Issue>, IssueError>;
    /// The repository's open issues, without pull requests, oldest first.
    async fn open_issues(&self, repo: &Repo) -> Result<Vec<IssueSummary>, ForgeError>;
    /// Whether the repository is private, from its metadata, read with an
    /// `issues: read`, `metadata: read` token. A public repository is reported,
    /// not refused.
    async fn visibility(&self, repo: &Repo) -> Result<Visibility, ForgeError>;
    async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<(), ForgeError>;
    async fn ruleset(&self, repo: &Repo) -> Result<ExpectedChecks, ForgeError>;
    /// Whether the plan allows a ruleset on this private repository, from a read of
    /// its rulesets, which GitHub refuses on a Free plan. Writes nothing.
    async fn rulesets_allowed(&self, repo: &Repo) -> Result<bool, ForgeError>;
    /// Read one PR.
    async fn pr(&self, repo: &Repo, number: u64) -> Result<PrObservation, ForgeError>;
    /// The most recent PR (any state) whose head is `branch`, if any.
    async fn find_pr(&self, repo: &Repo, branch: &str)
    -> Result<Option<PrObservation>, ForgeError>;
}
