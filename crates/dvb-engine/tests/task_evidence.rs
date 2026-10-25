//! Current-head evidence through the owner API, with a local Git mirror.
mod support;

use axum::http::{Method, StatusCode};
use dvb_core::run_verbs::Finding;
use dvb_core::{Actor, EventKind, ProjectId, RoleId, RunId, TaskId, Vendor, VerdictId};
use dvb_engine::{Config, Engine, publish};
use support::events::{backend_plan_event, goal_event, project_event};
use support::git::{git, seed_repository};
use support::http::{call, request};

fn published(head: &str, round: u8) -> EventKind {
    EventKind::BranchPublished {
        task: TaskId::new("t"),
        round,
        branch: "dvb/t-1".into(),
        head: head.into(),
    }
}

fn verdict(id: &str, task: &str, head: &str, open: bool) -> EventKind {
    EventKind::VerdictRecorded {
        review_stage: None,
        verdict: VerdictId::new(id),
        task: TaskId::new(task),
        head: head.into(),
        run: RunId::new("review"),
        role: RoleId::new("code-reviewer"),
        model: "fake-1".into(),
        vendor: Vendor::Fake,
        findings: vec![Finding {
            severity: dvb_core::run_verbs::Severity::High,
            file: "file.txt".into(),
            line: std::num::NonZeroU32::new(1),
            title: "Finding".into(),
            detail: "Details".into(),
            open,
        }],
        summary: id.into(),
    }
}

#[tokio::test]
async fn diff_and_latest_verdict_follow_the_current_head_and_survive_replay() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    seed_repository(&repo, "main", Some(("file.txt", "base\n")), "base");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("file.txt"), "first\n").unwrap();
    git(&repo, &["commit", "-am", "first"]);
    let first = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("file.txt"), "second  \n").unwrap();
    git(&repo, &["commit", "-am", "second"]);
    let second = git(&repo, &["rev-parse", "HEAD"]);
    let cfg = Config::dev(dir.path(), "dvb".into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("p"))
        .await
        .unwrap();
    let p = ProjectId::new("p");
    let g = dvb_core::GoalId::new("g");
    let t = TaskId::new("t");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(vec![
            project_event(&p, "p", &repo.to_string_lossy()),
            goal_event(&g, "Goal", None),
            backend_plan_event(&g, &t),
        ])
    })
    .unwrap();
    for path in ["/tasks/missing/diff", "/tasks/missing/verdict"] {
        let (status, body) = request(&e, Method::GET, path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"code":"NOT_FOUND", "detail":"task missing"})
        );
    }
    assert_eq!(
        call(&e, Method::GET, "/tasks/t/diff", None).await.1["code"],
        "HEAD_MISMATCH"
    );
    assert_eq!(
        call(&e, Method::GET, "/tasks/t/verdict", None).await.1["code"],
        "HEAD_MISMATCH"
    );
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            EventKind::RunStarted(
                support::assignment::Assignment::new(RunId::new("builder"))
                    .task(Some(t))
                    .reason("Build".into())
                    .base(Some(base.clone()))
                    .build(),
            ),
            published(&first, 1),
            verdict("first-review", "t", &first, true),
        ])
    })
    .unwrap();
    let (status, diff) = call(&e, Method::GET, "/tasks/t/diff", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(diff["base"], base);
    assert_eq!(diff["head"], first);
    assert!(diff["diff"].as_str().unwrap().contains("+first"));
    assert_eq!(
        call(&e, Method::GET, "/tasks/t/verdict", None).await.1["open_findings"],
        1
    );
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![published(&second, 2)])
    })
    .unwrap();
    let (status, body) = request(&e, Method::GET, "/tasks/t/verdict", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"code":"NOT_FOUND", "detail":format!("no verdict for task t on {second}")})
    );
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            verdict("second-old", "t", &second, true),
            verdict("second-latest", "t", &second, false),
            verdict("foreign-task", "other", &second, true),
            verdict("late-old-head", "t", &first, true),
        ])
    })
    .unwrap();
    drop(e);
    let e = Engine::open(cfg, Default::default()).unwrap();
    let (_, diff) = call(&e, Method::GET, "/tasks/t/diff", None).await;
    assert_eq!(diff["head"], second);
    // Read against the last head the review gave a verdict on.
    assert_eq!(diff["base"], first);
    let patch = diff["diff"].as_str().unwrap();
    assert!(patch.contains("-first") && patch.contains("+second"));
    assert!(patch.ends_with("+second  \n"));
    assert!(!patch.contains("-base"));
    let (status, review) = call(&e, Method::GET, "/tasks/t/verdict", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(review["verdict"], "second-latest");
    assert_eq!(review["head"], second);
    assert_eq!(review["role"], "code-reviewer");
    assert_eq!(review["run"], "review");
    assert_eq!(review["model"], "fake-1");
    assert_eq!(review["vendor"], "fake");
    assert_eq!(review["open_findings"], 0);
    assert_eq!(review["findings"][0]["detail"], "Details");
}

