use super::checks::{CHECKS_POLL, observed, publication_ready, repair};
use super::control::live_run;
use super::effects::{blocked, failed, forge_failed, pr_title};
use super::runs::start_approved;
use super::{Next, read, still};
use crate::engine::{AppState, Fail};
use crate::proj::{Project, Task};
use crate::publish;
use crate::spawn;
use crate::wake;
use crate::wake::Wake;
use dvb_core::event::{ChecksStatus, CiMode};
use dvb_core::forge::{MergeError, PrState, Repo};
use dvb_core::git::Git;
use dvb_core::roles::ReviewKind;
use dvb_core::state::Effect;
use dvb_core::{Actor, Command, EventId, EventKind, Refusal, RefusalCode, TaskId, TaskState};
use std::time::Duration;

/// How soon a merged task's cleanup is tried again.
const CLEANUP_AGAIN: Duration = Duration::from_secs(10);

/// Fetches the PR's branch and records its head as the task's. Whether it
/// moved: a branch still on the task's head (the forge's PR read lagging a
/// push) records nothing; the engine's own update of the branch (its receipt,
/// after a lost push reply or a crash) is recorded as that update.
pub(super) async fn adopt(
    e: &AppState,
    t: &Task,
    p: &Project,
    state: TaskState,
    old: &str,
    branch: String,
) -> Result<bool, Fail> {
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    let fetched = match e
        .fetch(forge.as_ref(), &repo, &e.mirror(&p.view.name), &branch)
        .await
    {
        Ok(head) => head,
        Err(fe) if fe.rate_limit.is_some() => {
            let effect = if state == TaskState::AwaitingChecks {
                Effect::ReadChecks
            } else {
                Effect::Merge
            };
            forge_failed(e, &t.view.id, effect, fe)?;
            return Ok(true);
        }
        Err(fe) => return Err(Fail::internal(fe)),
    };
    if fetched == old {
        return Ok(false);
    }
    if let Some(r) = receipt(e, p, &t.view.id).await
        && r.head == fetched
        && r.from == old
    {
        record_update(e, t, p, &state, &r, branch).await?;
        return Ok(true);
    }
    e.publication_moved(&t.view.id, &state, old, branch, fetched)?;
    Ok(true)
}

// --- Merge ----------------------------------

