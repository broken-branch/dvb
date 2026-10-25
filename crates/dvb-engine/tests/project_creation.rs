//! Project-add recovery through the real handler, with local Git and the Fake forge.
mod support;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use dvb_core::EventKind;
use dvb_core::forge::{Forge, Repo};
use dvb_engine::{AppState, Config, Engine, api, publish};
use std::sync::{Arc, atomic::Ordering};
use support::InterruptedForge;
use support::git::{git, seed_repository};
use tower::ServiceExt;

fn engine(root: &std::path::Path) -> (AppState, Arc<InterruptedForge>) {
    let interrupted = Arc::new(InterruptedForge::new(Arc::new(
        dvb_forge::FakeForge::local_git(),
    )));
    // Observe setup effects through the injected local Fake transport.
    let forge: Arc<dyn Forge> = interrupted.clone();
    let e = support::engine::open_with(
        root,
        "unused-dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge),
            launcher: None,
            ..Default::default()
        },
    );
    (e, interrupted)
}
async fn add(e: &AppState, repo: &Repo) -> StatusCode {
    api::router(e.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/projects")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"name":"project", "repo":repo.0,
            "create":true, "ci":"no_ci"})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn creation_and_a_lost_setup_reply_resume_after_restart_with_the_exact_commit() {
    for (after_push, finalized_mirror, public) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("engine");
        let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
        let (e, interrupted) = engine(&root);
        if after_push {
            interrupted.after_setup.store(true, Ordering::SeqCst);
        } else {
            interrupted.before_setup.store(true, Ordering::SeqCst);
        }
        assert_eq!(add(&e, &repo).await, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(e.proj().projects.is_empty());
        assert!(!e.mirror("project").exists());
        let events = support::events::all(&e);
        assert_eq!(
            events
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::RepositoryCreated { .. }))
                .count(),
            1
        );
        let head = events
            .iter()
            .find_map(|ev| {
                if let EventKind::RepositorySetupPrepared { head, .. } = &ev.kind {
                    Some(head.clone())
                } else {
                    None
                }
            })
            .unwrap();
        if !after_push {
            assert_eq!(git(std::path::Path::new(&repo.0), &["for-each-ref"]), "");
        }
        if finalized_mirror {
            // Simulate a crash after the staged mirror has been renamed, before registration.
            interrupted
                .clone_repo(&repo, &e.mirror("project"))
                .await
                .unwrap();
        }
        drop(interrupted);
        drop(e);
        // A new engine AND a new forge have no in-memory creation authorization.
        let (e, interrupted) = engine(&root);
        assert!(interrupted.created_repo_id(&repo).is_none());
        if public {
            interrupted.inner.set_public(&repo);
            interrupted.inner.set_free_plan();
        }
        assert_eq!(add(&e, &repo).await, StatusCode::CREATED);
        assert_eq!(e.proj().projects.len(), 1);
        assert_eq!(
            git(std::path::Path::new(&repo.0), &["rev-parse", "main"]),
            head
        );
        assert_eq!(
            git(
                std::path::Path::new(&repo.0),
                &["rev-list", "--count", "main"]
            ),
            "1"
        );
        assert_eq!(
            git(
                std::path::Path::new(&repo.0),
                &["log", "-1", "--format=%an <%ae>", "main"]
            ),
            "dvb <dvb@dvb.invalid>"
        );
        assert_eq!(
            support::events::all(&e)
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::RepositoryCreated { .. }))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn an_unrelated_empty_repo_never_gets_the_setup_exception() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, interrupted) = engine(&tmp.path().join("engine"));
    let repo = interrupted
        .inner
        .create_repo(tmp.path().to_str().unwrap(), "existing.git")
        .await
        .unwrap();
    // The forge created it, but this engine has no durable receipt for project add.
    assert_eq!(add(&e, &repo).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(git(std::path::Path::new(&repo.0), &["for-each-ref"]), "");
    assert!(
        !support::events::all(&e)
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::RepositoryCreated { .. }))
    );
}

