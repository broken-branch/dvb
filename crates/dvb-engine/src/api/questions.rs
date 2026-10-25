//! An owner answer wakes the orchestrator, except a no-effect choice and a
//! choice that parks a plan ("Not yet"). Plan approval also records
//! `goal.approved` and starts its tasks.

use crate::AppState;
use crate::engine::Fail;
use crate::proj::{DROP_GOAL, PLAN_AGAIN};
use crate::spawn;
use crate::wake;
use crate::wake::Wake;
use axum::Json;
use axum::extract::{Path, Query, State};
use dvb_core::api::{QuestionView, QuestionsQuery};
use dvb_core::event::{Answer, LapsesOn};
use dvb_core::{Actor, EventKind, QuestionId, Refusal, RefusalCode};

#[utoipa::path(
    get,
    path = "/questions",
    operation_id = "list_questions",
    tag = "questions",
    params(dvb_core::api::QuestionsQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Questions", body = Vec<dvb_core::api::QuestionView>))
)]
pub async fn list_questions(
    State(e): State<AppState>,
    Query(q): Query<QuestionsQuery>,
) -> Json<Vec<QuestionView>> {
    let only_open = q.open.unwrap_or(false);
    Json(
        e.proj()
            .questions
            .values()
            .filter(|q| !only_open || q.view.open)
            .map(|q| q.view.clone())
            .collect(),
    )
}

#[utoipa::path(
    post,
    path = "/questions/{id}/answer",
    operation_id = "answer_question",
    tag = "questions",
    params(("id" = dvb_core::QuestionId, Path)),
    request_body = dvb_core::event::Answer,
    responses(crate::api::ErrorResponses, (status = 200, description = "Answered", body = dvb_core::api::QuestionView))
)]
pub async fn answer_question(
    State(e): State<AppState>,
    Path(id): Path<QuestionId>,
    Json(answer): Json<Answer>,
) -> Result<Json<QuestionView>, Fail> {
    answer_as(&e, &id, answer, Actor::Owner).map(Json)
}

