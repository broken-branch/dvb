//! Every owner verb the app uses, through the API and through the command line,
//! against equivalent in-process dev engines with the Fake provider path: role add and
//! edit, the chart moves, a task's diff and its verdict (as text and as
//! `--json`). Each pair must record the same event or print the same result.

pub mod support;

use dvb_cli::client::Client;
use dvb_cli::verdict_text;
use dvb_core::api::{ChartMove, DiffView, HireCreated, NewHire, RoleFields, VerdictView};
use dvb_core::event::{CiMode, PlannedTask};
use dvb_core::roles::{HireRecord, HireStatus, ReviewKind, RoleRecord, default_records};
use dvb_core::run_verbs::{Finding, Severity};
use dvb_core::{
    Actor, Effort, Event, EventKind, GoalId, HireId, ProjectId, RoleId, RunAssignment, RunId,
    TaskId, Vendor, VerdictId,
};
use dvb_engine::{Config, Engine, publish};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::{engine_up, git};

fn dvb(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "dvb {args:?}: {out:?}");
    out
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit(repo: &Path, contents: &str) -> String {
    std::fs::write(repo.join("file.txt"), contents).unwrap();
    git(repo, &["add", "."]);
    git(
        repo,
        &[
            "-c",
            "user.name=s",
            "-c",
            "user.email=s@example.invalid",
            "commit",
            "-qm",
            contents,
        ],
    );
    git(repo, &["rev-parse", "HEAD"])
}

/// The roles as the engine holds them, read through the API.
async fn roles(c: &Client, p: &ProjectId) -> Vec<RoleRecord> {
    c.get(&format!("/projects/{p}/roles")).await.unwrap()
}

fn by_slug(roles: &[RoleRecord], slug: &str) -> RoleRecord {
    roles.iter().find(|r| r.slug == slug).unwrap().clone()
}

/// Compare every role field, including chart order, independently of the
/// projection's vector order (changed records are appended on replay).
fn by_identity(mut roles: Vec<RoleRecord>) -> Vec<RoleRecord> {
    roles.sort_by(|a, b| a.id.cmp(&b.id));
    roles
}

