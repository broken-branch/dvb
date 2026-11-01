//! A usage check through the real binary and the Fake provider: `dvb usage
//! --check` and `POST /usage/{vendor}/check` start a reading run (a child process
//! here) that reads the paused vendor's window once. Still full, the pause holds
//! until the new reset time; open again, the pause ends and the waiting run
//! launches.

pub mod support;

use dvb_cli::client::{CliError, Client};
use dvb_core::api::VendorWindow;
use dvb_core::event::{OutputItem, PlannedTask, ResourceWait};
use dvb_core::roles::ReviewKind;
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{
    Actor, Effort, EventKind, GoalId, HireId, RoleId, RunAssignment, RunId, TaskId, Timestamp,
    Vendor,
};
use dvb_engine::{AppState, dispatch, vendors};
use serde_json::json;
use support::planning::script;
use support::{engine_up, git, until};

fn limit(e: &AppState, vendor: Vendor, resets_at: Timestamp) {
    vendors::record(
        e,
        vendor,
        &RateLimitBody {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(resets_at),
        },
    )
    .unwrap();
}

fn kinds(e: &AppState) -> Vec<EventKind> {
    support::events::all(e)
        .into_iter()
        .map(|ev| ev.kind)
        .collect()
}

fn launched(e: &AppState, run: &RunId) -> bool {
    kinds(e)
        .iter()
        .any(|k| matches!(k, EventKind::RunLaunched { run: r } if r == run))
}

async fn refused(c: &Client, vendor: &str) -> String {
    let r: Result<VendorWindow, CliError> = c.post(&format!("/usage/{vendor}/check"), &()).await;
    let Err(CliError::Refused(refusal)) = r else {
        panic!("{vendor}: {r:?}");
    };
    refusal.code.as_str()
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_a_usage_check_keeps_or_ends_the_pause_and_the_waiting_run_launches() {
    let (tmp, e) = support::planning::fixture(
        "dvbu",
        true,
        Actor::Engine,
        |_| {},
        vec![
            EventKind::GoalStated {
                goal: GoalId::new("g"),
                text: "Write a note".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::PlanProposed {
                goal: GoalId::new("g"),
                tasks: vec![PlannedTask {
                    after: vec![],
                    task: TaskId::new("t"),
                    title: "Write a note".into(),
                    role: RoleId::new("backend"),
                    review: ReviewKind::Code,
                    summary: "One edit".into(),
                    routine: false,
                }],
            },
            EventKind::GoalApproved {
                goal: GoalId::new("g"),
            },
        ],
    )
    .await;
    script(&tmp, "builder", &json!([{ "exit": 0 }]));
    let now = e.clock.now();
    limit(&e, Vendor::Fake, now.plus_millis(3_600_000));
    limit(&e, Vendor::Claude, now.plus_millis(3_600_000));
    e.remember();
    let _serving = tokio::spawn(dvb_engine::serve::serve(e.clone()));
    let c = Client::new(e.cfg.paths.engine_sock.clone());
    engine_up(&c).await;

    assert_eq!(refused(&c, "codex").await, "VENDOR_NOT_LIMITED");
    assert_eq!(refused(&c, "claude").await, "NO_USAGE_READING");
    assert!(e.vendors.is_limited(Vendor::Claude));

    let b1 = RunId::new("b1");
    let base = git(&tmp.path().join("origin.git"), &["rev-parse", "main"]);
    dispatch::start_task(
        &e,
        RunAssignment {
            review_stage: None,
            goal_version: None,
            run: b1.clone(),
            task: Some(TaskId::new("t")),
            role: RoleId::new("backend"),
            hire: HireId::new("h-backend"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "Write a note".into(),
            reviews: None,
            base: Some(base),
            routine: false,
        },
        Actor::Engine,
    )
    .await
    .unwrap();
    until("the builder to wait for the usage limit", async || {
        kinds(&e)
            .iter()
            .any(|k| {
                matches!(k, EventKind::RunOutput { run, resource: Some(ResourceWait::UsageLimit), item: OutputItem::Note(_), .. } if *run == b1)
            })
            .then_some(())
    })
    .await;

    // Still full: the pause moves to the reading's reset and the run waits.
    let later = e.clock.now().plus_millis(7_200_000);
    script(
        &tmp,
        "usage",
        &json!([{ "rate_limit": { "used_pct": 100, "window": "7d", "resets_at": later.0 } }]),
    );
    let dir = tmp.path().join("e");
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(dir)
            .args(["usage", "--check", "fake"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8(out.stdout).unwrap();
    assert!(
        line.starts_with("fake: still limited until ") && line.lines().count() == 1,
        "{line}"
    );
    assert!(e.vendors.is_limited(Vendor::Fake));
    assert_eq!(e.vendors.resets_at(Vendor::Fake), Some(later));
    assert_eq!(
        kinds(&e).into_iter().rfind(|k| matches!(
            k,
            EventKind::VendorLimited {
                vendor: Vendor::Fake,
                ..
            }
        )),
        Some(EventKind::VendorLimited {
            vendor: Vendor::Fake,
            resets_at: Some(later),
        })
    );
    assert!(!launched(&e, &b1));

    // Open again: the pause ends and the waiting builder launches.
    script(
        &tmp,
        "usage",
        &json!([{ "rate_limit": { "used_pct": 12, "window": "7d", "resets_at": later.0 } }]),
    );
    let w: VendorWindow = c.post("/usage/fake/check", &()).await.unwrap();
    assert!(!w.limited, "{w:?}");
    assert!((w.used_pct - 12.0).abs() < f64::EPSILON);
    assert!(!e.vendors.is_limited(Vendor::Fake));
    assert!(kinds(&e).contains(&EventKind::VendorResumed {
        vendor: Vendor::Fake
    }));
    until("the waiting builder to launch", async || {
        launched(&e, &b1).then_some(())
    })
    .await;
    until("the builder to end", async || {
        e.proj().runs[&b1].ended.is_some().then_some(())
    })
    .await;
    assert_eq!(refused(&c, "fake").await, "VENDOR_NOT_LIMITED");
}
