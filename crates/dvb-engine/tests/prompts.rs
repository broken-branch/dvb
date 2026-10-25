//! The prompts a run is given: the response style, the
//! verb help, the events since the last wake, and the reviewer's and builder's
//! context. Every placeholder in a template is filled by someone.

use dvb_core::event::{Answer, LapsesOn, PlannedTask, QuestionFormat, QuestionOption};
use dvb_core::launcher::UnitState;
use dvb_core::roles::{HireRecord, HireStatus, ReviewKind, default_records};
use dvb_core::run_verbs::{Finding, Severity};
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, ProjectId, QuestionId, RoleId, RunAssignment, RunId,
    TaskId, Vendor, VerdictId,
};
use dvb_engine::launch;
use dvb_engine::prompt::render;
use dvb_engine::{AppState, prompt};

const RUN_VERBS: &str = include_str!("../../../prompts/run-verbs.md");
const ORCHESTRATOR_VERBS: &str = include_str!("../../../prompts/orchestrator-verbs.md");

fn engine() -> (tempfile::TempDir, AppState, ProjectId) {
    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(
        dir.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            disk_reading: Some(std::sync::Arc::new(|| Some(u64::MAX))),
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![support::events::project_event(
            &p,
            "fieldnotes",
            "/nowhere",
        )])
    })
    .unwrap();
    (dir, e, p)
}

fn assignment(run: &str) -> RunAssignment {
    support::assignment::Assignment::new(RunId::new(run))
        .role(RoleId::new("orchestrator"))
        .reason(String::new())
        .build()
}

async fn orchestrator_prompt(e: &AppState, p: &ProjectId, run: &str) -> String {
    let (r, proj) = {
        let g = e.proj();
        (g.runs[&RunId::new(run)].clone(), g.projects[p].clone())
    };
    let mut vars = prompt::context(e, &r, &proj).await.unwrap();
    vars.push(("project_name", proj.view.name.clone()));
    render(
        include_str!("../../../prompts/orchestrator.md"),
        &vars,
        false,
    )
}

fn set_style(e: &AppState, p: &ProjectId, style: &str) {
    e.append_checked(Some(p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "response_style".into(),
            old: serde_json::Value::Null,
            new: style.into(),
            restored_from: None,
        }])
    })
    .unwrap();
}

#[tokio::test]
async fn each_style_renders_a_distinct_fully_filled_orchestrator_prompt() {
    let (_d, e, p) = engine();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment("r1"))])
    })
    .unwrap();
    let styles = ["Simple", "Clear", "Technical", "Expert"];
    let mut got = Vec::new();
    for style in styles {
        set_style(&e, &p, style);
        got.push(orchestrator_prompt(&e, &p, "r1").await);
    }
    for (i, style) in styles.iter().enumerate() {
        assert!(
            got[i].contains(&prompt::style_instruction(style)),
            "{style}"
        );
        assert!(
            !got[i].contains("{{"),
            "unfilled placeholder in:\n{}",
            got[i]
        );
        for (j, other) in styles.iter().enumerate().filter(|(j, _)| *j != i) {
            assert!(
                !got[i].contains(&prompt::style_instruction(other)),
                "{style} prompt carries {other}"
            );
            assert_ne!(got[i], got[j]);
        }
    }
}

#[tokio::test]
async fn orchestrator_delivers_shared_verb_instructions_once() {
    let (_d, e, p) = engine();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment("r1"))])
    })
    .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    for block in [RUN_VERBS, ORCHESTRATOR_VERBS, prompt::MODEL_MODE_CUSTOM] {
        assert_eq!(text.matches(block).count(), 1, "{block}: {text}");
    }
    assert!(!text.contains("{{"), "{text}");
}

#[tokio::test]
async fn without_a_setting_the_style_is_clear() {
    let (_d, e, p) = engine();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment("r1"))])
    })
    .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    assert!(text.contains(&prompt::style_instruction("clear")));
    assert!(
        text.contains("# The `dvb run` verbs"),
        "the verb help is included"
    );
}

