//! `dvb goal <project> --issue <n>` over the owner socket with the Fake forge:
//! trusted issues are attached, an untrusted one refuses the goal and the
//! owner is told why.
pub mod support;

use dvb_cli::client::Client;
use dvb_core::event::CiMode;
use dvb_core::forge::{Association, Issue, IssueState, Repo};
use dvb_core::{Actor, EventKind, ProjectId};
use dvb_engine::{Config, Engine, EngineOptions};
use dvb_forge::FakeForge;
use std::process::Command;
use std::sync::Arc;
use support::engine_up;

struct Serving(tokio::task::JoinHandle<std::io::Result<()>>);
impl Drop for Serving {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn issue(number: u64, association: Association) -> Issue {
    Issue {
        number,
        url: format!("https://github.com/org/repo/issues/{number}"),
        title: format!("Issue {number}"),
        body: format!("Body of issue {number}"),
        state: IssueState::Open,
        labels: Vec::new(),
        author: "someone".into(),
        association,
        comments: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn goal_issue_flags_attach_trusted_issues_and_refuse_untrusted_ones() {
    let tmp = tempfile::Builder::new()
        .prefix("dvbissue")
        .tempdir()
        .unwrap();
    let dir = tmp.path().join("e");
    let fake = Arc::new(FakeForge::default());
    let repo = Repo("org/repo".into());
    fake.set_public(&repo);
    fake.set_issue(&repo, issue(1, Association::Owner));
    fake.set_issue(&repo, issue(2, Association::Collaborator));
    fake.set_issue(&repo, issue(3, Association::Contributor));
    let e = Engine::open(
        Config::dev(&dir, env!("CARGO_BIN_EXE_dvb").into()),
        EngineOptions {
            forge: Some(fake),
            ..Default::default()
        },
    )
    .unwrap();
    e.append_checked(Some(&ProjectId::new("p")), Actor::Owner, |_, _| {
        Ok(vec![
            EventKind::ProjectAdded {
                id: ProjectId::new("p"),
                name: "p".into(),
                repo: repo.0.clone(),
                default_branch: "main".into(),
                ci: CiMode::NoCi,
            },
            EventKind::ProjectPaused {},
        ])
    })
    .unwrap();
    let _serving = Serving(tokio::spawn(dvb_engine::serve::serve(e.clone())));
    engine_up(&Client::new(dir.join("engine.sock"))).await;
    let goal = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--dev")
            .arg(&dir)
            .arg("goal")
            .args(args)
            .output()
            .unwrap()
    };

    let refused = goal(&["p", "--issue", "1", "--issue", "3", "Fix", "both"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("ISSUE_UNTRUSTED"), "{stderr}");
    assert!(stderr.contains("https://github.com/org/repo/issues/3"));
    assert!(!stderr.contains("Body of issue 3"));
    assert!(
        stderr.contains("\"Issue 3\"")
            && stderr.contains("not opened by the owner, a member or a collaborator"),
        "the owner is told which issue and why: {stderr}"
    );
    let stated = |e: &dvb_engine::AppState| {
        support::events::all(e)
            .into_iter()
            .filter_map(|ev| {
                if let EventKind::GoalStated { text, issues, .. } = ev.kind {
                    Some((text, issues))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    assert!(stated(&e).is_empty(), "no goal is created");

    let out = goal(&["p", "--issue", "1", "--issue", "2"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains(": Resolve issues #1, #2.")
    );
    let goals = stated(&e);
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].1.iter().map(|i| i.number).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(goals[0].1[1].text.contains("Body of issue 2"));

    let usage = goal(&["p"]);
    assert_eq!(usage.status.code(), Some(2), "text or --issue is required");
}
