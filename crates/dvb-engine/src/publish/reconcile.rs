//! Head-pinned CI evidence, merge reconciliation, goal issue closure and branch
//! cleanup. Dispatch owns polling and merge scheduling.
use crate::{AppState, Fail};
use dvb_core::event::ChecksStatus;
use dvb_core::forge::{CheckObservation, Forge, MergeError, PrObservation, PrState, Repo};
use dvb_core::{Actor, EventKind, GoalId, TaskId};
use std::time::Duration;

pub fn aggregate(expected: &[String], observations: &[CheckObservation]) -> ChecksStatus {
    if expected.is_empty() {
        return ChecksStatus::Missing;
    }
    let mut states = Vec::new();
    for name in expected {
        let matches: Vec<_> = observations.iter().filter(|c| &c.name == name).collect();
        if matches.is_empty() {
            states.push(ChecksStatus::Missing);
        }
        states.extend(matches.into_iter().map(|c| c.status));
    }
    for status in [
        ChecksStatus::Unreadable,
        ChecksStatus::Failed,
        ChecksStatus::Missing,
        ChecksStatus::Pending,
    ] {
        if states.contains(&status) {
            return status;
        }
    }
    ChecksStatus::Success
}

/// Read before retrying a lost merge reply. A merged PR must carry its commit,
/// and must still name the head whose review authorized this attempt.
pub async fn merge_reconciled(
    forge: &dyn Forge,
    repo: &Repo,
    pr: u64,
    head: &str,
    title: &str,
    seen: &PrObservation,
) -> Result<String, MergeError> {
    if seen.head != head {
        return Err(MergeError::HeadMoved);
    }
    if seen.state == PrState::Merged {
        return seen
            .merge_commit
            .clone()
            .ok_or_else(|| MergeError::Refused("merged PR has no merge commit".into()));
    }
    if seen.state != PrState::Open {
        return Err(MergeError::Refused("PR is closed".into()));
    }
    match forge.merge(repo, pr, head, title).await {
        Ok(sha) => Ok(sha),
        Err(MergeError::Forge(error)) => match forge.pr(repo, pr).await {
            Ok(now) if now.state == PrState::Merged && now.head == head => now
                .merge_commit
                .ok_or_else(|| MergeError::Refused("merged PR has no merge commit".into())),
            Ok(now) if now.head != head => Err(MergeError::HeadMoved),
            Ok(_) | Err(_) => Err(MergeError::Forge(error)),
        },
        Err(error @ (MergeError::HeadMoved | MergeError::Refused(_))) => Err(error),
    }
}

/// Idempotent task branch cleanup, independent of the goal issue effect.
pub async fn cleanup(e: &AppState, task: &TaskId) -> Result<(), Fail> {
    let (repo, branch) = {
        let proj = e.proj();
        let t = proj
            .tasks
            .get(task)
            .ok_or_else(|| Fail::NotFound("task".into()))?;
        let p = proj
            .projects
            .get(&t.view.project)
            .ok_or_else(|| Fail::NotFound("project".into()))?;
        let result = (Repo(p.view.repo.clone()), t.branch.clone());
        drop(proj);
        result
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    if let Some(branch) = branch {
        forge
            .delete_branch(&repo, &branch)
            .await
            .map_err(Fail::internal)?;
    }
    Ok(())
}

/// A goal-completion effect with its own success receipt per issue. A failed or
/// lost reply leaves the durable GoalDone pending and can be retried without task cleanup.
pub async fn close_goal_issue(e: &AppState, goal: &GoalId) -> Result<(), Fail> {
    let (project, repo, issues) = {
        let proj = e.proj();
        let Some(g) = proj.goals.get(goal) else {
            return Ok(());
        };
        if g.state != dvb_core::api::GoalState::Done {
            return Ok(());
        }
        let issues = g.open_issues();
        if issues.is_empty() {
            return Ok(());
        }
        let project = proj
            .projects
            .get(&g.project)
            .ok_or_else(|| Fail::NotFound("goal project is missing".into()))?;
        let result = (g.project.clone(), Repo(project.view.repo.clone()), issues);
        drop(proj);
        result
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    for issue in issues {
        forge
            .close_issue(&repo, issue)
            .await
            .map_err(Fail::internal)?;
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![EventKind::GoalIssueClosed {
                goal: goal.clone(),
                issue,
            }])
        })?;
    }
    Ok(())
}

