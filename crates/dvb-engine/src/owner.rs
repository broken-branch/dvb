//! Owner project and goal commands.
use crate::dispatch::{kick_all, start_waiting};
use crate::proj::Goal;
use crate::wake::wake_if_missed;
use crate::{
    AppState, Fail, spawn,
    wake::{self, Wake},
};
use dvb_core::api::GoalState;
use dvb_core::{Actor, Command, EventKind, ProjectId, Refusal, RefusalCode, TaskId, TaskState};
/// Pauses or unpauses a project: no new runs and no new effects while paused;
/// runs and effects in flight carry on. Unpausing kicks its tasks,
/// starts tasks that waited for a run slot and gives the orchestrator the wake
/// it missed.
pub fn pause(e: &AppState, project: &ProjectId, paused: bool) -> Result<(), Fail> {
    e.remember();
    let now = e.proj().projects.get(project).map(|p| p.view.paused);
    let Some(now) = now else {
        return Err(e.refuse(
            None,
            Actor::Owner,
            if paused { "pause" } else { "resume" },
            Refusal::new(RefusalCode::ProjectUnknown, format!("no project {project}")),
        ));
    };
    if now == paused {
        return Ok(());
    }
    let kind = if paused {
        EventKind::ProjectPaused {}
    } else {
        EventKind::ProjectResumed {}
    };
    e.append_checked(Some(project), Actor::Owner, |_, _| Ok(vec![kind]))?;
    if !paused {
        kick_all(e, Some(project));
        spawn("start waiting tasks", start_waiting(e.clone()));
        wake_if_missed(e, project);
    }
    Ok(())
}

/// Cancels the goal snapshot's Planned tasks; redirect also includes stopped-Planned.
pub(crate) fn cancel_unstarted(
    e: &AppState,
    g: &Goal,
    command: &str,
    include_stopped: bool,
) -> Result<Vec<TaskId>, Fail> {
    let unstarted: Vec<TaskId> = {
        let proj = e.proj();
        g.tasks
            .iter()
            .filter(|t| {
                proj.tasks.get(*t).is_some_and(|t| {
                    t.view.state == TaskState::Planned
                        || (include_stopped
                            && t.view.state
                                == TaskState::Stopped {
                                    was: Box::new(TaskState::Planned),
                                })
                })
            })
            .cloned()
            .collect()
    };
    for t in &unstarted {
        // A stop that raced the snapshot stays stopped when dropping a goal.
        let result = e.decide(
            e.proj(),
            t,
            Actor::Owner,
            crate::Decision::new(command, Command::Cancel),
            |proj| {
                if !include_stopped && matches!(proj.tasks[t].view.state, TaskState::Stopped { .. })
                {
                    Err(Refusal::new(
                        RefusalCode::AlreadyStopped,
                        "the task stays stopped",
                    ))
                } else {
                    Ok(())
                }
            },
        );
        match result {
            Err(Fail::Refused(r)) if !include_stopped && r.code == RefusalCode::AlreadyStopped => {}
            other => {
                other?;
            }
        }
    }
    Ok(unstarted)
}

