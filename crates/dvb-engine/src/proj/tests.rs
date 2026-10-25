use super::Projections;
use crate::wake::Wake;
use dvb_core::event::{EndReason, LapsesOn, PreflightCode};
use dvb_core::roles::ReviewKind;
use dvb_core::{
    Actor, Event, EventId, EventKind, GoalId, ProjectId, QuestionId, RoleId, RunId, TaskId,
    TaskState, Timestamp,
};

use dvb_core::{Effort, Vendor};

fn ev(id: u64, kind: EventKind) -> Event {
    Event {
        id: EventId(id),
        at: Timestamp(0),
        actor: Actor::Engine,
        project: Some(ProjectId::new("p")),
        kind,
        evidence: None,
    }
}

#[test]
fn empty_natural_completion_requires_saved_work_or_an_explicit_release() {
    let goal = GoalId::new("g");
    let run = RunId::new("r");
    let events = vec![
        ev(
            1,
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Plan it".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        ev(
            2,
            EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(run.clone())
                    .role(RoleId::new("orchestrator"))
                    .reason("A goal was stated.".into())
                    .build(),
            ),
        ),
        ev(
            3,
            EventKind::RunUsage {
                run: run.clone(),
                uncached_in: 10,
                cached_in: 0,
                out: 1,
                est_cost: 0.0,
            },
        ),
        ev(
            4,
            EventKind::RunOutput {
                resource: None,
                run: run.clone(),
                item: dvb_core::event::OutputItem::Text("nothing to do".into()),
            },
        ),
        ev(
            6,
            EventKind::RunEnded {
                run: run.clone(),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
        ),
    ];
    let ready = |events: &[Event]| {
        let p = Projections::rebuild(events);
        p.plan_wake_ready(&p.goals[&goal])
    };
    assert!(!ready(&events)); // includes legacy starts without a binding field
    for record in [
        EventKind::DecisionRecorded {
            by: RoleId::new("orchestrator"),
            title: "Need more context".into(),
            options: None,
            swot: None,
            choice: "Ask the owner".into(),
            reason: "The requirements are unclear".into(),
        },
        EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: QuestionId::new("q"),
            subject: "Which parser?".into(),
            format: dvb_core::event::QuestionFormat::Simple,
            options: vec![],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(EventId(1)),
        },
    ] {
        let mut recorded = events.clone();
        let mut record = ev(5, record);
        record.actor = Actor::Run(run.clone());
        recorded.insert(4, record);
        let p = Projections::rebuild(&recorded);
        let question = matches!(recorded[4].kind, EventKind::QuestionOpened { .. });
        assert_eq!(p.goals[&goal].planning_held, !question);
        assert_eq!(
            ready(&recorded),
            question && !p.orchestrator_question_open(&p.goals[&goal])
        );
    }
    let mut independent = events.clone();
    independent.insert(
        4,
        ev(
            5,
            EventKind::ProjectBriefSet {
                brief: "Clarified requirements".into(),
            },
        ),
    );
    assert!(!ready(&independent)); // a brief is not a release, even while live
    let mut note = events.clone();
    let mut record = ev(
        5,
        EventKind::RunOutput {
            resource: None,
            run: run.clone(),
            item: dvb_core::event::OutputItem::Note("nothing to do".into()),
        },
    );
    record.actor = Actor::Run(run.clone());
    note.insert(4, record);
    assert!(!ready(&note));
    // Historical startup Completed records lack survival evidence and
    // retain their interruption meaning. Keep those bytes intact.
    let mut interrupted = events.clone();
    let mut startup = ev(
        5,
        EventKind::EngineStarted {
            version: "old".into(),
        },
    );
    startup.project = None;
    interrupted.insert(4, startup);
    assert!(ready(&interrupted));
    let mut surviving = interrupted.clone();
    surviving.insert(5, ev(6, EventKind::RunSurvivedRestart { run: run.clone() }));
    surviving.last_mut().unwrap().id = EventId(7);
    assert!(!ready(&surviving)); // a normal empty survivor still holds
    let replay = Projections::rebuild(&surviving);
    assert_eq!(replay.runs[&run].ended, Some(EndReason::Completed));
    assert!(!replay.runs[&run].interrupted);
    // Stop intent wins whether it precedes or follows the startup's
    // survival observation. Even a stale Completed end cannot set a hold.
    for index in [5, 6] {
        let mut stopped = surviving.clone();
        stopped.insert(
            index,
            ev(7, EventKind::RunStopRequested { run: run.clone() }),
        );
        for (index, event) in stopped.iter_mut().enumerate() {
            event.id = EventId(index as u64 + 1);
        }
        let replay = Projections::rebuild(&stopped);
        assert!(replay.runs[&run].stop_requested);
        assert!(!replay.goals[&goal].planning_held);
    }
    // Another restart requires new evidence; a prior observation cannot
    // certify this run survived a later startup.
    let mut cut_off_again = surviving.clone();
    let mut startup = ev(
        7,
        EventKind::EngineStarted {
            version: "new".into(),
        },
    );
    startup.project = None;
    cut_off_again.insert(6, startup);
    cut_off_again.last_mut().unwrap().id = EventId(8);
    assert!(ready(&cut_off_again));
    let mut restarted_hold = events.clone();
    let mut startup = ev(
        7,
        EventKind::EngineStarted {
            version: "new".into(),
        },
    );
    startup.project = None;
    restarted_hold.push(startup);
    assert!(!ready(&restarted_hold));
    for reason in [
        EndReason::Crashed,
        EndReason::Stopped,
        EndReason::Interrupted,
        EndReason::Oom,
        EndReason::Mismatch,
        EndReason::ParseError,
        EndReason::Preflight(PreflightCode::PfHome),
    ] {
        let mut cut_short = events.clone();
        cut_short[4].kind = EventKind::RunEnded {
            run: run.clone(),
            reason,
            detail: None,
            stderr: None,
        };
        assert!(ready(&cut_short));
    }
    for kind in [
        EventKind::GoalStated {
            goal: goal.clone(),
            text: "Restated".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::GoalDropped {
            goal: GoalId::new("other"),
        },
        EventKind::GoalDone {
            goal: GoalId::new("other"),
        },
    ] {
        let mut released = events.clone();
        // Another stated goal does not release this one; its final state does.
        released.insert(
            0,
            ev(
                0,
                EventKind::GoalStated {
                    goal: GoalId::new("other"),
                    text: "Other work".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
            ),
        );
        if let EventKind::RunStarted(a) = &mut released[2].kind {
            a.goal_version = Some(EventId(1));
        }
        assert!(!ready(&released));
        released.push(ev(7, kind));
        assert!(ready(&released));
    }
    let mut refused = events.clone();
    let mut refusal = ev(
        7,
        EventKind::CommandRefused(dvb_core::event::RefusedCommand {
            command: "plan".into(),
            refusal: dvb_core::Refusal::new(dvb_core::RefusalCode::ValueChanged, "stale").into(),
        }),
    );
    refusal.actor = Actor::Run(run.clone());
    refused.push(refusal);
    assert!(!ready(&refused));
    let mut failed = events;
    failed[4].kind = EventKind::RunEnded {
        run,
        reason: EndReason::Crashed,
        detail: None,
        stderr: None,
    };
    assert!(ready(&failed)); // retain the existing failure retry/escalation
}

#[test]
fn planning_release_boundaries_and_question_lapse_replay() {
    let goal = GoalId::new("held");
    let other = GoalId::new("other");
    let task = TaskId::new("task");
    let run = RunId::new("planner");
    let initial = vec![
        ev(
            1,
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Held".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        ev(
            2,
            EventKind::GoalStated {
                goal: other.clone(),
                text: "Other".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        ev(
            3,
            EventKind::PlanProposed {
                goal: other.clone(),
                tasks: vec![dvb_core::event::PlannedTask {
                    after: vec![],
                    task: task.clone(),
                    title: "Build".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "Build".into(),
                    routine: false,
                }],
            },
        ),
        ev(
            4,
            EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(run.clone())
                    .goal_version(Some(EventId(1)))
                    .role(RoleId::new("orchestrator"))
                    .reason("The owner answered a question.".into())
                    .build(),
            ),
        ),
        ev(
            5,
            EventKind::RunEnded {
                run: run.clone(),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
        ),
    ];
    let mut events = initial;
    let p = Projections::rebuild(&events);
    assert!(!p.plan_wake_ready(&p.goals[&goal])); // every orchestrator wake, not just Goal
    let question = Projections::planning_question(&p.goals[&goal]).unwrap();
    events.push(ev(
        6,
        EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::PlanningHold {
                attempt: run.clone(),
                fetch: false,
            }),
            main_ci: None,
            task: None,
            question: question.clone(),
            subject: "Held: planned nothing".into(),
            format: dvb_core::event::QuestionFormat::Simple,
            options: vec![],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(EventId(1)),
        },
    ));
    let p = Projections::rebuild(&events);
    assert!(p.questions[&question].view.open);
    for (kind, releases) in [
        (EventKind::ProjectSeen {}, false),
        (
            EventKind::ProjectBriefSet {
                brief: "New brief".into(),
            },
            false,
        ),
        (
            EventKind::GithubMirrorObserved {
                goal: other.clone(),
                task: None,
                mirrored: true,
                detail: None,
            },
            false,
        ),
        (
            EventKind::RunOutput {
                resource: None,
                run,
                item: dvb_core::event::OutputItem::Note("note".into()),
            },
            false,
        ),
        (
            EventKind::RunOutput {
                resource: None,
                run: RunId::new("builder"),
                item: dvb_core::event::OutputItem::Text("output".into()),
            },
            false,
        ),
        (
            EventKind::RunUsage {
                run: RunId::new("builder"),
                uncached_in: 1,
                cached_in: 2,
                out: 3,
                est_cost: 0.0,
            },
            false,
        ),
        (
            EventKind::RunEnded {
                run: RunId::new("builder"),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
            false,
        ),
        (
            EventKind::QuestionAnswered {
                question: QuestionId::new("unrelated"),
                answer: dvb_core::event::Answer::Choice(1),
            },
            false,
        ),
        (
            EventKind::DecisionRecorded {
                by: RoleId::new("backend"),
                title: "Other work".into(),
                options: None,
                swot: None,
                choice: "Build".into(),
                reason: "Build".into(),
            },
            false,
        ),
        (
            EventKind::RunStarted(
                crate::test_support::assignment::Assignment::new(RunId::new("other-planner"))
                    .goal_version(Some(EventId(2)))
                    .role(RoleId::new("orchestrator"))
                    .reason("A goal was stated.".into())
                    .build(),
            ),
            false,
        ),
        (
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Running,
            },
            false,
        ),
        (
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Stopped {
                    was: Box::new(TaskState::Planned),
                },
            },
            false,
        ),
        (
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Done,
            },
            true,
        ),
        (
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Failed {
                    reason: "failed".into(),
                },
            },
            true,
        ),
        (
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Cancelled,
            },
            true,
        ),
        (
            EventKind::GoalDone {
                goal: other.clone(),
            },
            true,
        ),
        (
            EventKind::GoalDropped {
                goal: other.clone(),
            },
            true,
        ),
        (
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Redirected".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            true,
        ),
        (
            EventKind::QuestionAnswered {
                question: question.clone(),
                answer: dvb_core::event::Answer::Choice(1),
            },
            true,
        ),
    ] {
        let mut scenario = events.clone();
        scenario.push(ev(7, kind.clone()));
        let p = Projections::rebuild(&scenario);
        assert_eq!(p.plan_wake_ready(&p.goals[&goal]), releases, "{kind:?}");
        assert_eq!(p.questions[&question].view.open, !releases, "{kind:?}");
        // The same event outside this project cannot release the hold.
        if !matches!(
            kind,
            EventKind::GoalStated { .. } | EventKind::QuestionAnswered { .. }
        ) {
            scenario.last_mut().unwrap().project = Some(ProjectId::new("elsewhere"));
            let p = Projections::rebuild(&scenario);
            assert!(!p.plan_wake_ready(&p.goals[&goal]), "{kind:?}");
        }
    }
    // Observing the same terminal state again is not progress. Dropping
    // a goal that was already Failed is a distinct allowed boundary.
    let mut already_failed = events.clone();
    already_failed.insert(
        3,
        ev(
            0,
            EventKind::GoalApproved {
                goal: other.clone(),
            },
        ),
    );
    already_failed.insert(
        4,
        ev(
            0,
            EventKind::TaskStateChanged {
                task: task.clone(),
                from: TaskState::Planned,
                to: TaskState::Failed {
                    reason: "failed".into(),
                },
            },
        ),
    );
    for (i, event) in already_failed.iter_mut().enumerate() {
        event.id = EventId(i as u64 + 1);
    }
    let p = Projections::rebuild(&already_failed);
    assert!(!p.plan_wake_ready(&p.goals[&goal]));
    let mut repeated = already_failed.clone();
    repeated.push(ev(
        9,
        EventKind::TaskStateChanged {
            task: task.clone(),
            from: TaskState::Failed {
                reason: "failed".into(),
            },
            to: TaskState::Failed {
                reason: "failed".into(),
            },
        },
    ));
    let p = Projections::rebuild(&repeated);
    assert!(!p.plan_wake_ready(&p.goals[&goal]));
    already_failed.push(ev(
        9,
        EventKind::GoalDropped {
            goal: other.clone(),
        },
    ));
    let p = Projections::rebuild(&already_failed);
    assert!(p.plan_wake_ready(&p.goals[&goal]));
    assert!(!p.questions[&question].view.open);

    // A final goal includes stopped tasks, but a non-final task alone
    // releases nothing. Approving the other goal supplies that distinction.
    let mut scenario = events.clone();
    scenario.push(ev(7, EventKind::GoalApproved { goal: other }));
    scenario.push(ev(
        8,
        EventKind::TaskStateChanged {
            task,
            from: TaskState::Planned,
            to: TaskState::Stopped {
                was: Box::new(TaskState::Planned),
            },
        },
    ));
    let p = Projections::rebuild(&scenario);
    assert!(p.plan_wake_ready(&p.goals[&goal]));
    assert!(!p.questions[&question].view.open);
}