pub async fn retry_goal_issue(
    e: AppState,
    goal: GoalId,
    mut failure: Option<Fail>,
) -> Result<(), Fail> {
    let mut delay_ms = 60_000;
    loop {
        if let Some(error) = failure.take() {
            if matches!(error, Fail::NotFound(_)) {
                return Err(error);
            }
            let rate_limit = if let Fail::Internal(crate::InternalError::Forge(forge)) = &error {
                if !forge.retryable {
                    return Err(error);
                }
                forge.rate_limit
            } else {
                None
            };
            let now = e.clock.now();
            let next = rate_limit.map_or_else(
                || now.plus_millis(delay_ms),
                |limit| limit.not_before(now, delay_ms),
            );
            eprintln!(
                "dvb engine: close goal issue: {error}; retry at {}",
                next.millis()
            );
            tokio::time::sleep(Duration::from_millis(
                u64::try_from(next.millis().saturating_sub(now.millis())).unwrap_or(0),
            ))
            .await;
            delay_ms = dvb_core::state::backoff_millis(2, delay_ms, 3_600_000);
        }
        let _effect = e.update.effect().await;
        match close_goal_issue(&e, &goal).await {
            Ok(()) => return Ok(()),
            Err(error) => failure = Some(error),
        }
    }
}

/// Restart recovery also repairs a crash between recording the last merge and
/// recording completion. It never needs a task to still be in Merging.
pub fn restore_goal_effects(e: &AppState) -> Result<(), Fail> {
    let goals: Vec<_> = e.proj().goals.keys().cloned().collect();
    for goal in goals {
        e.record_goal_completion(&goal)?;
        let pending = {
            let p = e.proj();
            let g = &p.goals[&goal];
            let pending = g.state == dvb_core::api::GoalState::Done && !g.open_issues().is_empty();
            drop(p);
            pending
        };
        if pending {
            crate::spawn("goal issue", retry_goal_issue(e.clone(), goal, None));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::forge::ForgeError;
    fn issue_fixture(
        issue: Option<u64>,
    ) -> (
        tempfile::TempDir,
        AppState,
        std::sync::Arc<dvb_core::FakeClock>,
        Repo,
        GoalId,
        std::sync::Arc<dvb_forge::FakeForge>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(dvb_core::FakeClock::at(dvb_core::Timestamp(0)));
        let fake = std::sync::Arc::new(dvb_forge::FakeForge::default().with_clock(clock.clone()));
        let e = crate::Engine::open(
            crate::Config::dev(tmp.path(), "unused".into()),
            crate::EngineOptions {
                clock: Some(clock.clone()),
                forge: Some(fake.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let project = dvb_core::ProjectId::new("p");
        let goal = GoalId::new("g");
        let repo = Repo("org/repo".into());
        e.append_checked(Some(&project), Actor::Engine, |_, _| {
            Ok(vec![
                EventKind::ProjectAdded {
                    id: project.clone(),
                    name: "project".into(),
                    repo: repo.0.clone(),
                    default_branch: "main".into(),
                    ci: dvb_core::event::CiMode::NoCi,
                },
                EventKind::GoalStated {
                    goal: goal.clone(),
                    text: "goal".into(),
                    from_issue: issue,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::GoalDone { goal: goal.clone() },
            ])
        })
        .unwrap();
        (tmp, e, clock, repo, goal, fake)
    }

    #[test]
    fn recovery_skips_done_goals_without_pending_issues_without_a_runtime() {
        let (_tmp, e, _, _, goal, _) = issue_fixture(None);
        restore_goal_effects(&e).unwrap();
        let (_tmp, e, _, _, goal_with_issue, _) = issue_fixture(Some(4));
        e.append_checked(
            Some(&dvb_core::ProjectId::new("p")),
            Actor::Engine,
            |_, _| {
                Ok(vec![EventKind::GoalIssueClosed {
                    goal: goal_with_issue,
                    issue: 4,
                }])
            },
        )
        .unwrap();
        restore_goal_effects(&e).unwrap();
        assert_eq!(e.proj().goals[&goal].closed_issues, vec![4]);
    }

    #[tokio::test]
    async fn every_issue_the_goal_carries_is_closed_once_with_its_own_receipt() {
        let (_tmp, e, _, repo, goal, fake) = issue_fixture(Some(4));
        e.append_checked(
            Some(&dvb_core::ProjectId::new("p")),
            Actor::Engine,
            |_, _| {
                Ok(vec![EventKind::GoalStated {
                    goal: goal.clone(),
                    text: "goal".into(),
                    from_issue: None,
                    issues: [4, 7]
                        .map(|number| dvb_core::event::GoalIssue {
                            number,
                            url: format!("https://github.com/org/repo/issues/{number}"),
                            text: String::new(),
                        })
                        .to_vec(),
                    after: None,
                }])
            },
        )
        .unwrap();
        e.append_checked(
            Some(&dvb_core::ProjectId::new("p")),
            Actor::Engine,
            |_, _| Ok(vec![EventKind::GoalDone { goal: goal.clone() }]),
        )
        .unwrap();
        fake.fail_next(
            "close_issue",
            ForgeError {
                rate_limit: None,
                retryable: true,
                detail: "lost".into(),
            },
        );
        close_goal_issue(&e, &goal).await.unwrap_err();
        assert!(e.proj().goals[&goal].closed_issues.is_empty());
        close_goal_issue(&e, &goal).await.unwrap();
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4, 7]);
        assert_eq!(e.proj().goals[&goal].closed_issues, vec![4, 7]);
    }

    #[tokio::test]
    async fn closing_an_issue_uses_projected_facts_without_reading_the_log() {
        let (_tmp, e, _, repo, goal, fake) = issue_fixture(Some(4));
        let conn = rusqlite::Connection::open(e.cfg.paths.state.join("dvb.sqlite")).unwrap();
        conn.execute(
            "UPDATE events SET payload = 'invalid json' WHERE id = (SELECT MIN(id) FROM events)",
            [],
        )
        .unwrap();
        assert!(e.store.after(dvb_core::EventId(0)).is_err());
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4]);
        assert_eq!(e.proj().goals[&goal].closed_issues, vec![4]);
    }

    #[tokio::test]
    async fn a_failed_issue_receipt_retains_the_store_source_and_can_be_retried() {
        let (_tmp, e, _, repo, goal, fake) = issue_fixture(Some(4));
        let conn = rusqlite::Connection::open(e.cfg.paths.state.join("dvb.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER reject_receipt BEFORE INSERT ON events
             WHEN NEW.kind = 'goal.issue_closed'
             BEGIN SELECT RAISE(ABORT, 'receipt rejected'); END;",
        )
        .unwrap();
        let error = close_goal_issue(&e, &goal).await.unwrap_err();
        assert!(error.to_string().contains("receipt rejected"));
        assert!(matches!(
            error,
            Fail::Internal(crate::InternalError::Store(dvb_store::StoreError::Sql(_)))
        ));
        assert!(e.proj().goals[&goal].closed_issues.is_empty());
        assert_eq!(fake.closed_issues(&repo), vec![4]);
        conn.execute_batch("DROP TRIGGER reject_receipt").unwrap();
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(e.proj().goals[&goal].closed_issues, vec![4]);
        assert_eq!(
            crate::test_support::events::all(&e)
                .iter()
                .filter(|event| matches!(event.kind, EventKind::GoalIssueClosed { .. }))
                .count(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue_retry_backs_off_and_stops_on_permanent_errors() {
        let (_tmp, e, clock, repo, goal, fake) = issue_fixture(Some(4));
        for retryable in [true, true, false] {
            fake.fail_next(
                "close_issue",
                dvb_core::forge::ForgeError {
                    retryable,
                    detail: if retryable {
                        "temporary"
                    } else {
                        "deleted issue"
                    }
                    .into(),
                    rate_limit: None,
                },
            );
        }
        let job = tokio::spawn(retry_goal_issue(e.clone(), goal.clone(), None));
        tokio::task::yield_now().await;
        let count = || {
            fake.calls()
                .iter()
                .filter(|c| c.as_str() == "close_issue")
                .count()
        };
        assert_eq!(count(), 1);
        clock.advance_millis(59_999);
        tokio::time::advance(Duration::from_millis(59_999)).await;
        tokio::task::yield_now().await;
        assert_eq!(count(), 1);
        clock.advance_millis(1);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(count(), 2);
        clock.advance_millis(120_000);
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        assert_eq!(count(), 3);
        assert!(job.is_finished());
        let error = job.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("deleted issue"));
        assert!(matches!(
            error,
            Fail::Internal(crate::InternalError::Forge(source))
                if !source.retryable && source.rate_limit.is_none()
        ));
        assert!(fake.closed_issues(&repo).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn issue_retry_respects_the_shared_budget_reset() {
        let (_tmp, e, clock, repo, goal, fake) = issue_fixture(Some(4));
        fake.fail_next(
            "close_issue",
            ForgeError {
                retryable: true,
                detail: "rate limited".into(),
                rate_limit: Some(dvb_core::forge::RateLimit {
                    reset: Some(dvb_core::Timestamp(90_000)),
                }),
            },
        );
        let job = tokio::spawn(retry_goal_issue(e, goal, None));
        tokio::task::yield_now().await;
        clock.advance_millis(90_999);
        tokio::time::advance(Duration::from_millis(90_999)).await;
        tokio::task::yield_now().await;
        assert!(!job.is_finished());
        assert!(fake.closed_issues(&repo).is_empty());
        clock.advance_millis(1);
        tokio::time::advance(Duration::from_millis(1)).await;
        job.await.unwrap().unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4]);
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.as_str() == "close_issue")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn historical_restatements_keep_the_first_linked_issue_and_closed_receipt() {
        let (_tmp, e, _, repo, goal, fake) = issue_fixture(None);
        for issue in [Some(4), Some(8), None] {
            e.append_checked(
                Some(&dvb_core::ProjectId::new("p")),
                Actor::Engine,
                |_, _| {
                    Ok(vec![
                        EventKind::GoalStated {
                            goal: goal.clone(),
                            text: "restated".into(),
                            from_issue: issue,
                            issues: Vec::new(),
                            after: None,
                        },
                        EventKind::GoalDone { goal: goal.clone() },
                    ])
                },
            )
            .unwrap();
        }
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4]);
        e.append_checked(
            Some(&dvb_core::ProjectId::new("p")),
            Actor::Engine,
            |_, _| {
                Ok(vec![
                    EventKind::GoalStated {
                        goal: goal.clone(),
                        text: "restated again".into(),
                        from_issue: Some(8),
                        issues: Vec::new(),
                        after: None,
                    },
                    EventKind::GoalDone { goal: goal.clone() },
                ])
            },
        )
        .unwrap();
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4]);
    }

    #[tokio::test]
    async fn closed_issue_receipt_replays_and_unlinked_goals_skip_recovery() {
        let (tmp, e, clock, repo, goal, fake) = issue_fixture(Some(4));
        close_goal_issue(&e, &goal).await.unwrap();
        close_goal_issue(&e, &goal).await.unwrap();
        assert_eq!(fake.closed_issues(&repo), vec![4]);
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.as_str() == "close_issue")
                .count(),
            1
        );
        drop(e);
        let fake = std::sync::Arc::new(dvb_forge::FakeForge::default().with_clock(clock.clone()));
        let e = crate::Engine::open(
            crate::Config::dev(tmp.path(), "unused".into()),
            crate::EngineOptions {
                clock: Some(clock),
                forge: Some(fake.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        close_goal_issue(&e, &goal).await.unwrap();
        assert!(fake.calls().is_empty());
        let (_tmp, e, _, _, goal, fake) = issue_fixture(None);
        close_goal_issue(&e, &goal).await.unwrap();
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn all_expected_observations_from_both_apis_must_succeed() {
        let expected = vec!["rust".into(), "legacy".into()];
        let good = vec![
            CheckObservation {
                name: "rust".into(),
                status: ChecksStatus::Success,
                run: None,
                conclusion: None,
                url: None,
            },
            CheckObservation {
                name: "legacy".into(),
                status: ChecksStatus::Success,
                run: None,
                conclusion: None,
                url: None,
            },
        ];
        assert_eq!(aggregate(&expected, &good), ChecksStatus::Success);
        assert_eq!(aggregate(&[], &good), ChecksStatus::Missing);
        assert_eq!(aggregate(&expected, &[]), ChecksStatus::Missing);
        for status in [
            ChecksStatus::Missing,
            ChecksStatus::Pending,
            ChecksStatus::Unreadable,
            ChecksStatus::Failed,
        ] {
            let mut mixed = good.clone();
            mixed.push(CheckObservation {
                name: "rust".into(),
                status,
                run: None,
                conclusion: None,
                url: None,
            });
            assert_eq!(aggregate(&expected, &mixed), status);
        }
        // Exercise each precedence boundary with all lower-priority statuses.
        let precedence = [
            ChecksStatus::Unreadable,
            ChecksStatus::Failed,
            ChecksStatus::Missing,
            ChecksStatus::Pending,
        ];
        for i in 0..precedence.len() {
            let mut mixed = good.clone();
            mixed.extend(precedence[i..].iter().map(|status| CheckObservation {
                name: "rust".into(),
                status: *status,
                run: None,
                conclusion: None,
                url: None,
            }));
            assert_eq!(aggregate(&expected, &mixed), precedence[i]);
            mixed.reverse();
            assert_eq!(aggregate(&expected, &mixed), precedence[i]);
        }
    }
    #[tokio::test]
    async fn a_lost_merge_reply_does_not_merge_twice() {
        let forge = dvb_forge::FakeForge::default();
        let repo = forge.create_repo("org", "repo").await.unwrap();
        forge
            .push(&repo, std::path::Path::new("/unused"), "dvb/t-1", "h1")
            .await
            .unwrap();
        let pr = forge.open_pr(&repo, "dvb/t-1", "t", "b").await.unwrap();
        let sha = forge.merge(&repo, pr.number, "h1", "t").await.unwrap();
        let seen = forge.pr(&repo, pr.number).await.unwrap();
        assert_eq!(
            merge_reconciled(&forge, &repo, pr.number, "h1", "t", &seen)
                .await
                .unwrap(),
            sha
        );
        assert_eq!(
            merge_reconciled(&forge, &repo, pr.number, "old", "t", &seen)
                .await
                .unwrap_err(),
            MergeError::HeadMoved
        );
    }
}
