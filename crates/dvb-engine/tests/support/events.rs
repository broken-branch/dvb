use dvb_core::event::{CiMode, PlannedTask};
use dvb_core::roles::{HireRecord, HireStatus, ReviewKind, default_records};
use dvb_core::{Effort, Event, EventId, EventKind, GoalId, HireId, ProjectId, RoleId, TaskId};

/// The engine's whole event log.
pub fn all(e: &dvb_engine::Engine) -> Vec<Event> {
    e.store.after(EventId(0)).unwrap()
}

pub fn project_event(id: &ProjectId, name: &str, repo: &str) -> EventKind {
    project_event_with_ci(id, name, repo, CiMode::NoCi)
}

pub fn project_event_with_ci(id: &ProjectId, name: &str, repo: &str, ci: CiMode) -> EventKind {
    EventKind::ProjectAdded {
        id: id.clone(),
        name: name.into(),
        repo: repo.into(),
        default_branch: "main".into(),
        ci,
    }
}

pub fn goal_event(goal: &GoalId, text: &str, from_issue: Option<u64>) -> EventKind {
    EventKind::GoalStated {
        goal: goal.clone(),
        text: text.into(),
        from_issue,
        issues: Vec::new(),
        after: None,
    }
}

pub fn backend_plan_event(goal: &GoalId, task: &TaskId) -> EventKind {
    EventKind::PlanProposed {
        goal: goal.clone(),
        tasks: vec![PlannedTask {
            after: vec![],
            task: task.clone(),
            title: "x".into(),
            role: RoleId::new("backend"),
            review: ReviewKind::Code,
            summary: "x".into(),
            routine: false,
        }],
    }
}

pub fn approved_task_events() -> Vec<EventKind> {
    let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
    vec![
        project_event(&p, "p", "/nowhere"),
        goal_event(&g, "x", None),
        backend_plan_event(&g, &t),
        EventKind::GoalApproved { goal: g },
    ]
}

/// Separate lists let callers preserve role/hire event ordering.
pub fn default_role_hire_events() -> (Vec<EventKind>, Vec<EventKind>) {
    let roles = default_records(|d| RoleId::new(d.slug));
    let hires = roles
        .iter()
        .map(|r| {
            EventKind::HireAdded(HireRecord {
                id: HireId::new(format!("h-{}", r.slug)),
                role: r.id.clone(),
                model: "fake-1".into(),
                effort: Effort::Medium,
                status: HireStatus::Active,
                routine: false,
            })
        })
        .collect();
    (roles.into_iter().map(EventKind::RoleAdded).collect(), hires)
}
