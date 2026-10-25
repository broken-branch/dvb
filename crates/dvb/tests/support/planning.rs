//! Fake-worker project setup shared by planning and admission socket tests.
use dvb_core::event::CiMode;
use dvb_core::roles::{HireRecord, HireStatus, default_records};
use dvb_core::{Actor, Effort, EventKind, HireId, ProjectId, RoleId};
use dvb_engine::{AppState, Config, Engine, publish};

pub async fn fixture(
    prefix: &str,
    backend_hired: bool,
    actor: Actor,
    configure: impl FnOnce(&mut AppState),
    extra_events: Vec<EventKind>,
) -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    let origin = super::seed_origin(tmp.path(), "main");
    let mut e = Engine::open(
        Config::dev(&tmp.path().join("e"), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    configure(&mut e);
    publish::create_mirror(&origin.to_string_lossy(), &e.mirror("project"))
        .await
        .unwrap();
    let mut events = vec![EventKind::ProjectAdded {
        id: ProjectId::new("p"),
        name: "project".into(),
        repo: origin.to_string_lossy().into_owned(),
        default_branch: "main".into(),
        ci: CiMode::NoCi,
    }];
    for role in default_records(|r| RoleId::new(r.slug)) {
        if role.slug != "backend" || backend_hired {
            events.push(EventKind::HireAdded(HireRecord {
                id: HireId::new(format!("h-{}", role.slug)),
                role: role.id.clone(),
                model: "fake-1".into(),
                effort: Effort::Medium,
                status: HireStatus::Active,
                routine: false,
            }));
        }
        events.push(EventKind::RoleAdded(role));
    }
    events.extend(extra_events);
    e.append_checked(Some(&ProjectId::new("p")), actor, |_, _| Ok(events))
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("e/fake")).unwrap();
    (tmp, e)
}

pub fn script(tmp: &tempfile::TempDir, name: &str, steps: &serde_json::Value) {
    std::fs::write(
        tmp.path().join(format!("e/fake/{name}.json")),
        steps.to_string(),
    )
    .unwrap();
}