pub(super) async fn merge(e: &AppState, t: &Task, p: &Project) -> Result<Next, Fail> {
    if super::outcomes::hold(e, t, p).await? {
        return Ok(Next::Idle);
    }
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let (Some(pr), Some(head)) = (t.view.pr, t.view.head.clone()) else {
        return failed(e, task, Effect::Merge, false, "nothing to merge");
    };
    let repo = Repo(p.view.repo.clone());
    // A merge whose reply was lost is read back before anything is sent.
    let seen = match forge.pr(&repo, pr).await {
        Ok(seen) => seen,
        Err(fe) => return forge_failed(e, task, Effect::Merge, fe),
    };
    // A choice accepted during the PR read must control this observation.
    // Keep it fixed through the merge reply: checking again without a shared
    // write lock would still leave a race between that check and the send.
    let checks = e.lock_project_checks(&p.view.id).await;
    let (_, current) = read(e, task)?;
    let p = &current;
    // The forge's PR read still on the head the engine's update replaced:
    // it is catching up, so look again (the update is not a moved head).
    if seen.head != head
        && receipt(e, p, task)
            .await
            .is_some_and(|r| r.head == head && r.from == seen.head)
    {
        return Ok(Next::Wait(CHECKS_POLL));
    }
    let merged = seen.state == PrState::Merged && seen.head == head;
    if !merged {
        // A stop or a pause recorded during the read: nothing is sent.
        if !still(e, task, &TaskState::Merging)? {
            return Ok(Next::Idle);
        }
        if seen.conflicting != Some(false)
            && let Some(next) = publication_ready(e, t, p, TaskState::Merging).await?
        {
            return Ok(next);
        }
        // The PR must merge into the project's default branch.
        if seen.base != p.view.default_branch {
            let why = format!(
                "PR #{pr} merges into {}, not {}",
                seen.base, p.view.default_branch
            );
            return failed(e, task, Effect::Merge, false, why);
        }
        // A clean verdict on this head, recorded since it was
        // published (`decide` only gets here that way; checked again).
        let reviewed = t.reviews_approved();
        if !reviewed {
            if t.view.review == ReviewKind::Design {
                e.decide(
                    e.proj(),
                    task,
                    Actor::Engine,
                    crate::Decision::new(
                        "merge_refused",
                        Command::MergeRefused("both reviews must approve this publication".into()),
                    ),
                    |_| Ok(()),
                )?;
                return Ok(Next::Idle);
            }
            return Err(Refusal::new(
                RefusalCode::HeadMismatch,
                "no clean reviewer verdict for this head",
            )
            .into());
        }
        // The verdict is the head's accountable reviewer's, not another role's.
        let accountable = t.reviewer(p, ReviewKind::Code);
        let by = t
            .facts
            .verdict
            .as_ref()
            .and_then(|v| e.proj().runs.get(&v.run).map(|r| r.assignment.role.clone()));
        let design_by = t
            .facts
            .design_verdict
            .as_ref()
            .and_then(|v| e.proj().runs.get(&v.run).map(|r| r.assignment.role.clone()));
        let code_by = if t.view.review == ReviewKind::Design {
            t.facts
                .code_verdict
                .as_ref()
                .and_then(|v| e.proj().runs.get(&v.run).map(|r| r.assignment.role.clone()))
        } else {
            by
        };
        let design_accountable = t.reviewer(p, ReviewKind::Design);
        if (accountable.is_some() && accountable != code_by)
            || (t.view.review == ReviewKind::Design
                && (design_accountable.is_none() || design_accountable != design_by))
        {
            // The role chart changed since the review: the verdict no longer
            // counts, so review re-enters with the reviewer now accountable.
            let cmd = Command::MergeRefused("the verdict is not the accountable reviewer's".into());
            e.decide(
                e.proj(),
                task,
                Actor::Engine,
                crate::Decision::new("merge_refused", cmd),
                |_| Ok(()),
            )?;
            return Ok(Next::Idle);
        }
        // Read CI checks again just before the merge.
        if let CiMode::Expected(names) = &p.view.ci {
            let status = match forge.checks(&repo, &head).await {
                Ok(seen) => publish::reconcile::aggregate(names, &seen),
                Err(_) => ChecksStatus::Unreadable,
            };
            if status != ChecksStatus::Success {
                observed(e, t, &head, status, names)?;
                return Ok(Next::Idle);
            }
            if !still(e, task, &TaskState::Merging)? {
                return Ok(Next::Idle);
            }
        }
        // Without a ruleset nothing refuses a branch behind main, and main
        // would get a combination no CI ran: it is brought up to date first.
        if let Some(next) = up_to_date(e, t, p, &head).await? {
            return Ok(next);
        }
    }
    let title = pr_title(t);
    let sha =
        match publish::reconcile::merge_reconciled(forge.as_ref(), &repo, pr, &head, &title, &seen)
            .await
        {
            Ok(sha) => {
                fail::fail_point!("effect.merge.sent");
                sha
            }
            Err(MergeError::HeadMoved) => {
                let now = match forge.pr(&repo, pr).await {
                    Ok(now) => now,
                    Err(fe) => return forge_failed(e, task, Effect::Merge, fe),
                };
                if now.head == head {
                    let cmd = Command::MergeRefused(format!("the PR's head is no longer {head}"));
                    e.decide(
                        e.proj(),
                        task,
                        Actor::Engine,
                        crate::Decision::new("merge_refused", cmd),
                        |_| Ok(()),
                    )?;
                } else {
                    // Review re-enters for the head the PR has now
                    // (its kick wakes this); otherwise read the PR again later.
                    adopt(e, t, p, TaskState::Merging, &head, now.branch).await?;
                }
                return Ok(Next::Wait(CHECKS_POLL));
            }
            // A ruleset that wants the branch up to date refuses it behind its
            // base (main moved after the check above).
            Err(MergeError::Refused(why)) => return behind(e, t, p, &head, why).await,
            Err(MergeError::Forge(fe)) => {
                return forge_failed(e, task, Effect::Merge, fe);
            }
        };
    drop(checks);
    // Recorded once, even when a merge that landed during a stop is read back.
    if t.merged.as_ref() != Some(&sha) {
        let merged = EventKind::PrMerged {
            task: task.clone(),
            pr,
            sha: sha.clone(),
        };
        e.append_checked(Some(&p.view.id), Actor::Engine, |_, _| Ok(vec![merged]))?;
        fail::fail_point!("effect.merge.recorded");
        spawn(
            "start approved tasks after merge",
            start_approved(e.clone(), t.view.goal.clone()),
        );
        // Tasks stopped as blocked on this one resume in their own step.
        let blocked: Vec<TaskId> = {
            let proj = e.proj();
            proj.tasks
                .values()
                .filter(|b| proj.merged_blocker(b).is_some_and(|m| m.view.id == *task))
                .map(|b| b.view.id.clone())
                .collect()
        };
        for b in &blocked {
            super::kick(e, b);
        }
    }
    // The task stays merging until its cleanup is done, so a retry reads the
    // merged PR and finishes it without asking the forge to merge twice.
    let cleaned = publish::reconcile::cleanup(e, task).await;
    if cleaned.is_ok() {
        e.decide(
            e.proj(),
            task,
            Actor::Engine,
            crate::Decision::new("merge_done", Command::MergeDone),
            |_| Ok(()),
        )?;
    }
    // Goal completion rests on the recorded merges, after the task is done on
    // the normal path; its effect goes ahead even when cleanup is retried.
    if e.record_goal_completion(&t.view.goal)? {
        if let Err(err) = publish::reconcile::close_goal_issue(e, &t.view.goal).await {
            eprintln!("dvb engine: close goal issue: {err}");
            if !matches!(err, Fail::NotFound(_))
                && !matches!(&err, Fail::Internal(crate::InternalError::Forge(forge)) if !forge.retryable)
            {
                spawn(
                    "goal issue",
                    publish::reconcile::retry_goal_issue(e.clone(), t.view.goal.clone(), Some(err)),
                );
            }
        }
        spawn(
            "wake the orchestrator",
            wake::wake(e.clone(), p.view.id.clone(), Wake::GoalDone, 0, 0),
        );
    }
    if let Err(err) = cleaned {
        eprintln!("dvb engine: task {task}: cleanup: {err}");
        return Ok(Next::Wait(CLEANUP_AGAIN));
    }
    Ok(Next::Idle)
}

