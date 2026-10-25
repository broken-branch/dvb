mod support;
use dvb_core::HireId;
use dvb_core::api::RoleFields;
use dvb_core::api::{ChartMove, NewHire, ProjectQuery};
use dvb_core::roles::HireStatus;
use dvb_core::roles::RoleRecord;
use dvb_core::roles::default_records;
use dvb_core::{Actor, EventKind, ProjectId, RefusalCode, RoleId};
use dvb_engine::Engine;
use dvb_engine::{AppState, Fail};

fn setup() -> (tempfile::TempDir, AppState, ProjectId) {
    let (dir, e) = support::engine::engine();
    let id = ProjectId::new("p");
    let mut kinds = vec![support::events::project_event(&id, "project", "local")];
    kinds.extend(
        default_records(|r| RoleId::new(r.slug))
            .into_iter()
            .map(EventKind::RoleAdded),
    );
    e.append_checked(Some(&id), Actor::Owner, |_, _| Ok(kinds))
        .unwrap();
    (dir, e, id)
}
fn body(role: &RoleRecord) -> RoleFields {
    RoleFields {
        name: role.name.clone(),
        job: role.job.clone(),
        lane: role.lane.clone(),
        lead: role.lead,
        reports_to: role.reports_to.clone(),
        reviewed_by: role.reviewed_by.clone(),
        review_instructions: role.review_instructions.clone(),
        group: role.group.clone(),
        order: role.order,
    }
}

