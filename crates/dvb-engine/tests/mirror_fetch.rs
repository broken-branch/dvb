//! Branch fetch callers share one bare mirror, independently of task locks.
mod support;

use axum::http::{Method, StatusCode};
use dvb_core::forge::{Forge, Repo};
use dvb_core::{Actor, EventKind, GoalId, ProjectId};
use dvb_engine::{EngineOptions, owner, wake};
use dvb_forge::FakeForge;
use futures_util::poll;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;
use support::{
    InterruptedForge,
    git::{git, seed_repository},
    http::call,
};
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_branch_refreshes_after_a_merge_all_resolve_the_new_head() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    seed_repository(&origin, "main", None, "seed");
    let repo = Repo(origin.to_string_lossy().into_owned());
    let forge = Arc::new(FakeForge::local_git());
    let e = support::engine::open_with(
        &dir.path().join("e"),
        "dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge.clone()),
            launcher: None,
            ..Default::default()
        },
    );
    let mirror = e.mirror("project");
    forge.clone_repo(&repo, &mirror).await.unwrap();
    let old = git(&mirror, &["rev-parse", "main"]);
    git(&origin, &["commit", "--allow-empty", "-qm", "merged task"]);
    git(&origin, &["branch", "task-head"]);
    let merged = git(&origin, &["rev-parse", "main"]);
    // Both branch names still fetch every ref. Independent task effects and
    // orchestrator wakes must share the lock, including the head resolution.
    for _ in 0..10 {
        git(&mirror, &["update-ref", "refs/heads/main", &old]);
        let barrier = Arc::new(Barrier::new(17));
        let mut reads = Vec::new();
        for i in 0..16 {
            let (e, forge, repo, mirror, barrier) = (
                e.clone(),
                forge.clone(),
                repo.clone(),
                mirror.clone(),
                barrier.clone(),
            );
            reads.push(tokio::spawn(async move {
                barrier.wait().await;
                e.fetch(
                    forge.as_ref(),
                    &repo,
                    &mirror,
                    if i % 2 == 0 { "main" } else { "task-head" },
                )
                .await
            }));
        }
        barrier.wait().await;
        for read in reads {
            assert_eq!(read.await.unwrap().unwrap(), merged);
        }
        assert_eq!(git(&mirror, &["rev-parse", "main"]), merged);
    }
}

#[tokio::test]
async fn a_held_fetch_blocks_only_its_mirror_and_an_error_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    seed_repository(&origin, "main", None, "seed");
    let repo = Repo(origin.to_string_lossy().into_owned());
    let forge = Arc::new(InterruptedForge::new(Arc::new(FakeForge::local_git())));
    let e = support::engine::open_with(
        &dir.path().join("e"),
        "dvb",
        dvb_engine::EngineOptions {
            forge: Some(forge.clone()),
            launcher: None,
            ..Default::default()
        },
    );
    let mirror = e.mirror("one");
    let other = e.mirror("two");
    forge.clone_repo(&repo, &mirror).await.unwrap();
    forge.clone_repo(&repo, &other).await.unwrap();
    let barrier = Arc::new(Barrier::new(2));
    *forge.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let first = {
        let (e, forge, repo, mirror) = (e.clone(), forge.clone(), repo.clone(), mirror.clone());
        tokio::spawn(async move { e.fetch(forge.as_ref(), &repo, &mirror, "main").await })
    };
    barrier.wait().await;
    let second = e.fetch(forge.as_ref(), &repo, &mirror, "main");
    tokio::pin!(second);
    // Poll the second future while the first has entered Forge::fetch. It
    // must wait at the engine boundary, even on this single-thread runtime.
    tokio::select! {
        biased;
        result = &mut second => panic!("overlapping mirror fetch: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert_eq!(
        forge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "fetch")
            .count(),
        1
    );
    let head = e
        .fetch(forge.as_ref(), &repo, &other, "main")
        .await
        .unwrap();
    forge.fetch_fails_once.store(true, Ordering::SeqCst);
    barrier.wait().await;
    assert!(first.await.unwrap().unwrap_err().retryable);
    assert_eq!(second.await.unwrap(), head);

    // A cancelled async request must also release its held mirror lock.
    *forge.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let cancelled = {
        let (e, forge, repo, mirror) = (e.clone(), forge.clone(), repo.clone(), mirror.clone());
        tokio::spawn(async move { e.fetch(forge.as_ref(), &repo, &mirror, "main").await })
    };
    barrier.wait().await;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    assert_eq!(
        e.fetch(forge.as_ref(), &repo, &mirror, "main")
            .await
            .unwrap(),
        head
    );
}