#[tokio::test]
async fn a_created_repo_initialized_by_another_writer_is_never_overwritten_on_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("engine");
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    let (e, interrupted) = engine(&root);
    interrupted.before_setup.store(true, Ordering::SeqCst);
    assert_eq!(add(&e, &repo).await, StatusCode::INTERNAL_SERVER_ERROR);
    let seed = tmp.path().join("seed");
    seed_repository(
        &seed,
        "main",
        Some(("unrelated.txt", "keep me\n")),
        "Unrelated",
    );
    git(&seed, &["push", "--quiet", &repo.0, "main"]);
    let original = git(&seed, &["rev-parse", "HEAD"]);
    drop(interrupted);
    drop(e);
    let (e, _) = engine(&root);
    assert_eq!(add(&e, &repo).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(e.proj().projects.is_empty());
    assert_eq!(
        git(std::path::Path::new(&repo.0), &["rev-parse", "main"]),
        original
    );
    assert_eq!(
        git(
            std::path::Path::new(&repo.0),
            &["show", "main:unrelated.txt"]
        ),
        "keep me"
    );
}

#[tokio::test]
async fn setup_keeps_the_empty_ref_lease_when_another_writer_wins_after_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, interrupted) = engine(&tmp.path().join("engine"));
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    let seed = tmp.path().join("seed");
    seed_repository(
        &seed,
        "main",
        Some(("other.txt", "other writer\n")),
        "Other writer",
    );
    let head = git(&seed, &["rev-parse", "HEAD"]);
    *interrupted.initialize_during_setup.lock().unwrap() = Some((seed, head.clone()));
    assert_eq!(add(&e, &repo).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(e.proj().projects.is_empty());
    assert_eq!(
        git(std::path::Path::new(&repo.0), &["rev-parse", "main"]),
        head
    );
    assert_eq!(
        git(std::path::Path::new(&repo.0), &["show", "main:other.txt"]),
        "other writer"
    );
}

#[tokio::test]
async fn existing_repo_add_recovers_a_finalized_mirror_and_refuses_conflicting_identity() {
    for conflict in [None, Some("repo"), Some("branch"), Some("head")] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("engine");
        let repo = tmp.path().join("repo");
        seed_repository(&repo, "main", None, "Seed");
        let (e, _) = engine(&root);
        publish::create_mirror(repo.to_str().unwrap(), &e.mirror("project"))
            .await
            .unwrap();
        match conflict {
            Some("repo") => {
                git(
                    &e.mirror("project"),
                    &["remote", "set-url", "origin", "/another/repo"],
                );
            }
            Some("branch") => {
                git(
                    &e.mirror("project"),
                    &["symbolic-ref", "HEAD", "refs/heads/other"],
                );
            }
            Some("head") => {
                git(
                    &repo,
                    &[
                        "-c",
                        "user.name=seed",
                        "-c",
                        "user.email=seed@example.invalid",
                        "commit",
                        "--quiet",
                        "--allow-empty",
                        "-m",
                        "Advance",
                    ],
                );
            }
            None => {}
            Some(_) => unreachable!(),
        }
        drop(e);
        let (e, forge) = engine(&root);
        let injected: Arc<dyn Forge> = forge.clone();
        assert!(Arc::ptr_eq(&e.forge().unwrap(), &injected));
        let remote = Repo(repo.to_str().unwrap().into());
        let refs = git(&repo, &["for-each-ref"]);
        let rules = forge.ruleset(&remote).await.unwrap();
        let body = serde_json::json!({"name":"project", "repo":repo.to_str().unwrap(), "create":false, "ci":"no_ci"});
        let response = api::router(e.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if conflict.is_none() {
                StatusCode::CREATED
            } else {
                StatusCode::CONFLICT
            }
        );
        assert_eq!(e.proj().projects.len(), usize::from(conflict.is_none()));
        // Neither outcome opens a PR or pushes to an existing repository.
        assert_eq!(git(&repo, &["for-each-ref"]), refs);
        assert!(forge.pr(&remote, 1).await.is_err());
        if conflict.is_some() {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["code"],
                "INVALID_VALUE"
            );
            assert_eq!(forge.ruleset(&remote).await.unwrap(), rules);
        } else {
            assert!(forge.ruleset(&remote).await.unwrap().pr_required);
        }
    }
}

#[tokio::test]
async fn conflicting_mirror_refuses_repository_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, forge) = engine(&tmp.path().join("engine"));
    let original = tmp.path().join("original.git");
    git(
        tmp.path(),
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            original.to_str().unwrap(),
        ],
    );
    forge
        .clone_repo(
            &Repo(original.to_str().unwrap().into()),
            &e.mirror("project"),
        )
        .await
        .unwrap();
    let requested = tmp.path().join("requested.git");
    let events = support::events::all(&e);
    assert_eq!(
        add(&e, &Repo(requested.to_str().unwrap().into())).await,
        StatusCode::CONFLICT
    );
    assert!(!requested.exists());
    assert!(e.proj().projects.is_empty());
    assert_eq!(support::events::all(&e), events);
}