#[test]
fn a_rerouted_run_follows_the_reroute() {
    let started = crate::test_support::assignment::Assignment::new(RunId::new("r"))
        .task(Some(TaskId::new("t")))
        .model("gpt-6.1-sol".into())
        .vendor(Vendor::Codex)
        .effort(Effort::High)
        .reason(String::new())
        .build();
    let p = Projections::rebuild(&[
        ev(1, EventKind::RunStarted(started.clone())),
        ev(
            2,
            EventKind::RunRerouted {
                run: started.run.clone(),
                task: Some(TaskId::new("t")),
                role: started.role.clone(),
                from_model: started.model.clone(),
                to_model: "claude-sonnet-5-5".into(),
                vendor: Vendor::Claude,
                effort: Effort::Medium,
                limited: Some(Vendor::Codex),
                resets_at: None,
            },
        ),
    ]);
    let a = &p.runs[&started.run].assignment;
    assert_eq!(
        (a.model.as_str(), a.vendor, a.effort, &a.hire),
        (
            "claude-sonnet-5-5",
            Vendor::Claude,
            Effort::Medium,
            &started.hire
        )
    );
}

#[test]
fn an_answer_wake_skips_a_dropped_goal() {
    let stated = |id, goal: &str| {
        ev(
            id,
            EventKind::GoalStated {
                goal: dvb_core::GoalId::new(goal),
                text: goal.into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        )
    };
    let open = |id, q: &str, v| {
        ev(
            id,
            EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: dvb_core::QuestionId::new(q),
                subject: q.into(),
                format: dvb_core::event::QuestionFormat::Simple,
                options: vec![],
                recommended: 1,
                lapses_on: LapsesOn::GoalVersion(EventId(v)),
            },
        )
    };
    let answer = |id, q: &str| {
        ev(
            id,
            EventKind::QuestionAnswered {
                question: dvb_core::QuestionId::new(q),
                answer: dvb_core::event::Answer::Reply("x".into()),
            },
        )
    };
    let p = Projections::rebuild(&[
        stated(1, "g1"),
        stated(2, "g2"),
        open(3, "q1", 1),
        open(4, "q2", 2),
        answer(5, "q1"),
        answer(6, "q2"),
        ev(
            7,
            EventKind::GoalDropped {
                goal: dvb_core::GoalId::new("g2"),
            },
        ),
    ]);
    let goal = p.goal_for_wake(
        &ProjectId::new("p"),
        Some(Wake::Answer),
        &dvb_core::ModelPool::default(),
    );
    assert_eq!(goal.map(|g| g.id.as_str()), Some("g1"));
}