#[tokio::test]
async fn the_orchestrators_project_lists_task_ids_role_slugs_and_active_hires() {
    let (_d, e, p) = engine();
    let roles = default_records(|r| RoleId::new(format!("role-{}", r.slug)));
    let clerk = roles.iter().find(|r| r.slug == "clerk").unwrap();
    let mut events: Vec<_> = roles.iter().cloned().map(EventKind::RoleAdded).collect();
    events.extend([
        EventKind::HireAdded(HireRecord {
            id: HireId::new("retired"),
            role: clerk.id.clone(),
            model: "fake-retired".into(),
            effort: Effort::Low,
            status: HireStatus::Retired,
            routine: false,
        }),
        EventKind::HireAdded(HireRecord {
            id: HireId::new("active"),
            role: clerk.id.clone(),
            model: "fake-1".into(),
            effort: Effort::High,
            status: HireStatus::Active,
            routine: false,
        }),
        EventKind::GoalStated {
            goal: GoalId::new("g1"),
            text: "Document it".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: GoalId::new("g1"),
            tasks: vec![
                PlannedTask {
                    after: vec![],
                    task: TaskId::new("doc-task"),
                    title: "Write\nthe docs".into(),
                    role: clerk.id.clone(),
                    review: ReviewKind::Code,
                    summary: "Write the documentation.".into(),
                    routine: false,
                },
                PlannedTask {
                    after: vec![],
                    task: TaskId::new("build-task"),
                    title: "Build it".into(),
                    role: RoleId::new("role-backend"),
                    review: ReviewKind::Code,
                    summary: "Build it.".into(),
                    routine: false,
                },
            ],
        },
        EventKind::GoalApproved {
            goal: GoalId::new("g1"),
        },
        EventKind::RunStarted(assignment("r1")),
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    let project = text
        .split("## The project\n")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    let tasks: Vec<_> = project
        .lines()
        .filter(|l| l.starts_with("- ") && l.contains(" · role "))
        .collect();
    assert_eq!(
        tasks,
        [
            "- doc-task · Write the docs · role clerk (hire: fake-1, high) · Planned",
            "- build-task · Build it · role backend (hire: none) · Planned",
        ]
    );
}

#[tokio::test]
async fn each_task_note_sits_on_its_own_line_under_its_own_tasks_bullet() {
    let (_d, e, p) = engine();
    let (first, second) = (TaskId::new("t1"), TaskId::new("t2"));
    let planned = |task: &TaskId, after: Vec<TaskId>| PlannedTask {
        after,
        task: task.clone(),
        title: format!("Build {task}"),
        role: RoleId::new("backend"),
        review: ReviewKind::Code,
        summary: "Build it.".into(),
        routine: false,
    };
    let (roles, hires) = support::events::default_role_hire_events();
    let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
    events.extend([
        EventKind::GoalStated {
            goal: GoalId::new("g1"),
            text: "Ship it".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: GoalId::new("g1"),
            tasks: vec![
                planned(&first, vec![]),
                planned(&second, vec![first.clone()]),
            ],
        },
        EventKind::GoalApproved {
            goal: GoalId::new("g1"),
        },
        EventKind::RunStarted(
            support::assignment::Assignment::new(RunId::new("b2"))
                .task(Some(second.clone()))
                .build(),
        ),
        EventKind::RunCollected {
            run: RunId::new("b2"),
            head: None,
            summary: None,
            outcome: Some(dvb_core::run_verbs::BuilderOutcome::Partial {
                reason: dvb_core::run_verbs::PartialReason::Other,
                detail: "Needs the parser".into(),
            }),
        },
        EventKind::RunStarted(assignment("r1")),
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    let lines: Vec<_> = text.lines().collect();
    let bullet = |task: &TaskId| {
        lines
            .iter()
            .position(|l| l.starts_with(&format!("- {task} · ")))
            .unwrap_or_else(|| panic!("no bullet for {task}:\n{text}"))
    };
    let (one, two) = (bullet(&first), bullet(&second));
    assert!(lines[one - 1].starts_with("Goal (Approved)"), "{text}");
    assert_eq!(two, one + 1, "task 1 carries no note:\n{text}");
    assert!(
        lines[two + 1].starts_with("  Task t2 starts after PRs for t1 merge."),
        "{text}"
    );
    assert_eq!(
        lines[two + 2],
        r#"  Task t2 builder outcome: {"detail":"Needs the parser","reason":"other","status":"partial"}. Reason: Needs the parser"#,
        "{text}"
    );
    assert!(!lines[two + 3].starts_with("  Task "), "{text}");
}

/// The orchestrator sees the role's routine hire and which tasks are routine,
/// each with the hire its next round runs on.
#[tokio::test]
async fn the_orchestrator_sees_routine_hires_and_routine_tasks() {
    let (_d, e, p) = engine();
    let planned = |task: &str, routine: bool| PlannedTask {
        after: vec![],
        task: TaskId::new(task),
        title: task.into(),
        role: RoleId::new("backend"),
        review: ReviewKind::Code,
        summary: "Build it.".into(),
        routine,
    };
    let (roles, hires) = support::events::default_role_hire_events();
    let mut events: Vec<_> = roles.into_iter().chain(hires).collect();
    events.extend([
        EventKind::HireAdded(dvb_core::roles::HireRecord {
            id: HireId::new("h-routine"),
            role: RoleId::new("backend"),
            model: "fake-next".into(),
            effort: Effort::Low,
            status: dvb_core::roles::HireStatus::Active,
            routine: true,
        }),
        EventKind::GoalStated {
            goal: GoalId::new("g1"),
            text: "Ship it".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: GoalId::new("g1"),
            tasks: vec![planned("feature", false), planned("tests", true)],
        },
        EventKind::RunStarted(assignment("r1")),
    ]);
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(events))
        .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    assert!(
        text.contains("  Active routine hire h-routine: fake-next, effort low\n"),
        "{text}"
    );
    assert!(
        text.contains("  Active hire h-backend: fake-1, effort medium\n"),
        "{text}"
    );
    assert!(
        text.contains("- feature · feature · role backend (hire: fake-1, medium) · Planned"),
        "{text}"
    );
    assert!(
        text.contains(
            "- tests · tests · role backend (hire: fake-next, low, routine task) · Planned"
        ),
        "{text}"
    );
}

/// A stopped task says whether it had started and why, so the orchestrator
/// need not guess.
#[tokio::test]
async fn a_stopped_task_shows_planned_or_running_and_its_reason() {
    let (_d, e, p) = engine();
    let planned = |task: &str| PlannedTask {
        after: vec![],
        task: TaskId::new(task),
        title: task.into(),
        role: RoleId::new("backend"),
        review: ReviewKind::Code,
        summary: "Build it.".into(),
        routine: false,
    };
    let stop = |task: &str, was: dvb_core::TaskState, reason: Option<&str>| {
        [
            EventKind::TaskStateChanged {
                task: TaskId::new(task),
                from: was.clone(),
                to: dvb_core::TaskState::Stopped { was: Box::new(was) },
            },
            EventKind::TaskStopped {
                task: TaskId::new(task),
                reason: reason.map(str::to_owned),
            },
        ]
    };
    let mut events = vec![
        EventKind::GoalStated {
            goal: GoalId::new("g1"),
            text: "Ship it".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: GoalId::new("g1"),
            tasks: vec![planned("held"), planned("cut")],
        },
    ];
    events.extend(stop(
        "held",
        dvb_core::TaskState::Planned,
        Some("Hold until #442 merges"),
    ));
    events.extend(stop("cut", dvb_core::TaskState::Running, None));
    events.push(EventKind::RunStarted(assignment("r1")));
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(events))
        .unwrap();
    let text = orchestrator_prompt(&e, &p, "r1").await;
    assert!(
        text.contains("\n  Task held: Stopped while planned. Reason: Hold until #442 merges\n"),
        "{text}"
    );
    assert!(
        text.contains("\n  Task cut: Stopped while running. Reason: no reason given"),
        "{text}"
    );
}

#[tokio::test]
async fn the_orchestrator_sees_what_happened_and_what_the_owner_said_since_it_last_ran() {
    let (_d, e, p) = engine();
    let q = QuestionId::new("q1");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(assignment("r1")),
            EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: q.clone(),
                subject: "Start this plan?".into(),
                format: QuestionFormat::Simple,
                options: vec![QuestionOption {
                    id: 1,
                    text: "Yes".into(),
                    effect: "starts the tasks".into(),
                    approves_plan: true,
                    no_effect: false,
                    retries_task: None,
                }],
                recommended: 1,
                lapses_on: LapsesOn::GoalVersion(dvb_core::EventId(1)),
            },
        ])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::QuestionAnswered {
            question: q,
            answer: Answer::Choice(1),
        }])
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment("r2"))])
    })
    .unwrap();
    let text = orchestrator_prompt(&e, &p, "r2").await;
    assert!(
        text.contains("- Start this plan? -> Yes (starts the tasks)"),
        "{text}"
    );
    assert!(text.contains("question.answered"), "{text}");
    assert!(!text.contains("run.started"), "run chatter is left out");
}

#[test]
fn reply_and_event_windows_stay_bound_to_the_run_even_after_later_runs_start() {
    let (_d, e, p) = engine();
    let answer = |text: &str| EventKind::QuestionAnswered {
        question: QuestionId::new(text),
        answer: Answer::Reply(text.into()),
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(assignment("r1")),
            answer("first-window"),
            EventKind::RunStarted(assignment("r2")),
            answer("second-window"),
            EventKind::RunStarted(assignment("r3")),
            answer("next-window"),
        ])
    })
    .unwrap();
    let events = support::events::all(&e);
    let proj = e.proj();
    for (run, expected) in [("r1", ""), ("r2", "first-window"), ("r3", "second-window")] {
        let current = &proj.runs[&RunId::new(run)];
        let replies = prompt::pending_replies(&events, &proj.projects[&p], current);
        let since = prompt::events_since(&events, &proj.projects[&p], current);
        for text in ["first-window", "second-window", "next-window"] {
            assert_eq!(replies.contains(text), text == expected, "{run}: {replies}");
            assert_eq!(since.contains(text), text == expected, "{run}: {since}");
        }
    }
    drop(proj);
}

#[tokio::test]
async fn historical_question_answers_keep_their_wording_and_retry_in_orchestrator_context() {
    let (_d, e, p) = engine();
    let question: EventKind = serde_json::from_str(
        r#"{"kind":"question.opened","payload":{
            "question":"retry-question","subject":"Retry the parser task?","format":"simple",
            "options":[{"id":1,"text":"Try again","effect":"restarts the parser task",
                "approves_plan":false,"retries_task":"parser-task"}],
            "recommended":1,"lapses_on":{"goal_version":1}
        }}"#,
    )
    .unwrap();
    assert!(matches!(
        &question,
        EventKind::QuestionOpened { kind: None, .. }
    ));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(assignment("r1")),
            question,
            EventKind::QuestionAnswered {
                question: QuestionId::new("retry-question"),
                answer: Answer::Choice(1),
            },
            EventKind::RunStarted(assignment("r2")),
        ])
    })
    .unwrap();
    let text = orchestrator_prompt(&e, &p, "r2").await;
    assert!(
        text.contains("- Retry the parser task? -> Try again (restarts the parser task) [retries_task: parser-task]"),
        "{text}"
    );
}