/// The owner's answer, or dvb's own (`Actor::Engine`) in an autonomous
/// project: the same checks, events and wakes.
pub(crate) fn answer_as(
    e: &AppState,
    id: &QuestionId,
    answer: Answer,
    actor: Actor,
) -> Result<QuestionView, Fail> {
    let id = id.clone();
    let project = e
        .proj()
        .questions
        .get(&id)
        .map(|q| q.view.project.clone())
        .ok_or_else(|| Fail::NotFound(format!("question {id}")))?;
    let mut approved = None;
    let result = e.append_checked(Some(&project), actor.clone(), |proj, _now| {
        let q = proj
            .questions
            .get(&id)
            .ok_or_else(|| Fail::NotFound(format!("question {id}")))?;
        // The goal the question is about (it lapses when the goal is
        // stated again); validated in the same transaction.
        let goal = match &q.lapses_on {
            LapsesOn::GoalVersion(v) => proj
                .goals
                .values()
                .find(|g| g.project == project && g.version == *v),
            LapsesOn::ValueVersion(_) => proj.latest_goal(&project),
        };
        let lapsed = match &q.lapses_on {
            LapsesOn::GoalVersion(_) => goal.is_none().then_some("goal"),
            LapsesOn::ValueVersion(_) => None,
        };
        if let Some(what) = lapsed {
            return Err(Refusal::new(
                RefusalCode::QuestionLapsed,
                format!("the {what} changed since this was asked"),
            )
            .into());
        }
        if !q.view.open {
            return Err(Refusal::new(
                RefusalCode::AlreadyTerminal,
                "this question is already answered",
            )
            .into());
        }
        if let dvb_core::event::QuestionKind::Completion { task, run, result } = &q.kind {
            let current = proj
                .tasks
                .get(task)
                .and_then(|t| crate::dispatch::completion_result(proj, t));
            if current != Some((run.clone(), *result)) {
                return Err(Refusal::new(
                    RefusalCode::QuestionLapsed,
                    "a newer builder result replaced this completion decision",
                )
                .into());
            }
            if !matches!(answer, Answer::Choice(1..=3)) {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "choose Merge anyway, Retry the check, or Leave it stopped",
                )
                .into());
            }
            return Ok(vec![EventKind::QuestionAnswered {
                question: id.clone(),
                answer,
            }]);
        }
        if q.main_ci.is_some() {
            if answer == Answer::Choice(1) && q.answered_at.is_some() {
                return Ok(vec![]);
            }
            if !matches!(answer, Answer::Choice(1 | 2)) {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "choose Wait for main to be fixed or Retry now",
                )
                .into());
            }
            return Ok(vec![EventKind::QuestionAnswered {
                question: id.clone(),
                answer,
            }]);
        }
        if let Some(run) = &q.planning_run {
            let g = goal
                .filter(|g| g.planning_held && g.planning_run.as_ref() == Some(run))
                .ok_or_else(|| Refusal::new(RefusalCode::QuestionLapsed, "planning hold ended"))?;
            if !matches!(answer, Answer::Choice(PLAN_AGAIN | DROP_GOAL)) {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "choose Plan it again or Drop the goal",
                )
                .into());
            }
            let mut kinds = vec![EventKind::QuestionAnswered {
                question: id.clone(),
                answer: answer.clone(),
            }];
            if answer == Answer::Choice(DROP_GOAL) {
                kinds.push(EventKind::GoalDropped { goal: g.id.clone() });
            }
            return Ok(kinds);
        }
        let approves = match &answer {
            Answer::Choice(n) => {
                let option = q.view.options.iter().find(|o| o.id == *n).ok_or_else(|| {
                    Refusal::new(RefusalCode::InvalidValue, format!("option {n}"))
                })?;
                option.approves_plan
            }
            Answer::Reply(_) => false,
        };
        // A reply needs its own goal's run, so accepting one while that goal
        // waits would leave an undeliverable answer ahead of prerequisite work.
        if let Some(goal) =
            goal.filter(|_| approves || (q.plan_question && matches!(answer, Answer::Reply(_))))
        {
            proj.check_goal_order(goal)?;
        }
        let mut kinds = vec![EventKind::QuestionAnswered {
            question: id.clone(),
            answer,
        }];
        if let Some(goal) = goal.filter(|_| approves) {
            approved = Some(goal.id.clone());
            kinds.push(EventKind::GoalApproved {
                goal: goal.id.clone(),
            });
        }
        Ok(kinds)
    });
    // Recording a refusal takes the projections lock too; do it after the check.
    result.map_err(|error| e.refuse(Some(&project), actor, "answer", error))?;
    // The engine starts the approved tasks itself: the orchestrator may be
    // in a run for another goal, which does not start this goal's tasks.
    if let Some(goal) = approved {
        spawn(
            "start the approved plan",
            crate::dispatch::start_approved(e.clone(), goal),
        );
    }
    // No reply for the orchestrator: a planning hold's answer acts by itself,
    // and a no-effect or parking ("Not yet") choice asks nothing of it.
    let quiet = e
        .proj()
        .questions
        .get(&id)
        .is_some_and(|q| q.planning_run.is_some() || q.no_effect() || q.parks_plan());
    let main_ci = e
        .proj()
        .questions
        .get(&id)
        .is_some_and(|q| q.main_ci.is_some());
    let completion = e.proj().questions.get(&id).and_then(|q| match &q.kind {
        dvb_core::event::QuestionKind::Completion { task, .. } => Some(task.clone()),
        dvb_core::event::QuestionKind::Ordinary
        | dvb_core::event::QuestionKind::OrchestratorFailed
        | dvb_core::event::QuestionKind::PlanningHold { .. } => None,
    });
    if let Some(task) = completion {
        crate::dispatch::kick(e, &task);
    } else if main_ci {
        crate::dispatch::kick_all(e, Some(&project));
    } else if quiet {
        // Anything else outstanding gets its wake.
        crate::wake::wake_if_missed(e, &project);
    } else {
        spawn(
            "wake the orchestrator",
            wake::wake(e.clone(), project, Wake::Answer, 0, 0),
        );
    }
    let view = e.proj().questions.get(&id).map(|q| q.view.clone());
    view.ok_or_else(|| Fail::internal("question vanished"))
}