// --- Behind its base -----------------------------------------------------

/// The merge was refused. When the PR's branch is behind its base, the engine
/// brings it up to date (`update`); any other refusal fails the task with a
/// plain reason.
pub(super) async fn behind(
    e: &AppState,
    t: &Task,
    p: &Project,
    head: &str,
    why: String,
) -> Result<Next, Fail> {
    if super::outcomes::hold(e, t, p).await? {
        return Ok(Next::Idle);
    }
    match up_to_date(e, t, p, head).await? {
        Some(next) => Ok(next),
        // Not behind (or not known to be): the refusal stands.
        None => failed(e, &t.view.id, Effect::Merge, false, why),
    }
}

/// Brings a merging task's branch on `head` up to date (`update`) when it is
/// behind the default branch. `None` when it is not (or is not known to be).
async fn up_to_date(e: &AppState, t: &Task, p: &Project, head: &str) -> Result<Option<Next>, Fail> {
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    let mirror = e.mirror(&p.view.name);
    let base = match e
        .fetch(forge.as_ref(), &repo, &mirror, &p.view.default_branch)
        .await
    {
        Ok(base) => base,
        Err(fe) => return forge_failed(e, task, Effect::Merge, fe).map(Some),
    };
    if !lacks(&mirror, &base, head).await {
        return Ok(None);
    }
    update(e, t, p, TaskState::Merging, head, &base)
        .await
        .map(Some)
}

