//! Stating a goal. The orchestrator is woken to plan it. Issues it names are
//! read and trust-checked first; an untrusted one refuses the goal.

use crate::AppState;
use crate::engine::Fail;
use crate::spawn;
use crate::wake;
use crate::wake::Wake;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use dvb_core::api::{GoalAfter, GoalState, GoalView, IssueCounts, NewGoal, ProjectQuery, Redirect};
use dvb_core::{Actor, EventKind, GoalId, ProjectId, Refusal, RefusalCode};

#[utoipa::path(
    post,
    path = "/projects/{id}/goals",
    operation_id = "state_goal",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    request_body = dvb_core::api::NewGoal,
    responses(crate::api::ErrorResponses, (status = 201, description = "Goal stated", body = dvb_core::api::GoalView))
)]
pub async fn state_goal(
    State(e): State<AppState>,
    Path(project): Path<ProjectId>,
    Json(body): Json<NewGoal>,
) -> Result<(StatusCode, Json<GoalView>), Fail> {
    let issues =
        crate::issues::for_goal(&e, &project, &body.issues, crate::issues::Reader::Owner).await?;
    let text = crate::issues::goal_text(&body.text, &issues);
    let goal = GoalId::new(e.new_id());
    e.append_checked(Some(&project), Actor::Owner, |proj, _| {
        if !proj.projects.contains_key(&project) {
            return Err(
                Refusal::new(RefusalCode::ProjectUnknown, format!("no project {project}")).into(),
            );
        }
        if let Some(after) = &body.after
            && proj
                .goals
                .get(after)
                .is_none_or(|g| g.project != project || g.proposal)
        {
            return Err(Refusal::new(
                RefusalCode::InvalidValue,
                "after must name an existing goal in this project",
            )
            .into());
        }
        Ok(vec![EventKind::GoalStated {
            goal: goal.clone(),
            text: text.clone(),
            from_issue: body.from_issue,
            issues: issues.clone(),
            after: body.after.clone(),
        }])
    })?;
    let view = {
        let proj = e.proj();
        GoalView {
            text,
            state: GoalState::Stated,
            plan_parked: false,
            ..proj.goal_view(&proj.goals[&goal])
        }
    };
    spawn(
        "wake the orchestrator",
        wake::wake(e, project, Wake::Goal, 0, 0),
    );
    Ok((StatusCode::CREATED, Json(view)))
}

/// The project's open issues for attaching to a goal; untrusted ones are listed
/// with title and link only and are refused as goal issues.
#[utoipa::path(
    get,
    path = "/projects/{id}/issues",
    operation_id = "list_project_issues",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Open issues, oldest first", body = Vec<dvb_core::api::IssueListing>))
)]
pub async fn list_project_issues(
    State(e): State<AppState>,
    Path(project): Path<ProjectId>,
) -> Result<Json<Vec<dvb_core::api::IssueListing>>, Fail> {
    let repo = crate::issues::repo_of(&e, &project)?;
    crate::issues::list(&e, &repo).await.map(Json)
}

/// How many open issues there are, how many are trusted and how they are
/// labelled, read from the forge now: the app offers "Work from the open
/// issues" when `trusted` is above zero.
#[utoipa::path(
    get,
    path = "/projects/{id}/issues/counts",
    operation_id = "count_project_issues",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Open issue counts", body = dvb_core::api::IssueCounts))
)]
pub async fn count_project_issues(
    State(e): State<AppState>,
    Path(project): Path<ProjectId>,
) -> Result<Json<IssueCounts>, Fail> {
    let repo = crate::issues::repo_of(&e, &project)?;
    let listed = crate::issues::list(&e, &repo).await?;
    Ok(Json(crate::issues::counts(&listed)))
}

