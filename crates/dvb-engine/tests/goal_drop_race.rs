//! A drop and a redirect racing for the same goal: a goal that was dropped is
//! never stated again.
use dvb_engine::owner;
mod support;
use dvb_core::api::GoalState;
use dvb_core::{EventKind, GoalId, ProjectId};

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_racing_a_drop_never_resurrects_the_goal() {
    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(dir.path(), "unused-dvb", Default::default());
    let p = ProjectId::new("p");
    let mut events = vec![
        support::events::project_event(&p, "project", "/nowhere"),
        EventKind::ProjectPaused {},
    ];
    let goals: Vec<GoalId> = (0..40).map(|i| GoalId::new(format!("g{i}"))).collect();
    for g in &goals {
        events.push(support::events::goal_event(g, "Original", None));
    }
    e.append_checked(Some(&p), dvb_core::Actor::Owner, |_, _| Ok(events))
        .unwrap();

    for g in goals {
        let (e1, g1) = (e.clone(), g.clone());
        let (e2, g2) = (e.clone(), g.clone());
        let r = tokio::task::spawn_blocking(move || owner::redirect(&e1, &g1, "New".into()));
        let d = tokio::task::spawn_blocking(move || owner::drop_goal(&e2, &g2));
        let (_, d) = (r.await.unwrap(), d.await.unwrap());
        d.unwrap();
        assert_eq!(e.proj().goals[&g].state, GoalState::Dropped, "{g}");
    }
}