/// Change course: the old plan's unstarted tasks are cancelled, the goal is
/// stated again with the new text (so a question about the old one lapses), and
/// the orchestrator plans it and asks for a new yes. Started tasks carry on
/// under the yes they had.
pub fn redirect(
    e: &AppState,
    goal: &dvb_core::GoalId,
    text: String,
) -> Result<dvb_core::api::GoalView, Fail> {
    let g = e
        .proj()
        .goals
        .get(goal)
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("goal {goal}")))?;
    if matches!(g.state, GoalState::Done | GoalState::Dropped) {
        return Err(e.refuse(
            Some(&g.project),
            Actor::Owner,
            "redirect",
            Refusal::new(RefusalCode::AlreadyTerminal, "the goal is done or dropped"),
        ));
    }
    let unstarted = cancel_unstarted(e, &g, "redirect", true)?;
    // The terminal check is made again under the lock the append takes, so a
    // drop that landed meanwhile is not undone by this redirect.
    let appended = e.append_checked(Some(&g.project), Actor::Owner, |proj, _now| {
        if proj
            .goals
            .get(goal)
            .is_some_and(|g| matches!(g.state, GoalState::Done | GoalState::Dropped))
        {
            return Err(Fail::Refused(Refusal::new(
                RefusalCode::AlreadyTerminal,
                "the goal is done or dropped",
            )));
        }
        Ok(vec![
            EventKind::PlanCancelled {
                goal: goal.clone(),
                tasks: unstarted,
            },
            EventKind::GoalStated {
                goal: goal.clone(),
                text: text.clone(),
                from_issue: None,
                issues: Vec::new(),
                after: g.after.clone(),
            },
        ])
    });
    match appended {
        Err(Fail::Refused(r)) => {
            return Err(e.refuse(Some(&g.project), Actor::Owner, "redirect", r));
        }
        other => other?,
    };
    spawn(
        "wake the orchestrator",
        wake::wake(e.clone(), g.project, Wake::Goal, 0, 0),
    );
    let proj = e.proj();
    Ok(dvb_core::api::GoalView {
        text,
        state: GoalState::Stated,
        plan_parked: false,
        ..proj.goal_view(&proj.goals[goal])
    })
}

/// Drops a goal: its unstarted tasks are cancelled and its questions closed
/// with it. The orchestrator is never woken for it again; merged work stays.
pub fn drop_goal(e: &AppState, goal: &dvb_core::GoalId) -> Result<dvb_core::api::GoalView, Fail> {
    let g = e
        .proj()
        .goals
        .get(goal)
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("goal {goal}")))?;
    let view = dvb_core::api::GoalView {
        waits_for: None,
        id: goal.clone(),
        project: g.project.clone(),
        text: g.text.clone(),
        state: GoalState::Dropped,
        plan_parked: false,
    };
    match g.state {
        GoalState::Dropped => return Ok(view),
        GoalState::Done => {
            return Err(e.refuse(
                Some(&g.project),
                Actor::Owner,
                "drop",
                Refusal::new(RefusalCode::AlreadyTerminal, "the goal is done"),
            ));
        }
        GoalState::Stated | GoalState::Approved | GoalState::InProgress | GoalState::Failed => {}
    }
    cancel_unstarted(e, &g, "drop", false)?;
    // Checked under the append's lock: a concurrent redirect or drop is seen.
    let appended = e.append_checked(Some(&g.project), Actor::Owner, |proj, _now| {
        match proj.goals.get(goal).map(|g| g.state) {
            Some(GoalState::Dropped) => Ok(Vec::new()),
            Some(GoalState::Done) => Err(Fail::Refused(Refusal::new(
                RefusalCode::AlreadyTerminal,
                "the goal is done",
            ))),
            Some(
                GoalState::Stated | GoalState::Approved | GoalState::InProgress | GoalState::Failed,
            )
            | None => Ok(vec![EventKind::GoalDropped { goal: goal.clone() }]),
        }
    });
    match appended {
        Err(Fail::Refused(r)) => return Err(e.refuse(Some(&g.project), Actor::Owner, "drop", r)),
        other => other?,
    };
    Ok(view)
}