#[tokio::test]
async fn changes_preserve_routing_identity_and_reject_cycles_or_foreign_targets() {
    let (_dir, e, p) = setup();
    let roles = e.proj().projects[&p].roles.clone();
    let backend = roles.iter().find(|r| r.slug == "backend").unwrap();
    let mut patch = body(backend);
    patch.name = "Builder".into();
    patch.reviewed_by = Some(RoleId::new("design-reviewer"));
    let changed = support::http::owner::<RoleRecord>(
        &e,
        axum::http::Method::PATCH,
        &format!("/roles/{}", backend.id.clone()),
        Some(serde_json::to_value(patch).unwrap()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(changed.slug, "backend");
    assert_eq!(changed.reports_to, backend.reports_to);
    assert_eq!(changed.reviewed_by, Some(RoleId::new("design-reviewer")));
    let mut cycle = body(&roles[0]);
    cycle.reports_to = Some(backend.id.clone());
    assert!(
        support::http::owner::<RoleRecord>(
            &e,
            axum::http::Method::PATCH,
            &format!("/roles/{}", roles[0].id.clone()),
            Some(serde_json::to_value(cycle).unwrap())
        )
        .await
        .is_err()
    );
    let mut foreign = body(&changed);
    foreign.reviewed_by = Some(RoleId::new("other-project-reviewer"));
    assert!(
        support::http::owner::<RoleRecord>(
            &e,
            axum::http::Method::PATCH,
            &format!("/roles/{}", changed.id),
            Some(serde_json::to_value(foreign).unwrap())
        )
        .await
        .is_err()
    );
    assert_eq!(e.proj().projects[&p].roles.clone()[0].reports_to, None);
}

#[tokio::test]
async fn new_hiring_replaces_every_active_hire_and_survives_replay() {
    let (dir, e, project) = setup();
    let request = |model: &str| NewHire {
        role: RoleId::new("backend"),
        model: model.into(),
        effort: dvb_core::Effort::High,
        routine: false,
    };
    let first = support::http::owner::<dvb_core::api::HireCreated>(
        &e,
        axum::http::Method::POST,
        "/hires",
        Some(serde_json::to_value(request("fake-1")).unwrap()),
    )
    .await
    .map(|body| body.0)
    .unwrap();
    assert!(first.retired.is_empty());
    let mut duplicate = first.hire.clone();
    duplicate.id = HireId::new("legacy");
    e.append_checked(Some(&project), Actor::Owner, |_, _| {
        Ok(vec![EventKind::HireAdded(duplicate.clone())])
    })
    .unwrap();
    let before = support::events::all(&e);
    let reopened = support::engine::open(dir.path());
    let replayed = support::events::all(&reopened);
    assert_eq!(&replayed[..before.len()], before.as_slice());
    assert!(
        replayed[before.len()..]
            .iter()
            .all(|ev| matches!(ev.kind, EventKind::EngineStarted { .. }))
    );
    assert_eq!(
        reopened.proj().projects[&project]
            .hires
            .iter()
            .filter(|h| h.status == HireStatus::Active)
            .count(),
        2
    );
    let replacement_after = e.store.last_id().unwrap();
    let replaced = support::http::owner::<dvb_core::api::HireCreated>(
        &e,
        axum::http::Method::POST,
        "/hires",
        Some(serde_json::to_value(request("fake-next")).unwrap()),
    )
    .await
    .map(|body| body.0)
    .unwrap();
    assert_eq!(
        replaced.retired.iter().map(|h| &h.id).collect::<Vec<_>>(),
        vec![&first.hire.id, &duplicate.id]
    );
    assert!(
        replaced
            .retired
            .iter()
            .all(|h| h.status == HireStatus::Retired)
    );
    assert_eq!(
        e.proj().projects[&project].hire(&request("fake-next").role, None),
        Some(&replaced.hire)
    );
    let events = e.store.after(replacement_after).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].at, events[2].at);
    assert!(matches!(&events[2].kind, EventKind::HireAdded(h) if h == &replaced.hire));
    assert!(
        support::http::owner::<dvb_core::api::HireCreated>(
            &e,
            axum::http::Method::POST,
            "/hires",
            Some(serde_json::to_value(request("missing")).unwrap())
        )
        .await
        .map(|body| body.0)
        .is_err()
    );
    assert_eq!(e.store.last_id().unwrap(), events[2].id);
    drop(reopened);
    let reopened = support::engine::open(dir.path());
    assert_eq!(
        reopened.proj().projects[&project].hires,
        e.proj().projects[&project].hires
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_owner_hires_leave_one_active_record() {
    let (_dir, e, project) = setup();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        for model in ["fake-1", "fake-next"] {
            let e = e.clone();
            let runtime = tokio::runtime::Handle::current();
            let gate = gate.clone();
            scope.spawn(move || {
                gate.wait();
                let _ = runtime
                    .block_on(support::http::owner::<dvb_core::api::HireCreated>(
                        &e,
                        axum::http::Method::POST,
                        "/hires",
                        Some(
                            serde_json::to_value(NewHire {
                                role: RoleId::new("backend"),
                                model: model.into(),
                                effort: dvb_core::Effort::Medium,
                                routine: false,
                            })
                            .unwrap(),
                        ),
                    ))
                    .unwrap();
            });
        }
        gate.wait();
    });
    let proj = e.proj();
    assert_eq!(
        proj.projects[&project]
            .hires
            .iter()
            .filter(|h| h.status == HireStatus::Active)
            .count(),
        1
    );
    assert_eq!(
        proj.projects[&project]
            .hires
            .iter()
            .filter(|h| h.status == HireStatus::Retired)
            .count(),
        1
    );
}

