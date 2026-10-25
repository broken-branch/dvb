//! A fast run can report before the launcher's start reply is read: the unit is
//! marked active once, by whichever comes first, and the other is a no-op. A
//! call from a run that is not the task's current run is not the task's news.

mod support;
use dvb_core::{Actor, Command, EventKind, ProjectId, RunId, TaskId, TaskState};

#[test]
fn unit_active_is_recorded_once() {
    let (_dir, e) = support::engine::approved();
    let (p, t) = (ProjectId::new("p"), TaskId::new("t"));
    let run = support::assignment::Assignment::new(RunId::new("older"))
        .task(Some(t.clone()))
        .reason("x".into())
        .base(Some("b".into()))
        .build();
    e.decide(
        e.proj(),
        &t,
        Actor::Engine,
        dvb_engine::Decision::new("start", Command::Start(run.clone())),
        |_| Ok(()),
    )
    .unwrap();
    let stale = run.run.clone();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![EventKind::RunStarted(
            support::assignment::Assignment::from_assignment(run)
                .run(RunId::new("r"))
                .build(),
        )])
    })
    .unwrap();
    {
        let proj = e.proj();
        assert_eq!(proj.runs[&stale].assignment.task, Some(t.clone()));
        assert_eq!(proj.runs[&stale].ended, None);
        assert_eq!(proj.tasks[&t].facts.run, Some(RunId::new("r")));
    }
    let before = e.store.last_id().unwrap();
    e.run_command(
        &stale,
        dvb_engine::Decision::new("unit_active", Command::UnitActive),
    )
    .unwrap();
    assert_eq!(e.proj().tasks[&t].view.state, TaskState::Preparing);
    assert_eq!(
        e.store.last_id().unwrap(),
        before,
        "stale run adds no events"
    );
    let r = RunId::new("r");
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_active", Command::UnitActive),
    )
    .unwrap();
    e.run_command(
        &r,
        dvb_engine::Decision::new("unit_active", Command::UnitActive),
    )
    .unwrap();
    let proj = e.proj();
    assert_eq!(proj.tasks[&t].view.state, TaskState::Running);
    drop(proj);
    let events = e.store.after(before).unwrap();
    let activations: Vec<_> = events
        .iter()
        .filter(|ev| {
            matches!(&ev.kind, EventKind::TaskStateChanged {
            task, from: TaskState::Preparing, to: TaskState::Running
        } if task == &t)
        })
        .collect();
    assert_eq!(activations.len(), 1);
    assert_eq!(activations[0].actor, Actor::Run(r));
    let refused = support::events::all(&e)
        .iter()
        .filter(|ev| matches!(ev.kind, EventKind::CommandRefused(_)))
        .count();
    assert_eq!(refused, 0);
}
