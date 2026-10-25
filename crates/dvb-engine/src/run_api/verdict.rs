//! The accountable reviewer's verdict, for its own head only. The
//! engine attaches the head, role, model and vendor from its record of the run.

use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::event::open_findings;
use dvb_core::run_verbs::VerdictBody;
use dvb_core::{Actor, Command, EventKind, Refusal, RefusalCode, VerdictId};

pub async fn verdict(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<VerdictBody>,
) -> Result<Json<serde_json::Value>, Fail> {
    let a = &c.run.assignment;
    let actor = Actor::Run(a.run.clone());
    let project = c.run.project.clone();
    let (Some(task), Some(head)) = (a.task.clone(), a.reviews.clone()) else {
        let r = Refusal::new(
            RefusalCode::RoleNotAllowed,
            "only a reviewer run gives a verdict",
        );
        return Err(e.refuse(Some(&project), actor, "verdict", r));
    };
    // Recorded only if `decide` accepts it: in review, for the task's head.
    let verdict = VerdictId::new(e.new_id());
    let open = open_findings(&body.findings);
    let recorded = EventKind::VerdictRecorded {
        review_stage: a.review_stage,
        verdict: verdict.clone(),
        task: task.clone(),
        head: head.clone(),
        run: a.run.clone(),
        role: a.role.clone(),
        model: a.model.clone(),
        vendor: a.vendor,
        findings: body.findings,
        summary: body.summary,
    };
    // At the round cap the verdict stops the task (also during a lost link),
    // and the owner's question is recorded with it.
    let question = {
        let t = e.proj().tasks.get(&task).cloned();
        let t = t.ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        crate::dispatch::round_cap_question_event(&e, &t, None)?
    };
    // Only from the task's current reviewer run that has not ended, checked in
    // the same transaction that records it: a review replaced by a stop and a
    // resume cannot land on its successor.
    let state = e.run_command(
        &a.run,
        crate::Decision {
            pre: vec![recorded],
            question: Some(question),
            ..crate::Decision::new("verdict", Command::VerdictRecorded { head, open })
        },
    )?;
    if state.is_none() {
        let r = Refusal::new(RefusalCode::ValueChanged, "this review was replaced");
        return Err(e.refuse(Some(&project), actor, "verdict", r));
    }
    Ok(Json(serde_json::json!({ "verdict": verdict })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use dvb_core::event::{CiMode, PlannedTask};
    use dvb_core::roles::{HireRecord, HireStatus, ReviewKind, default_records};
    use dvb_core::run_verbs::{Finding, Severity};
    use dvb_core::{Effort, GoalId, HireId, ProjectId, RoleId, RunId, TaskId, TaskState};

    /// At the review-round cap an open finding stops the task, and the owner's
    /// question is recorded in the verdict's own transaction.
    #[tokio::test]
    async fn a_verdict_at_the_round_cap_records_the_question_with_the_stop() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "dvb".into()),
            crate::EngineOptions {
                forge: None,
                launcher: None,
                ..Default::default()
            },
        )
        .unwrap();
        let (project, goal, task) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
        let mut kinds = vec![EventKind::ProjectAdded {
            id: project.clone(),
            name: "project".into(),
            repo: dir.path().join("repo").to_string_lossy().into_owned(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        }];
        for role in default_records(|r| RoleId::new(r.slug)) {
            kinds.push(EventKind::HireAdded(HireRecord {
                id: HireId::new(format!("h-{}", role.slug)),
                role: role.id.clone(),
                model: "fake-1".into(),
                effort: Effort::Medium,
                status: HireStatus::Active,
                routine: false,
            }));
            kinds.push(EventKind::RoleAdded(role));
        }
        let run = RunId::new("reviewer");
        kinds.extend([
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Ship it".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: goal.clone(),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Build it".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "One edit".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved { goal },
            EventKind::BranchPublished {
                task: task.clone(),
                round: dvb_core::caps::REVIEW_ROUNDS_PER_PR,
                branch: "dvb/t-1".into(),
                head: "head".into(),
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::InReview,
            },
            EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(run.clone())
                    .task(Some(task.clone()))
                    .role(RoleId::new("code-reviewer"))
                    .hire(HireId::new("h-code-reviewer"))
                    .reason("Review".into())
                    .reviews(Some("head".into()))
                    .base(Some("head".into()))
                    .build(),
            ),
        ]);
        e.append_checked(Some(&project), Actor::Engine, |_, _| Ok(kinds))
            .unwrap();
        let caller = Caller {
            run: e.proj().runs[&run].clone(),
        };
        let body = VerdictBody {
            findings: vec![Finding {
                severity: Severity::High,
                file: "file".into(),
                line: None,
                title: "Broken".into(),
                detail: "Broken".into(),
                open: true,
            }],
            summary: "Still broken".into(),
        };
        let _ = verdict(State(e.clone()), caller, Json(body)).await.unwrap();

        let t = e.proj().tasks[&task].view.state.clone();
        let was = Box::new(TaskState::InReview);
        assert_eq!(t, TaskState::Stopped { was });
        let events = crate::test_support::events::all(&e);
        let at = |f: fn(&EventKind) -> bool| events.iter().find(|ev| f(&ev.kind)).unwrap();
        let verdict = at(|k| matches!(k, EventKind::VerdictRecorded { .. }));
        let question = at(|k| matches!(k, EventKind::QuestionOpened { .. }));
        // The last event of the verdict's own batch, under its actor.
        assert!(question.id > verdict.id);
        assert_eq!(question.id, events.last().unwrap().id);
        assert_eq!(question.actor, verdict.actor);
        assert_eq!(e.proj().questions.len(), 1);
    }
}