#[tokio::test]
async fn verdict_uses_folded_evidence_without_decoding_the_log_again() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::dev(dir.path(), "dvb".into());
    let e = Engine::open(cfg.clone(), Default::default()).unwrap();
    let p = ProjectId::new("p");
    e.append_checked(Some(&p), Actor::Owner, |_, _| {
        Ok(support::events::approved_task_events())
    })
    .unwrap();
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            published("head", 1),
            verdict("latest", "t", "head", false),
        ])
    })
    .unwrap();
    // An unrelated historical payload cannot affect an already folded verdict read.
    let db = rusqlite::Connection::open(cfg.paths.state.join("dvb.sqlite")).unwrap();
    db.execute(
        "UPDATE events SET payload = 'invalid json' WHERE kind = 'project.added'",
        [],
    )
    .unwrap();
    let (status, body) = request(&e, Method::GET, "/tasks/t/verdict", None).await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["verdict"], "latest");
}

#[tokio::test]
async fn reviewer_and_diff_agree_on_publication_updated_and_merged_bases() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    seed_repository(&repo, "main", Some(("file.txt", "base\n")), "base");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("file.txt"), "feature\n").unwrap();
    git(&repo, &["commit", "-am", "feature"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "main"]);
    std::fs::write(repo.join("other.txt"), "main advance\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "advance main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "feature"]);
    git(&repo, &["merge", "--no-edit", "main"]);
    let merged = git(&repo, &["rev-parse", "HEAD"]);
    let e = support::engine::open(dir.path());
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("p"))
        .await
        .unwrap();
    let p = ProjectId::new("p");
    let g = dvb_core::GoalId::new("g");
    let t = TaskId::new("t");
    let assignment = |run: &str, base: &str, reviews: Option<String>| {
        support::assignment::Assignment::new(RunId::new(run))
            .task(Some(t.clone()))
            .base(Some(base.into()))
            .reviews(reviews)
            .build()
    };
    e.append_checked(Some(&p), Actor::Engine, |_, _| {
        Ok(vec![
            project_event(&p, "p", &repo.to_string_lossy()),
            goal_event(&g, "Goal", None),
            backend_plan_event(&g, &t),
            EventKind::RunStarted(assignment("published-builder", &base, None)),
            published(&head, 1),
            // A later builder must not change which base the published work uses.
            EventKind::RunStarted(assignment("later-builder", &head, None)),
            EventKind::RunStarted(assignment("review", &head, Some(head.clone()))),
        ])
    })
    .unwrap();
    for (step, expected) in [(0, &base), (1, &main), (2, &main)] {
        if step == 1 {
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::BranchUpdated {
                    task: t.clone(),
                    branch: "dvb/t-1".into(),
                    base: main.clone(),
                    from: head.clone(),
                    head: merged.clone(),
                    same_change: true,
                }])
            })
            .unwrap();
        } else if step == 2 {
            // A conflict-fix publication starts from its old head and merges main.
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![
                    EventKind::RunStarted(assignment("fix", &head, None)),
                    published(&merged, 2),
                ])
            })
            .unwrap();
        }
        let (reviewer, project) = {
            let proj = e.proj();
            (
                proj.runs[&RunId::new("review")].clone(),
                proj.projects[&p].clone(),
            )
        };
        let mut reviewer = reviewer;
        reviewer.assignment.reviews = Some(if step == 0 {
            head.clone()
        } else {
            merged.clone()
        });
        let vars = dvb_engine::prompt::context(&e, &reviewer, &project)
            .await
            .unwrap();
        let prompt_base = &vars.iter().find(|(key, _)| *key == "base").unwrap().1;
        let (status, diff) = call(&e, Method::GET, "/tasks/t/diff", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(prompt_base, expected, "step {step}");
        assert_eq!(diff["base"], *prompt_base, "step {step}");
        let patch = diff["diff"].as_str().unwrap();
        assert!(patch.contains("+feature"));
        assert!(
            !patch.contains("+main advance"),
            "base changes must stay outside the review"
        );
    }
}
