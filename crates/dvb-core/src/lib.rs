//! Shared identifiers, clock, durable events, task decisions, refusal codes,
//! owner and run APIs, roles, effect interfaces and Linux/file helpers.
//! Shared contracts change through a `[contract]` PR.

pub mod api;
pub mod caps;
pub mod clock;
pub mod event;
pub mod forge;
pub mod git;
pub mod ids;
pub mod io;
pub mod launcher;
pub mod model;
pub mod provider;
pub mod refusal;
pub mod roles;
pub mod run_outcome;
pub mod run_verbs;
pub mod state;
pub mod text;
pub mod wording;

pub use clock::{Clock, FakeClock, SystemClock, Timestamp};
pub use event::{Actor, Event, EventKind, Evidence, RunAssignment};
pub use ids::{EventId, GoalId, HireId, ProjectId, QuestionId, RoleId, RunId, TaskId, VerdictId};
pub use model::{Effort, ModelPool, PoolModel, Vendor};
pub use refusal::{Cap, Refusal, RefusalCode};
pub use run_outcome::RunOutcome;
pub use state::{Command, Context, TaskState, apply, decide};

/// The version every response carries in `dvb-version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod sync;