#[tokio::test]
async fn historical_plan_replies_reach_builder_and_reviewer_owner_context() {
    let (_d, e, p) = engine();
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::GoalStated {
            goal: GoalId::new("g1"),
            text: "Ship the parser".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        }])
    })
    .unwrap();
    let version = e.proj().goals[&GoalId::new("g1")].version;
    let question: EventKind = serde_json::from_value(serde_json::json!({
        "kind":"question.opened", "payload":{
            "question":"plan-question", "subject":"Start this plan?", "format":"simple",
            "options":[{"id":1,"text":"Start","effect":"starts the plan","approves_plan":true}],
            "recommended":1, "lapses_on":{"goal_version":version}
        }
    }))
    .unwrap();
    assert!(matches!(
        &question,
        EventKind::QuestionOpened { kind: None, .. }
    ));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            question,
            EventKind::QuestionAnswered {
                question: QuestionId::new("plan-question"),
                answer: Answer::Reply("Preserve the CSV input format.".into()),
            },
            EventKind::PlanProposed {
                goal: GoalId::new("g1"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: TaskId::new("parser-task"),
                    title: "Build the parser".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "Implement parsing".into(),
                    routine: false,
                }],
            },
        ])
    })
    .unwrap();
    for slug in ["backend", "code-reviewer", "design-reviewer"] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment(slug))
                    .task(Some(TaskId::new("parser-task")))
                    .role(RoleId::new(slug))
                    .reviews((slug != "backend").then(|| "h".repeat(40)))
                    .review_stage((slug == "design-reviewer").then_some(ReviewKind::Design))
                    .base(Some("b".repeat(40)))
                    .build(),
            )])
        })
        .unwrap();
        let vars = context_of(&e, &p, slug).await;
        let context = var(&vars, "owner_goal");
        assert!(context.contains("Ship the parser"), "{slug}: {context}");
        assert!(
            context.contains("Preserve the CSV input format."),
            "{slug}: {context}"
        );
        assert!(
            context.find("Ship the parser").unwrap()
                < context.find("Preserve the CSV input format.").unwrap()
        );
    }
}

#[test]
fn every_template_placeholder_has_a_filler() {
    // The launcher fills these itself (flow.rs); `prompt::FILLED` the rest.
    let launcher = [
        "project_name",
        "role_name",
        "role_job",
        "role_lane",
        "task",
        "task_title",
        "goal_text",
        "goal_id",
        "goal_version",
        "goal_state",
        "other_waiting_goals",
        "task_role",
    ];
    for (name, text) in [
        (
            "orchestrator",
            include_str!("../../../prompts/orchestrator.md"),
        ),
        ("builder", include_str!("../../../prompts/builder.md")),
        (
            "code-reviewer",
            include_str!("../../../prompts/code-reviewer.md"),
        ),
        (
            "design-reviewer",
            include_str!("../../../prompts/design-reviewer.md"),
        ),
        ("clerk", include_str!("../../../prompts/clerk.md")),
    ] {
        let mut rest = text;
        while let Some(i) = rest.find("{{") {
            let end = rest[i..].find("}}").unwrap();
            let var = &rest[i + 2..i + end];
            assert!(
                prompt::FILLED.contains(&var) || launcher.contains(&var),
                "{name}.md uses {{{{{var}}}}} and nothing fills it"
            );
            rest = &rest[i + end + 2..];
        }
    }
}

/// Every run that works in a checkout is told to bound its waits and leave
/// nothing running: a run that never exits is stopped at its time limit.
#[test]
fn working_roles_are_told_to_bound_their_waits() {
    let builder = include_str!("../../../prompts/builder.md");
    let start = builder.find("Never wait in an unbounded loop.").unwrap();
    let rule = builder[start..].split("\n\n").next().unwrap();
    for (name, text) in [
        ("builder", builder),
        (
            "code-reviewer",
            include_str!("../../../prompts/code-reviewer.md"),
        ),
        (
            "design-reviewer",
            include_str!("../../../prompts/design-reviewer.md"),
        ),
        ("clerk", include_str!("../../../prompts/clerk.md")),
    ] {
        assert_eq!(text.matches(rule).count(), 1, "{name}.md lacks: {rule}");
    }
}

#[tokio::test]
async fn rendered_builder_brief_carries_its_privacy_instruction_once() {
    let (_d, e, p) = engine();
    with_a_task(&e, &p);
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![EventKind::ProjectBriefSet {
            brief: "Synthetic brief for prompt tests.".into(),
        }])
    })
    .unwrap();
    let run = RunId::new("b1");
    launch::launch(e.clone(), run.clone(), "[]".into(), vec![], None)
        .await
        .unwrap();
    let text = builder_prompt(&e, &run).await;
    let privacy = "The brief is private to dvb, is never sent to GitHub, and must not be quoted or paraphrased in a PR title or body, commit, file or any other text sent to GitHub.";
    assert_eq!(text.matches(privacy).count(), 1, "{text}");
    assert!(text.contains(&format!(
        "## Project brief\n\n{privacy}\n\nSynthetic brief for prompt tests."
    )));
    assert!(text.contains("local path, host name or user name"));
}

#[tokio::test]
async fn rendered_builder_and_reviewer_prompts_require_github_readable_evidence() {
    let (_d, e, p) = engine();
    let task = with_a_task(&e, &p);
    let summary = "Changed the chart.\n\n![Before](/var/lib/dvb/work/example/ws/before.png)\n\nChecked: cargo test — passed.";
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunCollected {
                run: RunId::new("b1"),
                head: Some("c".repeat(40)),
                summary: Some(summary.into()),
                outcome: None,
            },
            EventKind::BranchPublished {
                task: task.clone(),
                round: 1,
                branch: "dvb/t1-1".into(),
                head: "h".repeat(40),
            },
        ])
    })
    .unwrap();
    for (slug, run) in [("code-reviewer", "v1"), ("design-reviewer", "v2")] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment(run))
                    .task(Some(task.clone()))
                    .role(RoleId::new(slug))
                    .reviews(Some("h".repeat(40)))
                    .review_stage((slug == "design-reviewer").then_some(ReviewKind::Design))
                    .base(Some("b".repeat(40)))
                    .build(),
            )])
        })
        .unwrap();
        launch::launch(e.clone(), RunId::new(run), "[]".into(), vec![], None)
            .await
            .unwrap();
        let text = builder_prompt(&e, &RunId::new(run)).await;
        assert!(
            text.contains("## PR description") && text.contains(summary),
            "{slug}: {text}"
        );
    }
}

#[tokio::test]
async fn reviewer_description_follows_publication_even_when_a_fix_keeps_the_same_head() {
    let (_d, e, p) = engine();
    let task = with_a_task(&e, &p);
    for (round, builder, reviewer, summary) in [
        (1, "b1", "v1", "Original picture description"),
        (2, "b2", "v2", "Corrected picture description"),
    ] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            let mut events = vec![];
            if builder != "b1" {
                events.push(EventKind::RunStarted(
                    support::assignment::Assignment::new(RunId::new(builder))
                        .task(Some(task.clone()))
                        .build(),
                ));
            }
            events.extend([
                EventKind::RunCollected {
                    run: RunId::new(builder),
                    head: Some("c".repeat(40)),
                    summary: Some(summary.into()),
                    outcome: None,
                },
                EventKind::BranchPublished {
                    task: task.clone(),
                    round,
                    branch: "dvb/t1-1".into(),
                    head: "h".repeat(40),
                },
                EventKind::RunStarted(
                    support::assignment::Assignment::new(RunId::new(reviewer))
                        .task(Some(task.clone()))
                        .reviews(Some("h".repeat(40)))
                        .role(RoleId::new("code-reviewer"))
                        .build(),
                ),
            ]);
            Ok(events)
        })
        .unwrap();
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::BranchUpdated {
                task: task.clone(),
                branch: "dvb/t1-1".into(),
                base: "b".repeat(40),
                from: "h".repeat(40),
                head: "u".repeat(40),
                same_change: false,
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("v3"))
                    .task(Some(task.clone()))
                    .reviews(Some("u".repeat(40)))
                    .role(RoleId::new("design-reviewer"))
                    .review_stage(Some(ReviewKind::Design))
                    .build(),
            ),
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("b3"))
                    .task(Some(task.clone()))
                    .build(),
            ),
            EventKind::RunCollected {
                run: RunId::new("b3"),
                head: None,
                summary: Some("Unpublished description".into()),
                outcome: None,
            },
        ])
    })
    .unwrap();
    for (run, expected, absent) in [
        (
            "v1",
            "Original picture description",
            "Corrected picture description",
        ),
        (
            "v2",
            "Corrected picture description",
            "Original picture description",
        ),
        (
            "v3",
            "Corrected picture description",
            "Original picture description",
        ),
    ] {
        let vars = context_of(&e, &p, run).await;
        let description = var(&vars, "pr_description");
        assert!(description.contains(expected), "{run}: {description}");
        assert!(!description.contains(absent), "{run}: {description}");
        assert!(!description.contains("Unpublished description"));
    }
}

