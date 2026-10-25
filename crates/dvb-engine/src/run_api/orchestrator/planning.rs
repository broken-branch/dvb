use super::{not_allowed, orchestrator};
use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::event::{Answer, LapsesOn, PlannedTask};
use dvb_core::run_verbs::{PlanBody, PlanReply};
use dvb_core::{Actor, EventKind, Refusal, RefusalCode};
use dvb_core::{GoalId, TaskId};

/// The goal this orchestrator run plans and asks for: the goal it was woken
/// for (or its own next-goal proposal), still at the version assigned to the
/// run. After an owner's redirect of that goal the run's calls are stale; a
/// goal stated meanwhile waits its turn.
pub(super) fn run_goal<'p>(
    proj: &'p crate::proj::Projections,
    c: &Caller,
) -> Result<&'p crate::proj::Goal, Fail> {
    let stale = || {
        Fail::Refused(Refusal::new(
            RefusalCode::ValueChanged,
            "the goal changed since this run started",
        ))
    };
    let started_for = proj
        .runs
        .get(&c.run.assignment.run)
        .and_then(|r| r.goal_version);
    proj.goals
        .values()
        .find(|g| g.project == c.run.project && Some(g.version) == started_for)
        .filter(|g| g.state != dvb_core::api::GoalState::Dropped && proj.waits_for(g).is_none())
        .ok_or_else(stale)
}

/// One replacement of the unapproved plan, only by the run receiving the
/// worded answer to its approval question. Use the prompt's fixed delivery
/// window, including failed-run retries; a newly saved plan consumes the reply.
fn revises_plan(
    proj: &crate::proj::Projections,
    c: &Caller,
    events: &[dvb_core::Event],
    g: &crate::proj::Goal,
) -> bool {
    let Some(run) = proj.runs.get(&c.run.assignment.run) else {
        return false;
    };
    let Some(planned) = g.planned.filter(|p| *p > g.version) else {
        return false;
    };
    let (cutoff, end) = crate::prompt::orchestrator_window(events, run);
    g.state == dvb_core::api::GoalState::Stated
        && events.iter().any(|ev| {
            ev.project.as_ref() == Some(&g.project)
                && ev.id > cutoff
                && ev.id < end
                && ev.id > planned
                && matches!(&ev.kind,
                    EventKind::QuestionAnswered { question, answer: Answer::Reply(_) }
                    if proj.questions.get(question).is_some_and(|q|
                        q.opened_at > planned && q.opened_at < ev.id
                            && q.lapses_on == LapsesOn::GoalVersion(g.version)
                            && q.view.options.iter().any(|o| o.approves_plan)))
        })
}

fn planning_goal<'p>(
    proj: &'p crate::proj::Projections,
    c: &Caller,
    body: &PlanBody,
    events: &[dvb_core::Event],
) -> Result<(&'p crate::proj::Goal, bool), Fail> {
    let g = run_goal(proj, c)?;
    let revising = revises_plan(proj, c, events, g);
    let extending = proj.can_extend_plan(g);
    if let Some(waiting) = proj
        .goals_in_order(&c.run.project)
        .into_iter()
        .find(|g| crate::proj::Projections::needs_plan(g) && proj.waits_for(g).is_none())
        && !crate::proj::Projections::needs_plan(g)
        && !revising
        && !extending
    {
        return Err(Refusal::new(
            RefusalCode::ValueChanged,
            format!("goal {} is waiting for its own plan", waiting.id),
        )
        .into());
    }
    if g.state != dvb_core::api::GoalState::Done
        && !crate::proj::Projections::needs_plan(g)
        && !revising
        && !extending
    {
        return Err(
            Refusal::new(RefusalCode::ValueChanged, "no goal is waiting for a plan").into(),
        );
    }
    if g.state != dvb_core::api::GoalState::Done && !body.issues.is_empty() {
        return Err(Refusal::new(
            RefusalCode::InvalidValue,
            "issues go with a next goal; this run plans a stated goal, which keeps its own",
        )
        .into());
    }
    if g.state != dvb_core::api::GoalState::Done
        && body.goal.as_ref().is_some_and(|text| text != &g.text)
    {
        return Err(Refusal::new(
            RefusalCode::ValueChanged,
            format!("this run plans goal {}; omit goal when planning it", g.id),
        )
        .into());
    }
    Ok((g, revising))
}

