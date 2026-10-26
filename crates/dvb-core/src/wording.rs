//! Owner-facing wording. The app consumes the generated copy of this catalogue.
use crate::api::GoalState;
use crate::{EventKind, TaskState, Timestamp};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const SOURCE: &str = include_str!("wording.json");
#[expect(
    clippy::expect_used,
    reason = "Malformed bundled wording is a build defect"
)]
static WORDS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(SOURCE).expect("wording catalogue"));

pub fn text(key: &str) -> &'static str {
    let value = &WORDS[key];
    value
        .as_str()
        .or_else(|| value["cli"].as_str())
        .unwrap_or_default()
}

/// Substitution reads the template once, so payload text cannot become a placeholder.
pub fn render(key: &str, values: &Value) -> String {
    let mut out = String::new();
    let mut rest = text(key);
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        let value = &values[&rest[start + 1..start + end]];
        if let Some(s) = value.as_str() {
            out.push_str(s);
        } else if !value.is_null() {
            out.push_str(&value.to_string());
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

pub fn end_words(reason: &crate::event::EndReason) -> &'static str {
    use crate::event::EndReason;
    text(match reason {
        EndReason::Completed => "end.completed",
        EndReason::Stopped => "end.stopped",
        EndReason::Crashed => "end.crashed",
        EndReason::Oom => "end.oom",
        EndReason::Interrupted => "end.interrupted",
        EndReason::Mismatch => "end.mismatch",
        EndReason::Preflight(_) => "end.preflight",
        EndReason::ParseError => "end.parse_error",
        EndReason::TimeLimit => "end.time_limit",
    })
}

/// How a run ended, with a crashed run's exit code and cause when it has one.
pub fn ended_words(reason: &crate::event::EndReason, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{} ({detail})", end_words(reason)),
        None => end_words(reason).to_owned(),
    }
}

pub fn effect_words(effect: crate::state::Effect) -> &'static str {
    use crate::state::Effect;
    text(match effect {
        Effect::StartUnit => "effect.start_unit",
        Effect::StopUnit => "effect.stop_unit",
        Effect::Push => "effect.push",
        Effect::OpenPr => "effect.open_pr",
        Effect::ReadChecks => "effect.read_checks",
        Effect::Merge => "effect.merge",
        Effect::CloseIssue => "effect.close_issue",
        Effect::DeleteBranch => "effect.delete_branch",
    })
}

pub fn review_reason(stage: crate::roles::ReviewKind) -> &'static str {
    text(match stage {
        crate::roles::ReviewKind::Code => "reason.review.code",
        crate::roles::ReviewKind::Design => "reason.review.design",
    })
}

pub fn resource_words(resource: crate::event::ResourceWait) -> &'static str {
    use crate::event::ResourceWait;
    text(match resource {
        ResourceWait::Memory => "resource.memory",
        ResourceWait::Disk => "resource.disk",
        ResourceWait::HostLoad => "resource.host_load",
        ResourceWait::Available => "resource.available",
    })
}

pub fn resource_notice(resource: crate::event::ResourceWait, detail: &str) -> String {
    if resource == crate::event::ResourceWait::Available {
        resource_words(resource).into()
    } else {
        render(
            "resource.notice",
            &json!({"resource": resource_words(resource), "detail": detail}),
        )
    }
}

pub fn goal_word(state: GoalState) -> &'static str {
    text(match state {
        GoalState::Stated => "goal_state.stated",
        GoalState::Approved => "goal_state.approved",
        GoalState::InProgress => "goal_state.in_progress",
        GoalState::Done => "goal_state.done",
        GoalState::Failed => "goal_state.failed",
        GoalState::Dropped => "goal_state.dropped",
    })
}

