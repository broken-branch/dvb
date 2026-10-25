//! The API and CLI use the engine's default hire under pool and timed model selection.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::api::{HireView, SetOnly, Settings};
use dvb_core::event::CiMode;
use dvb_core::roles::{HireRecord, HireStatus, default_records};
use dvb_core::{Actor, Effort, EventKind, FakeClock, HireId, ProjectId, RoleId, Timestamp};
use dvb_engine::{Config, Engine, EngineOptions};
use std::process::Command;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn socket_default_hires_agree_for_pool_selection_only_expiry_and_retired_records() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let e = Engine::open(
        Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
        EngineOptions {
            clock: Some(clock.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let roles = default_records(|role| RoleId::new(role.slug));
    let mut seed = vec![
        EventKind::ProjectAdded {
            id: ProjectId::new("p"),
            name: "p".into(),
            repo: "/unused".into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        },
        EventKind::ProjectPaused {},
    ];
    seed.extend(roles.iter().cloned().map(EventKind::RoleAdded));
    for role in &roles {
        for (suffix, model, status) in [
            ("missing", "outside-pool", HireStatus::Active),
            ("retired", "fake-next", HireStatus::Retired),
            ("first", "fake-1", HireStatus::Active),
            ("second", "fake-next", HireStatus::Active),
        ] {
            seed.push(EventKind::HireAdded(HireRecord {
                id: HireId::new(format!("{}-{suffix}", role.id)),
                role: role.id.clone(),
                model: model.into(),
                effort: Effort::Medium,
                status,
                routine: false,
            }));
        }
    }
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(seed))
        .unwrap();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(tmp.path().join("engine.sock"));
    support::engine_up(&c).await;

    for (selected, only, expected) in [
        (vec!["fake-1", "fake-next"], false, "first"),
        (vec!["fake-next"], false, "second"),
        (vec!["fake-next"], true, "first"),
    ] {
        c.put::<_, Settings>(
            "/settings",
            &serde_json::json!({"values":{"model_pool":selected}}),
        )
        .await
        .unwrap();
        if only {
            c.put::<_, dvb_core::model::OnlyModels>(
                "/only",
                &SetOnly {
                    models: vec!["fake-next".into()],
                    hours: 1,
                },
            )
            .await
            .unwrap();
        }
        assert_defaults(&c, tmp.path(), Some(expected)).await;
    }
    // No /only read occurs between advancing the clock and reading /hires.
    clock.advance_millis(3_600_000);
    assert_defaults(&c, tmp.path(), Some("second")).await;
    assert!(e.proj().only.is_none());
    c.put::<_, dvb_core::model::OnlyModels>(
        "/only",
        &SetOnly {
            models: vec!["fake-next".into()],
            hours: 1,
        },
    )
    .await
    .unwrap();
    c.post::<_, serde_json::Value>("/only/clear", &())
        .await
        .unwrap();
    assert_defaults(&c, tmp.path(), Some("second")).await;

    let unselected = e.proj().projects[&ProjectId::new("p")]
        .hires
        .iter()
        .filter(|h| h.status == HireStatus::Active && h.model == "fake-next")
        .cloned()
        .map(|mut h| {
            h.status = HireStatus::Retired;
            EventKind::HireRetired(h)
        })
        .collect();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(unselected)
    })
    .unwrap();
    assert_defaults(&c, tmp.path(), None).await;

    let hires = e.proj().projects[&ProjectId::new("p")].hires.clone();
    let retired = hires
        .into_iter()
        .filter(|h| h.status == HireStatus::Active)
        .map(|mut h| {
            h.status = HireStatus::Retired;
            EventKind::HireRetired(h)
        })
        .collect();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(retired))
        .unwrap();
    let views: Vec<HireView> = c.get("/hires?project=p").await.unwrap();
    assert!(views.iter().all(|h| h.default_hire.is_none()));
    serving.abort();
}

