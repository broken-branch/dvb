//! Version and status (`/version` and `/status`).

use crate::AppState;
use axum::Json;
use axum::extract::{Query, State};
use dvb_core::api::{GoalState, NeedsYouTask, ProjectQuery, Status, VersionInfo};
use dvb_core::{GoalId, TaskState};
use std::collections::BTreeSet;

#[utoipa::path(
    get,
    path = "/version",
    operation_id = "get_version",
    tag = "version",
    responses(crate::api::ErrorResponses, (status = 200, description = "The engine's version", body = dvb_core::api::VersionInfo))
)]
pub async fn get_version() -> Json<VersionInfo> {
    Json(VersionInfo {
        version: dvb_core::VERSION.into(),
    })
}

/// What is running and what needs the owner, for one project or all.
#[utoipa::path(
    get,
    path = "/status",
    operation_id = "get_status",
    tag = "status",
    params(dvb_core::api::ProjectQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "What is running and what needs the owner", body = dvb_core::api::Status))
)]
pub async fn get_status(State(e): State<AppState>, Query(q): Query<ProjectQuery>) -> Json<Status> {
    let proj = e.proj();
    let mine = |p: &dvb_core::ProjectId| q.project.as_ref().is_none_or(|want| want == p);
    let running = proj.live_runs().filter(|r| mine(&r.project)).count();
    // The app's Needs you rule: a goal's failed or stopped tasks count while
    // the goal is current, or while it ended failed and is still the
    // project's last goal; a retry, a drop or a later goal clears them.
    let counted: BTreeSet<&GoalId> = proj
        .goals
        .values()
        .filter(|g| !g.proposal && mine(&g.project))
        .filter(|g| match proj.goal_state(g) {
            GoalState::Stated | GoalState::Approved | GoalState::InProgress => true,
            GoalState::Failed => proj
                .goals_in_order(&g.project)
                .into_iter()
                .rfind(|l| !l.proposal)
                .is_some_and(|l| l.id == g.id),
            GoalState::Done | GoalState::Dropped => false,
        })
        .map(|g| &g.id)
        .collect();
    let open: Vec<_> = proj
        .questions
        .values()
        .map(|q| &q.view)
        .filter(|q| q.open && mine(&q.project))
        .collect();
    let tasks: Vec<NeedsYouTask> = proj
        .tasks
        .values()
        .map(|t| &t.view)
        .filter(|t| {
            matches!(
                t.state,
                TaskState::Failed { .. } | TaskState::Stopped { .. }
            ) && !t.problem_superseded
                && counted.contains(&t.goal)
        })
        .map(|t| NeedsYouTask {
            id: t.id.clone(),
            project: t.project.clone(),
            title: t.title.clone(),
            state: t.state.clone(),
            reason: t.reason.clone().or_else(|| {
                if let TaskState::Failed { reason } = &t.state {
                    Some(reason.clone())
                } else {
                    None
                }
            }),
            // A question whose recommended choice leaves the task stopped
            // asks about the same wait, so the task counts once.
            question: open
                .iter()
                .find(|q| {
                    matches!(t.state, TaskState::Stopped { .. })
                        && q.task.as_ref() == Some(&t.id)
                        && q.project == t.project
                        && q.leaves_task_stopped()
                })
                .map(|q| q.id.clone()),
        })
        .collect();
    let rulesets_missing = proj
        .projects
        .values()
        .filter(|p| p.ruleset_missing && mine(&p.view.id))
        .map(|p| p.view.name.clone())
        .collect();
    let folded = tasks.iter().filter(|t| t.question.is_some()).count();
    let needs_you = open.len() - folded + tasks.len();
    drop(proj);
    Json(Status {
        running: u32::try_from(running).unwrap_or(u32::MAX),
        needs_you: u32::try_from(needs_you).unwrap_or(u32::MAX),
        warnings: crate::launch::install_warnings(&e.cfg),
        rulesets_missing,
        tasks,
        mirror_pass: e
            .mirror_pass
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::events;
    use crate::{Config, Engine};
    use dvb_core::event::{Answer, EndReason, LapsesOn, PlannedTask, QuestionFormat};
    use dvb_core::roles::ReviewKind;
    use dvb_core::{
        Actor, EventId, EventKind, HireId, ProjectId, QuestionId, RoleId, RunId, TaskId,
    };

    #[tokio::test]
    async fn status_counts_active_runs_and_open_questions_scoped_to_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        for name in ["a", "b"] {
            e.append_checked(Some(&ProjectId::new(name)), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::RunStarted(
                        crate::test_support::assignment::Assignment::new(RunId::new(name))
                            .role(RoleId::new(name))
                            .hire(HireId::new(name))
                            .reason("Plan".into())
                            .build(),
                    ),
                    EventKind::QuestionOpened {
                        kind: Some(dvb_core::event::QuestionKind::Ordinary),
                        main_ci: None,
                        task: None,
                        question: QuestionId::new(name),
                        subject: "Proceed?".into(),
                        format: QuestionFormat::Simple,
                        options: vec![],
                        recommended: 1,
                        lapses_on: LapsesOn::ValueVersion(EventId(0)),
                    },
                ])
            })
            .unwrap();
        }
        assert_eq!(
            get_status(State(e.clone()), Query(ProjectQuery::default()))
                .await
                .0,
            Status {
                running: 2,
                needs_you: 2,
                warnings: vec![],
                rulesets_missing: vec![],
                mirror_pass: None,
                tasks: vec![],
            }
        );
        e.append_checked(Some(&ProjectId::new("a")), Actor::Owner, |_, _| {
            Ok(vec![
                EventKind::RunEnded {
                    run: RunId::new("a"),
                    reason: EndReason::Completed,
                    detail: None,
                    stderr: None,
                },
                EventKind::QuestionAnswered {
                    question: QuestionId::new("a"),
                    answer: Answer::Choice(1),
                },
            ])
        })
        .unwrap();
        assert_eq!(
            get_status(
                State(e.clone()),
                Query(ProjectQuery {
                    project: Some(ProjectId::new("a"))
                })
            )
            .await
            .0,
            Status {
                running: 0,
                needs_you: 0,
                warnings: vec![],
                rulesets_missing: vec![],
                mirror_pass: None,
                tasks: vec![],
            }
        );
        assert_eq!(
            get_status(State(e), Query(ProjectQuery::default())).await.0,
            Status {
                running: 1,
                needs_you: 1,
                warnings: vec![],
                rulesets_missing: vec![],
                mirror_pass: None,
                tasks: vec![],
            }
        );
    }

    #[tokio::test]
    async fn status_warns_when_the_run_units_are_older_than_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::dev(dir.path(), "dvb".into());
        cfg.units_version = Some(dir.path().join("units-version"));
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(
            get_status(State(e), Query(ProjectQuery::default()))
                .await
                .0
                .warnings,
            [dvb_core::api::InstallWarning::CacheIsolationMissing]
        );
    }

    async fn needs_you(e: &AppState, project: Option<&str>) -> u32 {
        let q = ProjectQuery {
            project: project.map(ProjectId::new),
        };
        get_status(State(e.clone()), Query(q)).await.0.needs_you
    }

    fn plan(goal: &GoalId, tasks: &[&str]) -> EventKind {
        EventKind::PlanProposed {
            goal: goal.clone(),
            tasks: tasks
                .iter()
                .map(|t| PlannedTask {
                    after: vec![],
                    task: TaskId::new(*t),
                    title: (*t).into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                })
                .collect(),
        }
    }

    fn state(task: &str, from: TaskState, to: TaskState) -> EventKind {
        EventKind::TaskStateChanged {
            task: TaskId::new(task),
            from,
            to,
        }
    }

    fn failed() -> TaskState {
        TaskState::Failed {
            reason: "merging failed".into(),
        }
    }

    #[tokio::test]
    async fn a_goal_that_ended_failed_needs_you_until_retried_dropped_or_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        let append = |project: &str, kinds: Vec<EventKind>| {
            e.append_checked(Some(&ProjectId::new(project)), Actor::Owner, |_, _| {
                Ok(kinds)
            })
            .unwrap();
        };
        for (project, tasks) in [
            ("a", &["a1", "a2"][..]),
            ("b", &["b1"]),
            ("c", &["c1", "c2"]),
        ] {
            let goal = GoalId::new(project);
            append(
                project,
                vec![
                    events::project_event(&ProjectId::new(project), project, "/nowhere"),
                    events::goal_event(&goal, project, None),
                    plan(&goal, tasks),
                    EventKind::GoalApproved { goal },
                ],
            );
        }
        // a: one task merged, one failed to merge. b: its task stopped.
        // c: a failed task, then a later merge in the goal supersedes it.
        append(
            "a",
            vec![
                state("a1", TaskState::Planned, TaskState::Done),
                state("a2", TaskState::Planned, failed()),
            ],
        );
        append(
            "b",
            vec![state(
                "b1",
                TaskState::Planned,
                TaskState::Stopped {
                    was: Box::new(TaskState::Running),
                },
            )],
        );
        append(
            "c",
            vec![
                state("c1", TaskState::Planned, failed()),
                EventKind::PrMerged {
                    task: TaskId::new("c2"),
                    pr: 2,
                    sha: "s".into(),
                },
                state("c2", TaskState::Planned, TaskState::Done),
            ],
        );
        let goal_state = |g: &str| {
            let proj = e.proj();
            proj.goal_state(&proj.goals[&GoalId::new(g)])
        };
        assert_eq!(goal_state("a"), GoalState::Failed);
        assert_eq!(goal_state("c"), GoalState::Failed);
        assert_eq!(needs_you(&e, Some("a")).await, 1);
        assert_eq!(needs_you(&e, Some("b")).await, 1);
        assert_eq!(needs_you(&e, Some("c")).await, 0, "a superseded problem");
        assert_eq!(needs_you(&e, None).await, 2);

        append("a", vec![state("a2", failed(), TaskState::Running)]);
        assert_eq!(needs_you(&e, Some("a")).await, 0, "retried");
        append("a", vec![state("a2", TaskState::Running, failed())]);
        assert_eq!(needs_you(&e, Some("a")).await, 1, "failed again");
        append(
            "a",
            vec![events::goal_event(&GoalId::new("a-next"), "next", None)],
        );
        assert_eq!(
            needs_you(&e, Some("a")).await,
            0,
            "a later goal replaces it"
        );

        append(
            "b",
            vec![EventKind::GoalDropped {
                goal: GoalId::new("b"),
            }],
        );
        assert_eq!(needs_you(&e, Some("b")).await, 0, "dropped");
        assert_eq!(needs_you(&e, None).await, 0);
    }

    #[tokio::test]
    async fn status_lists_each_task_it_counts_and_a_stopped_tasks_question_once() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        let p = ProjectId::new("p");
        let goal = GoalId::new("g");
        let stopped = || TaskState::Stopped {
            was: Box::new(TaskState::Running),
        };
        let leave_stopped = |id: &str, task: &str| EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: Some(TaskId::new(task)),
            question: QuestionId::new(id),
            subject: "Retry it?".into(),
            format: QuestionFormat::Simple,
            options: ["Retry it", "Leave it stopped"]
                .into_iter()
                .zip(1..)
                .map(|(text, id)| dvb_core::event::QuestionOption {
                    id,
                    text: text.into(),
                    effect: "x".into(),
                    approves_plan: false,
                    no_effect: false,
                    retries_task: None,
                })
                .collect(),
            recommended: 2,
            lapses_on: LapsesOn::ValueVersion(EventId(0)),
        };
        let append = |kinds: Vec<EventKind>| {
            e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(kinds))
                .unwrap();
        };
        append(vec![
            events::project_event(&p, "p", "/nowhere"),
            events::goal_event(&goal, "p", None),
            plan(&goal, &["held", "broken", "asked"]),
            EventKind::GoalApproved { goal },
            state("held", TaskState::Planned, stopped()),
            EventKind::TaskStopped {
                update: false,
                task: TaskId::new("held"),
                reason: Some("not before Monday".into()),
            },
        ]);
        let status = || async {
            get_status(State(e.clone()), Query(ProjectQuery::default()))
                .await
                .0
        };
        let line = |id: &str, state: TaskState, question: Option<&str>| NeedsYouTask {
            id: TaskId::new(id),
            project: p.clone(),
            title: id.into(),
            // The stop's reason, or the failure's, as the task view says.
            reason: if let TaskState::Failed { reason } = &state {
                Some(reason.clone())
            } else {
                (id == "held").then(|| "not before Monday".into())
            },
            state,
            question: question.map(QuestionId::new),
        };
        let s = status().await;
        assert_eq!(s.needs_you, 1, "a stopped task with no question");
        assert_eq!(s.tasks, [line("held", stopped(), None)]);

        append(vec![state("broken", TaskState::Planned, failed())]);
        let s = status().await;
        assert_eq!(s.needs_you, 2);
        assert_eq!(
            s.tasks,
            [
                line("broken", failed(), None),
                line("held", stopped(), None)
            ]
        );

        append(vec![
            state("asked", TaskState::Planned, stopped()),
            leave_stopped("asked-q", "asked"),
        ]);
        let s = status().await;
        assert_eq!(
            s.needs_you, 3,
            "the stopped task and its question count once"
        );
        assert_eq!(s.tasks[0], line("asked", stopped(), Some("asked-q")));

        // The same question about a failed task is not folded: both wait.
        append(vec![leave_stopped("broken-q", "broken")]);
        assert_eq!(status().await.needs_you, 4);
    }
}