/// Ask the orchestrator to propose the next goal from the open trusted issues,
/// for a project with no goal stated or in progress. It asks the owner one
/// question, as after a goal is done; approving it states the goal from the
/// proposed issues.
#[utoipa::path(
    post,
    path = "/projects/{id}/proposal",
    operation_id = "propose_goal",
    tag = "projects",
    params(("id" = dvb_core::ProjectId, Path)),
    responses(crate::api::ErrorResponses, (status = 202, description = "The orchestrator is woken to propose; the counts it starts from", body = dvb_core::api::IssueCounts))
)]
pub async fn propose_goal(
    State(e): State<AppState>,
    Path(project): Path<ProjectId>,
) -> Result<(StatusCode, Json<IssueCounts>), Fail> {
    let repo = crate::issues::repo_of(&e, &project)?;
    let counts = crate::issues::counts(&crate::issues::list(&e, &repo).await?);
    if counts.trusted == 0 {
        return Err(Refusal::new(
            RefusalCode::InvalidValue,
            "the project has no open trusted issues: state a goal instead",
        )
        .into());
    }
    wake::request_proposal(&e, &project, Actor::Owner, |proj| {
        if let Some(g) = proj.goal_under_way(&project) {
            return Err(Refusal::new(
                RefusalCode::ValueChanged,
                format!(
                    "goal {} is stated or in progress; its own run carries on",
                    g.id
                ),
            )
            .into());
        }
        if crate::wake::proposing(proj, &project) {
            return Err(Refusal::new(
                RefusalCode::ValueChanged,
                "the next goal is already being proposed: answer its question",
            )
            .into());
        }
        Ok(true)
    })?;
    Ok((StatusCode::ACCEPTED, Json(counts)))
}

#[utoipa::path(get, path = "/goals", operation_id = "list_goals", tag = "goals",
    params(dvb_core::api::ProjectQuery), responses(crate::api::ErrorResponses, (status = 200, description = "Goal snapshot", body = Vec<dvb_core::api::GoalView>)))]
pub async fn list_goals(
    State(e): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Json<Vec<GoalView>> {
    let proj = e.proj();
    Json(
        proj.goals
            .values()
            .filter(|g| !g.proposal && q.project.as_ref().is_none_or(|p| p == &g.project))
            .map(|g| proj.goal_view(g))
            .collect(),
    )
}

/// Drop a goal: cancel its planned tasks and close its questions.
#[utoipa::path(
    post,
    path = "/goals/{id}/drop",
    operation_id = "drop_goal",
    tag = "goals",
    params(("id" = dvb_core::GoalId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Goal dropped", body = dvb_core::api::GoalView))
)]
pub async fn drop_goal(
    State(e): State<AppState>,
    Path(id): Path<GoalId>,
) -> Result<Json<GoalView>, Fail> {
    crate::owner::drop_goal(&e, &id).map(Json)
}

/// Ask a parked plan's question again ("Ask me again"): the same plan and
/// goal version, with no orchestrator run and no new plan.
#[utoipa::path(
    post,
    path = "/goals/{id}/resume",
    operation_id = "resume_goal",
    tag = "goals",
    params(("id" = dvb_core::GoalId, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "Plan question asked again", body = dvb_core::api::QuestionView))
)]
pub async fn resume_goal(
    State(e): State<AppState>,
    Path(id): Path<GoalId>,
) -> Result<Json<dvb_core::api::QuestionView>, Fail> {
    crate::owner::resume_plan(&e, &id).map(Json)
}

/// "Change course".
#[utoipa::path(
    post,
    path = "/goals/{id}/redirect",
    operation_id = "redirect_goal",
    tag = "goals",
    params(("id" = dvb_core::GoalId, Path)),
    request_body = dvb_core::api::Redirect,
    responses(crate::api::ErrorResponses, (status = 200, description = "Course changed", body = dvb_core::api::GoalView))
)]
pub async fn redirect_goal(
    State(e): State<AppState>,
    Path(id): Path<GoalId>,
    Json(body): Json<Redirect>,
) -> Result<Json<GoalView>, Fail> {
    crate::owner::redirect(&e, &id, body.text).map(Json)
}

/// Change the goal a stated goal with no started task waits for before
/// planning; `after: null` clears it.
#[utoipa::path(
    post,
    path = "/goals/{id}/after",
    operation_id = "set_goal_after",
    tag = "goals",
    params(("id" = dvb_core::GoalId, Path)),
    request_body = dvb_core::api::GoalAfter,
    responses(crate::api::ErrorResponses, (status = 200, description = "Goal order changed", body = dvb_core::api::GoalView))
)]
pub async fn set_goal_after(
    State(e): State<AppState>,
    Path(id): Path<GoalId>,
    Json(body): Json<GoalAfter>,
) -> Result<Json<GoalView>, Fail> {
    crate::owner::set_after(&e, &id, body.after.as_ref()).map(Json)
}
