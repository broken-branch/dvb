//! Headless agent CLIs inside run units. Fake reads JSON scripts for offline
//! tests; Claude and Codex wrap their provider CLIs.

use crate::clock::Timestamp;
use crate::event::ServedModel;
use crate::model::Effort;
use std::path::Path;

/// How the CLI process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
}

/// One item from a running session, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum RunItem {
    ModelServed(ServedModel),
    Text(String),
    ToolCall {
        name: String,
    },
    Usage {
        uncached_in: u64,
        cached_in: u64,
        out: u64,
    },
    RateLimit {
        used_pct: f64,
        window: String,
        resets_at: Option<Timestamp>,
    },
    Ended(ExitStatus),
    Error(ProviderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Malformed output from a recognized vendor protocol.
    Parse(String),
    /// Transport, process or ordinary vendor failure.
    Other(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "parse error: {message}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait Provider {
    fn start(
        &self,
        ws: &Path,
        home: &Path,
        prompt: &str,
        model: &str,
        effort: Effort,
    ) -> Result<Box<dyn Session>, ProviderError>;
}

/// A running CLI session. `next` blocks until the next item; `None` after `Ended`.
pub trait Session {
    fn next(&mut self) -> Option<RunItem>;
    fn stop(&mut self);
}
