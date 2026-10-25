//! The record routes on the Fake path: history from the events (newest
//! first), restore, the bug report preview and the login status.

mod support;

use axum::http::{Method, StatusCode};
use dvb_core::{Actor, EventKind, ProjectId};
use dvb_engine::AppState;
use serde_json::{Value, json};
use support::events::{default_role_hire_events, project_event};
use support::http::{call, request};

fn engine(options: dvb_engine::EngineOptions) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(dir.path(), "dvb", options);
    let p = ProjectId::new("p");
    let (roles, hires) = default_role_hire_events();
    let mut kinds = vec![project_event(&p, "p", "/nowhere")];
    kinds.extend(roles);
    kinds.extend(hires);
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(kinds))
        .unwrap();
    (dir, e)
}

fn event_count(e: &AppState) -> u64 {
    e.store.last_id().unwrap().0
}

#[tokio::test]
async fn role_history_is_newest_first_and_restore_appends_a_restored_change() {
    let (_dir, e) = engine(Default::default());
    let (_, roles) = call(&e, Method::GET, "/projects/p/roles", None).await;
    let role = roles[0].clone();
    let id = role["id"].as_str().unwrap().to_owned();
    let mut body = role.clone();
    body["job"] = json!("A new job");
    let (s, _) = call(&e, Method::PATCH, &format!("/roles/{id}"), Some(body)).await;
    assert_eq!(s, StatusCode::OK);

    let (s, h) = call(&e, Method::GET, &format!("/roles/{id}/history"), None).await;
    assert_eq!(s, StatusCode::OK);
    let versions = h["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["value"]["job"], "A new job");
    assert_eq!(versions[1]["value"]["job"], role["job"]);
    let (newest, oldest) = (versions[0]["event"].clone(), versions[1]["event"].clone());

    // A stale `current` is refused, and nothing is appended but the refusal record.
    let (s, r) = call(
        &e,
        Method::POST,
        &format!("/roles/{id}/restore"),
        Some(json!({"event": oldest, "current": oldest})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "VALUE_CHANGED");
    let (_, again) = call(&e, Method::GET, &format!("/roles/{id}/history"), None).await;
    assert_eq!(again["versions"].as_array().unwrap().len(), 2);

    let (s, restored) = call(
        &e,
        Method::POST,
        &format!("/roles/{id}/restore"),
        Some(json!({"event": oldest, "current": newest})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{restored}");
    assert_eq!(restored["job"], role["job"]);
    let events = support::events::all(&e);
    let last = events.last().unwrap();
    assert_eq!(last.actor, Actor::Owner);
    let EventKind::RoleChanged { restored_from, .. } = &last.kind else {
        panic!("{:?}", last.kind);
    };
    assert_eq!(restored_from.map(|i| json!(i.0)), Some(oldest));
    let (_, h) = call(&e, Method::GET, &format!("/roles/{id}/history"), None).await;
    assert_eq!(h["versions"].as_array().unwrap().len(), 3);
    assert_eq!(h["versions"][0]["value"]["job"], role["job"]);
    let (s, body) = request(&e, Method::GET, "/roles/nope/history", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"code":"NOT_FOUND", "detail":"role nope"})
    );
}

#[tokio::test]
async fn hire_history_and_restore_bring_back_an_active_hire() {
    let (_dir, e) = engine(Default::default());
    let (_, hires) = call(&e, Method::GET, "/hires", None).await;
    let id = hires[0]["id"].as_str().unwrap().to_owned();
    let role = dvb_core::RoleId::new(hires[0]["role"].as_str().unwrap());
    let (s, receipt) = call(
        &e,
        Method::POST,
        "/hires",
        Some(json!({
            "role": hires[0]["role"], "model": "fake-next", "effort": "high"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(receipt["retired"][0]["id"], id);
    assert_eq!(receipt["retired"][0]["status"], "retired");

    let (_, h) = call(&e, Method::GET, &format!("/hires/{id}/history"), None).await;
    let versions = h["versions"].as_array().unwrap();
    assert_eq!(versions[0]["value"]["status"], "retired");
    assert_eq!(versions[1]["value"]["status"], "active");
    let (newest, oldest) = (versions[0]["event"].clone(), versions[1]["event"].clone());

    let (s, r) = call(
        &e,
        Method::POST,
        &format!("/hires/{id}/restore"),
        Some(json!({"event": oldest, "current": oldest})),
    )
    .await;
    assert_eq!(
        (s, r["code"].as_str()),
        (StatusCode::CONFLICT, Some("VALUE_CHANGED"))
    );
    let (s, r) = call(
        &e,
        Method::POST,
        &format!("/hires/{id}/restore"),
        Some(json!({"event": oldest, "current": newest})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["status"], "active");
    let (_, hires) = call(&e, Method::GET, "/hires", None).await;
    assert!(
        hires
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == id && h["status"] == "active")
    );
    assert!(
        hires
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == receipt["id"] && h["status"] == "retired")
    );
    let proj = e.proj();
    let project = &proj.projects[&ProjectId::new("p")];
    assert_eq!(project.hire(&role, None).unwrap().id.as_str(), id);
    assert_eq!(
        project
            .hires
            .iter()
            .filter(|h| h.role == role && h.status == dvb_core::roles::HireStatus::Active)
            .count(),
        1
    );
    drop(proj);
}

#[tokio::test]
async fn hire_restore_validates_the_pool_and_effort_before_retiring_anyone() {
    for (model, effort, selected) in [
        ("unavailable", "medium", None),
        ("fake-1", "low", None),
        ("fake-1", "medium", Some(json!(["fake-next"]))),
    ] {
        let (_dir, e) = engine(Default::default());
        let original = e.proj().projects[&ProjectId::new("p")].hires[0].clone();
        let mut old = original.clone();
        old.model = model.into();
        old.effort = serde_json::from_value(json!(effort)).unwrap();
        let version = e
            .append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
                Ok(vec![EventKind::HireChanged {
                    hire: old,
                    restored_from: None,
                }])
            })
            .unwrap()[0]
            .id;
        let current = e
            .append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
                Ok(vec![EventKind::HireChanged {
                    hire: original.clone(),
                    restored_from: None,
                }])
            })
            .unwrap()[0]
            .id;
        if let Some(selected) = selected {
            let (status, _) = call(
                &e,
                Method::PUT,
                "/settings",
                Some(json!({"values": {"model_pool": selected}})),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let before = e.store.last_id().unwrap();
        let (status, body) = call(
            &e,
            Method::POST,
            &format!("/hires/{}/restore", original.id),
            Some(json!({"event": version.0, "current": current.0})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "MODEL_NOT_IN_POOL");
        let events = e.store.after(before).unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0].kind, EventKind::CommandRefused(r) if r.command == "hire.restore")
        );
        assert_eq!(
            e.proj().projects[&ProjectId::new("p")].hire(&original.role, None),
            Some(&original)
        );
    }
}

#[tokio::test]
async fn hire_restore_allows_a_retired_version_outside_the_selected_pool() {
    let (_dir, e) = engine(Default::default());
    let hire = e.proj().projects[&ProjectId::new("p")].hires[0].clone();
    assert_eq!(hire.model, "fake-1");
    let path = format!("/hires/{}", hire.id);
    let (_, history) = call(&e, Method::GET, &format!("{path}/history"), None).await;
    let added = history["versions"][0]["event"].clone();
    let (status, body) = call(&e, Method::POST, &format!("{path}/retire"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let retired = e.store.last_id().unwrap();
    let (status, body) = call(
        &e,
        Method::POST,
        &format!("{path}/restore"),
        Some(json!({"event": added, "current": retired.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "active");
    let current = e.store.last_id().unwrap();
    let (status, body) = call(
        &e,
        Method::PUT,
        "/settings",
        Some(json!({"values": {"model_pool": ["fake-next"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = e.store.last_id().unwrap();
    let (status, body) = call(
        &e,
        Method::POST,
        &format!("{path}/restore"),
        Some(json!({"event": retired.0, "current": current.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "retired");
    let events = e.store.after(before).unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].kind, EventKind::HireChanged { hire: restored, restored_from }
        if restored.id == hire.id && restored.status == dvb_core::roles::HireStatus::Retired
            && *restored_from == Some(retired))
    );
}

#[tokio::test]
async fn hire_restore_of_an_already_active_hire_does_not_retire_it() {
    let (_dir, e) = engine(Default::default());
    let hire = e.proj().projects[&ProjectId::new("p")].hires[0].clone();
    let path = format!("/hires/{}", hire.id);
    let (_, history) = call(&e, Method::GET, &format!("{path}/history"), None).await;
    let added = history["versions"][0]["event"].clone();
    let (status, body) = call(&e, Method::POST, &format!("{path}/retire"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let retired = e.store.last_id().unwrap();
    let (status, body) = call(
        &e,
        Method::POST,
        &format!("{path}/restore"),
        Some(json!({"event": added, "current": retired.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "active");
    let before = e.store.last_id().unwrap();
    let (status, body) = call(
        &e,
        Method::POST,
        &format!("{path}/restore"),
        Some(json!({"event": added, "current": before.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "active");
    let events = e.store.after(before).unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].kind, EventKind::HireChanged { hire: restored, restored_from }
        if restored == &hire && restored_from.map(|id| json!(id.0)) == Some(added))
    );
    let (status, history) = call(&e, Method::GET, &format!("{path}/history"), None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let versions = history["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 4);
    assert_eq!(
        versions
            .iter()
            .map(|v| v["value"]["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["active", "active", "retired", "active"]
    );
}

#[tokio::test]
async fn malformed_login_code_returns_a_coded_conflict() {
    let (_dir, e) = engine(Default::default());
    let (status, body) = call(
        &e,
        Method::POST,
        "/logins/claude",
        Some(json!({"code": "unused"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "INVALID_VALUE");
}

#[tokio::test]
async fn settings_history_and_restore_by_key() {
    let (_dir, e) = engine(Default::default());
    for style in ["Technical", "Expert"] {
        let (s, _) = call(
            &e,
            Method::PUT,
            "/settings",
            Some(json!({"values": {"response_style": style}})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    call(
        &e,
        Method::PUT,
        "/settings",
        Some(json!({"values": {"theme": "dark"}})),
    )
    .await;

    let (s, h) = call(
        &e,
        Method::GET,
        "/settings/history?key=response_style",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let versions = h["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2, "other keys are not in it: {h}");
    assert_eq!(versions[0]["value"], "Expert");
    assert_eq!(versions[1]["value"], "Technical");
    let (newest, oldest) = (versions[0]["event"].clone(), versions[1]["event"].clone());

    let stale = json!({"key": "response_style", "event": oldest, "current": oldest});
    let before = event_count(&e);
    let (s, r) = call(&e, Method::POST, "/settings/restore", Some(stale)).await;
    assert_eq!(
        (s, r["code"].as_str()),
        (StatusCode::CONFLICT, Some("VALUE_CHANGED"))
    );
    let (_, settings) = call(&e, Method::GET, "/settings", None).await;
    assert_eq!(settings["values"]["response_style"], "Expert");
    assert!(event_count(&e) <= before + 1, "only the refusal record");

    let (s, r) = call(
        &e,
        Method::POST,
        "/settings/restore",
        Some(json!({"key": "response_style", "event": oldest, "current": newest})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["values"]["response_style"], "Technical");
    let last = support::events::all(&e).pop().unwrap();
    assert!(matches!(
        last.kind,
        EventKind::SettingsChanged {
            restored_from: Some(_),
            ..
        }
    ));
    let (_, h) = call(
        &e,
        Method::GET,
        "/settings/history?key=response_style",
        None,
    )
    .await;
    assert_eq!(h["versions"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_report_is_a_preview_with_the_footer_and_files_nothing() {
    let (_dir, e) = engine(Default::default());
    let before = event_count(&e);
    let (s, p) = call(
        &e,
        Method::POST,
        "/report",
        Some(json!({"kind": "bug", "title": "It breaks & burns", "text": "Steps: one"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["title"], "It breaks & burns");
    let body = p["body"].as_str().unwrap();
    assert!(body.starts_with("Steps: one\n\n---\n"), "{body}");
    for want in [
        format!("dvb: {}", dvb_core::VERSION),
        "Install: engine on this machine (dev mode)".into(),
        "OS: ".into(),
        "Logged in: none".into(),
    ] {
        assert!(body.contains(&want), "{want} not in {body}");
    }
    let url = p["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://github.com/broken-branch/dvb/issues/new?title=It%20breaks%20%26%20burns&body=Steps%3A%20one"),
        "{url}"
    );
    assert_eq!(event_count(&e), before, "nothing is recorded");
}

#[tokio::test]
async fn logins_follow_the_vendor_login_files() {
    let (dir, e) = engine(Default::default());
    let (s, l) = call(&e, Method::GET, "/logins", None).await;
    assert_eq!(
        (s, l),
        (
            StatusCode::OK,
            json!({"claude": false, "codex": false, "needed": []})
        )
    );
    let claude = dir.path().join("logins/claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join(".credentials.json"), "{}").unwrap();
    let (_, l) = call(&e, Method::GET, "/logins", None).await;
    assert_eq!(l, json!({"claude": true, "codex": false, "needed": []}));
    let (_, p) = call(
        &e,
        Method::POST,
        "/report",
        Some(json!({"kind": "feature", "title": "t", "text": "x"})),
    )
    .await;
    assert!(p["body"].as_str().unwrap().contains("Logged in: claude\n"));
}

#[tokio::test]
async fn cli_login_recognizes_a_result_written_before_the_completion_poll() {
    use dvb_core::api::{LoginCode, LoginStarted};
    use dvb_core::{FakeClock, RunId};

    let dir = tempfile::tempdir().unwrap();
    let e = support::engine::open_with(
        dir.path(),
        "dvb",
        dvb_engine::EngineOptions {
            clock: Some(std::sync::Arc::new(FakeClock::default())),
            ..Default::default()
        },
    );
    let run = RunId::new("01M5A00000000000000000G1N0");
    let work = e.cfg.paths.work.join(run.as_str());
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("env"), "DVB_LOGIN=1\nDVB_VENDOR=codex\n").unwrap();
    std::fs::write(
        work.join("login.json"),
        r#"{"url":"https://example.test/login"}"#,
    )
    .unwrap();
    for result in ["ok", "failed: denied"] {
        std::fs::write(work.join("login-result"), result).unwrap();
        let done = dvb_cli::finish_login(String::new(), |code| {
            let e = e.clone();
            let body = json!(LoginCode {
                run: run.clone(),
                code
            });
            async move {
                let (status, value) = call(&e, Method::POST, "/logins/codex", Some(body)).await;
                assert_eq!(status, StatusCode::OK);
                Ok(serde_json::from_value::<LoginStarted>(value).unwrap())
            }
        })
        .await
        .unwrap();
        assert_eq!(done.run, run);
        assert_eq!(done.url, "https://example.test/login");
        assert!(!work.join("code").exists());
        if result == "ok" {
            assert_eq!(done.status, dvb_core::api::LoginStatus::LoggedIn);
            assert!(done.prompt.contains("Logged in."));
        } else {
            assert_eq!(done.status, dvb_core::api::LoginStatus::Failed);
            assert_eq!(
                done.prompt,
                "This login run has already ended (Login failed: denied); start a new one."
            );
        }
    }
}

#[tokio::test]
async fn restoring_a_role_cannot_create_a_reporting_cycle() {
    let (_dir, e) = engine(Default::default());
    let (_, roles) = call(&e, Method::GET, "/projects/p/roles", None).await;
    let (a, b) = (roles[1].clone(), roles[2].clone());
    let (ida, idb) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
    let patch = |r: &Value, reports_to: Value| {
        let mut r = r.clone();
        r["reports_to"] = reports_to;
        r
    };
    // a reports to b (version 1), then to nobody (version 2); then b reports to a.
    for (id, body) in [
        (ida, patch(&a, json!(idb))),
        (ida, patch(&a, Value::Null)),
        (idb, patch(&b, json!(ida))),
    ] {
        let (s, r) = call(&e, Method::PATCH, &format!("/roles/{id}"), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{r}");
    }
    let (_, h) = call(&e, Method::GET, &format!("/roles/{ida}/history"), None).await;
    let v = h["versions"].as_array().unwrap();
    let (newest, old) = (v[0]["event"].clone(), v[1]["event"].clone());
    assert_eq!(v[1]["value"]["reports_to"], json!(idb));
    let (s, r) = call(
        &e,
        Method::POST,
        &format!("/roles/{ida}/restore"),
        Some(json!({"event": old, "current": newest})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{r}");
    let (_, roles) = call(&e, Method::GET, "/projects/p/roles", None).await;
    let now = roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(ida))
        .unwrap();
    assert_eq!(now["reports_to"], Value::Null, "the role is unchanged");
}

#[tokio::test]
async fn project_checks_replace_refuse_empty_and_replay_with_scoped_history() {
    use dvb_core::event::CiMode;
    let (dir, e) = engine(Default::default());
    let old = support::events::all(&e);
    let initial = old
        .iter()
        .find(|ev| matches!(ev.kind, EventKind::ProjectAdded { .. }))
        .unwrap();
    // The durable project.added shape predates mutable checks and still replays.
    let historical = serde_json::json!({"kind":"project.added", "payload":{
        "id":"p", "name":"p", "repo":"/nowhere", "default_branch":"main", "ci":"no_ci"
    }});
    assert_eq!(
        serde_json::from_value::<EventKind>(historical).unwrap(),
        initial.kind
    );
    for names in [
        json!(["rust", "ui"]),
        json!(["contracts", "e2e", "identity", "rust", "ui", "upgrade"]),
    ] {
        let (s, saved) = call(
            &e,
            Method::PUT,
            "/projects/p/checks",
            Some(json!({"expected":names})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{saved}");
        assert_eq!(saved["ci"], json!({"expected":names}));
        let rules = e
            .forge()
            .unwrap()
            .ruleset(&dvb_core::forge::Repo("/nowhere".into()))
            .await
            .unwrap();
        assert_eq!(
            rules.required_checks,
            serde_json::from_value::<Vec<String>>(names).unwrap()
        );

        let (_, read) = call(&e, Method::GET, "/projects/p", None).await;
        assert_eq!(read["ci"], saved["ci"]);
    }
    let changes: Vec<_> = support::events::all(&e).into_iter().filter(|ev| {
        matches!(&ev.kind, EventKind::SettingsChanged { key, .. } if key == "project_checks")
    }).collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].actor, Actor::Owner);
    assert_eq!(changes[0].project, Some(ProjectId::new("p")));
    let EventKind::SettingsChanged {
        old: prior, new, ..
    } = &changes[0].kind
    else {
        unreachable!()
    };
    assert_eq!(prior, &json!("no_ci"));
    assert_eq!(new, &json!({"expected":["rust", "ui"]}));
    let saved = e.proj().projects[&ProjectId::new("p")].view.ci.clone();
    for names in [json!([]), json!([""]), json!([" "])] {
        let (s, refused) = call(
            &e,
            Method::PUT,
            "/projects/p/checks",
            Some(json!({"expected":names})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(refused["code"], "CI_EVIDENCE_MISSING");
        assert_eq!(e.proj().projects[&ProjectId::new("p")].view.ci, saved);
    }
    let (s, refused) = call(
        &e,
        Method::PUT,
        "/projects/p/checks",
        Some(json!({"expected":[format!("ghp_{}", "a".repeat(30))]})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(refused["code"], "SCAN_HIT");
    assert_eq!(e.proj().projects[&ProjectId::new("p")].view.ci, saved);
    let (s, refused) = call(
        &e,
        Method::PUT,
        "/projects/missing/checks",
        Some(json!("no_ci")),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(refused["code"], "PROJECT_UNKNOWN");
    let reopened = support::engine::open(dir.path());
    assert_eq!(
        reopened.proj().projects[&ProjectId::new("p")].view.ci,
        saved
    );
    let (s, read) = call(
        &reopened,
        Method::PUT,
        "/projects/p/checks",
        Some(json!("no_ci")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(read["ci"], "no_ci");
    let other = ProjectId::new("other");
    reopened
        .append_checked(Some(&other), Actor::Owner, |_, _| {
            Ok(vec![
                project_event(&other, "other", "/other"),
                EventKind::SettingsChanged {
                    key: "project_checks".into(),
                    old: json!("no_ci"),
                    new: json!({"expected":["other"]}),
                    restored_from: None,
                },
            ])
        })
        .unwrap();
    let (s, h) = call(&reopened, Method::GET, "/projects/p/checks/history", None).await;
    assert_eq!(s, StatusCode::OK);
    let versions = h["versions"].as_array().unwrap();
    assert_eq!(
        versions.len(),
        4,
        "refusals and other projects are excluded"
    );
    assert_eq!(versions[0]["value"], "no_ci");
    assert_eq!(
        versions[1]["value"],
        json!({"expected":["contracts", "e2e", "identity", "rust", "ui", "upgrade"]})
    );
    assert_eq!(versions[2]["value"], json!({"expected":["rust", "ui"]}));
    assert_eq!(versions[3]["event"], initial.id.0);
    for pair in versions.windows(2) {
        assert!(pair[0]["event"].as_u64() > pair[1]["event"].as_u64());
    }
    assert!(versions.iter().all(|v| v["actor"] == "owner"));
    assert_eq!(
        &support::events::all(&e)[..old.len()],
        old.as_slice(),
        "historical events are unchanged"
    );
    let final_replay = support::engine::open(dir.path());
    assert_eq!(
        final_replay.proj().projects[&ProjectId::new("p")].view.ci,
        CiMode::NoCi
    );
    let (_, replay_history) = call(
        &final_replay,
        Method::GET,
        "/projects/p/checks/history",
        None,
    )
    .await;
    assert_eq!(replay_history, h);
    let (s, body) = request(
        &final_replay,
        Method::GET,
        "/projects/missing/checks/history",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"code":"NOT_FOUND", "detail":"project missing"})
    );
}

#[tokio::test]
async fn no_ci_removes_required_checks_and_a_failed_ruleset_update_is_not_saved() {
    use dvb_core::forge::{Forge, Repo};
    let forge = std::sync::Arc::new(dvb_forge::FakeForge::local_git());
    let (_dir, e) = engine(dvb_engine::EngineOptions {
        forge: Some(forge.clone()),
        ..Default::default()
    });
    let path = "/projects/p/checks";
    assert_eq!(
        call(
            &e,
            Method::PUT,
            path,
            Some(json!({"expected":["new-check"]}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        forge
            .ruleset(&Repo("/nowhere".into()))
            .await
            .unwrap()
            .required_checks,
        ["new-check"]
    );
    let before = e.store.last_id().unwrap();
    forge.fail_next(
        "install_ruleset",
        dvb_core::forge::ForgeError {
            retryable: false,
            rate_limit: None,
            detail: "offline failure".into(),
        },
    );
    let (status, body) = call(&e, Method::PUT, path, Some(json!("no_ci"))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["code"], "INTERNAL_ERROR");
    assert!(body["detail"].as_str().unwrap().contains("offline failure"));
    assert_eq!(e.store.last_id().unwrap(), before);
    assert_eq!(
        call(&e, Method::PUT, path, Some(json!("no_ci"))).await.0,
        StatusCode::OK
    );
    let rules = forge.ruleset(&Repo("/nowhere".into())).await.unwrap();
    assert!(rules.required_checks.is_empty());
    assert!(rules.pr_required);
    assert!(support::events::all(&e).iter().any(|e| matches!(&e.kind, EventKind::SettingsChanged { key, new, .. } if key == "project_checks" && *new == json!("no_ci"))));
    let mut config = e.cfg.clone();
    config.fake = false;
    drop(e);
    let e = support::engine::open_config(config, Default::default());
    assert!(e.forge().is_none());
    let saved = e.store.last_id().unwrap();
    assert_eq!(
        request(
            &e,
            Method::PUT,
            path,
            Some(json!({"expected":["unapplied"]}))
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(e.store.last_id().unwrap(), saved);
    assert_eq!(
        e.proj().projects[&ProjectId::new("p")].view.ci,
        dvb_core::event::CiMode::NoCi
    );
}

#[tokio::test]
async fn partial_role_save_preserves_current_fields_and_explicit_nulls_clear_assignments() {
    let (_dir, e) = engine(Default::default());
    let (_, roles) = call(&e, Method::GET, "/projects/p/roles", None).await;
    let role = roles[1].clone();
    let id = role["id"].as_str().unwrap();
    let path = format!("/roles/{id}");
    let mut concurrent = role.clone();
    concurrent["job"] = json!("Review catalogue imports");
    concurrent["reviewed_by"] = roles[4]["id"].clone();
    concurrent["group"] = json!("Catalogue");
    let (status, _) = call(&e, Method::PATCH, &path, Some(concurrent)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, bytes) = request(
        &e,
        Method::PATCH,
        &path,
        Some(json!({"name": "Catalogue architect"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let saved: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(saved["job"], "Review catalogue imports");
    assert_eq!(saved["reviewed_by"], roles[4]["id"]);
    assert_eq!(saved["group"], "Catalogue");
    assert_eq!(saved["reports_to"], role["reports_to"]);
    let (status, cleared) = call(
        &e,
        Method::PATCH,
        &path,
        Some(json!({"reports_to": null, "reviewed_by": null, "group": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["reports_to"].is_null());
    assert!(cleared["reviewed_by"].is_null());
    assert!(cleared["group"].is_null());
    assert_eq!(cleared["name"], "Catalogue architect");
    let before = event_count(&e);
    let (status, _) = call(&e, Method::PATCH, &path, Some(json!({"name": ""}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, roles) = call(&e, Method::GET, "/projects/p/roles", None).await;
    assert_eq!(
        roles
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()["name"],
        "Catalogue architect"
    );
    assert_eq!(event_count(&e), before);
}