#[test]
fn a_reply_to_an_unapproved_plan_is_not_empty_planning_and_old_holds_replay_released() {
    let goal = GoalId::new("g");
    let planner = RunId::new("planner");
    let answerer = RunId::new("answerer");
    let orchestrator = |run: &RunId, reason: &str| {
        EventKind::RunStarted(
            crate::test_support::assignment::Assignment::new(run.clone())
                .goal_version(Some(EventId(1)))
                .role(RoleId::new("orchestrator"))
                .reason(reason.into())
                .build(),
        )
    };
    let by = |run: &RunId, mut e: Event| {
        e.actor = Actor::Run(run.clone());
        e
    };
    let option = |id, text: &str, approves_plan| dvb_core::event::QuestionOption {
        approves_plan,
        ..dvb_core::event::QuestionOption::owner(id, text, "")
    };
    let mut events = vec![
        ev(
            1,
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Add a parser".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        ev(2, orchestrator(&planner, "A goal was stated.")),
        by(
            &planner,
            ev(
                3,
                EventKind::PlanProposed {
                    goal: goal.clone(),
                    tasks: vec![dvb_core::event::PlannedTask {
                        after: vec![],
                        task: TaskId::new("t"),
                        title: "Add a parser".into(),
                        role: RoleId::new("backend"),
                        review: ReviewKind::Code,
                        summary: "One edit".into(),
                        routine: false,
                    }],
                },
            ),
        ),
        by(
            &planner,
            ev(
                4,
                EventKind::QuestionOpened {
                    kind: Some(dvb_core::event::QuestionKind::Ordinary),
                    main_ci: None,
                    task: None,
                    question: QuestionId::new("plan"),
                    subject: "Start this plan?".into(),
                    format: dvb_core::event::QuestionFormat::Simple,
                    options: vec![option(1, "Yes", true), option(2, "Not yet", false)],
                    recommended: 1,
                    lapses_on: LapsesOn::GoalVersion(EventId(1)),
                },
            ),
        ),
        ev(
            5,
            EventKind::RunEnded {
                run: planner.clone(),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
        ),
        ev(
            6,
            EventKind::QuestionAnswered {
                question: QuestionId::new("plan"),
                answer: dvb_core::event::Answer::Choice(2),
            },
        ),
        ev(7, orchestrator(&answerer, "The owner answered a question.")),
        ev(
            8,
            EventKind::RunEnded {
                run: answerer.clone(),
                reason: EndReason::Completed,
                detail: None,
                stderr: None,
            },
        ),
    ];
    let p = Projections::rebuild(&events);
    let g = &p.goals[&goal];
    assert!(!g.planning_held);
    assert!(p.plan_unapproved(g));
    assert!(!p.goal_question_open(g)); // recovery asks about the saved plan
    // Engines before this fix held the goal and asked "planned nothing";
    // replay releases it and closes that question, open or answered.
    let hold = Projections::planning_question(g).unwrap();
    events.push(ev(
        9,
        EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::PlanningHold {
                attempt: answerer,
                fetch: false,
            }),
            main_ci: None,
            task: None,
            question: hold.clone(),
            subject: "Add a parser: planned nothing".into(),
            format: dvb_core::event::QuestionFormat::Simple,
            options: vec![],
            recommended: 1,
            lapses_on: LapsesOn::GoalVersion(EventId(1)),
        },
    ));
    for answered in [false, true] {
        if answered {
            events.push(ev(
                10,
                EventKind::QuestionAnswered {
                    question: hold.clone(),
                    answer: dvb_core::event::Answer::Choice(super::PLAN_AGAIN),
                },
            ));
        }
        let p = Projections::rebuild(&events);
        let g = &p.goals[&goal];
        assert!(!g.planning_held);
        assert!(!p.questions[&hold].view.open);
        assert!(p.plan_unapproved(g));
        assert!(!p.goal_question_open(g));
    }
}