pub fn state_word(state: &TaskState) -> String {
    let key = match state {
        TaskState::Planned => "state.planned",
        TaskState::Preparing => "state.preparing",
        TaskState::Running => "state.running",
        TaskState::Collecting => "state.collecting",
        TaskState::Publishing => "state.publishing",
        TaskState::InReview => "state.in_review",
        TaskState::AwaitingChecks => "state.awaiting_checks",
        TaskState::Merging => "state.merging",
        TaskState::RetryPending { .. } => "state.retry_pending",
        TaskState::Stopped { .. } => "state.stopped",
        TaskState::Unknown { .. } => "state.unknown",
        TaskState::Done => "state.done",
        TaskState::Cancelled => "state.cancelled",
        TaskState::Failed { .. } => "state.failed",
    };
    let values = if let TaskState::Failed { reason } = state {
        json!({"reason":reason})
    } else {
        Value::Null
    };
    render(key, &values)
}

/// A stopped task's state in words: planned when it had not started, else running.
pub fn stopped_word(was: &TaskState) -> &'static str {
    match was {
        TaskState::Planned => text("stopped.planned"),
        TaskState::Preparing
        | TaskState::Running
        | TaskState::Collecting
        | TaskState::Publishing
        | TaskState::InReview
        | TaskState::AwaitingChecks
        | TaskState::Merging
        | TaskState::RetryPending { .. }
        | TaskState::Stopped { .. }
        | TaskState::Unknown { .. }
        | TaskState::Done
        | TaskState::Cancelled
        | TaskState::Failed { .. } => text("stopped.running"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "EventKind contains only serializable contract fields"
)]
pub fn event_text(kind: &EventKind, time: impl Fn(Timestamp) -> String) -> Option<String> {
    use EventKind::*;
    let mut values = serde_json::to_value(kind).expect("event wording payload")["payload"].clone();
    let key = match kind {
        EngineReconciled { state, .. } => {
            values["state"] = state_word(state).into();
            kind.kind()
        }
        RoleChanged { restored_from, .. } => {
            if restored_from.is_some() {
                "role.changed.restored"
            } else {
                kind.kind()
            }
        }
        HireChanged { restored_from, .. } => {
            if restored_from.is_some() {
                "hire.changed.restored"
            } else {
                kind.kind()
            }
        }
        SettingsChanged { restored_from, .. } => {
            if restored_from.is_some() {
                "settings.changed.restored"
            } else {
                kind.kind()
            }
        }
        GithubMirrorObserved {
            mirrored, detail, ..
        } => {
            if *mirrored {
                kind.kind()
            } else if detail.is_some() {
                "github.mirror_observed.detail"
            } else {
                "github.mirror_observed.failed"
            }
        }
        GoalAfterSet { after, .. } => {
            if after.is_some() {
                kind.kind()
            } else {
                "goal.after_set.none"
            }
        }
        PlanProposed { tasks, .. } => {
            values["count"] = tasks.len().into();
            kind.kind()
        }
        QuestionAnswered { answer, .. } => {
            values["answer"] = match answer {
                crate::event::Answer::Choice(n) => render("answer.choice", &json!({"choice":n})),
                crate::event::Answer::Reply(text) => text.clone(),
            }
            .into();
            kind.kind()
        }
        CommandRefused(r) => {
            values["refusal"] = r.refusal.as_str().into();
            kind.kind()
        }
        ProjectIssuesFound(counts) => {
            values["labelled"] = counts
                .labels
                .iter()
                .map(|l| {
                    render(
                        "project.issues_found.label",
                        &json!({"open": l.open, "label": l.label}),
                    )
                })
                .collect::<String>()
                .into();
            kind.kind()
        }
        RunStarted(a) => {
            values["task"] = a
                .task
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| text("run.started.work").into())
                .into();
            if let Some(stage) = a.review_stage {
                values["stage"] = text(match stage {
                    crate::roles::ReviewKind::Code => "stage.code",
                    crate::roles::ReviewKind::Design => "stage.design",
                })
                .into();
                "run.started.review"
            } else {
                kind.kind()
            }
        }
        RunPreflight { checks, .. } => {
            if let Some(failed) = checks.iter().find(|c| !c.ok) {
                values["failed"] = failed.detail.clone().into();
                "run.preflight.failed"
            } else {
                kind.kind()
            }
        }
        RunModel { served, .. } => {
            if let crate::event::ServedModel::Mismatch(model) = served {
                values["served"] = model.clone().into();
                "run.model.mismatch"
            } else {
                kind.kind()
            }
        }
        RunOutput { item, resource, .. } => match item {
            crate::event::OutputItem::Note(t) => {
                values["output"] = resource
                    .map_or_else(|| t.clone(), |resource| resource_notice(resource, t))
                    .into();
                kind.kind()
            }
            crate::event::OutputItem::Text(t) => {
                values["output"] = t.clone().into();
                kind.kind()
            }
            crate::event::OutputItem::ToolCall { name } => {
                values["name"] = name.clone().into();
                "run.output.tool"
            }
        },
        RunEnded { reason, detail, .. } => {
            use crate::event::EndReason;
            values["reason"] = ended_words(reason, detail.as_deref()).into();
            if *reason == EndReason::Completed {
                "run.ended.completed"
            } else {
                kind.kind()
            }
        }
        RunRerouted { limited, .. } => {
            if limited.is_some() {
                kind.kind()
            } else {
                "run.rerouted.only"
            }
        }
        OnlyStarted(only) => {
            values["models"] = only.models.join(", ").into();
            values["ends_at"] = time(only.ends_at).into();
            kind.kind()
        }
        VendorLimited { resets_at, .. } => {
            if let Some(reset) = resets_at {
                values["resets_at"] = time(*reset).into();
                "vendor.limited.reset"
            } else {
                kind.kind()
            }
        }
        BranchUpdated { same_change, .. } => {
            if *same_change {
                "branch.updated.same"
            } else {
                kind.kind()
            }
        }
        VerdictRecorded {
            findings,
            review_stage,
            ..
        } => {
            values["open"] = crate::event::open_findings(findings).into();
            values["stage"] = text(match review_stage {
                Some(crate::roles::ReviewKind::Code) => "stage.code",
                Some(crate::roles::ReviewKind::Design) => "stage.design",
                None => "stage.none",
            })
            .into();
            kind.kind()
        }
        EffectRetry { effect, .. } => {
            values["effect"] = effect_words(*effect).into();
            kind.kind()
        }
        TaskRetryTextSet { .. }
        | TaskReasonChanged { .. }
        | TaskStateChanged { .. }
        | RunSurvivedRestart { .. }
        | RunStopRequested { .. }
        | RunLaunched { .. }
        | RunUsage { .. }
        | ProjectSeen {}
        | GithubMirrorSent { .. }
        | ChecksPending { .. } => return None,
        EngineStarted { .. }
        | RunTimeLimitReached { .. }
        | EngineUpdated { .. }
        | EngineUpdateFailed { .. }
        | ProjectAdded { .. }
        | ProjectPaused {}
        | ProjectResumed {}
        | ProjectBriefSet { .. }
        | ProjectRulesetMissing {}
        | ProjectRulesetInstalled {}
        | ProjectRepoChanged { .. }
        | ProjectRepoUnreachable { .. }
        | ProjectRemoved { .. }
        | RoleAdded(_)
        | RoleRemoved(_)
        | HireAdded(_)
        | HireRetired(_)
        | GoalStated { .. }
        | GoalApproved { .. }
        | GoalDone { .. }
        | GoalDropped { .. }
        | GoalIssueClosed { .. }
        | ProposalRequested {}
        | RepositoryCreated { .. }
        | RepositorySetupPrepared { .. }
        | PlanCancelled { .. }
        | QuestionOpened { .. }
        | QuestionClosed { .. }
        | DecisionRecorded { .. }
        | TaskStarted { .. }
        | TaskWaiting { .. }
        | TaskStopped { .. }
        | TaskResumed { .. }
        | TaskCancelled { .. }
        | TaskDone { .. }
        | TaskFailed { .. }
        | RunCollected { .. }
        | OnlyEnded { .. }
        | VendorResumed { .. }
        | BranchPublished { .. }
        | ScanHit { .. }
        | PrOpened { .. }
        | ChecksObserved { .. }
        | ChecksStuck { .. }
        | MergeRefused { .. }
        | PrMerged { .. } => kind.kind(),
    };
    Some(render(key, &values))
}