/// A task with a plan summary, built by run `b1` from `base`.
fn with_a_task(e: &AppState, p: &ProjectId) -> TaskId {
    with_a_task_goal(e, p, "Ship it")
}

fn with_a_task_goal(e: &AppState, p: &ProjectId, goal: &str) -> TaskId {
    let task = TaskId::new("t1");
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::GoalStated {
                goal: GoalId::new("g1"),
                text: goal.into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: GoalId::new("g1"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Add the thing".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "Add the thing to the parser.".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g1"),
            },
            EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment("b1"))
                    .task(Some(task.clone()))
                    .role(RoleId::new("backend"))
                    .base(Some("b".repeat(40)))
                    .build(),
            ),
            EventKind::TaskStarted {
                branch_repair: false,
                retried: false,
                task: task.clone(),
                run: RunId::new("b1"),
                resume: false,
            },
        ])
    })
    .unwrap();
    task
}

async fn context_of(e: &AppState, p: &ProjectId, run: &str) -> Vec<(&'static str, String)> {
    let (r, proj) = {
        let g = e.proj();
        (g.runs[&RunId::new(run)].clone(), g.projects[p].clone())
    };
    prompt::context(e, &r, &proj).await.unwrap()
}

fn var<'a>(vars: &'a [(&str, String)], name: &str) -> &'a str {
    vars.iter()
        .find(|(k, _)| *k == name)
        .map_or("", |(_, v)| v.as_str())
}

/// Synthetic context is generated at runtime, never copied into demo/PR text.
fn long_goal(version: &str) -> String {
    let mut goal = format!("Synthetic {version} goal\n");
    goal.push_str(&"  café 雪 🦀 \"quotes\" <section>HTML & text</section>\r\n{{project_name}} {{task_title}} {{unknown}}\n".repeat(600));
    goal.push_str("\n  trailing whitespace \t\n");
    assert!(goal.chars().count() >= 32_000);
    goal
}

fn assert_prompt_goal(text: &str, goal: &str) {
    let heading = "## Owner's goal (read-only context)\n\n";
    let after_heading = text.split_once(heading).unwrap().1;
    let (_, after_meaning) = after_heading.split_once("\n\n").unwrap();
    assert_eq!(&after_meaning.as_bytes()[..goal.len()], goal.as_bytes());
    assert!(after_meaning[goal.len()..].starts_with("\n\n"));
    assert_eq!(text.matches(heading).count(), 1);
}

#[tokio::test]
async fn all_four_roles_receive_the_complete_goal_task_and_browser_rule() {
    let (_d, e, p) = engine();
    let goal = long_goal("original");
    let task = with_a_task_goal(&e, &p, &goal);
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(default_records(|role| RoleId::new(role.slug))
            .into_iter()
            .map(EventKind::RoleAdded)
            .collect())
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::BranchPublished {
            task: task.clone(),
            branch: "dvb/t".into(),
            head: "h".repeat(40),
            round: 1,
        }])
    })
    .unwrap();
    for (slug, run) in [
        ("backend", "b1"),
        ("clerk", "c1"),
        ("code-reviewer", "v1"),
        ("design-reviewer", "v2"),
    ] {
        if slug != "backend" {
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunStarted(
                    support::assignment::Assignment::from_assignment(assignment(run))
                        .task(Some(task.clone()))
                        .role(RoleId::new(slug))
                        .reviews((slug != "clerk").then(|| "h".repeat(40)))
                        .review_stage((slug == "design-reviewer").then_some(ReviewKind::Design))
                        .base(Some("b".repeat(40)))
                        .build(),
                )])
            })
            .unwrap();
            let vars = context_of(&e, &p, run).await;
            if slug != "clerk" {
                assert_eq!(var(&vars, "head"), "h".repeat(40));
                assert_eq!(var(&vars, "base"), "b".repeat(40));
            }
        }
        launch::launch(e.clone(), RunId::new(run), "[]".into(), vec![], None)
            .await
            .unwrap();
        let dir = e.cfg.paths.work.join(run);
        let text = std::fs::read_to_string(dir.join("prompt.md")).unwrap();
        let heading = match slug {
            "backend" => "# Backend builder",
            "clerk" => "# Clerk",
            "code-reviewer" => "# Code reviewer",
            "design-reviewer" => "# Design reviewer",
            _ => unreachable!(),
        };
        assert!(text.starts_with(heading), "{slug}: {text}");
        assert!(
            text.contains("Add the thing\n\nAdd the thing to the parser."),
            "{text}"
        );
        if slug != "clerk" {
            let browser = include_str!("../../../prompts/browser.md").trim();
            assert_eq!(text.matches(browser).count(), 1, "{slug}: {text}");
        }
        assert_eq!(text.matches(RUN_VERBS).count(), 1, "{slug}");
        assert!(!text.contains(ORCHESTRATOR_VERBS.trim()), "{slug}");
        assert!(!text.contains("{{browser}}"));
        assert_prompt_goal(&text, &goal);
        assert!(
            text.find("Add the thing to the parser.").unwrap()
                < text.find("## Owner's goal").unwrap()
        );
        // Literal template-like goal text must survive, but template keys do not.
        assert!(!text.contains("{{owner_goal}}"));
        assert!(!text.contains("{{task_summary}}"));
    }
}

#[tokio::test]
async fn a_builder_is_given_the_plans_summary_and_the_open_findings_to_fix() {
    let (_d, e, p) = engine();
    let task = with_a_task(&e, &p);
    let vars = context_of(&e, &p, "b1").await;
    assert_eq!(var(&vars, "task_summary"), "Add the thing to the parser.");
    assert_eq!(var(&vars, "review_findings"), "");

    let finding = |open| Finding {
        severity: Severity::High,
        file: "src/lib.rs".into(),
        line: std::num::NonZeroU32::new(42),
        title: if open { "Unchecked index" } else { "Style" }.into(),
        detail: "check first".into(),
        open,
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::VerdictRecorded {
            review_stage: None,
            verdict: VerdictId::new("v"),
            task,
            head: "h".repeat(40),
            run: RunId::new("v1"),
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            findings: vec![finding(true), finding(false)],
            summary: "s".into(),
        }])
    })
    .unwrap();
    let vars = context_of(&e, &p, "b1").await;
    let f = var(&vars, "review_findings");
    assert!(f.contains("src/lib.rs:42: Unchecked index"), "{f}");
    assert!(!f.contains("Style"), "only open findings: {f}");
}

