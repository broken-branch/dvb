//! Verbs any run may call: decide, note, and the read-only issue and issues.

use super::driver::bounded;
use crate::AppState;
use crate::engine::Fail;
use crate::serve::Caller;
use axum::Json;
use axum::extract::State;
use dvb_core::event::OutputItem;
use dvb_core::run_verbs::{DecideBody, IssueBody, IssueReply, NoteBody};
use dvb_core::{Actor, EventKind, Refusal, RefusalCode};

pub async fn decide(
    State(e): State<AppState>,
    c: Caller,
    Json(b): Json<DecideBody>,
) -> Result<Json<serde_json::Value>, Fail> {
    let ev = e.append_checked(
        Some(&c.run.project),
        Actor::Run(c.run.assignment.run.clone()),
        |proj, _now| {
            let run = proj
                .runs
                .get(&c.run.assignment.run)
                .filter(|r| r.ended.is_none())
                .ok_or_else(|| Refusal::new(RefusalCode::NotARun, "the run has ended"))?;
            Ok(vec![EventKind::DecisionRecorded {
                by: run.assignment.role.clone(),
                title: b.title,
                options: b.options,
                swot: b.swot,
                choice: b.choice,
                reason: b.reason,
            }])
        },
    )?;
    Ok(Json(
        serde_json::json!({ "event": ev.first().map(|e| e.id) }),
    ))
}

/// A note: recorded as the run's output, then `NOTES.md` under the state
/// directory (every note; the orchestrator's `{{notes}}` holds a bounded part)
/// is rebuilt from the committed notes, so the file never gets ahead of the log.
pub async fn note(
    State(e): State<AppState>,
    c: Caller,
    Json(b): Json<NoteBody>,
) -> Result<Json<serde_json::Value>, Fail> {
    let ev = e.append_checked(
        Some(&c.run.project),
        Actor::Run(c.run.assignment.run.clone()),
        |_, _| {
            Ok(vec![EventKind::RunOutput {
                resource: None,
                run: c.run.assignment.run.clone(),
                item: OutputItem::Note(bounded(b.text)),
            }])
        },
    )?;
    e.sync_notes(&c.run.project)?;
    Ok(Json(
        serde_json::json!({ "event": ev.first().map(|e| e.id) }),
    ))
}

/// One trusted issue of the run's repository, read by the engine; the run
/// holds no GitHub credential. An untrusted issue is refused with its number
/// and link, never its title.
pub async fn issue(
    State(e): State<AppState>,
    c: Caller,
    Json(b): Json<IssueBody>,
) -> Result<Json<IssueReply>, Fail> {
    let repo = crate::issues::repo_of(&e, &c.run.project)?;
    let trusted =
        crate::issues::read_trusted(&e, &repo, b.number, crate::issues::Reader::Run).await?;
    Ok(Json(crate::issues::reply(&trusted)))
}

/// The open issues of the run's repository, never a body; untrusted ones by
/// number and link only.
pub async fn issues(
    State(e): State<AppState>,
    c: Caller,
) -> Result<Json<dvb_core::run_verbs::IssuesReply>, Fail> {
    let repo = crate::issues::repo_of(&e, &c.run.project)?;
    crate::issues::list(&e, &repo)
        .await
        .map(|listed| Json(crate::issues::run_listing(listed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use dvb_core::event::{CiMode, EndReason};
    use dvb_core::{EventId, ProjectId, RoleId, RunId};

    #[tokio::test]
    async fn decisions_record_the_callers_role_project_and_complete_body() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        let p = ProjectId::new("p");
        e.append_checked(Some(&p), Actor::Owner, |_, _| {
            Ok(vec![EventKind::ProjectAdded {
                id: p.clone(),
                name: "p".into(),
                repo: "local".into(),
                default_branch: "main".into(),
                ci: CiMode::NoCi,
            }])
        })
        .unwrap();
        for role in ["backend", "code-reviewer", "orchestrator"] {
            let run = RunId::new(role);
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunStarted(
                    crate::test_support::assignment::Assignment::new(run.clone())
                        .role(RoleId::new(role))
                        .reason("Work".into())
                        .build(),
                )])
            })
            .unwrap();
            let caller = || Caller {
                run: e.proj().runs[&run].clone(),
            };
            let body = || DecideBody {
                title: "Choose storage".into(),
                options: Some(vec!["SQLite".into(), "Files".into()]),
                swot: Some("Strength: transactions".into()),
                choice: "SQLite".into(),
                reason: "Atomic writes".into(),
            };
            let reply = decide(State(e.clone()), caller(), Json(body()))
                .await
                .unwrap()
                .0;
            let events = e
                .store
                .after(EventId(reply["event"].as_u64().unwrap() - 1))
                .unwrap();
            assert_eq!(events[0].actor, Actor::Run(run.clone()));
            assert_eq!(events[0].project, Some(p.clone()));
            assert_eq!(
                events[0].kind,
                EventKind::DecisionRecorded {
                    by: RoleId::new(role),
                    title: body().title,
                    options: body().options,
                    swot: body().swot,
                    choice: body().choice,
                    reason: body().reason,
                }
            );
            e.append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunEnded {
                    run: run.clone(),
                    reason: EndReason::Completed,
                    detail: None,
                }])
            })
            .unwrap();
            let before = e.store.last_id().unwrap();
            assert!(
                matches!(decide(State(e.clone()), caller(), Json(body())).await, Err(Fail::Refused(r)) if r.code == RefusalCode::NotARun)
            );
            assert_eq!(e.store.last_id().unwrap(), before);
        }
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(
            crate::test_support::events::all(&e)
                .iter()
                .filter(|ev| matches!(ev.kind, EventKind::DecisionRecorded { .. }))
                .count(),
            3
        );
    }
}