#[tokio::test]
async fn removing_a_project_waiting_for_fetch_does_not_block_another_project() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin");
    seed_repository(&origin, "main", None, "seed");
    let repo = Repo(origin.to_string_lossy().into_owned());
    let forge = Arc::new(InterruptedForge::new(Arc::new(FakeForge::local_git())));
    let e = support::engine::open_with(
        &dir.path().join("engine"),
        "unused",
        EngineOptions {
            forge: Some(forge.clone()),
            launcher: Some(Arc::new(support::launcher::FakeLauncher::default())),
            pool: Some(support::engine::fake_pool()),
            ..Default::default()
        },
    );
    let a = ProjectId::new("a");
    let b = ProjectId::new("b");
    for p in [&a, &b] {
        forge
            .clone_repo(&repo, &e.mirror(p.as_str()))
            .await
            .unwrap();
        let (roles, hires) = support::events::default_role_hire_events();
        let mut events = vec![support::events::project_event(p, p.as_str(), &repo.0)];
        events.extend(roles);
        events.extend(hires);
        if p == &a {
            events.push(support::events::goal_event(
                &GoalId::new("ga"),
                "Plan A",
                None,
            ));
        }
        e.append_checked(Some(p), Actor::Owner, |_, _| Ok(events))
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    *forge.fetch_barrier.lock().unwrap() = Some(barrier.clone());
    let waking = tokio::spawn(wake::wake(e.clone(), a.clone(), wake::Wake::Goal, 0, 0));
    barrier.wait().await;
    owner::pause(&e, &a, true).unwrap();
    assert!(
        e.proj().runs.is_empty(),
        "the planning fetch has not started a run"
    );

    // A must wait for its fetch without holding the global mirror lock. Paused
    // time makes an erroneous MIRROR_PASS_RUNNING refusal deterministic.
    tokio::time::pause();
    let mut removing_a = Box::pin(call(&e, Method::POST, "/projects/a/remove", None));
    assert!(poll!(&mut removing_a).is_pending());
    let (status, body) = call(&e, Method::POST, "/projects/b/remove", None).await;

    // Cancel both waiters before asserting, so a failure leaves no fixture work.
    drop(removing_a);
    waking.abort();
    assert!(waking.await.unwrap_err().is_cancelled());
    assert!(e.proj().projects.contains_key(&a));
    assert!(e.mirror("a").exists());
    assert!(!support::events::all(&e).iter().any(|event| {
        event.project.as_ref() == Some(&a) && matches!(event.kind, EventKind::ProjectRemoved { .. })
    }));

    // A fresh removal needs both locks: cancellation must release its place in
    // the queue. Resume real time so blocking filesystem cleanup can finish.
    tokio::time::resume();
    let (retry, reply) = tokio::time::timeout(
        Duration::from_secs(30),
        call(&e, Method::POST, "/projects/a/remove", None),
    )
    .await
    .expect("cancelling the waiting removal must release its locks");
    assert_eq!(retry, StatusCode::OK, "{reply}");
    assert!(!e.mirror("a").exists());
    assert_eq!(
        status,
        StatusCode::OK,
        "independent removal must still work: {body}"
    );
    assert!(!e.mirror("b").exists());
    for p in [&a, &b] {
        assert!(!e.proj().projects.contains_key(p));
        assert_eq!(
            support::events::all(&e)
                .iter()
                .filter(|event| {
                    event.project.as_ref() == Some(p)
                        && matches!(event.kind, EventKind::ProjectRemoved { .. })
                })
                .count(),
            1
        );
    }
}
