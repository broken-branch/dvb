//! Run request types for `dvb run <verb>` on `/run/dvb/run.sock`.
//! The engine resolves the caller from its cgroup and supplies its role, task,
//! model and head. Roles supplied in bodies are slugs such as `backend`.

use crate::clock::Timestamp;
use crate::event::{OutputItem, PreflightCheck, QuestionFormat, ServedModel};
use crate::ids::{HireId, QuestionId, RunId, TaskId};
use crate::model::Effort;
use crate::roles::ReviewKind;
use serde::{Deserialize, Deserializer, Serialize};
use std::num::NonZeroU32;
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    /// Tasks whose PRs must merge first. Numbers are 1-based positions in this
    /// proposal; ids refer to existing tasks of this goal during recovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<PlanDependency>,
    pub title: String,
    pub role: String,
    pub summary: String,
    /// Which reviewer the task needs when no lead or assignment decides it.
    #[serde(default = "code_review")]
    pub review: ReviewKind,
    /// Routine work: its builder rounds run on the role's routine hire.
    #[serde(default)]
    pub routine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlanDependency {
    Position(u32),
    Task(TaskId),
}

fn code_review() -> ReviewKind {
    ReviewKind::Code
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBody {
    /// The next goal, when the previous goal is done. It still needs plan approval.
    #[serde(default)]
    pub goal: Option<String>,
    /// Trusted issues of the project the next goal carries, as `dvb goal --issue`:
    /// their text reaches its runs and its last PR closes them. Only with `goal`,
    /// which may then be empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<u64>,
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReply {
    pub tasks: Vec<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub text: String,
    pub effect: String,
    #[serde(default)]
    pub approves_plan: bool,
    /// Choosing it changes nothing: the answer wakes no orchestrator.
    #[serde(default)]
    pub no_effect: bool,
    /// A failed task of the project this option retries, when the owner chooses it.
    #[serde(default)]
    pub retries_task: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskBody {
    pub subject: String,
    pub options: Vec<AskOption>,
    /// 1-based position of the recommended option.
    pub recommended: u32,
    /// The owner's setting when absent.
    #[serde(default)]
    pub format: Option<QuestionFormat>,
    /// The issues a what-next proposal is built from. In an autonomous
    /// project dvb takes the recommended proposal only when all are trusted.
    #[serde(default)]
    pub issues: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskReply {
    pub question: QuestionId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecideBody {
    pub title: String,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub swot: Option<String>,
    pub choice: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStartBody {
    pub task: TaskId,
    pub role: String,
    /// The role's active hire when omitted; otherwise an exact model pool id.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
    pub reason: String,
    /// Marks the task routine: its builder rounds run on the role's routine hire.
    #[serde(default)]
    pub routine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStartReply {
    pub run: Option<RunId>,
    #[serde(default)]
    pub waiting_for: Vec<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStopBody {
    pub task: TaskId,
}

/// Retry a failed task, or send one stopped or in review with a PR back to
/// its builder; only after the owner chose an option naming it in
/// `retries_task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRetryBody {
    pub task: TaskId,
    /// Missing information accompanying this retry, at most 16 KiB of UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HireBody {
    pub role: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HireReply {
    pub hire: HireId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireBody {
    pub hire: HireId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteBody {
    pub text: String,
}

/// `dvb run issue <number>`; `dvb run issues` takes an empty body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueBody {
    pub number: u64,
}

/// A trusted issue for `dvb run issue`. Its text is the issue's content, not
/// instructions to dvb; untrusted comments are left out and counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueReply {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: crate::forge::IssueState,
    pub labels: Vec<String>,
    pub body: String,
    pub comments: Vec<IssueCommentReply>,
    pub comments_left_out: usize,
}

/// `dvb run issues`: the open issues, never a body. `note` says that titles
/// are the issues' content; an untrusted issue has no title or labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuesReply {
    pub note: String,
    pub issues: Vec<ListedIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedIssue {
    pub number: u64,
    pub url: String,
    pub trusted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCommentReply {
    pub author: String,
    pub association: crate::forge::Association,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Medium,
    Low,
}

/// One review finding. Its schema is generated into `prompts/verdict.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    /// Required; `null` when the finding has no line, otherwise 1 or more.
    #[serde(deserialize_with = "required_line")]
    #[schema(value_type = Option<u32>, required = true, minimum = 1)]
    pub line: Option<NonZeroU32>,
    pub title: String,
    pub detail: String,
    /// True while the finding blocks the merge.
    pub open: bool,
}

/// `line` must be present (an `Option` field is otherwise optional in serde);
/// `NonZeroU32` refuses 0.
fn required_line<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NonZeroU32>, D::Error> {
    Option::<NonZeroU32>::deserialize(d)
}

/// The body of `dvb run verdict`. The engine adds head, pr, model, vendor and
/// role from its own record of the run; a body that fails this schema is not a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VerdictBody {
    pub findings: Vec<Finding>,
    /// What was reviewed and what was run. In a project without CI, say which local gates ran.
    pub summary: String,
}

/// JSON Schema 2020-12 for the provider's strict verdict output.
pub fn verdict_schema_json() -> Result<String, serde_json::Error> {
    use utoipa::PartialSchema;
    let mut root = serde_json::to_value(VerdictBody::schema())?;
    let mut schemas = Vec::new();
    VerdictBody::schemas(&mut schemas);
    let defs = schemas
        .into_iter()
        .map(|(name, schema)| Ok((name, serde_json::to_value(schema)?)))
        .collect::<Result<serde_json::Map<_, _>, serde_json::Error>>()?;
    root["$schema"] = "https://json-schema.org/draft/2020-12/schema".into();
    root["$id"] = "urn:dvb:verdict".into();
    root["title"] = "dvb run verdict".into();
    root["$defs"] = defs.into();
    root["examples"] = serde_json::json!([
        {"findings": [], "summary": "Reviewed the diff; cargo test and clippy ran clean."},
        {
            "findings": [{
                "severity": "high", "file": "crates/dvb-engine/src/publish/mod.rs", "line": 42,
                "title": "Push happens before the stopped check",
                "detail": "Check the task is not stopped in the same transaction before sending.",
                "open": true
            }],
            "summary": "One blocking finding."
        }
    ]);
    // OpenAPI 3.1 schemas use JSON Schema; only their reference namespace differs.
    let json = serde_json::to_string_pretty(&root)?;
    Ok(json.replace("#/components/schemas/", "#/$defs/"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightBody {
    pub checks: Vec<PreflightCheck>,
    pub cli_version: Option<String>,
    pub boundary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBody {
    pub requested: String,
    pub served: ServedModel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputBody {
    pub item: OutputItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBody {
    pub uncached_in: u64,
    pub cached_in: u64,
    pub out: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitBody {
    pub used_pct: f64,
    /// The vendor's window, e.g. `5h` or `7d`.
    pub window: String,
    pub resets_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectedBody {
    /// Absent on driver reports; retains the builder's saved result. A builder
    /// that never reports an outcome keeps the historical complete behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BuilderOutcome>,
    /// Only the driver supplies the snapshot head; builders omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Change/check write-up for the PR body, at most 16 KiB of UTF-8.
    #[serde(
        default,
        deserialize_with = "short_summary",
        skip_serializing_if = "Option::is_none"
    )]
    pub summary: Option<String>,
}

/// A builder's completion assertion, independent of review and CI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuilderOutcome {
    #[default]
    Complete,
    Partial {
        reason: PartialReason,
        detail: String,
    },
    Blocked {
        prerequisite: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PartialReason {
    OutOfRole,
    CompletionUnmet,
    Other,
}

impl BuilderOutcome {
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Complete => None,
            Self::Partial { detail, .. } => Some(detail),
            Self::Blocked { prerequisite } => Some(prerequisite),
        }
    }
}

fn short_summary<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let summary = Option::<String>::deserialize(d)?;
    if summary.as_ref().is_some_and(|s| s.len() > 16 * 1024) {
        return Err(serde::de::Error::custom("summary exceeds 16 KiB"));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collected_accepts_old_heads_and_short_summary_only_bodies() {
        let old: CollectedBody = serde_json::from_str(r#"{"head":"snapshot"}"#).unwrap();
        assert_eq!(old.head.as_deref(), Some("snapshot"));
        assert_eq!(old.summary, None);
        let body: CollectedBody =
            serde_json::from_str(r#"{"summary":"Changed and checked."}"#).unwrap();
        assert_eq!(body.head, None);
        assert_eq!(body.summary.as_deref(), Some("Changed and checked."));
        for (summary, accepted) in [("é".repeat(8192), true), ("é".repeat(8193), false)] {
            let result =
                serde_json::from_value::<CollectedBody>(serde_json::json!({"summary": summary}));
            assert_eq!(result.is_ok(), accepted);
        }
    }

    #[test]
    fn plan_task_review_defaults_to_code() {
        let t: PlanTask =
            serde_json::from_str(r#"{"title": "t", "role": "backend", "summary": "s"}"#).unwrap();
        assert_eq!(t.review, ReviewKind::Code);
    }

    #[test]
    fn the_verdict_schema_example_is_a_verdict() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../prompts/verdict.schema.json")).unwrap();
        let examples = schema["examples"].as_array().unwrap();
        assert!(!examples.is_empty());
        for ex in examples {
            serde_json::from_value::<VerdictBody>(ex.clone()).unwrap();
        }
    }

    fn finding(line: &str) -> serde_json::Result<VerdictBody> {
        let line = if line.is_empty() {
            String::new()
        } else {
            format!(r#""line": {line},"#)
        };
        serde_json::from_str(&format!(
            r#"{{"findings": [{{"severity": "high", "file": "f", {line} "title": "t", "detail": "d", "open": true}}], "summary": "s"}}"#
        ))
    }

    #[test]
    fn line_is_required_nullable_and_positive() {
        assert_eq!(finding("7").unwrap().findings[0].line, NonZeroU32::new(7));
        assert_eq!(finding("null").unwrap().findings[0].line, None);
        assert!(finding("").is_err(), "missing line");
        assert!(finding("0").is_err(), "line 0");
        assert!(finding("-1").is_err(), "negative line");
    }

    #[test]
    fn schema_openapi_and_serde_agree_on_line() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../prompts/verdict.schema.json")).unwrap();
        let release = &schema["$defs"]["Finding"];
        assert_eq!(release["properties"]["line"]["minimum"], 1);
        assert_eq!(
            release["properties"]["line"]["type"],
            serde_json::json!(["integer", "null"])
        );

        let api = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../contracts/openapi.json"
        ))
        .unwrap();
        let openapi = &api["components"]["schemas"]["Finding"];
        let expected = std::collections::BTreeSet::from([
            "severity", "file", "line", "title", "detail", "open",
        ]);
        for finding in [release, openapi] {
            let properties = finding["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            let required = finding["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(properties, expected);
            assert_eq!(required, expected);
            assert_eq!(finding["additionalProperties"], false);
        }
        assert_eq!(openapi["properties"]["line"]["minimum"], 1);
        assert_eq!(
            openapi["properties"]["line"]["type"],
            serde_json::json!(["integer", "null"])
        );
    }

    #[test]
    fn a_verdict_with_extra_fields_is_refused() {
        let r = serde_json::from_str::<VerdictBody>(
            r#"{"findings": [], "summary": "ok", "head": "abc"}"#,
        );
        assert!(r.is_err());
    }
}

#[cfg(test)]
mod outcome_contract_tests {
    use super::*;
    #[test]
    fn old_collection_defaults_to_complete_and_structured_blockers_round_trip() {
        let old: CollectedBody = serde_json::from_str(r#"{"summary":"ok"}"#).unwrap();
        assert_eq!(old.outcome.unwrap_or_default(), BuilderOutcome::Complete);
        for json in [
            r#"{"outcome":{"status":"partial","reason":"out_of_role","detail":"needs API"}}"#,
            r#"{"outcome":{"status":"partial","reason":"completion_unmet","detail":"third pass failed"}}"#,
            r#"{"outcome":{"status":"blocked","prerequisite":"backend API merges"}}"#,
        ] {
            let body: CollectedBody = serde_json::from_str(json).unwrap();
            assert_ne!(body.outcome.clone().unwrap(), BuilderOutcome::Complete);
            assert_eq!(
                serde_json::from_value::<CollectedBody>(serde_json::to_value(&body).unwrap())
                    .unwrap(),
                body
            );
        }
    }
}