#[tokio::test]
async fn creates_roles_and_validates_hires_and_retirement() {
    let (_dir, e, p) = setup();
    let mut fields = body(&e.proj().projects[&p].roles.clone()[1]);
    fields.name = "Documentation".into();
    let role = support::http::owner::<RoleRecord>(
        &e,
        axum::http::Method::POST,
        &format!("/projects/{}/roles", p),
        Some(serde_json::to_value(fields).unwrap()),
    )
    .await
    .unwrap()
    .0;
    let request = NewHire {
        role: role.id.clone(),
        model: "fake-1".into(),
        effort: dvb_core::Effort::Medium,
        routine: false,
    };
    let hire = support::http::owner::<dvb_core::api::HireCreated>(
        &e,
        axum::http::Method::POST,
        "/hires",
        Some(serde_json::to_value(request.clone()).unwrap()),
    )
    .await
    .unwrap()
    .0;
    let mut bad = request;
    bad.model = "unavailable".into();
    assert!(
        matches!(support::http::owner::<dvb_core::api::HireCreated>(&e, axum::http::Method::POST, "/hires", Some(serde_json::to_value(bad).unwrap())).await, Err(Fail::Refused(r)) if r.code == RefusalCode::ModelNotInPool)
    );
    assert_eq!(
        support::http::owner::<dvb_core::roles::HireRecord>(
            &e,
            axum::http::Method::POST,
            &format!("/hires/{}/retire", hire.hire.id.clone()),
            None
        )
        .await
        .unwrap()
        .0
        .status,
        HireStatus::Retired
    );
    let before = e.store.last_id().unwrap();
    let _ = support::http::owner::<dvb_core::roles::HireRecord>(
        &e,
        axum::http::Method::POST,
        &format!("/hires/{}/retire", hire.hire.id),
        None,
    )
    .await
    .unwrap();
    assert_eq!(e.store.last_id().unwrap(), before);
    assert_eq!(
        support::http::owner::<Vec<dvb_core::roles::HireRecord>>(
            &e,
            axum::http::Method::GET,
            &format!(
                "/hires{}",
                (ProjectQuery::default())
                    .project
                    .map(|id| format!("?project={id}"))
                    .unwrap_or_default()
            ),
            None
        )
        .await
        .unwrap()
        .0
        .len(),
        1
    );
}
#[tokio::test]
async fn chart_moves_are_atomic_preserve_review_assignments_and_survive_restart() {
    let (_dir, e, p) = setup();
    let original = e.proj().projects[&p].roles.clone();
    let root = RoleId::new("orchestrator");
    let builder = RoleId::new("backend");
    let changed = support::http::owner::<Vec<RoleRecord>>(
        &e,
        axum::http::Method::POST,
        &format!("/projects/{}/chart", p.clone()),
        Some(
            serde_json::to_value(ChartMove::Swap {
                role: builder.clone(),
                lead: root.clone(),
            })
            .unwrap(),
        ),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        changed.iter().find(|r| r.id == builder).unwrap().reports_to,
        None
    );
    assert_eq!(
        changed.iter().find(|r| r.id == root).unwrap().reports_to,
        Some(builder.clone())
    );
    assert!(
        changed
            .iter()
            .filter(|r| r.id != builder && r.id != root)
            .all(|r| r.reports_to == Some(builder.clone()))
    );
    assert!(
        changed
            .iter()
            .zip(&original)
            .all(|(a, b)| a.reviewed_by == b.reviewed_by && a.slug == b.slug)
    );
    let before = e.store.last_id().unwrap();
    assert!(
        support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Report {
                    role: builder.clone(),
                    parent: Some(root.clone())
                })
                .unwrap()
            )
        )
        .await
        .is_err()
    );
    assert!(
        support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Group {
                    roles: vec![builder.clone(), root.clone()],
                    group: Some("invalid".into())
                })
                .unwrap()
            )
        )
        .await
        .is_err()
    );
    assert!(
        support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Swap {
                    role: root.clone(),
                    lead: RoleId::new("foreign")
                })
                .unwrap()
            )
        )
        .await
        .is_err()
    );
    assert_eq!(e.store.last_id().unwrap(), before);
    let group = vec![RoleId::new("frontend"), RoleId::new("architect")];
    let _ = support::http::owner::<Vec<RoleRecord>>(
        &e,
        axum::http::Method::POST,
        &format!("/projects/{}/chart", p.clone()),
        Some(
            serde_json::to_value(ChartMove::Group {
                roles: group.clone(),
                group: Some("Product".into()),
            })
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let _ = support::http::owner::<Vec<RoleRecord>>(
        &e,
        axum::http::Method::POST,
        &format!("/projects/{}/chart", p.clone()),
        Some(
            serde_json::to_value(ChartMove::Reorder {
                role: group[0].clone(),
                before: Some(group[1].clone()),
            })
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let roles = e.proj().projects[&p].roles.clone();
    let front = roles.iter().find(|r| r.id == group[0]).unwrap();
    let arch = roles.iter().find(|r| r.id == group[1]).unwrap();
    assert_eq!(front.group.as_deref(), Some("Product"));
    assert!(front.order < arch.order);
    let reopened = Engine::open(e.cfg.clone(), Default::default()).unwrap();
    assert_eq!(reopened.proj().projects[&p].roles.clone(), roles);
}

#[tokio::test]
async fn swaps_round_trip_and_reorder_across_groups_persists_including_append() {
    let (_dir, e, p) = setup();
    let original = e.proj().projects[&p].roles.clone();
    for (role, lead) in [("backend", "orchestrator"), ("orchestrator", "backend")] {
        let _ = support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Swap {
                    role: RoleId::new(role),
                    lead: RoleId::new(lead),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(e.proj().projects[&p].roles.clone(), original);
    for (role, group) in [("backend", "Zebra"), ("frontend", "Alpha")] {
        let _ = support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Group {
                    roles: vec![RoleId::new(role)],
                    group: Some(group.into()),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    }
    for before in [Some(RoleId::new("frontend")), None] {
        let roles = support::http::owner::<Vec<RoleRecord>>(
            &e,
            axum::http::Method::POST,
            &format!("/projects/{}/chart", p.clone()),
            Some(
                serde_json::to_value(ChartMove::Reorder {
                    role: RoleId::new("backend"),
                    before: before.clone(),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        let mut siblings = roles
            .iter()
            .filter(|r| r.reports_to == Some(RoleId::new("orchestrator")))
            .collect::<Vec<_>>();
        siblings.sort_by_key(|r| r.order);
        let i = siblings.iter().position(|r| r.slug == "backend").unwrap();
        if before.is_some() {
            assert_eq!(siblings[i + 1].slug, "frontend");
        } else {
            assert_eq!(i, siblings.len() - 1);
        }
        assert_eq!(siblings[i].group.as_deref(), Some("Zebra"));
        let reopened = Engine::open(e.cfg.clone(), Default::default()).unwrap();
        assert_eq!(reopened.proj().projects[&p].roles.clone(), roles);
    }
}
#[tokio::test]
async fn goal_snapshot_reads_current_text_state_and_project_filter() {
    let (_dir, e, p) = setup();
    let goal = dvb_core::GoalId::new("g");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "First".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Changed".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::GoalApproved { goal: goal.clone() },
            EventKind::GoalDone { goal },
        ])
    })
    .unwrap();
    let goals = support::http::owner::<Vec<dvb_core::api::GoalView>>(
        &e,
        axum::http::Method::GET,
        &format!(
            "/goals{}",
            (ProjectQuery { project: Some(p) })
                .project
                .map(|id| format!("?project={id}"))
                .unwrap_or_default()
        ),
        None,
    )
    .await
    .unwrap()
    .0;
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].text, "Changed");
    assert_eq!(goals[0].state, dvb_core::api::GoalState::Done);
    assert!(
        support::http::owner::<Vec<dvb_core::api::GoalView>>(
            &e,
            axum::http::Method::GET,
            &format!(
                "/goals{}",
                (ProjectQuery {
                    project: Some(ProjectId::new("other"))
                })
                .project
                .map(|id| format!("?project={id}"))
                .unwrap_or_default()
            ),
            None
        )
        .await
        .unwrap()
        .0
        .is_empty()
    );
}