#[tokio::test]
async fn reviewer_findings_follow_the_stage_even_when_one_role_reviews_both() {
    let (d, e, p) = engine();
    let task = with_a_task(&e, &p);
    for (stage, run) in [(ReviewKind::Code, "code"), (ReviewKind::Design, "design")] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment(run))
                    .task(Some(task.clone()))
                    .role(RoleId::new("code-reviewer"))
                    .reviews(Some("h".repeat(40)))
                    .review_stage(Some(stage))
                    .build(),
            )])
        })
        .unwrap();
    }
    let verdict = |stage, title: Option<&str>| EventKind::VerdictRecorded {
        review_stage: stage,
        verdict: VerdictId::new("v"),
        task: task.clone(),
        head: "h".repeat(40),
        run: RunId::new("previous-review"),
        role: RoleId::new("code-reviewer"),
        model: "fake-1".into(),
        vendor: Vendor::Fake,
        findings: title
            .map(|title| Finding {
                severity: Severity::Medium,
                file: "src/lib.rs".into(),
                line: None,
                title: title.into(),
                detail: "Recheck this".into(),
                open: true,
            })
            .into_iter()
            .collect(),
        summary: "Review".into(),
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            verdict(Some(ReviewKind::Code), Some("Code finding")),
            verdict(Some(ReviewKind::Design), Some("Design finding")),
        ])
    })
    .unwrap();
    let findings = async |run| var(&context_of(&e, &p, run).await, "review_findings").to_owned();
    assert!(findings("code").await.contains("Code finding"));
    assert!(!findings("code").await.contains("Design finding"));
    assert!(findings("design").await.contains("Design finding"));
    assert!(!findings("design").await.contains("Code finding"));
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![verdict(Some(ReviewKind::Code), None)])
    })
    .unwrap();
    assert!(findings("code").await.is_empty());
    assert!(findings("design").await.contains("Design finding"));
    let replay = support::engine::open_with(
        d.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    for run in ["code", "design"] {
        assert_eq!(
            var(&context_of(&replay, &p, run).await, "review_findings"),
            findings(run).await
        );
    }
    // Historical verdicts have no stage; retain their finding-recheck behavior.
    for title in [Some("Legacy finding"), None] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![verdict(None, title)])
        })
        .unwrap();
        for run in ["code", "design"] {
            assert_eq!(
                findings(run).await.contains("Legacy finding"),
                title.is_some()
            );
            assert!(!findings(run).await.contains("Design finding"));
        }
    }
}

#[tokio::test]
async fn a_clerks_prompt_carries_the_task_it_was_started_for() {
    let (_d, e, p) = engine();
    let task = with_a_task(&e, &p);
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::from_assignment(assignment("c1"))
                .task(Some(task))
                .role(RoleId::new("clerk"))
                .base(Some("b".repeat(40)))
                .build(),
        )])
    })
    .unwrap();
    let mut vars = context_of(&e, &p, "c1").await;
    vars.push(("project_name", "fieldnotes".into()));
    vars.push(("task_title", "Add the thing".into()));
    let text = render(include_str!("../../../prompts/clerk.md"), &vars, false);
    assert!(text.contains("Add the thing"), "{text}");
    assert!(text.contains("Add the thing to the parser."), "{text}");
    assert_eq!(text.matches(RUN_VERBS).count(), 1, "{text}");
    assert!(!text.contains(ORCHESTRATOR_VERBS.trim()), "{text}");
    assert!(!text.contains("{{"), "{text}");
}

fn note(e: &AppState, p: &ProjectId, text: &str) {
    e.append_checked(Some(p), Actor::Run(RunId::new("r1")), |_, _| {
        Ok(vec![EventKind::RunOutput {
            resource: None,
            run: RunId::new("r1"),
            item: dvb_core::event::OutputItem::Note(text.into()),
        }])
    })
    .unwrap();
}

#[tokio::test]
async fn a_saved_note_reaches_the_next_orchestrator_prompt_and_the_file() {
    let (d, e, p) = engine();
    for run in ["r1", "r2"] {
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(assignment(run))])
        })
        .unwrap();
        if run == "r1" {
            note(&e, &p, "marker-7\nsecond line");
            for text in ["waiting for disk space: 10 GB free", "same-vendor review"] {
                e.append_checked(Some(&p), Actor::Engine, |_, _| {
                    Ok(vec![EventKind::RunOutput {
                        resource: None,
                        run: RunId::new("r1"),
                        item: dvb_core::event::OutputItem::Note(text.into()),
                    }])
                })
                .unwrap();
            }
            note(&e, &p, "waiting for disk space: my own note");
        }
    }
    let text = orchestrator_prompt(&e, &p, "r2").await;
    assert!(text.contains("- marker-7\n  second line"), "{text}");
    let file = std::fs::read_to_string(
        e.cfg
            .paths
            .state
            .join("projects")
            .join(p.to_string())
            .join("NOTES.md"),
    )
    .unwrap();
    assert_eq!(
        file,
        "- marker-7\n  second line\n- waiting for disk space: my own note\n"
    );
    assert!(!text.contains("same-vendor review"));
    assert!(!text.contains("10 GB free"));
    let notes = support::events::all(&e);
    assert_eq!(
        notes
            .iter()
            .filter(|event| matches!(
                &event.kind,
                EventKind::RunOutput {
                    resource: None,
                    item: dvb_core::event::OutputItem::Note(_),
                    ..
                }
            ))
            .count(),
        4,
        "engine status stays durable"
    );
    drop(d);
}

#[test]
fn concurrent_syncs_leave_one_whole_bullet_per_committed_note() {
    let (_d, e, p) = engine();
    for i in 0..50 {
        note(&e, &p, &format!("note {i}"));
    }
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                for _ in 0..50 {
                    e.sync_notes(&p).unwrap();
                }
            });
        }
    });
    let file = std::fs::read_to_string(
        e.cfg
            .paths
            .state
            .join("projects")
            .join(p.to_string())
            .join("NOTES.md"),
    )
    .unwrap();
    assert_eq!(file.lines().count(), 50);
    assert!(file.lines().all(|l| l.starts_with("- note ")));
}

#[test]
fn a_stale_notes_file_is_repaired_from_the_log() {
    let (_d, e, p) = engine();
    let dir = e.cfg.paths.state.join("projects").join(p.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("NOTES.md"), "- never committed\n").unwrap();
    note(&e, &p, "committed");
    e.sync_notes(&p).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("NOTES.md")).unwrap(),
        "- committed\n"
    );
}

fn run_note(e: &AppState, p: &ProjectId, run: &str, text: &str) {
    e.append_checked(Some(p), Actor::Run(RunId::new(run)), |_, _| {
        Ok(vec![EventKind::RunOutput {
            resource: None,
            run: RunId::new(run),
            item: dvb_core::event::OutputItem::Note(text.into()),
        }])
    })
    .unwrap();
}

/// States a goal and starts an orchestrator wake bound to it.
fn bound_wake(e: &AppState, p: &ProjectId, goal: &str, run: &str) -> dvb_core::EventId {
    let version = e
        .append_checked(Some(p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::GoalStated {
                goal: GoalId::new(goal),
                text: format!("Do {goal}"),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            }])
        })
        .unwrap()[0]
        .id;
    start_wake(e, p, run, Some(version));
    version
}

fn start_wake(e: &AppState, p: &ProjectId, run: &str, goal: Option<dvb_core::EventId>) {
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::new(RunId::new(run))
                .role(RoleId::new("orchestrator"))
                .goal_version(goal)
                .build(),
        )])
    })
    .unwrap();
}

fn notes_block(text: &str) -> &str {
    text.split("## Your notes\n\n")
        .nth(1)
        .unwrap()
        .split("\n## Since you last ran")
        .next()
        .unwrap()
        .trim_end()
}

fn notes_file(e: &AppState, p: &ProjectId) -> String {
    std::fs::read_to_string(
        e.cfg
            .paths
            .state
            .join("projects")
            .join(p.to_string())
            .join("NOTES.md"),
    )
    .unwrap()
}