#[tokio::test]
async fn owner_project_add_preconditions_return_coded_conflicts() {
    for (configured, name, duplicate, code) in [
        (false, "valid", false, "INVALID_VALUE"),
        (true, "bad/name", false, "INVALID_PROJECT_NAME"),
        (true, "project", true, "INVALID_VALUE"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::dev(tmp.path(), "unused-dvb".into());
        cfg.fake = configured;
        let e = Engine::open(cfg, Default::default()).unwrap();
        if duplicate {
            e.append_checked(None, dvb_core::Actor::Owner, |_, _| {
                Ok(vec![support::events::project_event(
                    &dvb_core::ProjectId::new("p"),
                    name,
                    "example/repo",
                )])
            })
            .unwrap();
        }
        let response = api::router(e)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"name":name, "repo":"example/repo", "ci":"no_ci"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["code"],
            code
        );
    }
}

#[tokio::test]
async fn only_a_badly_formed_project_name_is_refused_with_its_own_code() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    seed_repository(&repo, "main", None, "Seed");
    let (e, _) = engine(&tmp.path().join("engine"));
    let body =
        |name: &str| serde_json::json!({"name":name, "repo":repo.to_str().unwrap(), "ci":"no_ci"});
    for name in ["", "notes.v2", "my project"] {
        let (status, refusal) =
            support::http::call(&e, axum::http::Method::POST, "/projects", Some(body(name))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(refusal["code"], "INVALID_PROJECT_NAME");
        assert_eq!(refusal["detail"], format!("bad project name {name:?}"));
        assert!(e.proj().projects.is_empty());
    }
    let (status, _) = support::http::call(
        &e,
        axum::http::Method::POST,
        "/projects",
        Some(body("notes-v2_1")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(e.proj().projects.len(), 1);
}

fn ruleset_events(e: &AppState) -> Vec<&'static str> {
    support::events::all(e)
        .iter()
        .map(|ev| ev.kind.kind())
        .filter(|kind| kind.starts_with("project.ruleset_"))
        .collect()
}

async fn rulesets_missing(e: &AppState) -> Vec<String> {
    let (status, body) = support::http::call(e, axum::http::Method::GET, "/status", None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_value(body["rulesets_missing"].clone()).unwrap_or_default()
}

#[tokio::test]
async fn project_config_may_name_the_owner_but_not_hold_a_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("engine/state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("publication.json"),
        serde_json::json!({"owner_name":"example-owner", "owner_email":"owner@example.invalid"})
            .to_string(),
    )
    .unwrap();
    let repo = tmp.path().join("example-owner");
    seed_repository(&repo, "main", None, "Seed");
    let (e, _) = engine(&tmp.path().join("engine"));
    let add = |ci: serde_json::Value| serde_json::json!({"name":"example-owner", "repo":repo.to_str().unwrap(), "ci":ci});
    let secret = format!("ghp_{}", "a".repeat(36));
    let (status, refusal) = support::http::call(
        &e,
        axum::http::Method::POST,
        "/projects",
        Some(add(serde_json::json!({"expected":[secret.clone()]}))),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["code"], "SCAN_HIT");
    let named = serde_json::json!({"expected":["example-owner owner@example.invalid"]});
    let (status, added) = support::http::call(
        &e,
        axum::http::Method::POST,
        "/projects",
        Some(add(named.clone())),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    let checks = format!("/projects/{}/checks", added["id"].as_str().unwrap());
    let (status, saved) =
        support::http::call(&e, axum::http::Method::PUT, &checks, Some(named)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, refusal) = support::http::call(
        &e,
        axum::http::Method::PUT,
        &checks,
        Some(serde_json::json!({"expected":[secret]})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["code"], "SCAN_HIT");
}

#[tokio::test]
async fn a_private_repository_on_github_free_is_added_without_a_ruleset_and_warned_once() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    seed_repository(&repo, "main", None, "Seed");
    let body = serde_json::json!({"name":"project", "repo":repo.to_str().unwrap(), "ci":"no_ci"});
    let (e, forge) = engine(&tmp.path().join("engine"));
    forge.inner.set_free_plan();
    let (status, added) =
        support::http::call(&e, axum::http::Method::POST, "/projects", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    let calls = forge.calls.lock().unwrap().clone();
    assert_eq!(calls.first(), Some(&"rulesets_allowed"));
    assert!(!calls.contains(&"install_ruleset"), "{calls:?}");
    assert!(!calls.contains(&"ruleset"), "{calls:?}");
    assert!(e.mirror("project").exists());
    assert_eq!(ruleset_events(&e), ["project.ruleset_missing"]);
    assert_eq!(rulesets_missing(&e).await, ["project"]);
    // Changing the checks saves them and does not warn again.
    let checks = format!("/projects/{}/checks", added["id"].as_str().unwrap());
    for ci in [
        serde_json::json!({"expected":["ci"]}),
        serde_json::json!("no_ci"),
    ] {
        let (status, saved) =
            support::http::call(&e, axum::http::Method::PUT, &checks, Some(ci.clone())).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["ci"], ci);
    }
    assert!(!forge.calls.lock().unwrap().contains(&"install_ruleset"));
    assert_eq!(ruleset_events(&e), ["project.ruleset_missing"]);
    drop(e);
    // On a paid plan the next check change installs the ruleset and clears the warning.
    let (e, forge) = engine(&tmp.path().join("engine"));
    let ci = serde_json::json!({"expected":["ci"]});
    let (status, _) = support::http::call(&e, axum::http::Method::PUT, &checks, Some(ci)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(forge.calls.lock().unwrap().contains(&"install_ruleset"));
    assert_eq!(
        ruleset_events(&e),
        ["project.ruleset_missing", "project.ruleset_installed"]
    );
    assert!(rulesets_missing(&e).await.is_empty());
}

#[tokio::test]
async fn a_public_repository_on_github_free_is_added_with_its_ruleset() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    seed_repository(&repo, "main", None, "Seed");
    let (e, forge) = engine(&tmp.path().join("engine"));
    // GitHub Free has rulesets on public repositories.
    forge.inner.set_free_plan();
    forge.inner.set_public(&Repo(repo.to_string_lossy().into()));
    let (status, body) = support::http::call(
        &e,
        axum::http::Method::POST,
        "/projects",
        Some(serde_json::json!({"name":"project", "repo":repo.to_str().unwrap(), "ci":"no_ci"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let calls = forge.calls.lock().unwrap().clone();
    assert_eq!(calls.first(), Some(&"rulesets_allowed"));
    assert!(calls.contains(&"install_ruleset"));
    assert_eq!(e.proj().projects.len(), 1);
    assert!(e.mirror("project").exists());
    assert!(ruleset_events(&e).is_empty());
}

fn created_receipts(e: &AppState) -> usize {
    support::events::all(e)
        .iter()
        .filter(|ev| matches!(ev.kind, EventKind::RepositoryCreated { .. }))
        .count()
}

async fn add_refusal(e: &AppState, repo: &Repo) -> (StatusCode, serde_json::Value) {
    support::http::call(
        e,
        axum::http::Method::POST,
        "/projects",
        Some(serde_json::json!({"name":"project", "repo":repo.0, "create":true, "ci":"no_ci"})),
    )
    .await
}

#[tokio::test]
async fn a_paid_organization_with_no_private_repository_creates_its_first_project() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    // The App reaches no repository at all: the new repository's rulesets decide.
    let (e, forge) = engine(&tmp.path().join("engine"));
    assert_eq!(add(&e, &repo).await, StatusCode::CREATED);
    let calls = forge.calls.lock().unwrap().clone();
    assert_eq!(
        calls[..3],
        ["create_repo", "created_repo_id", "rulesets_allowed"]
    );
    assert!(!calls.contains(&"accessible_repositories"));
    assert_eq!(e.proj().projects.len(), 1);
    assert_eq!(created_receipts(&e), 1);
}

#[tokio::test]
async fn a_repository_created_on_github_free_is_kept_and_added_without_a_ruleset() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    let (e, forge) = engine(&tmp.path().join("engine"));
    forge.inner.set_free_plan();
    assert_eq!(add(&e, &repo).await, StatusCode::CREATED);
    let calls = forge.calls.lock().unwrap().clone();
    assert_eq!(
        calls[..3],
        ["create_repo", "created_repo_id", "rulesets_allowed"]
    );
    assert!(!calls.contains(&"install_ruleset"), "{calls:?}");
    assert!(std::path::Path::new(&repo.0).exists());
    assert_eq!(created_receipts(&e), 1);
    assert_eq!(e.proj().projects.len(), 1);
    assert_eq!(ruleset_events(&e), ["project.ruleset_missing"]);
}

#[tokio::test]
async fn a_personal_account_adds_an_existing_repository_and_create_is_one_sentence() {
    let tmp = tempfile::tempdir().unwrap();
    let existing = tmp.path().join("existing");
    seed_repository(&existing, "main", None, "Seed");
    let (e, forge) = engine(&tmp.path().join("engine"));
    forge.inner.set_personal_account();
    let (status, _) = support::http::call(
        &e,
        axum::http::Method::POST,
        "/projects",
        Some(
            serde_json::json!({"name":"existing", "repo":existing.to_str().unwrap(), "ci":"no_ci"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // GitHub lets an installation create repositories only in an organization.
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    forge.calls.lock().unwrap().clear();
    let (status, refusal) = add_refusal(&e, &repo).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["code"], "INVALID_VALUE");
    assert_eq!(refusal["detail"], publish::project::PERSONAL_CREATE);
    assert!(forge.calls.lock().unwrap().is_empty());
    assert!(!std::path::Path::new(&repo.0).exists());
    assert_eq!(created_receipts(&e), 0);
    assert_eq!(e.proj().projects.len(), 1);
}

#[tokio::test]
async fn overlapping_project_adds_cannot_register_the_same_name_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    seed_repository(&repo, "main", None, "Seed");
    let (e, forge) = engine(&tmp.path().join("engine"));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    *forge.clone_barrier.lock().unwrap() = Some(barrier.clone());
    let body = serde_json::json!({"name":"project", "repo":repo.to_str().unwrap(), "ci":"no_ci"});
    let delayed = tokio::spawn({
        let e = e.clone();
        let body = body.clone();
        async move { support::http::call(&e, axum::http::Method::POST, "/projects", Some(body)).await }
    });
    barrier.wait().await;
    let (status, _) =
        support::http::call(&e, axum::http::Method::POST, "/projects", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    barrier.wait().await;
    let (status, body) = delayed.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "VALUE_CHANGED");
    assert_eq!(e.proj().projects.len(), 1);
    assert_eq!(
        support::events::all(&e)
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::ProjectAdded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn invalid_repository_creation_inputs_return_coded_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let (production, _) = engine(&tmp.path().join("production"));
    let fake =
        support::engine::open_with(&tmp.path().join("fake"), "unused-dvb", Default::default());
    for e in [production, fake] {
        let (status, body) = support::http::call(&e, axum::http::Method::POST, "/projects", Some(serde_json::json!({"name":"project", "repo":"missing-slash", "create":true, "ci":"no_ci"}))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "INVALID_VALUE");
        assert!(e.proj().projects.is_empty());
    }
}

#[tokio::test]
async fn new_project_hires_the_first_owner_selected_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, _) = engine(tmp.path());
    e.append_checked(None, dvb_core::Actor::Owner, |_, _| {
        Ok(vec![EventKind::SettingsChanged {
            key: "model_pool".into(),
            old: serde_json::Value::Null,
            new: serde_json::json!(["fake-next"]),
            restored_from: None,
        }])
    })
    .unwrap();
    let repo = Repo(tmp.path().join("origin.git").to_string_lossy().into());
    assert_eq!(add(&e, &repo).await, StatusCode::CREATED);
    let proj = e.proj();
    let project = proj.projects.values().next().unwrap();
    assert_eq!(project.hires.len(), project.roles.len());
    assert!(project.hires.iter().all(|h| h.model == "fake-next"));
    drop(proj);
}

/// An origin whose default branch has dvb's squash merge, authored as GitHub
/// authors it (by the App), of a publication by `who` on its task branch.
fn origin_with_dvb_merge(dir: &std::path::Path, who: &str) -> Repo {
    let seed = dir.join("seed");
    seed_repository(&seed, "main", Some(("file", "base\n")), "base");
    git(&seed, &["checkout", "--quiet", "-b", "dvb/t-1"]);
    std::fs::write(seed.join("file"), "changed\n").unwrap();
    git(
        &seed,
        &["commit", "--quiet", "-am", "Task", "--author", who],
    );
    let publication = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["checkout", "--quiet", "main"]);
    git(&seed, &["merge", "--quiet", "--squash", "dvb/t-1"]);
    git(
        &seed,
        &[
            "commit",
            "--quiet",
            "-m",
            "Task (#1)",
            "-m",
            &format!("Reviewed publication {publication}."),
            "--author",
            "dvb-example[bot] <1+dvb-example[bot]@users.noreply.github.com>",
        ],
    );
    let origin = dir.join("origin.git");
    git(
        dir,
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    Repo(origin.to_string_lossy().into())
}

async fn add_existing(e: &AppState, name: &str, repo: &Repo) -> serde_json::Value {
    let (status, body) = support::http::call(
        e,
        axum::http::Method::POST,
        "/projects",
        Some(serde_json::json!({"name":name, "repo":repo.0, "create":false, "ci":"no_ci"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn adding_a_repository_whose_dvb_commits_name_another_identity_reports_both() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, _) = engine(&tmp.path().join("engine"));
    let repo = origin_with_dvb_merge(tmp.path(), "example-owner <owner@example.com>");
    let body = add_existing(&e, "project", &repo).await;
    assert_eq!(
        body["publication_mismatch"],
        serde_json::json!({
            "publishes_as": {"name": "dvb", "email": "dvb@dvb.invalid"},
            "recent": {"name": "example-owner", "email": "owner@example.com"},
            "recent_is_owner": false,
        })
    );
    assert_eq!(e.proj().projects.len(), 1);
}

#[tokio::test]
async fn adding_a_repository_whose_dvb_commits_match_or_are_absent_reports_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, _) = engine(&tmp.path().join("engine"));
    let (status, _) = support::http::call(
        &e,
        axum::http::Method::PUT,
        "/settings/publication",
        Some(serde_json::json!({"name":"example-owner", "email":"owner@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let same = tmp.path().join("same");
    let repo = origin_with_dvb_merge(&same, "example-owner <owner@example.com>");
    let body = add_existing(&e, "same", &repo).await;
    assert!(body.get("publication_mismatch").is_none(), "{body}");
    // No dvb commit at all.
    let plain = tmp.path().join("plain");
    seed_repository(&plain, "main", None, "base");
    let body = add_existing(&e, "plain", &Repo(plain.to_string_lossy().into())).await;
    assert!(body.get("publication_mismatch").is_none(), "{body}");
    assert_eq!(e.proj().projects.len(), 2);
}

#[tokio::test]
async fn adding_after_task_branch_cleanup_still_reports_the_publication_author() {
    for keep_pull_ref in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let (e, forge) = engine(&tmp.path().join("engine"));
        let repo = origin_with_dvb_merge(tmp.path(), "other <other@example.invalid>");
        let origin = std::path::Path::new(&repo.0);
        let publication = git(origin, &["rev-parse", "dvb/t-1"]);
        if keep_pull_ref {
            // GitHub keeps PR heads independently of the deleted task branch.
            git(origin, &["update-ref", "refs/pull/1/head", &publication]);
        }
        git(origin, &["branch", "-D", "dvb/t-1"]);
        if keep_pull_ref {
            git(origin, &["gc", "--quiet", "--prune=now"]);
        }
        // Use the transport: a local path clone also copies unreachable objects.
        let repo = Repo(format!("file://{}", repo.0));
        let body = add_existing(&e, "gone", &repo).await;
        assert_eq!(
            body["publication_mismatch"],
            serde_json::json!({
                "publishes_as": {"name": "dvb", "email": "dvb@dvb.invalid"},
                "recent": {"name": "other", "email": "other@example.invalid"},
                "recent_is_owner": false,
            })
        );
        assert!(
            dvb_core::git::Git::new(&e.mirror("gone"))
                .run(&["cat-file", "-e", &publication], &[])
                .await
                .is_err()
        );
        assert_eq!(e.proj().projects.len(), 1);
        assert!(forge.calls.lock().unwrap().contains(&"commit_author"));
        let (status, _) = support::http::call(
            &e,
            axum::http::Method::PUT,
            "/settings/publication",
            Some(serde_json::json!({"name":"other", "email":"other@example.invalid"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = add_existing(&e, "matching", &repo).await;
        assert!(body.get("publication_mismatch").is_none(), "{body}");
        assert_eq!(e.proj().projects.len(), 2);
    }
}
