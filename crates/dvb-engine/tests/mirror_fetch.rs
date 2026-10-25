//! Branch fetch callers share one bare mirror, independently of task locks.
mod support;

use dvb_core::forge::{Forge, Repo};
use dvb_forge::FakeForge;
use std::sync::{Arc, atomic::Ordering};
use support::{
    InterruptedForge,
    git::{git, seed_repository},
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