#[tokio::test]
async fn the_orchestrator_sees_only_open_goals_notes_and_the_newest_untied_ones() {
    let (_d, e, p) = engine();
    start_wake(&e, &p, "free", None);
    for i in 0..7 {
        run_note(&e, &p, "free", &format!("untied {i}"));
    }
    bound_wake(&e, &p, "done", "w-done");
    for i in 0..100 {
        run_note(
            &e,
            &p,
            "w-done",
            &format!("done {i:03} {}", "x".repeat(300)),
        );
    }
    bound_wake(&e, &p, "dropped", "w-dropped");
    run_note(&e, &p, "w-dropped", "dropped note");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalDone {
                goal: GoalId::new("done"),
            },
            EventKind::GoalDropped {
                goal: GoalId::new("dropped"),
            },
        ])
    })
    .unwrap();
    let open = bound_wake(&e, &p, "open", "w-open");
    run_note(&e, &p, "w-open", "open from the orchestrator");
    let task = TaskId::new("t1");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("open"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Build it".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "Build it.".into(),
                    routine: false,
                }],
            },
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("b1"))
                    .task(Some(task.clone()))
                    .build(),
            ),
        ])
    })
    .unwrap();
    run_note(&e, &p, "b1", "open from the builder");
    start_wake(&e, &p, "now", Some(open));
    let text = orchestrator_prompt(&e, &p, "now").await;
    let notes = notes_block(&text);
    assert_eq!(
        notes.lines().collect::<Vec<_>>(),
        [
            "- untied 2",
            "- untied 3",
            "- untied 4",
            "- untied 5",
            "- untied 6",
            "- open from the orchestrator",
            "- open from the builder",
        ],
        "{notes}"
    );
    assert!(notes.len() < 16 * 1024);
    let file = notes_file(&e, &p);
    assert_eq!(file.lines().count(), 7 + 100 + 1 + 2);
    assert!(file.contains("- done 099 ") && file.contains("- dropped note\n"));
}

#[tokio::test]
async fn past_the_cap_the_oldest_notes_are_left_out_with_one_line() {
    let (_d, e, p) = engine();
    let open = bound_wake(&e, &p, "open", "w-open");
    for i in 0..40 {
        run_note(
            &e,
            &p,
            "w-open",
            &format!("note {i:02} {}", "y".repeat(1000)),
        );
    }
    start_wake(&e, &p, "now", Some(open));
    let text = orchestrator_prompt(&e, &p, "now").await;
    let notes = notes_block(&text);
    assert!(notes.len() <= 16 * 1024, "{}", notes.len());
    let mut lines = notes.lines();
    let first = lines.next().unwrap();
    let kept: Vec<_> = lines.collect();
    assert_eq!(
        first,
        format!(
            "({} older notes left out to keep this short; NOTES.md has every note.)",
            40 - kept.len()
        )
    );
    assert!(kept.len() > 10, "{}", kept.len());
    for (line, i) in kept.iter().zip(40 - kept.len()..) {
        assert!(line.starts_with(&format!("- note {i:02} ")), "{line}");
    }
    assert_eq!(notes_file(&e, &p).lines().count(), 40);
}

#[test]
fn concurrent_appends_and_syncs_never_lose_a_committed_note() {
    let (_d, e, p) = engine();
    std::thread::scope(|s| {
        for t in 0..4 {
            let (e, p) = (&e, &p);
            s.spawn(move || {
                for i in 0..25 {
                    note(e, p, &format!("note {t}-{i}"));
                    e.sync_notes(p).unwrap();
                }
            });
        }
    });
    let file = std::fs::read_to_string(
        e.cfg
            .paths
            .state
            .join("projects")
            .join(p.to_string())
            .join("NOTES.md"),
    )
    .unwrap();
    assert_eq!(file.lines().count(), 100);
}

/// A durable failed builder, with role/hire fixtures reused by owner retries.
fn failed_builder(e: &AppState, p: &ProjectId, task: &TaskId) {
    let (from, run) = {
        let proj = e.proj();
        let t = &proj.tasks[task];
        let result = (t.view.state.clone(), t.facts.run.clone().unwrap());
        drop(proj);
        result
    };
    e.append_checked(Some(p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunEnded {
                run,
                reason: dvb_core::event::EndReason::Crashed,
                detail: None,
            },
            EventKind::TaskFailed {
                task: task.clone(),
                reason: Some("Missing parser format".into()),
            },
            EventKind::TaskStateChanged {
                task: task.clone(),
                from,
                to: dvb_core::TaskState::Failed {
                    reason: "Missing parser format".into(),
                },
            },
        ])
    })
    .unwrap();
}

async fn builder_prompt(e: &AppState, run: &RunId) -> String {
    let file = e.cfg.paths.work.join(run.as_str()).join("prompt.md");
    support::wait::FAST
        .until("retry prompt", || file.exists())
        .await;
    std::fs::read_to_string(&file).unwrap()
}

mod support;