/// The default branch moved past the head's base while its CI runs: that
/// result no longer decides anything, so the branch is brought up to date at
/// once (`update`) and the old head's CI cancelled. `None` when it has not
/// moved (or is not known to have): the head's own CI decides.
pub(super) async fn moved_base(
    e: &AppState,
    t: &Task,
    p: &Project,
    head: &str,
) -> Result<Option<Next>, Fail> {
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    let mirror = e.mirror(&p.view.name);
    let main = &p.view.default_branch;
    let base = match e.fetch(forge.as_ref(), &repo, &mirror, main).await {
        Ok(base) => base,
        Err(fe) if fe.rate_limit.is_some() => {
            return forge_failed(e, task, Effect::ReadChecks, fe).map(Some);
        }
        Err(fe) => {
            eprintln!(
                "dvb engine: task {task}: {main} is unreadable: {}",
                fe.detail
            );
            return Ok(None);
        }
    };
    if !lacks(&mirror, &base, head).await {
        return Ok(None);
    }
    // The task's own run could publish over the update: it waits for its end.
    if live_run(e, t).is_some() {
        return Ok(Some(Next::Wait(CHECKS_POLL)));
    }
    update(e, t, p, TaskState::AwaitingChecks, head, &base)
        .await
        .map(Some)
}

/// Whether `head` lacks `base`: their merge base is older than `base`.
async fn lacks(mirror: &std::path::Path, base: &str, head: &str) -> bool {
    Git::new(mirror)
        .run(&["merge-base", base, head], &[])
        .await
        .is_ok_and(|fork| fork != base)
}