#[test]
fn not_yet_parks_only_an_unapproved_plan_and_a_newer_question_unparks_it() {
    let goal = GoalId::new("g");
    let option =
        |id, approves_plan, retries_task: Option<TaskId>| dvb_core::event::QuestionOption {
            approves_plan,
            retries_task,
            ..dvb_core::event::QuestionOption::owner(id, "", "")
        };
    let ask = |id: u64, question: &str, options| {
        ev(
            id,
            EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: None,
                question: QuestionId::new(question),
                subject: "Start this plan?".into(),
                format: dvb_core::event::QuestionFormat::Simple,
                options,
                recommended: 1,
                lapses_on: LapsesOn::GoalVersion(EventId(1)),
            },
        )
    };
    let answer = |id: u64, question: &str, answer| {
        ev(
            id,
            EventKind::QuestionAnswered {
                question: QuestionId::new(question),
                answer,
            },
        )
    };
    let plan = vec![option(1, true, None), option(2, false, None)];
    let mut events = vec![
        ev(
            1,
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Add a parser".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        ),
        ev(
            2,
            EventKind::PlanProposed {
                goal: goal.clone(),
                tasks: vec![dvb_core::event::PlannedTask {
                    after: vec![],
                    task: TaskId::new("t"),
                    title: "Add a parser".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "One edit".into(),
                    routine: false,
                }],
            },
        ),
        ask(3, "plan", plan.clone()),
        answer(4, "plan", dvb_core::event::Answer::Choice(2)),
    ];
    let p = Projections::rebuild(&events);
    assert!(p.questions[&QuestionId::new("plan")].parks_plan());
    assert_eq!(
        p.plan_parked(&p.goals[&goal]).map(|q| q.view.id.as_str()),
        Some("plan")
    );
    // The resumed copy is open, so nothing is parked until it is answered.
    events.push(ask(5, "again", plan.clone()));
    let p = Projections::rebuild(&events);
    assert!(p.plan_parked(&p.goals[&goal]).is_none());
    // A written reply asks for a new plan: it parks nothing.
    events.push(answer(
        6,
        "again",
        dvb_core::event::Answer::Reply("Split it".into()),
    ));
    let p = Projections::rebuild(&events);
    assert!(!p.questions[&QuestionId::new("again")].parks_plan());
    assert!(p.plan_parked(&p.goals[&goal]).is_none());
    // Once approved, a question offering more tasks or a retry is no plan
    // question: declining it parks nothing and still reaches the orchestrator.
    events.push(ask(7, "last", plan));
    events.push(answer(8, "last", dvb_core::event::Answer::Choice(1)));
    events.push(ev(9, EventKind::GoalApproved { goal }));
    events.push(ask(
        10,
        "later",
        vec![
            option(1, true, None),
            option(2, false, Some(TaskId::new("t"))),
        ],
    ));
    events.push(answer(11, "later", dvb_core::event::Answer::Choice(2)));
    let p = Projections::rebuild(&events);
    let later = &p.questions[&QuestionId::new("later")];
    assert!(!later.plan_question);
    assert!(!later.parks_plan());
}