#[tokio::test]
async fn owner_retry_text_is_bounded_replaced_cleared_and_replayed_in_saved_prompts() {
    use axum::http::{Method, StatusCode};
    use serde_json::json;
    use support::http::{call, request};
    let (d, e, p) = engine();
    let task = with_a_task(&e, &p);
    let (roles, hires) = support::events::default_role_hire_events();
    let mut setup: Vec<_> = hires
        .into_iter()
        .zip(roles)
        .flat_map(|(hire, role)| [hire, role])
        .collect();
    setup.push(EventKind::ProjectBriefSet {
        brief: "Keep the private parser conventions.".into(),
    });
    setup.push(EventKind::VerdictRecorded {
        review_stage: None,
        verdict: VerdictId::new("finding"),
        task: task.clone(),
        head: "h".repeat(40),
        run: RunId::new("v0"),
        role: RoleId::new("code-reviewer"),
        model: "fake-1".into(),
        vendor: Vendor::Fake,
        findings: vec![Finding {
            severity: Severity::High,
            file: "parser.rs".into(),
            line: None,
            title: "Check missing fields".into(),
            detail: "Keep this finding".into(),
            open: true,
        }],
        summary: "Keep findings".into(),
    });
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(setup))
        .unwrap();
    let legacy: EventKind = serde_json::from_value(
        json!({"kind":"task.resumed", "payload":{"task":task,"reason":"retried after it failed"}}),
    )
    .unwrap();
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(vec![legacy]))
        .unwrap();
    failed_builder(&e, &p, &task);
    let historical = support::engine::open_with(
        d.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    assert_eq!(
        var(&context_of(&historical, &p, "b1").await, "retry_text"),
        ""
    );
    drop(historical);
    let path = format!("/tasks/{task}/retry");
    // Invalid types and oversize Unicode bytes refuse without changing retry context.
    for text in [json!(1), json!("é".repeat(8193))] {
        let (status, body) = call(&e, Method::POST, &path, Some(json!({"text":text}))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "INVALID_VALUE");
        if text.is_string() {
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap()
                    .contains(&dvb_core::api::RETRY_TEXT_MAX_BYTES.to_string())
            );
        }
    }
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::TaskRetryTextSet { .. }))
    );
    let mut text = format!("{}\n{{{{literal}}}}", "é".repeat(8180));
    text.push_str(&"x".repeat(dvb_core::api::RETRY_TEXT_MAX_BYTES - text.len()));
    assert_eq!(text.len(), dvb_core::api::RETRY_TEXT_MAX_BYTES);
    let (status, _) = call(&e, Method::POST, &path, Some(json!({"text":text}))).await;
    assert_eq!(status, StatusCode::OK);
    let run = e.proj().tasks[&task].facts.builder.clone().unwrap();
    let delivered = builder_prompt(&e, &run).await;
    assert!(delivered.contains(&text));
    assert!(
        delivered.find("Add the thing to the parser.").unwrap()
            < delivered.find("## Retry context").unwrap()
    );
    assert!(
        delivered.find("## Retry context").unwrap() < delivered.find("## Fix what failed").unwrap()
    );
    assert!(delivered.contains("Keep the private parser conventions."));
    assert!(delivered.contains("Check missing fields"));
    assert!(delivered.contains("Missing parser format"));
    // State refusal leaves the accepted text and its prompt unchanged.
    let (status, _) = call(
        &e,
        Method::POST,
        &path,
        Some(json!({"text":"refused replacement"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        var(&context_of(&e, &p, run.as_str()).await, "retry_text"),
        format!(
            "## Retry context\n\nThis text accompanies this retry and supplies missing information. It does not replace the task.\n\n{text}\n\n"
        )
    );
    // Reopen SQLite and rebuild; the same run and subsequent builder retain the text.
    let reopened = support::engine::open_with(
        d.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    assert_eq!(
        var(&context_of(&reopened, &p, run.as_str()).await, "retry_text"),
        var(&context_of(&e, &p, run.as_str()).await, "retry_text")
    );
    let next = RunId::new("following-builder");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::from_assignment(assignment(next.as_str()))
                .task(Some(task.clone()))
                .role(RoleId::new("backend"))
                .base(Some("b".repeat(40)))
                .build(),
        )])
    })
    .unwrap();
    launch::launch(e.clone(), next.clone(), "[]".into(), vec![], None)
        .await
        .unwrap();
    assert!(builder_prompt(&e, &next).await.contains(&text));
    // Keep the original instance for replacements, ending the previous builder.
    failed_builder(&e, &p, &task);
    let (status, _) = call(
        &e,
        Method::POST,
        &path,
        Some(json!({"text":"Use CSV instead."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let replacement = e.proj().tasks[&task].facts.builder.clone().unwrap();
    let delivered = builder_prompt(&e, &replacement).await;
    assert!(delivered.contains("Use CSV instead."));
    assert!(!delivered.contains(&text));
    // Rendering an older run is still bound to the first text.
    assert!(var(&context_of(&e, &p, run.as_str()).await, "retry_text").contains(&text));
    for body in [
        None,
        Some(json!(null)),
        Some(json!({})),
        Some(json!({"text":null})),
        Some(json!({"text":" \n\t"})),
    ] {
        failed_builder(&e, &p, &task);
        let (status, _) = request(&e, Method::POST, &path, body).await;
        assert_eq!(status, StatusCode::OK);
        let run = e.proj().tasks[&task].facts.builder.clone().unwrap();
        let delivered = builder_prompt(&e, &run).await;
        assert!(!delivered.contains("## Retry context"));
        assert!(!delivered.contains("Use CSV instead."));
        let mut vars = context_of(&e, &p, run.as_str()).await;
        let (project, role, title) = {
            let proj = e.proj();
            let project = &proj.projects[&p];
            (
                project.view.name.clone(),
                project.role(&RoleId::new("backend")).unwrap().clone(),
                proj.tasks[&task].view.title.clone(),
            )
        };
        vars.extend([
            ("project_name", project),
            ("role_name", role.name),
            ("role_job", role.job),
            ("role_lane", role.lane),
            ("task_title", title),
        ]);
        // Preserve the historical fixture; goal context, scope-block reporting
        // and summary/evidence handoff rules are outside its no-retry shape.
        let (before, instructions) = delivered
            .split_once("You cannot ask the owner questions.")
            .unwrap();
        let (_, after) = instructions
            .split_once("If this machine has a headless browser")
            .unwrap();
        let historical = format!("{before}If this machine has a headless browser{after}");
        assert_eq!(
            historical.replacen(var(&vars, "owner_goal"), "", 1),
            render(
                include_str!("fixtures/builder-before-retry-text.md"),
                &vars,
                false
            ),
            "apart from goal context, scope-block reporting and summary/evidence handoff, no-text delivery matches the historical builder template exactly"
        );
    }
    // The original historical run predates all retry events and sees no text.
    assert_eq!(var(&context_of(&e, &p, "b1").await, "retry_text"), "");
}

#[tokio::test]
async fn redirect_new_plan_and_retry_preserve_each_tasks_goal_through_replay() {
    use axum::http::{Method, StatusCode};
    use support::http::call;
    let (d, e, p) = engine();
    let original = long_goal("original");
    let redirected = long_goal("redirected");
    let task = with_a_task_goal(&e, &p, &original);
    let (roles, hires) = support::events::default_role_hire_events();
    let mut setup: Vec<_> = hires
        .into_iter()
        .zip(roles)
        .flat_map(|(hire, role)| [hire, role])
        .collect();
    setup.push(EventKind::TaskStateChanged {
        task: task.clone(),
        from: dvb_core::TaskState::Planned,
        to: dvb_core::TaskState::Running,
    });
    // An already-running planner keeps redirect from launching another unit.
    setup.push(EventKind::RunStarted(assignment("live-planner")));
    e.append_checked(Some(&p), Actor::Engine, |_, _| Ok(setup))
        .unwrap();
    dvb_engine::owner::redirect(&e, &GoalId::new("g1"), redirected.clone()).unwrap();
    let newer = TaskId::new("new-plan-task");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::PlanProposed {
                goal: GoalId::new("g1"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: newer.clone(),
                    title: "New task".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "Work after redirect.".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g1"),
            },
            EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment("new-plan-run"))
                    .task(Some(newer))
                    .role(RoleId::new("backend"))
                    .base(Some("b".repeat(40)))
                    .build(),
            ),
        ])
    })
    .unwrap();
    failed_builder(&e, &p, &task);
    let (status, _) = call(
        &e,
        Method::POST,
        &format!("/tasks/{task}/retry"),
        Some(serde_json::json!({"text":"Additional retry information."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let retry = e.proj().tasks[&task].facts.builder.clone().unwrap();
    assert_prompt_goal(&builder_prompt(&e, &retry).await, &original);
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::from_assignment(assignment(
                "old-task-review-after-redirect",
            ))
            .task(Some(task.clone()))
            .role(RoleId::new("code-reviewer"))
            .reviews(Some("h".repeat(40)))
            .base(Some("h".repeat(40)))
            .build(),
        )])
    })
    .unwrap();
    // Rebuild all projections from the unchanged log, then render/deliver both
    // the historical run and its retry, and the task planned after redirect.
    let events = support::events::all(&e);
    let schema = dvb_store::schema_version();
    let reopened = support::engine::open_with(
        d.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    let replayed = support::events::all(&reopened);
    assert!(
        replayed.starts_with(&events),
        "replay must preserve historical events"
    );
    assert_eq!(dvb_store::schema_version(), schema);
    for (run, goal) in [
        ("b1", &original),
        (retry.as_str(), &original),
        ("old-task-review-after-redirect", &original),
        ("new-plan-run", &redirected),
    ] {
        launch::launch(reopened.clone(), RunId::new(run), "[]".into(), vec![], None)
            .await
            .unwrap();
        assert_prompt_goal(&builder_prompt(&reopened, &RunId::new(run)).await, goal);
    }
    assert_eq!(
        prompt::summary_of(&events, &task),
        "Add the thing to the parser."
    );
}

