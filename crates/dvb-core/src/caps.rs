//! Release constants. Not settings: changing one is a release.

use crate::event::RunKind;
use std::time::Duration;

pub const ATTEMPTS_PER_TASK: u8 = 3;
pub const REVIEW_ROUNDS_PER_PR: u8 = 3;
pub const RETRIES_PER_EFFECT: u8 = 5;
/// Branch repair rounds a task may need (a builder merging a conflicting
/// base). The engine's own updates of a branch behind its base are not
/// counted: each one needs another merge on the base.
pub const BRANCH_REPAIRS_PER_TASK: u8 = 3;
/// Publications by a role's routine hire that review findings or failed CI
/// may send back before the task's next rounds go to the role's default hire.
pub const ROUTINE_SEND_BACKS: u8 = 2;
pub const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// After a reviewer's verdict and its last output, how long its unit may
/// stay up before the engine ends it as finished.
pub const REVIEWER_GRACE: Duration = Duration::from_secs(60);

/// How long a required check may stay Pending on a head before the owner is
/// told (once); dvb does not know a project's CI timeout.
pub const CHECK_PENDING_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How long a run may last from its launch before the engine stops it.
pub const fn time_limit(kind: RunKind) -> Duration {
    match kind {
        RunKind::Builder => Duration::from_secs(4 * 60 * 60),
        RunKind::Reviewer => Duration::from_secs(90 * 60),
        RunKind::Orchestrator => Duration::from_secs(45 * 60),
    }
}