/// The role events on the log, in order.
async fn role_events(c: &Client) -> Vec<EventKind> {
    let mut out = Vec::new();
    c.read_events(0, true, |ev: Event| {
        if matches!(
            ev.kind,
            EventKind::RoleAdded(_) | EventKind::RoleChanged { .. }
        ) {
            out.push(ev.kind);
        }
        true
    })
    .await
    .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_each_app_verb_matches_its_api_route() {
    // Short: a socket path must fit in 108 bytes.
    let tmp = tempfile::Builder::new().prefix("dvbp").tempdir().unwrap();
    let dir = tmp.path().join("e");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let base = commit(&repo, "base\n");
    let head = commit(&repo, "head\n");

    // Seed one project with its default roles, and a task with a published
    // change and a review of it.
    let e = Engine::open(
        Config::dev(&dir, PathBuf::from(env!("CARGO_BIN_EXE_dvb"))),
        Default::default(),
    )
    .unwrap();
    publish::create_mirror(&repo.to_string_lossy(), &e.mirror("p"))
        .await
        .unwrap();
    let (p, g, t) = (ProjectId::new("p"), GoalId::new("g"), TaskId::new("t"));
    let mut seed = vec![
        EventKind::ProjectAdded {
            id: p.clone(),
            name: "p".into(),
            repo: repo.to_string_lossy().into(),
            default_branch: "main".into(),
            ci: CiMode::NoCi,
        },
        EventKind::GoalStated {
            goal: g.clone(),
            text: "Goal".into(),
            from_issue: None,
            issues: Vec::new(),
            after: None,
        },
        EventKind::PlanProposed {
            goal: g,
            tasks: vec![PlannedTask {
                after: vec![],
                task: t.clone(),
                title: "x".into(),
                role: RoleId::new("backend"),
                review: ReviewKind::Code,
                summary: "x".into(),
                routine: false,
            }],
        },
    ];
    seed.extend(
        default_records(|d| RoleId::new(d.slug))
            .into_iter()
            .map(EventKind::RoleAdded),
    );
    // Duplicate active hires and a retired hire retain their stored order.
    seed.extend(
        [
            (
                "retired",
                "backend",
                "fake-1",
                Effort::High,
                HireStatus::Retired,
            ),
            (
                "z-default",
                "backend",
                "fake-1",
                Effort::Medium,
                HireStatus::Active,
            ),
            (
                "a-duplicate",
                "backend",
                "fake-next",
                Effort::High,
                HireStatus::Active,
            ),
            (
                "architect-hire",
                "architect",
                "fake-1",
                Effort::High,
                HireStatus::Active,
            ),
        ]
        .into_iter()
        .map(|(id, role, model, effort, status)| {
            EventKind::HireAdded(HireRecord {
                id: HireId::new(id),
                role: RoleId::new(role),
                model: model.into(),
                effort,
                status,
                routine: false,
            })
        }),
    );
    seed.extend([
        EventKind::RunStarted(RunAssignment {
            review_stage: None,
            goal_version: None,
            run: RunId::new("builder"),
            task: Some(t.clone()),
            role: RoleId::new("backend"),
            hire: HireId::new("h"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "Build".into(),
            reviews: None,
            base: Some(base),
            routine: false,
        }),
        EventKind::BranchPublished {
            task: t.clone(),
            round: 1,
            branch: "dvb/t-1".into(),
            head: head.clone(),
        },
        EventKind::VerdictRecorded {
            review_stage: None,
            verdict: VerdictId::new("v"),
            task: t,
            head,
            run: RunId::new("review"),
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            findings: vec![Finding {
                severity: Severity::High,
                file: "file.txt".into(),
                line: std::num::NonZeroU32::new(1),
                title: "Finding".into(),
                detail: "Details".into(),
                open: true,
            }],
            summary: "Needs work".into(),
        },
    ]);
    let api_dir = tmp.path().join("a");
    let api_engine = Engine::open(
        Config::dev(&api_dir, PathBuf::from(env!("CARGO_BIN_EXE_dvb"))),
        Default::default(),
    )
    .unwrap();
    e.append_checked(Some(&p), Actor::Owner, |_, _| Ok(seed.clone()))
        .unwrap();
    api_engine
        .append_checked(Some(&p), Actor::Owner, |_, _| Ok(seed))
        .unwrap();
    tokio::spawn(async move {
        if let Err(err) = dvb_engine::start(api_engine).await {
            eprintln!("API engine: {err}");
        }
    });
    tokio::spawn(async move {
        if let Err(err) = dvb_engine::start(e).await {
            eprintln!("engine: {err}");
        }
    });
    let c = Client::new(dir.join("engine.sock"));
    engine_up(&c).await;
    let api_client = Client::new(api_dir.join("engine.sock"));
    engine_up(&api_client).await;

    // role list prints what the API lists.
    let api = roles(&c, &p).await;
    let listed = text(&dvb(&dir, &["role", "list", "p"]));
    assert_eq!(listed.lines().count(), api.len(), "{listed}");
    assert!(listed.contains("backend"), "{listed}");

    // hires lists the same records as GET /hires, grouped in role-list order.
    let api_hires: Vec<HireRecord> = api_client.get("/hires?project=p").await.unwrap();
    for all in [false, true] {
        let args = if all {
            vec!["hires", "p", "--all"]
        } else {
            vec!["hires", "p"]
        };
        let listed = text(&dvb(&dir, &args));
        let expected = api
            .iter()
            .flat_map(|role| {
                api_hires
                    .iter()
                    .filter(move |h| h.role == role.id && (all || h.status == HireStatus::Active))
                    .map(move |h| {
                        format!(
                            "{}  {}  {}  {}  {}{}",
                            role.name,
                            role.id,
                            h.id,
                            h.model,
                            h.effort.as_str(),
                            if h.status == HireStatus::Retired {
                                "  [retired]"
                            } else if h.id == HireId::new("z-default") {
                                "  [default]"
                            } else {
                                ""
                            }
                        )
                    })
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(listed, expected);
        assert!(listed.contains("z-default  fake-1  medium  [default]"));
        assert_eq!(listed.contains("[retired]"), all);
    }
    // Default builder admission skips an earlier hire outside the selected pool.
    c.put::<_, dvb_core::api::Settings>(
        "/settings",
        &serde_json::json!({"values": {"model_pool": ["fake-next"]}}),
    )
    .await
    .unwrap();
    let listed = text(&dvb(&dir, &["hires", "p"]));
    assert!(
        listed.contains("a-duplicate  fake-next  high  [default]"),
        "{listed}"
    );
    assert!(
        !listed.contains("z-default  fake-1  medium  [default]"),
        "{listed}"
    );
    c.put::<_, dvb_core::api::Settings>(
        "/settings",
        &serde_json::json!({"values": {"model_pool": ["fake-1", "fake-next"]}}),
    )
    .await
    .unwrap();

    // Hire replacement reports every retired record from the mutation itself.
    let output = text(&dvb(
        &dir,
        &["hire", "backend", "fake-next", "--effort", "high"],
    ));
    assert!(
        output.contains("Retired fake-1 (z-default)  medium"),
        "{output}"
    );
    assert!(
        output.contains("Retired fake-next (a-duplicate)  high"),
        "{output}"
    );
    let receipt: HireCreated = api_client
        .post(
            "/hires",
            &NewHire {
                role: RoleId::new("backend"),
                model: "fake-next".into(),
                effort: Effort::High,
                routine: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        receipt
            .retired
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        vec!["z-default", "a-duplicate"]
    );
    let cli_hires: Vec<HireRecord> = c.get("/hires?project=p").await.unwrap();
    let mut expected: Vec<HireRecord> = api_client.get("/hires?project=p").await.unwrap();
    let new_id = cli_hires
        .iter()
        .find(|h| h.role == RoleId::new("backend") && h.status == HireStatus::Active)
        .unwrap()
        .id
        .clone();
    for h in &mut expected {
        if h.id == receipt.hire.id {
            h.id = new_id.clone();
        }
    }
    assert_eq!(cli_hires, expected);

    // role add: the same fields, once each way, record the same event.
    let backend = by_slug(&api, "backend").id;
    let cli_add = text(&dvb(
        &dir,
        &[
            "role",
            "add",
            "p",
            "--name",
            "Tester",
            "--job",
            "Tests",
            "--lane",
            "Test plans",
            "--reports-to",
            "backend",
            "--review-instructions",
            "Run it",
        ],
    ));
    assert!(cli_add.starts_with("Role Tester added"), "{cli_add}");
    let fields = RoleFields {
        name: "Tester".into(),
        job: "Tests".into(),
        lane: "Test plans".into(),
        lead: false,
        reports_to: Some(backend.clone()),
        reviewed_by: None,
        review_instructions: "Run it".into(),
        group: None,
        order: 0,
    };
    let cli_added = by_slug(&roles(&c, &p).await, "tester");
    let api_added: RoleRecord = api_client
        .post(&format!("/projects/{p}/roles"), &fields)
        .await
        .unwrap();
    // Only the generated role id differs; every supplied field must agree.
    let same = |a: &RoleRecord, b: &RoleRecord| RoleRecord {
        id: b.id.clone(),
        ..a.clone()
    };
    assert_eq!(same(&api_added, &cli_added), cli_added);
    let cli_events = role_events(&c).await;
    let api_events = role_events(&api_client).await;
    assert_eq!(
        cli_events.last(),
        Some(&EventKind::RoleAdded(cli_added.clone()))
    );
    assert_eq!(
        api_events.last(),
        Some(&EventKind::RoleAdded(api_added.clone()))
    );

    // role edit: the same change, once each way, records the same new role.
    dvb(
        &dir,
        &[
            "role", "edit", "p", "tester", "--job", "Checks", "--lead", "true",
        ],
    );
    let cli_edited = by_slug(&roles(&c, &p).await, "tester");
    assert_eq!(
        cli_edited,
        RoleRecord {
            job: "Checks".into(),
            lead: true,
            ..cli_added.clone()
        }
    );
    let api_edited: RoleRecord = api_client
        .patch(
            &format!("/roles/{}", api_added.id),
            &RoleFields {
                job: "Checks".into(),
                lead: true,
                ..fields
            },
        )
        .await
        .unwrap();
    assert_eq!(same(&api_edited, &cli_edited), cli_edited);
    assert_eq!(
        role_events(&c).await.last(),
        Some(&EventKind::RoleChanged {
            role: cli_edited.clone(),
            restored_from: None,
        })
    );
    assert_eq!(
        role_events(&api_client).await.last(),
        Some(&EventKind::RoleChanged {
            role: api_edited.clone(),
            restored_from: None,
        })
    );

    // The charts start with identical role fields. Normalize only the new id,
    // including references to it after swap, and inspect CLI state first.
    let normalize = |mut records: Vec<RoleRecord>| {
        for r in &mut records {
            if r.id == api_added.id {
                r.id = cli_added.id.clone();
            }
            if r.reports_to.as_ref() == Some(&api_added.id) {
                r.reports_to = Some(cli_added.id.clone());
            }
            if r.reviewed_by.as_ref() == Some(&api_added.id) {
                r.reviewed_by = Some(cli_added.id.clone());
            }
        }
        records
    };
    let mut expected = roles(&c, &p).await;
    assert_eq!(
        by_identity(normalize(roles(&api_client, &p).await)),
        by_identity(expected.clone())
    );
    let root = by_slug(&expected, "orchestrator");
    let moves = [
        (
            vec!["chart", "p", "report", "tester", "--to", "orchestrator"],
            ChartMove::Report {
                role: api_added.id.clone(),
                parent: Some(root.id.clone()),
            },
        ),
        (
            vec!["chart", "p", "group", "--name", "crew", "tester", "backend"],
            ChartMove::Group {
                roles: vec![api_added.id.clone(), backend.clone()],
                group: Some("crew".into()),
            },
        ),
        (
            vec!["chart", "p", "reorder", "tester", "--before", "backend"],
            ChartMove::Reorder {
                role: api_added.id.clone(),
                before: Some(backend.clone()),
            },
        ),
        (
            vec!["chart", "p", "swap", "tester", "orchestrator"],
            ChartMove::Swap {
                role: api_added.id.clone(),
                lead: root.id.clone(),
            },
        ),
    ];
    for (i, (args, movement)) in moves.into_iter().enumerate() {
        match i {
            0 => {
                let order = expected.iter().map(|r| r.order).max().unwrap() + 1;
                for r in &mut expected {
                    if r.id == cli_added.id {
                        r.reports_to = Some(root.id.clone());
                        r.group = None;
                        r.order = order;
                    }
                    if r.id == backend {
                        r.lead = false;
                    }
                }
            }
            1 => {
                for r in &mut expected {
                    if r.id == cli_added.id || r.id == backend {
                        r.group = Some("crew".into());
                    }
                }
            }
            2 => {
                // All non-root roles are siblings; the tester moves from last
                // to immediately before the backend, shifting later siblings.
                let mut siblings: Vec<_> = expected
                    .iter()
                    .filter(|r| r.id != root.id && r.id != cli_added.id)
                    .cloned()
                    .collect();
                // Equal orders retain the current persisted sibling sequence,
                // including records appended by the preceding group move.
                siblings.sort_by_key(|r| r.order);
                let mut siblings: Vec<_> = siblings.into_iter().map(|r| r.id).collect();
                let position = siblings.iter().position(|id| id == &backend).unwrap();
                siblings.insert(position, cli_added.id.clone());
                for (order, id) in siblings.iter().enumerate() {
                    expected.iter_mut().find(|r| &r.id == id).unwrap().order =
                        u32::try_from(order).unwrap();
                }
            }
            3 => {
                let tester = by_slug(&expected, "tester");
                for r in &mut expected {
                    if r.id == cli_added.id {
                        r.reports_to = root.reports_to.clone();
                        r.lead = root.lead;
                        r.group = root.group.clone();
                        r.order = root.order;
                    } else if r.id == root.id {
                        r.reports_to = Some(cli_added.id.clone());
                        r.lead = tester.lead;
                        r.group = tester.group.clone();
                        r.order = tester.order;
                    } else if r.reports_to.as_ref() == Some(&root.id) {
                        r.reports_to = Some(cli_added.id.clone());
                    }
                }
            }
            _ => unreachable!("four chart moves"),
        }
        dvb(&dir, &args);
        let cli_result = roles(&c, &p).await;
        assert_eq!(
            by_identity(cli_result.clone()),
            by_identity(expected),
            "CLI {args:?}"
        );
        let api_result: Vec<RoleRecord> = api_client
            .post(&format!("/projects/{p}/chart"), &movement)
            .await
            .unwrap();
        assert_eq!(
            by_identity(normalize(api_result)),
            by_identity(cli_result.clone()),
            "API {args:?}"
        );
        assert_eq!(
            by_identity(normalize(roles(&api_client, &p).await)),
            by_identity(cli_result.clone())
        );
        // The next move derives its expectation from this verified persisted
        // sequence, rather than a pre-mutation vector or POST response.
        expected = cli_result;
    }

    // task diff and verdict print what the API returns.
    let d: DiffView = c.get("/tasks/t/diff").await.unwrap();
    assert!(d.diff.contains("+head"), "{}", d.diff);
    assert_eq!(text(&dvb(&dir, &["task", "diff", "t"])), d.diff);
    let v: VerdictView = c.get("/tasks/t/verdict").await.unwrap();
    let said = text(&dvb(&dir, &["task", "verdict", "t"]));
    assert_eq!(said.trim_end(), verdict_text(&v));
    assert!(
        said.contains("Needs work")
            && said.contains("[open] ")
            && said.contains("file.txt:1")
            && said.contains("Details"),
        "{said}"
    );
    // With --json they print the API's objects.
    let diff: DiffView =
        serde_json::from_slice(&dvb(&dir, &["task", "diff", "t", "--json"]).stdout).unwrap();
    assert_eq!(diff, d);
    let verdict: VerdictView =
        serde_json::from_slice(&dvb(&dir, &["task", "verdict", "t", "--json"]).stdout).unwrap();
    assert_eq!(verdict, v);
    assert!(verdict.open_findings > 0 && !verdict.findings.is_empty());

    // The default and explicit medium reach the same hire payload. Low parses,
    // then retains the model-pool refusal because Fake does not support it.
    for args in [
        vec!["hire", "backend", "fake-1"],
        vec!["hire", "backend", "fake-1", "--effort", "medium"],
    ] {
        dvb(&dir, &args);
        let hires: Vec<HireRecord> = c.get("/hires?project=p").await.unwrap();
        let active = hires
            .iter()
            .find(|hire| hire.role == RoleId::new("backend") && hire.status == HireStatus::Active)
            .unwrap();
        assert_eq!(active.effort, Effort::Medium);
    }
    let before: Vec<HireRecord> = c.get("/hires?project=p").await.unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(&dir)
        .args(["hire", "backend", "fake-1", "--effort", "low"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("MODEL_NOT_IN_POOL"),
        "{output:?}"
    );
    assert_eq!(
        c.get::<Vec<HireRecord>>("/hires?project=p").await.unwrap(),
        before
    );
}

/// Real CLI set/read/replace/clear, invalid arguments, status placement and local
/// time, against the same API calls on an equivalent Fake-clock engine.
#[tokio::test(flavor = "multi_thread")]
async fn socket_only_command_matches_api_and_global_project_status() {
    use dvb_core::api::{SetOnly, Settings};
    use dvb_core::model::OnlyModels;
    use dvb_core::{FakeClock, Timestamp};
    use std::sync::Arc;

    let tmp = tempfile::Builder::new()
        .prefix("dvbonly")
        .tempdir()
        .unwrap();
    let clock = Arc::new(FakeClock::at(Timestamp(1_000_000)));
    let mut engines = Vec::new();
    for name in ["cli", "api"] {
        let dir = tmp.path().join(name);
        let e = Engine::open(
            Config::dev(&dir, env!("CARGO_BIN_EXE_dvb").into()),
            dvb_engine::EngineOptions {
                clock: Some(clock.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let (roles, hires) = dvb_core::roles::default_records(|d| RoleId::new(d.slug))
            .into_iter()
            .map(|role| {
                let hire = HireRecord {
                    id: HireId::new(format!("h-{}", role.slug)),
                    role: role.id.clone(),
                    model: "fake-1".into(),
                    effort: Effort::Medium,
                    status: HireStatus::Active,
                    routine: false,
                };
                (EventKind::RoleAdded(role), EventKind::HireAdded(hire))
            })
            .unzip::<_, _, Vec<_>, Vec<_>>();
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
        seed.extend(roles.into_iter().chain(hires));
        e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| Ok(seed))
            .unwrap();
        let server = tokio::spawn(dvb_engine::start(e.clone()));
        let c = Client::new(dir.join("engine.sock"));
        engine_up(&c).await;
        engines.push((dir, e, c, server));
    }
    let (dir, e, c, _) = &engines[0];
    let (_, _, api_client, _) = &engines[1];
    let before: Settings = c.get("/settings").await.unwrap();
    let hires: Vec<HireRecord> = c.get("/hires?project=p").await.unwrap();
    let global = text(&dvb(dir, &["status"]));
    let project = text(&dvb(dir, &["status", "p"]));
    let bar = text(&dvb(dir, &["status", "--bar"]));
    assert_eq!(text(&dvb(dir, &["only"])), "No model switch is active.\n");
    let set_args = SetOnly {
        models: vec!["fake-next".into(), "fake-1".into()],
        hours: 1,
    };
    let api_only: OnlyModels = api_client.put("/only", &set_args).await.unwrap();
    let result = text(&dvb(dir, &["only", "fake-next", "fake-1", "--hours", "1"]));
    let cli_only: Option<OnlyModels> = c.get("/only").await.unwrap();
    assert_eq!(cli_only, Some(api_only.clone()));
    assert_eq!(result, format!("{}\n", dvb_cli::only_line(&api_only)));
    assert_eq!(text(&dvb(dir, &["only"])), result);
    for args in [
        vec!["status"],
        vec!["status", "p"],
        vec!["status", "p", "--all"],
    ] {
        let status = text(&dvb(dir, &args));
        assert_eq!(status.lines().nth(1), Some(result.trim_end()));
        assert_eq!(status.lines().next(), Some("0 running, 0 need you"));
    }
    assert_eq!(text(&dvb(dir, &["status", "--bar"])), bar);
    let local = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--dev")
        .arg(dir)
        .arg("only")
        .env("TZ", "Pacific/Honolulu")
        .output()
        .unwrap();
    assert!(local.status.success());
    assert!(text(&local).contains("1969-12-31 15:16"), "{local:?}");
    // Exact pool/selected ids have one plainly listed set of valid choices.
    let _: Settings = c
        .put(
            "/settings",
            &serde_json::json!({"values":{"model_pool":["fake-1"]}}),
        )
        .await
        .unwrap();
    for id in ["outside-pool", "fake-next", "fake", "FAKE-1"] {
        let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(dir)
            .args(["only", id, "--hours", "1"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("MODEL_NOT_IN_POOL") && err.contains("valid model ids: fake-1"),
            "{err}"
        );
    }
    let _: Settings = c.put("/settings", &before).await.unwrap();
    for hours in ["0", "169", "-1", "1.5", "abc"] {
        let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(dir)
            .args(["only", "fake-1", "--hours", hours])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{hours}: {out:?}");
    }
    for args in [
        vec!["only", "fake-1"],
        vec!["only", "--hours", "1"],
        vec!["only", "--clear", "fake-1", "--hours", "1"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
    let replacement: OnlyModels = api_client
        .put(
            "/only",
            &SetOnly {
                models: vec!["fake-1".into()],
                hours: 168,
            },
        )
        .await
        .unwrap();
    dvb(dir, &["only", "fake-1", "--hours", "168"]);
    assert_eq!(
        c.get::<Option<OnlyModels>>("/only").await.unwrap(),
        Some(replacement)
    );
    let _: Option<OnlyModels> = api_client
        .post("/only/clear", &serde_json::Value::Null)
        .await
        .unwrap();
    assert_eq!(
        text(&dvb(dir, &["only", "--clear"])),
        "No model switch is active.\n"
    );
    dvb(dir, &["only", "--clear"]);
    assert_eq!(c.get::<Option<OnlyModels>>("/only").await.unwrap(), None);
    assert_eq!(text(&dvb(dir, &["status"])), global);
    assert_eq!(text(&dvb(dir, &["status", "p"])), project);
    // An overdue read also disappears from both status forms and deduplicates
    // with the timer and clear. Live timer/restart evidence is in only_models.
    dvb(dir, &["only", "fake-next", "--hours", "1"]);
    clock.advance_millis(3_600_000);
    assert_eq!(text(&dvb(dir, &["status"])), global);
    assert_eq!(text(&dvb(dir, &["status", "p"])), project);
    assert_eq!(c.get::<Settings>("/settings").await.unwrap(), before);
    assert_eq!(
        c.get::<Vec<HireRecord>>("/hires?project=p").await.unwrap(),
        hires
    );
    let events = support::events::all(e);
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::OnlyStarted(_)))
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::OnlyEnded { .. }))
            .count(),
        3
    );
    for (_, _, _, server) in engines {
        server.abort();
    }
}