/// Brings the PR's branch on `head` up to date with `base` (the default
/// branch's tip): the engine's own neutral merge commit, pushed like a
/// publication. The merge rule stays: CI runs again on the new head; the
/// clean verdict carries over only when the PR's own change (its patch id) is
/// the same, else review re-enters. A conflict while awaiting checks starts a
/// branch repair round; while merging it fails the task with a plain reason.
async fn update(
    e: &AppState,
    t: &Task,
    p: &Project,
    state: TaskState,
    head: &str,
    base: &str,
) -> Result<Next, Fail> {
    let task = &t.view.id;
    let effect = if state == TaskState::AwaitingChecks {
        Effect::ReadChecks
    } else {
        Effect::Merge
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let repo = Repo(p.view.repo.clone());
    let mirror = e.mirror(&p.view.name);
    let main = &p.view.default_branch;
    let branch = t
        .branch
        .clone()
        .ok_or_else(|| Fail::internal("the task has no branch"))?;
    let policy = e.publication().policy();
    // The receipt (`refs/dvb/updates/<task>`) is written before the push and
    // reused, so a lost push reply or a crash finds this same commit.
    let updated = match receipt(e, p, task).await {
        Some(r) if r.from == head && r.base == base => r.head,
        Some(_) | None => {
            let message = format!("Bring {branch} up to date with {main}\n\ndvb task {task}.");
            if let Some(hit) = policy.scan("message", &message) {
                return blocked(e, task, Some(&p.view.id), effect, hit);
            }
            let settings = e.publication();
            match publish::update_commit(&mirror, head, base, &message, &settings).await {
                Ok(Some(updated)) => updated,
                Ok(None) if state == TaskState::AwaitingChecks => {
                    return repair(e, t, p, effect).await;
                }
                Ok(None) => {
                    let why = format!("conflicts with {main}; needs a fix round");
                    return failed(e, task, effect, false, why);
                }
                Err(err) => return failed(e, task, effect, false, err),
            }
        }
    };
    match publish::scan_commit(&mirror, base, &updated).await {
        Ok(Some(hit)) => return blocked(e, task, Some(&p.view.id), effect, hit),
        Ok(None) => {}
        Err(err) => return failed(e, task, effect, false, err),
    }
    // A stop or a pause recorded meanwhile: nothing is sent.
    if !still(e, task, &state)? {
        return Ok(Next::Idle);
    }
    for (name, commit) in [
        (receipt_ref(task), &updated),
        (format!("refs/heads/{branch}"), &updated),
    ] {
        Git::new(&mirror)
            .run(&["update-ref", &name, commit], &[])
            .await
            .map_err(Fail::internal)?;
    }
    if let Err(fe) = forge.push(&repo, &mirror, &branch, &updated).await {
        return forge_failed(e, task, effect, fe);
    }
    let r = receipt(e, p, task)
        .await
        .ok_or_else(|| Fail::internal("the update receipt is gone"))?;
    record_update(e, t, p, &state, &r, branch).await?;
    // CI runs on the pushed head, with the carried verdict: look again once
    // the forge's PR shows it (review re-entering kicks the task itself).
    Ok(Next::Wait(CHECKS_POLL))
}

/// The engine's own update of a task's branch, as its receipt records it.
struct Update {
    head: String,
    from: String,
    base: String,
}

fn receipt_ref(task: &TaskId) -> String {
    format!("refs/dvb/updates/{task}")
}

/// The task's update receipt: the merge commit and its two parents.
async fn receipt(e: &AppState, p: &Project, task: &TaskId) -> Option<Update> {
    let name = receipt_ref(task);
    let read = Git::new(&e.mirror(&p.view.name))
        .run(
            &[
                "rev-parse",
                &name,
                &format!("{name}^1"),
                &format!("{name}^2"),
            ],
            &[],
        )
        .await
        .ok()?;
    let mut lines = read.lines().map(str::to_owned);
    Some(Update {
        head: lines.next()?,
        from: lines.next()?,
        base: lines.next()?,
    })
}

/// Records the update (`branch.updated`): the same patch carries the clean
/// verdict over, otherwise review re-enters. While awaiting checks, the old
/// head's CI runs are cancelled.
async fn record_update(
    e: &AppState,
    t: &Task,
    p: &Project,
    state: &TaskState,
    r: &Update,
    branch: String,
) -> Result<(), Fail> {
    let mirror = e.mirror(&p.view.name);
    let fork = Git::new(&mirror)
        .run(&["merge-base", &r.base, &r.from], &[])
        .await
        .map_err(Fail::internal)?;
    let same = publish::patch_id(&mirror, &fork, &r.from)
        .await
        .map_err(Fail::internal)?
        == publish::patch_id(&mirror, &r.base, &r.head)
            .await
            .map_err(Fail::internal)?;
    let carried = if same {
        carried_verdict(e, t, &r.from, &r.head, &p.view.default_branch)?
    } else {
        Vec::new()
    };
    let event = EventKind::BranchUpdated {
        task: t.view.id.clone(),
        branch,
        base: r.base.clone(),
        from: r.from.clone(),
        head: r.head.clone(),
        same_change: !carried.is_empty(),
    };
    e.branch_updated(&t.view.id, state, &r.from, event, carried)?;
    // Only runner time is at stake: a cancel the forge refuses is logged.
    if *state == TaskState::AwaitingChecks
        && let Some(forge) = e.forge()
        && let Err(fe) = forge
            .cancel_checks(&Repo(p.view.repo.clone()), &r.from)
            .await
    {
        eprintln!(
            "dvb engine: task {}: the CI of {} was not cancelled: {}",
            t.view.id, r.from, fe.detail
        );
    }
    Ok(())
}

/// A copy of the clean verdict on `from` for `to`, saying why no review ran.
fn carried_verdict(
    e: &AppState,
    t: &Task,
    from: &str,
    to: &str,
    main: &str,
) -> Result<Vec<EventKind>, Fail> {
    if !t.reviews_approved() {
        return Ok(Vec::new());
    }
    let approvals = if t.view.review == ReviewKind::Design {
        vec![
            t.facts.code_verdict.as_ref(),
            t.facts.design_verdict.as_ref(),
        ]
    } else {
        vec![t.facts.verdict.as_ref()]
    };
    let mut carried = Vec::new();
    for approval in approvals {
        let Some(approval) = approval.filter(|v| v.head == from && v.open == 0) else {
            return Ok(Vec::new());
        };
        let verdicts = e
            .store
            .select(EventId(0), dvb_store::Select::Kinds(&["verdict.recorded"]))?;
        let found = verdicts.into_iter().rev().find_map(|ev| {
            let EventKind::VerdictRecorded {
                task,
                head,
                run,
                role,
                model,
                vendor,
                findings,
                summary,
                review_stage,
                ..
            } = ev.kind
            else {
                return None;
            };
            (task == t.view.id && head == from && run == approval.run).then(|| {
                EventKind::VerdictRecorded {
                    review_stage,
                    verdict: dvb_core::VerdictId::new(e.new_id()),
                    task,
                    head: to.to_owned(),
                    run,
                    role,
                    model,
                    vendor,
                    findings,
                    summary: format!(
                        "Carried over from the review of {from}: the branch was brought up to date \
                 with {main} and its own change is the same (same patch id). {summary}"
                    ),
                }
            })
        });
        let Some(found) = found else {
            return Ok(Vec::new());
        };
        carried.push(found);
    }
    Ok(carried)
}