#[tokio::test]
async fn absent_goal_text_preserves_all_four_saved_prompts() {
    let (d, e, p) = engine();
    let task = with_a_task_goal(&e, &p, "");
    let mut roles = default_records(|r| RoleId::new(r.slug));
    // Empty role fields make the historical-template comparison direct.
    for role in &mut roles {
        role.name.clear();
        role.job.clear();
        role.lane.clear();
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(roles.into_iter().map(EventKind::RoleAdded).collect())
    })
    .unwrap();
    // Also cover a legacy plan without any preceding goal.stated.
    let missing = TaskId::new("missing-goal");
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::PlanProposed {
            goal: GoalId::new("missing"),
            tasks: vec![PlannedTask {
                after: vec![],
                task: missing.clone(),
                title: "Legacy task".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "Legacy summary".into(),
                routine: false,
            }],
        }])
    })
    .unwrap();
    for (i, slug) in ["backend", "clerk", "code-reviewer", "design-reviewer"]
        .iter()
        .enumerate()
    {
        let run = RunId::new(format!("no-goal-{i}"));
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment(run.as_str()))
                    .task(Some(if i % 2 == 0 {
                        task.clone()
                    } else {
                        missing.clone()
                    }))
                    .role(RoleId::new(*slug))
                    .base(Some("b".repeat(40)))
                    .reviews((i >= 2).then(|| "h".repeat(40)))
                    .review_stage((*slug == "design-reviewer").then_some(ReviewKind::Design))
                    .build(),
            )])
        })
        .unwrap();
        launch::launch(e.clone(), run.clone(), "[]".into(), vec![], None)
            .await
            .unwrap();
        let delivered = builder_prompt(&e, &run).await;
        assert!(!delivered.contains("## Owner's goal"));
        let template = match *slug {
            "backend" => include_str!("../../../prompts/builder.md"),
            "clerk" => include_str!("../../../prompts/clerk.md"),
            "code-reviewer" => include_str!("../../../prompts/code-reviewer.md"),
            "design-reviewer" => include_str!("../../../prompts/design-reviewer.md"),
            _ => unreachable!(),
        };
        let before = template.replace("{{owner_goal}}", "");
        let mut vars = context_of(&e, &p, run.as_str()).await;
        vars.push(("project_name", "fieldnotes".into()));
        vars.push((
            "task_title",
            if i % 2 == 0 {
                "Add the thing"
            } else {
                "Legacy task"
            }
            .into(),
        ));
        assert_eq!(delivered, render(&before, &vars, false));
    }
    drop(d);
}

#[tokio::test]
async fn earlier_release_stored_payloads_load_and_render_without_rewriting() {
    let (d, e, p) = engine();
    let db = e.cfg.paths.state.join("dvb.sqlite");
    drop(e);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/goal-context-before.json")).unwrap();
    for event in &fixture {
        // Insert raw earlier-release payloads, rather than serializing them with
        // today's types. In particular run.started has no goal_version field.
        let payload = serde_json::to_string_pretty(&event["payload"]).unwrap();
        conn.execute("INSERT INTO events (at, actor, project, kind, payload) VALUES ('1', '\"engine\"', ?1, ?2, ?3)",
            rusqlite::params![p.as_str(), event["kind"].as_str().unwrap(), payload]).unwrap();
    }
    let raw_rows = || {
        conn.prepare("SELECT kind, payload FROM events ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = raw_rows();
    let schema = dvb_store::schema_version();
    let reopened = support::engine::open_with(
        d.path(),
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: None,
            launcher: Some(std::sync::Arc::new(support::launcher::FakeLauncher::new(
                UnitState::Active,
            ))),
            ..Default::default()
        },
    );
    launch::launch(
        reopened.clone(),
        RunId::new("legacy-run"),
        "[]".into(),
        vec![],
        None,
    )
    .await
    .unwrap();
    assert_prompt_goal(
        &builder_prompt(&reopened, &RunId::new("legacy-run")).await,
        fixture[0]["payload"]["text"].as_str().unwrap(),
    );
    let after = raw_rows();
    assert!(
        after.starts_with(&before),
        "stored event bytes must remain untouched"
    );
    let added: Vec<&str> = after[before.len()..]
        .iter()
        .map(|(kind, _)| kind.as_str())
        .collect();
    assert_eq!(added, ["engine.started", "run.launched"]);
    assert_eq!(dvb_store::schema_version(), schema);
}

#[tokio::test]
async fn architect_review_receives_the_review_template_in_the_actual_prompt_file() {
    for stage in [None, Some(ReviewKind::Code), Some(ReviewKind::Design)] {
        let (_d, e, p) = engine();
        let task = with_a_task_goal(&e, &p, "Review this goal");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(default_records(|r| RoleId::new(r.slug))
                .into_iter()
                .map(EventKind::RoleAdded)
                .collect())
        })
        .unwrap();
        let run = RunId::new("architect-review");
        e.append_checked(Some(&p), Actor::Engine, |_, _| {
            Ok(vec![EventKind::RunStarted(
                support::assignment::Assignment::from_assignment(assignment(run.as_str()))
                    .task(Some(task))
                    .role(RoleId::new("architect"))
                    .reviews(Some("h".repeat(40)))
                    .review_stage(stage)
                    .base(Some("b".repeat(40)))
                    .build(),
            )])
        })
        .unwrap();
        launch::launch(e.clone(), run.clone(), "[]".into(), vec![], None)
            .await
            .unwrap();
        let text =
            std::fs::read_to_string(e.cfg.paths.work.join(run.as_str()).join("prompt.md")).unwrap();
        assert!(
            text.starts_with(if stage == Some(ReviewKind::Design) {
                "# Design reviewer"
            } else {
                "# Code reviewer"
            }),
            "{text}"
        );
        assert!(text.contains("dvb run verdict"));
    }
}

#[tokio::test]
async fn redirected_orchestrator_prompt_identifies_exhausted_stopped_tasks_and_kept_branch() {
    let (_dir, e, p) = engine();
    let task = TaskId::new("exhausted");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::ProjectPaused {},
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "Original".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Old work".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "x".into(),
                    routine: false,
                }],
            },
            EventKind::BranchPublished {
                task: task.clone(),
                branch: "dvb/kept-1".into(),
                head: "head".into(),
                round: dvb_core::caps::REVIEW_ROUNDS_PER_PR,
            },
            EventKind::VerdictRecorded {
                review_stage: None,
                verdict: VerdictId::new("v"),
                task: task.clone(),
                head: "head".into(),
                run: RunId::new("review"),
                role: RoleId::new("code-reviewer"),
                model: "fake-1".into(),
                vendor: Vendor::Fake,
                findings: vec![Finding {
                    severity: Severity::Medium,
                    file: "src/lib.rs".into(),
                    line: None,
                    title: "Open defect".into(),
                    detail: "Needs another fix round".into(),
                    open: true,
                }],
                summary: "Changes needed".into(),
            },
            EventKind::TaskStateChanged {
                task,
                from: dvb_core::TaskState::InReview,
                to: dvb_core::TaskState::Stopped {
                    was: Box::new(dvb_core::TaskState::InReview),
                },
            },
        ])
    })
    .unwrap();
    dvb_engine::owner::redirect(
        &e,
        &GoalId::new("g"),
        "Keep the old branch's work and change direction".into(),
    )
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(assignment("redirected"))])
    })
    .unwrap();
    let prompt = orchestrator_prompt(&e, &p, "redirected").await;
    assert!(prompt.contains("cannot be resumed"), "{prompt}");
    assert!(prompt.contains("new task"));
    assert!(prompt.contains("dvb/kept-1"));
    assert!(prompt.contains("preserve"));
}

#[tokio::test]
async fn orchestrator_context_renders_configured_lanes_and_all_active_hires_before_any_plan() {
    let (_d, e, p) = engine();
    let (mut roles, hires) = support::events::default_role_hire_events();
    for event in &mut roles {
        if let EventKind::RoleAdded(role) = event {
            if role.slug == "frontend" {
                role.lane = "UI code and UI tests only".into();
            }
            if role.slug == "backend" {
                role.lane = "Engine, API and CLI only".into();
            }
        }
    }
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        let mut events = roles.into_iter().chain(hires).collect::<Vec<_>>();
        events.push(EventKind::RunStarted(assignment("lane-plan")));
        Ok(events)
    })
    .unwrap();
    let text = orchestrator_prompt(&e, &p, "lane-plan").await;
    assert!(text.contains("frontend:") && text.contains("UI code and UI tests only"));
    assert!(text.contains("backend:") && text.contains("Engine, API and CLI only"));
    assert!(text.contains("h-frontend: fake-1") && text.contains("h-backend: fake-1"));
    assert!(text.contains("Split work that spans lanes"));
    assert!(text.contains("\"after\":[1]"));
}