async fn assert_defaults(c: &Client, dir: &std::path::Path, suffix: Option<&str>) {
    let views: Vec<HireView> = c.get("/hires?project=p").await.unwrap();
    let roles: Vec<dvb_core::roles::RoleRecord> = c.get("/projects/p/roles").await.unwrap();
    for all in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dvb"));
        command.arg("--dev").arg(dir).args(["hires", "p"]);
        if all {
            command.arg("--all");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let mut expected = Vec::new();
        for role in &roles {
            let default = suffix.map(|suffix| HireId::new(format!("{}-{suffix}", role.id)));
            for view in views.iter().filter(|h| h.hire.role == role.id) {
                assert_eq!(view.default_hire.as_ref(), default.as_ref());
                let hire = &view.hire;
                if !all && hire.status == HireStatus::Retired {
                    continue;
                }
                let marker = if hire.status == HireStatus::Retired {
                    "  [retired]"
                } else if Some(&hire.id) == default.as_ref() {
                    "  [default]"
                } else {
                    ""
                };
                expected.push(format!(
                    "{}  {}  {}  {}  {}{}",
                    role.name,
                    role.id,
                    hire.id,
                    hire.model,
                    hire.effort.as_str(),
                    marker
                ));
            }
        }
        assert_eq!(text, expected.join("\n") + "\n");
    }
}

/// `dvb hire --routine` adds the role's routine hire beside its default hire
/// and replaces only the earlier routine hire; a default hire replaces only
/// the default. The default hire stays the role's default.
#[tokio::test(flavor = "multi_thread")]
async fn socket_a_routine_hire_replaces_only_the_roles_routine_hire() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Engine::open(
        Config::dev(tmp.path(), env!("CARGO_BIN_EXE_dvb").into()),
        EngineOptions::default(),
    )
    .unwrap();
    let backend = RoleId::new("backend");
    let mut seed = vec![EventKind::ProjectAdded {
        id: ProjectId::new("p"),
        name: "p".into(),
        repo: "/unused".into(),
        default_branch: "main".into(),
        ci: CiMode::NoCi,
    }];
    seed.extend(
        default_records(|role| RoleId::new(role.slug))
            .into_iter()
            .map(EventKind::RoleAdded),
    );
    seed.push(EventKind::HireAdded(HireRecord {
        id: HireId::new("opus"),
        role: backend.clone(),
        model: "fake-1".into(),
        effort: Effort::High,
        status: HireStatus::Active,
        routine: false,
    }));
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(seed))
        .unwrap();
    let serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(tmp.path().join("engine.sock"));
    support::engine_up(&c).await;
    let dvb = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(tmp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let active = || {
        let mut hires: Vec<_> = e.proj().projects[&ProjectId::new("p")]
            .hires
            .iter()
            .filter(|h| h.role == backend && h.status == HireStatus::Active)
            .map(|h| (h.model.clone(), h.routine))
            .collect();
        hires.sort();
        hires
    };

    let hired = dvb(&["hire", "backend", "fake-next", "--routine"]);
    assert!(hired.starts_with("Hired fake-next ("), "{hired}");
    assert!(hired.ends_with(") as routine\n"), "{hired}");
    assert_eq!(
        active(),
        [("fake-1".into(), false), ("fake-next".into(), true)]
    );
    let listed = dvb(&["hires", "p"]);
    assert!(
        listed.contains("opus  fake-1  high  [default]\n"),
        "{listed}"
    );
    assert!(
        listed.contains("fake-next  medium  [routine]\n"),
        "{listed}"
    );
    let views: Vec<HireView> = c.get("/hires?project=p").await.unwrap();
    assert!(
        views
            .iter()
            .all(|v| v.default_hire == Some(HireId::new("opus")))
    );

    let replaced = dvb(&["hire", "backend", "fake-1", "--routine"]);
    assert!(replaced.contains("Retired fake-next ("), "{replaced}");
    assert_eq!(
        active(),
        [("fake-1".into(), false), ("fake-1".into(), true)]
    );
    let default = dvb(&["hire", "backend", "fake-next"]);
    assert!(default.contains("Retired fake-1 (opus)"), "{default}");
    assert_eq!(
        active(),
        [("fake-1".into(), true), ("fake-next".into(), false)]
    );
    serving.abort();
}
