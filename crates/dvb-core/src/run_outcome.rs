//! Worker outcomes mapped to lifecycle end reasons.

use crate::event::EndReason;
use std::process::{ExitCode, Termination};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    SetupFailed,
    PreflightRefused,
    SessionFailed,
    Mismatch,
    ParseError,
}

impl RunOutcome {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::SetupFailed => 2,
            Self::PreflightRefused => 3,
            Self::SessionFailed => 4,
            Self::Mismatch => 5,
            Self::ParseError => 6,
        }
    }

    /// Only these two classifications override the engine's unit observation.
    pub const fn driver_reason(self) -> Option<&'static str> {
        match self {
            Self::Mismatch => Some("mismatch"),
            Self::ParseError => Some("parse_error"),
            Self::Completed | Self::SetupFailed | Self::PreflightRefused | Self::SessionFailed => {
                None
            }
        }
    }

    pub fn from_driver_reason(text: &str) -> Option<Self> {
        [Self::Mismatch, Self::ParseError]
            .into_iter()
            .find(|outcome| outcome.driver_reason() == Some(text))
    }

    /// The engine refines failed preflight to its check code.
    pub const fn end_reason(self) -> EndReason {
        match self {
            Self::Completed => EndReason::Completed,
            Self::Mismatch => EndReason::Mismatch,
            Self::ParseError => EndReason::ParseError,
            Self::SetupFailed | Self::PreflightRefused | Self::SessionFailed => EndReason::Crashed,
        }
    }
}

impl Termination for RunOutcome {
    fn report(self) -> ExitCode {
        ExitCode::from(self.exit_code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PreflightCode;
    use crate::{EventKind, RunId};

    #[test]
    fn worker_codes_text_and_termination_agree() {
        for (outcome, code, text, reason) in [
            (RunOutcome::Completed, 0, None, EndReason::Completed),
            (RunOutcome::SetupFailed, 2, None, EndReason::Crashed),
            (RunOutcome::PreflightRefused, 3, None, EndReason::Crashed),
            (RunOutcome::SessionFailed, 4, None, EndReason::Crashed),
            (
                RunOutcome::Mismatch,
                5,
                Some("mismatch"),
                EndReason::Mismatch,
            ),
            (
                RunOutcome::ParseError,
                6,
                Some("parse_error"),
                EndReason::ParseError,
            ),
        ] {
            assert_eq!(outcome.exit_code(), code);
            assert_eq!(outcome.report(), ExitCode::from(code));
            assert_eq!(outcome.driver_reason(), text);
            assert_eq!(outcome.end_reason(), reason);
            if let Some(text) = text {
                assert_eq!(RunOutcome::from_driver_reason(text), Some(outcome));
                assert_eq!(
                    serde_json::to_value(reason).unwrap(),
                    serde_json::json!(text)
                );
            }
        }
        for unknown in ["", "crashed", "Mismatch", "parse error", "setup_failed"] {
            assert_eq!(RunOutcome::from_driver_reason(unknown), None);
        }
    }

    #[test]
    fn historical_run_ended_payloads_decode_and_round_trip_unchanged() {
        // Literal stored forms from before the shared worker outcome type.
        for (payload, expected) in [
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"completed"}}"#,
                EndReason::Completed,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"stopped"}}"#,
                EndReason::Stopped,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"crashed"}}"#,
                EndReason::Crashed,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"oom"}}"#,
                EndReason::Oom,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"interrupted"}}"#,
                EndReason::Interrupted,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"mismatch"}}"#,
                EndReason::Mismatch,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":"parse_error"}}"#,
                EndReason::ParseError,
            ),
            (
                r#"{"kind":"run.ended","payload":{"run":"r1","reason":{"preflight":"PF_CLI_MISSING"}}}"#,
                EndReason::Preflight(PreflightCode::PfCliMissing),
            ),
        ] {
            let decoded: EventKind = serde_json::from_str(payload).unwrap();
            assert_eq!(
                decoded,
                EventKind::RunEnded {
                    run: RunId::new("r1"),
                    reason: expected,
                    detail: None,
                    stderr: None,
                }
            );
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::from_str::<serde_json::Value>(payload).unwrap()
            );
        }
    }
}