/// Changes the goal a stated goal waits for before planning; `None` clears it.
/// Only a goal with no started task can be reordered. Recover planning and
/// saved plans' missing approval questions when the goal no longer waits.
pub fn set_after(
    e: &AppState,
    goal: &dvb_core::GoalId,
    after: Option<&dvb_core::GoalId>,
) -> Result<dvb_core::api::GoalView, Fail> {
    let project = e
        .proj()
        .goals
        .get(goal)
        .filter(|g| !g.proposal)
        .map(|g| g.project.clone())
        .ok_or_else(|| Fail::NotFound(format!("goal {goal}")))?;
    // Checked under the append's lock: a start, drop or other reorder is seen.
    e.append_checked(Some(&project), Actor::Owner, |proj, _now| {
        let g = &proj.goals[goal];
        match proj.goal_state(g) {
            GoalState::Done | GoalState::Dropped => {
                return Err(Refusal::new(
                    RefusalCode::AlreadyTerminal,
                    "the goal is done or dropped",
                )
                .into());
            }
            GoalState::Approved | GoalState::InProgress | GoalState::Failed => {
                return Err(
                    Refusal::new(RefusalCode::GoalStarted, "the goal's plan is approved").into(),
                );
            }
            GoalState::Stated => {}
        }
        if g.tasks.iter().filter_map(|t| proj.tasks.get(t)).any(|t| {
            !matches!(&t.view.state, TaskState::Planned | TaskState::Cancelled)
                && t.view.state
                    != (TaskState::Stopped {
                        was: Box::new(TaskState::Planned),
                    })
        }) {
            return Err(Refusal::new(RefusalCode::GoalStarted, "the goal has started work").into());
        }
        if let Some(after) = after {
            if after == goal {
                return Err(Refusal::new(
                    RefusalCode::GoalOrderCycle,
                    "a goal cannot wait for itself",
                )
                .into());
            }
            let Some(target) = proj
                .goals
                .get(after)
                .filter(|a| a.project == project && !a.proposal)
            else {
                return Err(Refusal::new(
                    RefusalCode::InvalidValue,
                    "after must name an existing goal in this project",
                )
                .into());
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut next = target.after.as_ref();
            while let Some(id) = next.filter(|id| seen.insert(*id)) {
                if id == goal {
                    return Err(Refusal::new(
                        RefusalCode::GoalOrderCycle,
                        format!("goal {after} already waits for goal {goal}"),
                    )
                    .into());
                }
                next = proj.goals.get(id).and_then(|g| g.after.as_ref());
            }
        }
        if g.after.as_ref() == after {
            return Ok(Vec::new());
        }
        Ok(vec![EventKind::GoalAfterSet {
            goal: goal.clone(),
            after: after.cloned(),
        }])
    })
    .map_err(|err| e.refuse(Some(&project), Actor::Owner, "after", err))?;
    wake_if_missed(e, &project);
    let proj = e.proj();
    Ok(proj.goal_view(&proj.goals[goal]))
}

/// Asks a parked plan's question again: a copy of the question that parked it,
/// for the same plan and goal version. No orchestrator run starts and nothing
/// is planned; the owner answers it as before.
pub fn resume_plan(
    e: &AppState,
    goal: &dvb_core::GoalId,
) -> Result<dvb_core::api::QuestionView, Fail> {
    let project = e
        .proj()
        .goals
        .get(goal)
        .map(|g| g.project.clone())
        .ok_or_else(|| Fail::NotFound(format!("goal {goal}")))?;
    let question = dvb_core::QuestionId::new(e.new_id());
    // The engine asks, as it does about a saved plan; a refusal is the owner's.
    let appended = e.append_checked(Some(&project), Actor::Engine, |proj, _now| {
        let parked = proj
            .goals
            .get(goal)
            .and_then(|g| proj.plan_parked(g))
            .ok_or_else(|| {
                Refusal::new(
                    RefusalCode::PlanNotParked,
                    "the goal has no plan on hold to resume",
                )
            })?;
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(parked.kind.clone()),
            main_ci: None,
            task: None,
            question: question.clone(),
            subject: parked.view.subject.clone(),
            format: parked.view.format,
            options: parked.view.options.clone(),
            recommended: parked.view.recommended,
            lapses_on: parked.lapses_on.clone(),
        }])
    });
    match appended {
        Err(Fail::Refused(r)) => return Err(e.refuse(Some(&project), Actor::Owner, "resume", r)),
        other => other?,
    };
    let view = e.proj().questions.get(&question).map(|q| q.view.clone());
    view.ok_or_else(|| Fail::internal("question vanished"))
}