pub async fn plan(
    State(e): State<AppState>,
    c: Caller,
    Json(body): Json<PlanBody>,
) -> Result<Json<PlanReply>, Fail> {
    orchestrator(&e, &c, false)?;
    let project = c.run.project.clone();
    let events = e
        .store
        .select(dvb_core::EventId(0), dvb_store::Select::WithoutTelemetry)?;
    let task_ids: Vec<_> = body.tasks.iter().map(|_| TaskId::new(e.new_id())).collect();
    let replacement = {
        let proj = e.proj();
        let (g, revising) = planning_goal(&proj, &c, &body, &events)?;
        // Refuse invalid tasks before cancelling any part of the old plan.
        for t in &body.tasks {
            if proj.projects[&project].role_by_slug(&t.role).is_none() {
                return Err(not_allowed(format!("no role {}", t.role)));
            }
        }
        resolve_dependencies(&body, &task_ids, g, &proj)?;
        revising.then(|| g.clone())
    };
    // Read and trust-checked before the goal exists, as for `dvb goal --issue`.
    let issues =
        crate::issues::for_goal(&e, &project, &body.issues, crate::issues::Reader::Run).await?;
    let cancelled = replacement
        .as_ref()
        .map(|g| crate::owner::cancel_unstarted(&e, g, "plan", true))
        .transpose()?;
    let mut ids = Vec::new();
    // Checked and appended in one transaction (a redirect cannot come between).
    e.append_checked(
        Some(&project),
        Actor::Run(c.run.assignment.run.clone()),
        |proj, _now| {
            let p = proj
                .projects
                .get(&project)
                .ok_or_else(|| Fail::NotFound(format!("project {project}")))?;
            let (g, _) = planning_goal(proj, &c, &body, &events)?;
            let next_goal = g.state == dvb_core::api::GoalState::Done;
            // A completed goal can lead to a new one, without reopening its tasks.
            // Otherwise only the delivered worded approval reply permits revision.
            let mut kinds = Vec::new();
            if let Some(tasks) = cancelled {
                kinds.push(EventKind::PlanCancelled {
                    goal: g.id.clone(),
                    tasks,
                });
            }
            let goal = if next_goal {
                let text = Some(crate::issues::goal_text(
                    body.goal.as_deref().unwrap_or_default(),
                    &issues,
                ))
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| {
                    Fail::Refused(Refusal::new(
                        RefusalCode::ValueChanged,
                        "the completed goal needs a next goal: supply goal in dvb run plan",
                    ))
                })?;
                let goal = GoalId::new(e.new_id());
                kinds.push(EventKind::GoalStated {
                    goal: goal.clone(),
                    text,
                    from_issue: None,
                    issues: issues.clone(),
                    after: None,
                });
                goal
            } else {
                g.id.clone()
            };
            let dependencies = resolve_dependencies(&body, &task_ids, g, proj)?;
            let mut tasks = Vec::new();
            for ((t, id), after) in body.tasks.into_iter().zip(task_ids).zip(dependencies) {
                let role = p
                    .role_by_slug(&t.role)
                    .ok_or_else(|| not_allowed(format!("no role {}", t.role)))?;
                tasks.push(PlannedTask {
                    after,
                    task: id,
                    title: t.title,
                    role: role.id.clone(),
                    review: t.review,
                    summary: t.summary,
                    routine: t.routine,
                });
            }
            ids = tasks.iter().map(|t| t.task.clone()).collect();
            kinds.push(EventKind::PlanProposed { goal, tasks });
            Ok(kinds)
        },
    )?;
    Ok(Json(PlanReply { tasks: ids }))
}

fn resolve_dependencies(
    body: &PlanBody,
    ids: &[TaskId],
    goal: &crate::proj::Goal,
    proj: &crate::proj::Projections,
) -> Result<Vec<Vec<TaskId>>, Fail> {
    use dvb_core::run_verbs::PlanDependency;
    let invalid = |detail| Fail::Refused(Refusal::new(RefusalCode::InvalidValue, detail));
    if body.tasks.is_empty() {
        return Err(invalid("a plan needs at least one task"));
    }
    let mut graph = vec![vec![]; body.tasks.len()];
    let mut result = vec![vec![]; body.tasks.len()];
    for (i, task) in body.tasks.iter().enumerate() {
        for dep in &task.after {
            let id = match dep {
                PlanDependency::Position(n) => {
                    let j = usize::try_from(*n)
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .filter(|j| *j < ids.len())
                        .ok_or_else(|| invalid("dependency position is outside this plan"))?;
                    if i == j {
                        return Err(invalid("a task cannot depend on itself"));
                    }
                    graph[i].push(j);
                    ids[j].clone()
                }
                PlanDependency::Task(id) => {
                    if goal.state == dvb_core::api::GoalState::Done {
                        return Err(invalid(
                            "a new goal's dependencies must use proposal positions",
                        ));
                    }
                    if !proj.tasks.get(id).is_some_and(|t| {
                        t.view.goal == goal.id
                            && !matches!(t.view.state, dvb_core::TaskState::Cancelled)
                    }) {
                        return Err(invalid(
                            "dependency must name an existing task of this goal",
                        ));
                    }
                    id.clone()
                }
            };
            if !result[i].contains(&id) {
                result[i].push(id);
            }
        }
    }
    // Kahn's algorithm counts only new references: persisted tasks cannot
    // refer to ids that have not been allocated yet.
    let mut done = vec![false; graph.len()];
    loop {
        let next = (0..graph.len()).find(|i| !done[*i] && graph[*i].iter().all(|j| done[*j]));
        let Some(i) = next else {
            break;
        };
        done[i] = true;
    }
    if done.iter().any(|v| !v) {
        return Err(invalid("task dependencies contain a cycle"));
    }
    Ok(result)
}
