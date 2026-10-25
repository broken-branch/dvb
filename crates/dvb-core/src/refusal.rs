//! Refusal codes. A refusal is a code, never prose to match on.
//! Owner errors share this JSON body; command refusals use HTTP 409.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Which cap was reached (`dvb_core::caps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    ConcurrentRuns,
    AttemptsPerTask,
    ReviewRoundsPerPr,
    RetriesPerEffect,
}

/// The code, with the few fields some codes carry. Serialized with the code in
/// `code`, e.g. `{"code": "CAP_REACHED", "which": "concurrent_runs"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RefusalCode {
    GoalNotApproved,
    DependencyNotMerged {
        task: crate::TaskId,
    },
    QuestionLapsed,
    ValueChanged,
    InvalidValue,
    NotFound,
    InternalError,
    AlreadyStopped,
    AlreadyTerminal,
    CapReached {
        which: Cap,
    },
    RoleNotAllowed,
    ModelNotInPool,
    NoActiveHire,
    ProjectPaused,
    ProjectUnknown,
    EngineUpdating,
    /// An update was refused before it began; the detail says why.
    UpdateRefused,
    VersionMismatch {
        client: String,
        engine: String,
    },
    NotARun,
    HeadMismatch,
    /// A run was served by a model other than the one asked for.
    ModelMismatch,
    ScanHit {
        file: String,
        line: u32,
        rule: String,
    },
    CiEvidenceMissing,
    /// The issue, in a public repository, was not opened by the owner, a
    /// member or a collaborator; only its link is given.
    IssueUntrusted {
        issue: u64,
        url: String,
    },
    /// A project name is empty or has a character other than ASCII letters,
    /// digits, `-` and `_`.
    InvalidProjectName,
    /// Only a goal whose plan is on hold (`plan_parked`) can be resumed.
    PlanNotParked,
    /// A goal's order can change only while it is stated with no started task.
    GoalStarted,
    /// The goal order would wait on itself.
    GoalOrderCycle,
    /// The new repository name reaches no repository through the App's
    /// installation, or a different one from the project's.
    RepositoryMismatch,
    /// A project with running or open tasks, or a run still working, is not
    /// removed; `tasks` names them and the detail says how to stop each.
    ProjectBusy {
        tasks: Vec<crate::TaskId>,
    },
}

impl RefusalCode {
    /// The code as written on the wire and in `command.refused`.
    #[expect(
        clippy::expect_used,
        reason = "RefusalCode always serializes to an object with its serde code tag"
    )]
    pub fn as_str(&self) -> String {
        serde_json::to_value(self).expect("refusal serialization")["code"]
            .as_str()
            .expect("refusal code tag")
            .to_owned()
    }

    /// The HTTP status of this refusal on the API.
    pub fn http_status(&self) -> u16 {
        match self {
            RefusalCode::NotFound => 404,
            RefusalCode::InternalError => 500,
            RefusalCode::DependencyNotMerged { .. }
            | RefusalCode::GoalNotApproved
            | RefusalCode::QuestionLapsed
            | RefusalCode::InvalidValue
            | RefusalCode::ValueChanged
            | RefusalCode::AlreadyStopped
            | RefusalCode::AlreadyTerminal
            | RefusalCode::CapReached { .. }
            | RefusalCode::RoleNotAllowed
            | RefusalCode::ModelNotInPool
            | RefusalCode::NoActiveHire
            | RefusalCode::ProjectPaused
            | RefusalCode::ProjectUnknown
            | RefusalCode::EngineUpdating
            | RefusalCode::UpdateRefused
            | RefusalCode::VersionMismatch { .. }
            | RefusalCode::NotARun
            | RefusalCode::HeadMismatch
            | RefusalCode::ModelMismatch
            | RefusalCode::ScanHit { .. }
            | RefusalCode::CiEvidenceMissing
            | RefusalCode::IssueUntrusted { .. }
            | RefusalCode::InvalidProjectName
            | RefusalCode::PlanNotParked
            | RefusalCode::GoalStarted
            | RefusalCode::GoalOrderCycle
            | RefusalCode::RepositoryMismatch
            | RefusalCode::ProjectBusy { .. } => 409,
        }
    }
}

/// A refused command: the code and a detail for people (never matched on).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Refusal {
    #[serde(flatten)]
    pub code: RefusalCode,
    pub detail: String,
}

impl Refusal {
    pub fn new(code: RefusalCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape_carries_the_code_and_its_fields() {
        let r = Refusal::new(
            RefusalCode::CapReached {
                which: Cap::ConcurrentRuns,
            },
            "3 runs",
        );
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"code": "CAP_REACHED", "which": "concurrent_runs", "detail": "3 runs"})
        );
        assert_eq!(serde_json::from_value::<Refusal>(json).unwrap(), r);
    }
}
