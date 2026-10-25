use super::control::{live_run, retry_run, round_cap_question_event};
use super::effects::{failed, forge_failed};
use super::merge::{adopt, moved_base};
use super::runs::{builder_assignment, launch_run, start_run};
use super::{Next, kick_all, read, slot_free, still};
use crate::engine::{AppState, Fail};
use crate::proj::{Project, Task};
use crate::publish;
use dvb_core::event::{ChecksStatus, CiMode};
use dvb_core::forge::{CheckObservation, PrState, Repo};
use dvb_core::git::Git;
use dvb_core::state::Effect;
use dvb_core::{Actor, Command, EventId, EventKind, TaskState, caps};
use std::time::Duration;

/// How often CI is read while a task waits for it.
pub(super) const CHECKS_POLL: Duration = Duration::from_secs(60);

/// A conflicting publication needs its builder before CI or review can run.
pub(super) async fn publication_ready(
    e: &AppState,
    t: &Task,
    p: &Project,
    state: TaskState,
) -> Result<Option<Next>, Fail> {
    let task = &t.view.id;
    let Some(pr) = t.view.pr else {
        return Ok(None);
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    let effect = if state == TaskState::InReview {
        Effect::StartUnit
    } else if state == TaskState::Merging {
        Effect::Merge
    } else {
        Effect::ReadChecks
    };
    let seen = match forge.pr(&repo, pr).await {
        Ok(seen) => seen,
        Err(fe) if fe.rate_limit.is_some() || state == TaskState::Merging => {
            return forge_failed(e, task, effect, fe).map(Some);
        }
        Err(fe) => return Err(Fail::internal(fe)),
    };
    publication_observed(e, t, p, state, effect, seen).await
}

async fn publication_observed(
    e: &AppState,
    t: &Task,
    p: &Project,
    state: TaskState,
    effect: Effect,
    seen: dvb_core::forge::PrObservation,
) -> Result<Option<Next>, Fail> {
    let task = &t.view.id;
    if !still(e, task, &state)? {
        return Ok(Some(Next::Idle));
    }
    if seen.state != PrState::Open {
        return failed(e, task, effect, false, "the PR is no longer open").map(Some);
    }
    if seen.base != p.view.default_branch {
        return failed(e, task, effect, false, "the PR has a different base branch").map(Some);
    }
    if t.view.head.as_ref() != Some(&seen.head) {
        if let Some(old) = &t.view.head {
            adopt(e, t, p, state, old, seen.branch).await?;
        }
        return Ok(Some(Next::Wait(CHECKS_POLL)));
    }
    let conflicting = match seen.conflicting {
        Some(conflicting) => conflicting,
        None => {
            let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
            let mirror = e.mirror(&p.view.name);
            let base = match e
                .fetch(
                    forge.as_ref(),
                    &Repo(p.view.repo.clone()),
                    &mirror,
                    &p.view.default_branch,
                )
                .await
            {
                Ok(base) => base,
                Err(fe) => return forge_failed(e, task, effect, fe).map(Some),
            };
            publish::conflicts(&mirror, &seen.head, &base)
                .await
                .map_err(Fail::internal)?
        }
    };
    if !still(e, task, &state)? {
        return Ok(Some(Next::Idle));
    }
    if !conflicting {
        return Ok(None);
    }
    repair(e, t, p, effect).await.map(Some)
}

/// Starts the task's builder to merge the default branch into its
/// conflicting branch (a branch repair round), at most
/// `caps::BRANCH_REPAIRS_PER_TASK` times, once no run of the task is live and
/// a run slot is free.
pub(super) async fn repair(
    e: &AppState,
    t: &Task,
    p: &Project,
    effect: Effect,
) -> Result<Next, Fail> {
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    if live_run(e, t).is_some() || !slot_free(e) {
        return Ok(Next::Wait(CHECKS_POLL));
    }
    if t.repairs >= caps::BRANCH_REPAIRS_PER_TASK {
        return failed(
            e,
            task,
            effect,
            false,
            "the branch repair limit was reached",
        );
    }
    let mirror = e.mirror(&p.view.name);
    let main = match e
        .fetch(forge.as_ref(), &repo, &mirror, &p.view.default_branch)
        .await
    {
        Ok(main) => main,
        Err(fe) => return forge_failed(e, task, effect, fe),
    };
    let merges = publish::merges_ref(task.as_str());
    Git::new(&mirror)
        .run(&["update-ref", &merges, &main], &[])
        .await
        .map_err(Fail::internal)?;
    let a = builder_assignment(
        e,
        p,
        t,
        dvb_core::wording::render(
            "question.repair-reason",
            &serde_json::json!({"branch": p.view.default_branch.to_string()}),
        ),
    )
    .await?;
    start_run(
        e,
        task,
        Actor::Engine,
        "repair branch",
        Command::RepairBranch(a.clone()),
        &a,
        None,
    )?;
    Ok(Next::Idle)
}

/// Poll the shared question independently of this head's current CI result.
async fn recover_main_ci(
    e: &AppState,
    t: &Task,
    p: &Project,
) -> Result<Option<Vec<CheckObservation>>, Fail> {
    let question = e
        .proj()
        .questions
        .values()
        .find(|q| q.view.project == p.view.id && q.view.open && q.main_ci.is_some())
        .cloned();
    let Some(q) = question else {
        return Ok(None);
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let seen = match forge
        .main_checks(&Repo(p.view.repo.clone()), &p.view.default_branch)
        .await
    {
        Ok(seen) => seen,
        Err(fe) => {
            forge_failed(e, &t.view.id, Effect::ReadChecks, fe)?;
            return Ok(None);
        }
    };
    let checks = q
        .main_ci
        .as_ref()
        .ok_or_else(|| Fail::internal("main CI question has no checks"))?;
    let mut unchanged = false;
    let closed = e.append_checked(Some(&p.view.id), Actor::Engine, |proj, _now| {
        unchanged = proj
            .questions
            .get(&q.view.id)
            .is_some_and(|current| current.view.open && current.opened_at == q.opened_at);
        Ok(
            if unchanged
                && checks.iter().all(|c| {
                    seen.iter()
                        .any(|now| now.name == c.name && now.status == ChecksStatus::Success)
                })
            {
                vec![EventKind::QuestionClosed {
                    question: q.view.id.clone(),
                    task: t.view.id.clone(),
                }]
            } else {
                vec![]
            },
        )
    })?;
    // Partial observations can also authorize recovery reruns in the caller.
    if !unchanged {
        return Ok(None);
    }
    if !closed.is_empty() {
        kick_all(e, Some(&p.view.id));
    }
    Ok(Some(seen))
}

/// Open one project question with all failing required jobs on main. A task
/// stays AwaitingChecks; the question and exact recovery ids survive replay.
async fn hold_for_main(
    e: &AppState,
    t: &Task,
    p: &Project,
    failing: &[CheckObservation],
) -> Result<bool, Fail> {
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let seen = match forge
        .main_checks(&Repo(p.view.repo.clone()), &p.view.default_branch)
        .await
    {
        Ok(seen) => seen,
        Err(fe) => {
            forge_failed(e, &t.view.id, Effect::ReadChecks, fe)?;
            return Ok(true);
        }
    };
    let CiMode::Expected(names) = &p.view.ci else {
        return Ok(false);
    };
    let mut broken: Vec<_> = seen
        .into_iter()
        .filter(|c| {
            names.contains(&c.name)
                && c.status == ChecksStatus::Failed
                && c.conclusion.as_deref() != Some("neutral")
        })
        .collect();
    broken.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.url.cmp(&b.url)));
    if !failing
        .iter()
        .any(|c| broken.iter().any(|m| m.name == c.name))
    {
        return Ok(false);
    }
    if !still(e, &t.view.id, &TaskState::AwaitingChecks)? {
        return Ok(true);
    }
    let id = dvb_core::QuestionId::new(e.new_id());
    e.append_checked(Some(&p.view.id), Actor::Engine, |proj, _now| {
        let now = &proj.tasks[&t.view.id];
        if now.view.state != TaskState::AwaitingChecks
            || now.view.head != t.view.head
            || proj.projects[&p.view.id].view.paused
        {
            return Ok(vec![]);
        }
        let existing = proj
            .questions
            .values()
            .find(|q| q.view.project == p.view.id && q.main_ci.is_some());
        let question = existing.map_or(id.clone(), |q| q.view.id.clone());
        // An answer accepted during a forge read must reach the held heads
        // before another observation can reopen the shared question.
        if existing.is_some_and(|q| !q.view.open)
            && proj.tasks.values().any(|t| {
                t.ci_question.as_ref() == Some(&question)
                    && t.ci_retry_runs.is_empty()
                    && t.view.state == TaskState::AwaitingChecks
            })
        {
            return Ok(vec![]);
        }
        let mut kinds = Vec::new();
        if existing.is_none_or(|q| !q.view.open || q.main_ci.as_ref() != Some(&broken)) {
            let details = broken
                .iter()
                .map(|c| {
                    format!(
                        "`{}` ({})",
                        c.name,
                        c.url
                            .as_deref()
                            .unwrap_or(crate::prompt::fragment_text("check-link-unavailable"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            kinds.push(EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                task: None,
                question: question.clone(),
                subject: dvb_core::wording::render(
                    "question.main-ci-question",
                    &serde_json::json!({"branch": p.view.default_branch.to_string(), "details": details}),
                ),
                format: dvb_core::event::QuestionFormat::Simple,
                options: vec![
                    dvb_core::event::QuestionOption::owner(
                        1,
                        dvb_core::wording::text("question.main-ci-wait"),
                        dvb_core::wording::text("question.main-ci-wait-effect"),
                    ),
                    dvb_core::event::QuestionOption::owner(
                        2,
                        dvb_core::wording::text("question.main-ci-retry"),
                        dvb_core::wording::text("question.main-ci-retry-effect"),
                    ),
                ],
                recommended: 1,
                lapses_on: dvb_core::event::LapsesOn::ValueVersion(EventId(0)),
                main_ci: Some(broken),
            });
        }
        if now.ci_question.as_ref() != Some(&question) || !now.ci_retry_runs.is_empty() {
            kinds.push(EventKind::ChecksObserved {
                task: t.view.id.clone(),
                head: t.view.head.clone().unwrap_or_default(),
                status: ChecksStatus::Failed,
                names: names.clone(),
                retry_question: Some(question),
                retry_runs: vec![],
            });
        }
        Ok(kinds)
    })?;
    Ok(true)
}

// --- CI --------------------------------------------------

pub(super) async fn checks(e: &AppState, t: &Task, p: &Project) -> Result<Next, Fail> {
    if let Some(task) = e.proj().tasks.get_mut(&t.view.id) {
        task.checks_waiting_for_slot = false;
    }
    let _checks = e.lock_project_checks(&p.view.id).await;
    let (_, current) = read(e, &t.view.id)?;
    let p = &current;
    if !still(e, &t.view.id, &TaskState::AwaitingChecks)? {
        return Ok(Next::Idle);
    }
    if let Some(next) = publication_ready(e, t, p, TaskState::AwaitingChecks).await? {
        return Ok(next);
    }
    let CiMode::Expected(names) = &p.view.ci else {
        let head = t
            .view
            .head
            .as_deref()
            .ok_or_else(|| Fail::internal("no head to check"))?;
        // The owner's explicit choice releases the wait without inventing CI evidence.
        e.decide(
            e.proj(),
            &t.view.id,
            Actor::Engine,
            crate::Decision::new(
                "checks",
                Command::ChecksObserved {
                    head: head.to_owned(),
                    status: ChecksStatus::Missing,
                },
            ),
            |_| Ok(()),
        )?;
        return Ok(Next::Idle);
    };
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let head = t
        .view
        .head
        .clone()
        .ok_or_else(|| Fail::internal("no head to check"))?;
    let repo = Repo(p.view.repo.clone());
    // None when CI is unreadable: that look neither starts nor ends a wait.
    let mut pending = None;
    let (status, failing, running) = match forge.checks(&repo, &head).await {
        Ok(seen) => {
            pending = Some(
                seen.iter()
                    .filter(|c| names.contains(&c.name) && c.status == ChecksStatus::Pending)
                    .cloned()
                    .collect(),
            );
            let failing: Vec<CheckObservation> = seen
                .iter()
                .filter(|c| names.contains(&c.name) && c.status == ChecksStatus::Failed)
                .cloned()
                .collect();
            let running = seen
                .iter()
                .any(|c| names.contains(&c.name) && c.status == ChecksStatus::Pending);
            (
                publish::reconcile::aggregate(names, &seen),
                failing,
                running,
            )
        }
        Err(fe) if fe.rate_limit.is_some() => return forge_failed(e, task, Effect::ReadChecks, fe),
        Err(_) => (ChecksStatus::Unreadable, vec![], false),
    };
    if !still(e, task, &TaskState::AwaitingChecks)? {
        return Ok(Next::Idle);
    }
    // A move of the head or of the default branch during CI must invalidate
    // its result before rerun/fix/failure/merge decisions.
    if let Some(pr) = t.view.pr {
        let seen = match forge.pr(&repo, pr).await {
            Ok(seen) => seen,
            Err(fe) if fe.rate_limit.is_some() => {
                return forge_failed(e, task, Effect::ReadChecks, fe);
            }
            Err(fe) => return Err(Fail::internal(fe)),
        };
        if let Some(next) =
            publication_observed(e, t, p, TaskState::AwaitingChecks, Effect::ReadChecks, seen)
                .await?
        {
            return Ok(next);
        }
        if let Some(next) = moved_base(e, t, p, &head).await? {
            return Ok(next);
        }
    }
    if !still(e, task, &TaskState::AwaitingChecks)? {
        return Ok(Next::Idle);
    }
    if let Some(pending) = pending {
        stuck(e, t, p, &head, pending)?;
    }
    let main_checks = recover_main_ci(e, t, p).await?;
    if !still(e, task, &TaskState::AwaitingChecks)? {
        return Ok(Next::Idle);
    }
    if status == ChecksStatus::Failed && !failing.is_empty() {
        let mut runs: Vec<u64> = failing.iter().filter_map(|c| c.run).collect();
        runs.sort_unstable();
        runs.dedup();
        let (now, _) = read(e, task)?;
        let released = now.ci_question.as_ref().is_some_and(|id| {
            !now.ci_retry_runs.is_empty()
                || e.proj().questions.get(id).is_some_and(|q| !q.view.open)
                || main_checks.as_ref().is_some_and(|seen| {
                    failing.iter().all(|c| {
                        seen.iter()
                            .any(|m| m.name == c.name && m.status == ChecksStatus::Success)
                    })
                })
        });
        let held = now.ci_question.as_ref().is_some_and(|id| {
            e.proj().questions.get(id).is_some_and(|q| {
                q.view.open
                    && q.main_ci.as_ref().is_some_and(|checks| {
                        checks
                            .iter()
                            .any(|m| failing.iter().any(|c| c.name == m.name))
                    })
            })
        });
        // A missing observation cannot release an existing wait for main.
        if held && !released {
            hold_for_main(e, t, p, &failing).await?;
            return Ok(Next::Wait(CHECKS_POLL));
        }
        let recovery = released
            && (now.ci_retry_runs.is_empty() || now.ci_retry_runs == runs)
            && !runs.is_empty();
        if recovery && now.ci_retry_runs.is_empty() {
            e.decide(
                e.proj(),
                task,
                Actor::Engine,
                crate::Decision {
                    pre: vec![EventKind::ChecksObserved {
                        task: task.clone(),
                        head: head.clone(),
                        status,
                        names: names.clone(),
                        retry_question: now.ci_question.clone(),
                        retry_runs: runs.clone(),
                    }],
                    ..crate::Decision::new(
                        "retry checks",
                        Command::ChecksObserved {
                            head: head.clone(),
                            status,
                        },
                    )
                },
                |_| Ok(()),
            )?;
        }
        if !still(e, task, &TaskState::AwaitingChecks)? {
            return Ok(Next::Idle);
        }
        let rerun_failure = if !runs.is_empty() {
            match forge.rerun_checks(&repo, &head, &runs, recovery).await {
                Ok(()) => return Ok(Next::Wait(CHECKS_POLL)),
                Err(fe) if fe.retryable || fe.rate_limit.is_some() => {
                    return forge_failed(e, task, Effect::ReadChecks, fe);
                }
                Err(fe) => Some(fe.detail),
            }
        } else {
            None
        };
        if hold_for_main(e, t, p, &failing).await? {
            return Ok(Next::Wait(CHECKS_POLL));
        }
        // One round for all of CI: a required check still queued, running or
        // rerun may fail too.
        if running {
            return Ok(Next::Wait(CHECKS_POLL));
        }
        observed(e, t, &head, status, names)?;
        return fix_round(e, t, &failing, rerun_failure.as_deref(), "dvb").await;
    }
    let state = observed(e, t, &head, status, names)?;
    Ok(if state == TaskState::AwaitingChecks {
        Next::Wait(CHECKS_POLL)
    } else {
        Next::Idle
    })
}

/// Tells the owner once per check and head (`checks.stuck`, with its link)
/// that a required check has been Pending for `caps::CHECK_PENDING_LIMIT`.
/// Nothing is rerun, cancelled or failed: the task keeps waiting.
fn stuck(
    e: &AppState,
    t: &Task,
    p: &Project,
    head: &str,
    mut pending: Vec<CheckObservation>,
) -> Result<(), Fail> {
    // One name reported twice (by two apps, or as a run and a status) is one check.
    pending.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| b.url.is_some().cmp(&a.url.is_some()))
    });
    pending.dedup_by(|a, b| a.name == b.name);
    let limit = i64::try_from(caps::CHECK_PENDING_LIMIT.as_millis()).unwrap_or(i64::MAX);
    let key = |c: &CheckObservation| (head.to_owned(), c.name.clone());
    e.append_checked(Some(&p.view.id), Actor::Engine, |proj, now| {
        let Some(task) = proj.tasks.get(&t.view.id) else {
            return Ok(vec![]);
        };
        if task.view.state != TaskState::AwaitingChecks || task.view.head.as_deref() != Some(head) {
            return Ok(vec![]);
        }
        let mut kinds = Vec::new();
        if task.pending_since.len() != pending.len()
            || pending
                .iter()
                .any(|c| !task.pending_since.contains_key(&key(c)))
        {
            kinds.push(EventKind::ChecksPending {
                task: t.view.id.clone(),
                head: head.to_owned(),
                checks: pending.iter().map(|c| c.name.clone()).collect(),
            });
        }
        kinds.extend(
            pending
                .iter()
                .filter(|c| {
                    !task.stuck_checks.contains(&key(c))
                        && task
                            .pending_since
                            .get(&key(c))
                            .is_some_and(|since| now.millis() - since.millis() >= limit)
                })
                .map(|c| EventKind::ChecksStuck {
                    task: t.view.id.clone(),
                    head: head.to_owned(),
                    check: c.name.clone(),
                    url: c.url.clone(),
                    limit_minutes: caps::CHECK_PENDING_LIMIT.as_secs() / 60,
                }),
        );
        Ok(kinds)
    })?;
    Ok(())
}

/// Fails the task awaiting checks with why CI `failing` failed and starts its
/// fix round at once, the way the owner's retry of a failed task does, under
/// the task's lock.
/// While its last run (the reviewer) still runs, or no run slot is free, the
/// task keeps awaiting checks and the round is asked again at the next look.
/// At the review-round cap the task fails with that reason and asks the owner.
/// The round's run is prepared first (a fetch, its assignment); the failure
/// and the round's start are recorded together only once it is ready, so a
/// failed preparation, a lost slot or a crash leaves the task awaiting checks,
/// looked at again.
async fn fix_round(
    e: &AppState,
    t: &Task,
    failing: &[CheckObservation],
    rerun_failure: Option<&str>,
    who: &str,
) -> Result<Next, Fail> {
    let task = &t.view.id;
    let (now, p) = read(e, task)?;
    if now.facts.round >= dvb_core::caps::REVIEW_ROUNDS_PER_PR {
        let why = ci_failure(e, &now, &p, failing, rerun_failure).await?;
        let question = round_cap_question_event(e, &now, Some(&why))?;
        e.fail_and_ask(task, Effect::ReadChecks, why, question)?;
        return Ok(Next::Idle);
    }
    if live_run(e, &now).is_some() {
        return Ok(Next::Wait(CHECKS_POLL));
    }
    if let Some(task) = e.proj().tasks.get_mut(task) {
        task.checks_waiting_for_slot = true;
    }
    if !slot_free(e) {
        return Ok(Next::Wait(CHECKS_POLL));
    }
    let why = ci_failure(e, &now, &p, failing, rerun_failure).await?;
    let started = match retry_run(e, &now, &p, who, true).await {
        Ok((a, from)) => e
            .fail_and_retry(task, Effect::ReadChecks, why, vec![], a.clone())
            .map(|_| (a, from)),
        Err(err) => Err(err),
    };
    match started {
        Ok((a, from)) => {
            if e.proj().runs.contains_key(&a.run) {
                launch_run(e, &a, from);
            }
            Ok(Next::Idle)
        }
        Err(err) => {
            eprintln!("dvb engine: task {task}: the fix round waits: {err}");
            Ok(Next::Wait(CHECKS_POLL))
        }
    }
}

/// Why CI failed: every failed required check, each with a short excerpt of
/// its log, or its link when the log cannot be read.
async fn ci_failure(
    e: &AppState,
    t: &Task,
    p: &Project,
    failing: &[CheckObservation],
    rerun_failure: Option<&str>,
) -> Result<String, Fail> {
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    // One name reported twice (by two apps, or as a run and a status) is one check.
    let mut failed: Vec<&CheckObservation> = failing.iter().collect();
    failed.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| b.run.is_some().cmp(&a.run.is_some()))
    });
    failed.dedup_by(|a, b| a.name == b.name);
    let mut details = Vec::new();
    for c in &failed {
        let excerpt = match c.run {
            Some(run) => match forge.check_excerpt(&repo, run).await {
                Ok(excerpt) => Some(excerpt).filter(|x| !x.trim().is_empty()),
                Err(fe) => {
                    eprintln!(
                        "dvb engine: task {}: the log of check `{}` is unreadable: {}",
                        t.view.id, c.name, fe.detail
                    );
                    None
                }
            },
            None => None,
        };
        details.push(match excerpt {
            Some(excerpt) => format!("`{}`: {excerpt}", c.name),
            None => format!(
                "`{}` ({}): its log could not be read",
                c.name,
                c.url
                    .as_deref()
                    .unwrap_or(crate::prompt::fragment_text("check-link-unavailable"))
            ),
        });
    }
    let (checks, cannot) = if failed.len() == 1 {
        ("check", "this check cannot be rerun")
    } else {
        ("checks", "these checks cannot be rerun")
    };
    Ok(format!(
        "CI failed: required {checks} {} failed ({}); {}; needs a fix round",
        failed
            .iter()
            .map(|c| format!("`{}`", c.name))
            .collect::<Vec<_>>()
            .join(", "),
        rerun_failure.unwrap_or(cannot),
        details.join("; "),
    ))
}

/// Records CI on the head (when it changed) and decides it.
pub(super) fn observed(
    e: &AppState,
    t: &Task,
    head: &str,
    status: ChecksStatus,
    names: &[String],
) -> Result<TaskState, Fail> {
    let pre = if t.checks.as_ref() == Some(&(head.to_owned(), status)) {
        vec![]
    } else {
        vec![EventKind::ChecksObserved {
            retry_question: if status == ChecksStatus::Success {
                None
            } else {
                t.ci_question.clone()
            },
            retry_runs: if status == ChecksStatus::Success {
                vec![]
            } else {
                t.ci_retry_runs.clone()
            },
            task: t.view.id.clone(),
            head: head.to_owned(),
            status,
            names: names.to_vec(),
        }]
    };
    let cmd = Command::ChecksObserved {
        head: head.to_owned(),
        status,
    };
    e.decide(
        e.proj(),
        &t.view.id,
        Actor::Engine,
        crate::Decision {
            pre,
            ..crate::Decision::new("checks", cmd)
        },
        |_| Ok(()),
    )
}
