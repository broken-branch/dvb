use super::checks::publication_ready;
use super::runs::{
    builder_assignment, launch_run, preserved_workspace, reviewer_assignment, start_run,
};
use super::{Next, read, slot_free, still};
use crate::engine::{AppState, Fail};
use crate::proj::{Project, Task};
use crate::publish::{self, without_run_ids};
use dvb_core::forge::{PrState, Repo};
use dvb_core::git::Git;
use dvb_core::state::Effect;
use dvb_core::{Actor, Command, EventKind, ProjectId, RunId, TaskId, TaskState};
use std::time::Duration;

/// An effect failed: `decide` classes it (retry or fail the task).
pub(super) fn failed(
    e: &AppState,
    task: &TaskId,
    effect: Effect,
    transient: bool,
    detail: impl std::fmt::Display,
) -> Result<Next, Fail> {
    let cmd = Command::EffectFailed {
        rate_limit: None,
        effect,
        transient,
        detail: detail.to_string(),
    };
    e.decide(
        e.proj(),
        task,
        Actor::Engine,
        crate::Decision::new("effect_failed", cmd),
        |_| Ok(()),
    )?;
    Ok(Next::Idle)
}

pub(super) fn forge_failed(
    e: &AppState,
    task: &TaskId,
    effect: Effect,
    error: dvb_core::forge::ForgeError,
) -> Result<Next, Fail> {
    e.decide(
        e.proj(),
        task,
        Actor::Engine,
        crate::Decision::new(
            "effect_failed",
            Command::EffectFailed {
                effect,
                transient: error.retryable,
                detail: error.detail,
                rate_limit: error.rate_limit,
            },
        ),
        |_| Ok(()),
    )?;
    Ok(Next::Idle)
}

// --- Publishing --------------------------------------------------

/// Builds the round's publication commit once (kept in the mirror as
/// `refs/dvb/published/<run>`), scans it, pushes the task's branch, and opens
/// the PR once per task.
pub(super) async fn publish(e: &AppState, t: &Task, p: &Project) -> Result<Next, Fail> {
    if super::outcomes::hold(e, t, p).await? {
        return Ok(Next::Idle);
    }
    let task = &t.view.id;
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let title = pr_title(t);
    let repo = Repo(p.view.repo.clone());
    let mirror = e.mirror(&p.view.name);
    let branch = t.branch.clone().unwrap_or_else(|| format!("dvb/{task}-1"));
    let project = Some(&p.view.id);

    if t.facts.published_by.is_none() || t.facts.published_by != t.facts.builder {
        let run = t
            .facts
            .builder
            .clone()
            .ok_or_else(|| Fail::internal("the task has no builder run"))?;
        let r = e
            .proj()
            .runs
            .get(&run)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("run {run}")))?;
        // The base the engine recorded and the head the run reported, never
        // what the workspace says now.
        let base = r
            .assignment
            .base
            .clone()
            .ok_or_else(|| Fail::internal("the run has no recorded base"))?;
        let collected = r
            .collected
            .clone()
            .ok_or_else(|| Fail::internal("the run reported no head"))?;
        let round = t
            .facts
            .round
            .saturating_add(u8::from(!t.facts.branch_repair));
        let message = format!("{title}\n\ndvb task {task}, round {round}.");
        let issues = goal_issue_refs(&e.proj(), task);
        let body = pr_body(task, round, &run, r.summary.as_deref(), &issues);
        let body = if r.outcome == dvb_core::run_verbs::BuilderOutcome::Complete {
            body
        } else {
            super::outcomes::references_only(&body)
        };
        // Recovery can reuse a publication marker, but must scan current text.
        for (name, text) in [
            ("message", &message),
            ("pr title", &title),
            ("pr body", &body),
        ] {
            if let Some(hit) = e.publication().policy().scan(name, text) {
                return blocked(e, task, project, Effect::Push, hit);
            }
        }
        let marker = format!("refs/dvb/published/{run}");
        let head = match Git::new(&mirror)
            .run(&["rev-parse", "--verify", "--quiet", &marker], &[])
            .await
        {
            Ok(head) if !head.is_empty() => head,
            Ok(_) | Err(_) => {
                let dir = e.cfg.paths.work.join(run.as_str());
                let merges = publish::merges(&mirror, task.as_str()).await;
                let head = match publish::collected_publication(
                    &mirror,
                    &dir,
                    run.as_str(),
                    &collected,
                    (&base, merges.as_deref()),
                    &message,
                    &e.publication(),
                )
                .await
                {
                    Ok(head) => head,
                    // A fix round that changed nothing (it closed its finding
                    // outside the code, rejected it, or met a flaky check):
                    // the head it started on goes back to review unchanged.
                    Err(publish::CollectionError::NoChanges)
                        if merges.is_none()
                            && t.view.pr.is_some()
                            && t.view.head.as_ref() == Some(&base) =>
                    {
                        base.clone()
                    }
                    Err(publish::CollectionError::NoChanges) => {
                        e.no_changes(task)?;
                        return Ok(Next::Idle);
                    }
                    Err(publish::CollectionError::Other(err)) => {
                        return failed(e, task, Effect::Push, false, err);
                    }
                };
                match publish::scan_commit(&mirror, &base, &head).await {
                    Ok(Some(hit)) if t.scan_hit => {
                        return blocked(e, task, project, Effect::Push, hit);
                    }
                    Ok(Some(hit)) => {
                        // A conflict round's head merges the default branch:
                        // only the lines it adds to that branch are the builder's.
                        let own = match &merges {
                            None => Some(hit.clone()),
                            Some(main) => match publish::scan_commit(&mirror, main, &head).await {
                                Ok(own) => own,
                                Err(err) => return failed(e, task, Effect::Push, false, err),
                            },
                        };
                        if own.as_ref() == Some(&hit) {
                            return scan_fix_round(e, t, p, hit).await;
                        }
                        return blocked(e, task, project, Effect::Push, hit);
                    }
                    Ok(None) => {}
                    Err(err) => return failed(e, task, Effect::Push, false, err),
                }
                Git::new(&mirror)
                    .run(&["update-ref", &marker, &head], &[])
                    .await
                    .map_err(Fail::internal)?;
                head
            }
        };
        Git::new(&mirror)
            .run(&["update-ref", &format!("refs/heads/{branch}"), &head], &[])
            .await
            .map_err(Fail::internal)?;
        // An unchanged fix round's head is on the branch already.
        if t.view.head.as_ref() != Some(&head)
            && let Err(fe) = forge.push(&repo, &mirror, &branch, &head).await
        {
            return forge_failed(e, task, Effect::Push, fe);
        }
        fail::fail_point!("effect.push.sent");
        let published = EventKind::BranchPublished {
            task: task.clone(),
            round,
            branch: branch.clone(),
            head: head.clone(),
        };
        let cmd = Command::Published;
        let state = e.decide(
            e.proj(),
            task,
            Actor::Engine,
            crate::Decision {
                pre: vec![published],
                ..crate::Decision::new("published", cmd)
            },
            |_| Ok(()),
        )?;
        // A conflict fix round is over once published.
        let merges = publish::merges_ref(task.as_str());
        Git::new(&mirror)
            .run(&["update-ref", "-d", &merges], &[])
            .await
            .map_err(Fail::internal)?;
        if state != TaskState::Publishing {
            return Ok(Next::Idle);
        }
    }

    let (t, _) = read(e, task)?;
    if !still(e, task, &TaskState::Publishing)? {
        return Ok(Next::Idle);
    }
    let run = t
        .facts
        .published_by
        .as_ref()
        .or(t.facts.builder.as_ref())
        .cloned()
        .unwrap_or_else(|| RunId::new(""));
    let summary = e.proj().runs.get(&run).and_then(|r| r.summary.clone());
    let issues = goal_issue_refs(&e.proj(), task);
    let body = pr_body(task, t.facts.round, &run, summary.as_deref(), &issues);
    let body = if t.view.outcome == dvb_core::run_verbs::BuilderOutcome::Complete {
        body
    } else {
        super::outcomes::references_only(&body)
    };
    // Also covers restart after BranchPublished or a lost open/edit reply.
    for (name, text) in [("pr title", &title), ("pr body", &body)] {
        if let Some(hit) = e.publication().policy().scan(name, text) {
            return blocked(e, task, project, Effect::OpenPr, hit);
        }
    }
    // One PR per task; later rounds replace its body before returning to review.
    if let Some(pr) = t.view.pr {
        if let Err(fe) = forge.edit_pr_body(&repo, pr, &body).await {
            return forge_failed(e, task, Effect::OpenPr, fe);
        }
        e.decide(
            e.proj(),
            task,
            Actor::Engine,
            crate::Decision::new("pr_opened", Command::PrOpened),
            |_| Ok(()),
        )?;
        return Ok(Next::Idle);
    }
    let (number, url) = match forge.find_pr(&repo, &branch).await {
        // A stop or a pause recorded during the read: nothing more is sent.
        Ok(_) if !still(e, task, &TaskState::Publishing)? => return Ok(Next::Idle),
        Ok(Some(pr)) if pr.state == PrState::Open => {
            if let Err(fe) = forge.edit_pr_body(&repo, pr.number, &body).await {
                return forge_failed(e, task, Effect::OpenPr, fe);
            }
            (pr.number, pr.url)
        }
        Ok(_) => match forge.open_pr(&repo, &branch, &title, &body).await {
            Ok(pr) => {
                fail::fail_point!("effect.open_pr.sent");
                (pr.number, pr.url)
            }
            Err(fe) => return forge_failed(e, task, Effect::OpenPr, fe),
        },
        Err(fe) => return forge_failed(e, task, Effect::OpenPr, fe),
    };
    let opened = EventKind::PrOpened {
        task: task.clone(),
        pr: number,
        url,
    };
    e.decide(
        e.proj(),
        task,
        Actor::Engine,
        crate::Decision {
            pre: vec![opened],
            ..crate::Decision::new("pr_opened", Command::PrOpened)
        },
        |_| Ok(()),
    )?;
    Ok(Next::Idle)
}

/// The task's title as its PR (and its squash merge) carries it.
pub(super) fn pr_title(t: &Task) -> String {
    let title = without_run_ids(&t.view.title);
    if t.view.outcome == dvb_core::run_verbs::BuilderOutcome::Complete {
        title.into_owned()
    } else {
        super::outcomes::references_only(&title)
    }
}

pub(super) fn pr_body(
    task: &TaskId,
    round: u8,
    run: &RunId,
    summary: Option<&str>,
    issues: &str,
) -> String {
    let ids = format!("Task {task}, round {round}, built by run {run}.{issues}");
    match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(summary) => format!("{}\n\n{ids}", without_run_ids(summary)),
        None => ids,
    }
}

/// The issues the task's goal carries: `Closes #n` from the goal's last
/// unmerged task, `Refs #n` from earlier ones. The engine still closes them
/// when the goal is Done, as when several tasks finish in parallel.
pub(super) fn goal_issue_refs(proj: &crate::proj::Projections, task: &TaskId) -> String {
    let Some(t) = proj.tasks.get(task) else {
        return String::new();
    };
    let Some(g) = proj.goals.get(&t.view.goal) else {
        return String::new();
    };
    // Historical `from_issue` goals keep their PR text; the engine closes them.
    let issues: Vec<u64> = g.attached.iter().map(|i| i.number).collect();
    if issues.is_empty() {
        return String::new();
    }
    let last = g.tasks.iter().filter(|id| *id != task).all(|id| {
        proj.tasks
            .get(id)
            .is_none_or(|other| matches!(other.view.state, TaskState::Done | TaskState::Cancelled))
    });
    let word = if last { "Closes" } else { "Refs" };
    let lines: Vec<_> = issues.iter().map(|n| format!("{word} #{n}")).collect();
    format!("\n\n{}", lines.join("\n"))
}

/// A scan hit blocks the publication with its file, line and rule.
pub(super) fn blocked(
    e: &AppState,
    task: &TaskId,
    project: Option<&ProjectId>,
    effect: Effect,
    hit: publish::Hit,
) -> Result<Next, Fail> {
    let detail = format!("publish blocked: {} in {}:{}", hit.rule, hit.file, hit.line);
    e.append_checked(project, Actor::Engine, |_, _| {
        Ok(vec![EventKind::ScanHit {
            task: task.clone(),
            file: hit.file,
            line: hit.line,
            rule: hit.rule,
        }])
    })?;
    failed(e, task, effect, false, detail)
}

/// A scan hit in the builder's own change: one fix round, in its kept
/// workspace, told the file, line and rule. Nothing is pushed; a hit on the
/// round's head blocks the publication (`Task::scan_hit`).
async fn scan_fix_round(
    e: &AppState,
    t: &Task,
    p: &Project,
    hit: publish::Hit,
) -> Result<Next, Fail> {
    // The next run end kicks the task again.
    if !slot_free(e) {
        return Ok(Next::Idle);
    }
    let a = builder_assignment(e, p, t, "Fix the publish scan hit.".into()).await?;
    let from = preserved_workspace(e, &t.view.id, &a.base);
    let detail = format!(
        "publish blocked: {} in {}:{}. Nothing that hits the publish scan is pushed: \
         take it out of the change (for example, build fake tokens at run time); \
         the task fails if the next head still hits it",
        hit.rule, hit.file, hit.line
    );
    let scan = EventKind::ScanHit {
        task: t.view.id.clone(),
        file: hit.file,
        line: hit.line,
        rule: hit.rule,
    };
    e.fail_and_retry(&t.view.id, Effect::Push, detail, vec![scan], a.clone())?;
    if e.proj().runs.contains_key(&a.run) {
        launch_run(e, &a, from);
    }
    Ok(Next::Idle)
}

// --- Review -----------------------------------

/// The head's next review stage, or after a fix-first verdict the fix round. A
/// reviewer of the head still running is waited for.
pub(super) async fn review(e: &AppState, t: &Task, p: &Project) -> Result<Next, Fail> {
    if let Some(next) = publication_ready(e, t, p, TaskState::InReview).await? {
        return Ok(next);
    }
    let head = t
        .view
        .head
        .clone()
        .ok_or_else(|| Fail::internal("no head to review"))?;
    let reviewing = e.proj().runs.values().any(|r| {
        r.assignment.task.as_ref() == Some(&t.view.id)
            && r.assignment.reviews.as_ref() == Some(&head)
            && r.ended.is_none()
    });
    if reviewing || !slot_free(e) {
        return Ok(Next::Idle);
    }
    if !t.needs_reviewer() {
        // Fix-first: a builder run on the head, with the findings.
        let a = builder_assignment(e, p, t, format!("Fix round {}.", t.facts.round + 1)).await?;
        start_run(
            e,
            &t.view.id,
            Actor::Engine,
            crate::Decision::new("fix", Command::Start(a.clone())),
            &a,
            None,
            |_| Ok(()),
        )?;
        return Ok(Next::Idle);
    }
    let a = reviewer_assignment(e, p, t, &head)?;
    // The cap again under the projections' lock: another start may have taken
    // the slot since; the next run end kicks the task again.
    let started = e.append_checked(Some(&p.view.id), Actor::Engine, |proj, _now| {
        Ok(
            if u64::try_from(proj.live_runs().count()).unwrap_or(u64::MAX) < proj.max_runs()
                && !proj.projects[&p.view.id].view.paused
                && proj.tasks[&t.view.id].view.state == TaskState::InReview
                && proj.tasks[&t.view.id].view.head.as_ref() == Some(&head)
                && !proj.runs.values().any(|r| {
                    r.assignment.task.as_ref() == Some(&t.view.id)
                        && r.assignment.reviews.as_ref() == Some(&head)
                        && r.ended.is_none()
                })
            {
                vec![EventKind::RunStarted(a.clone())]
            } else {
                vec![]
            },
        )
    })?;
    if !started.is_empty() {
        launch_run(e, &a, None);
    }
    Ok(Next::Idle)
}

// --- Retries ----------------------------------------------------

pub(super) async fn retry(
    e: &AppState,
    t: &Task,
    p: &Project,
    effect: Effect,
) -> Result<Next, Fail> {
    let now = e.clock.now();
    if let Some(at) = t.retry_at
        && at > now
    {
        let ms = u64::try_from(at.millis() - now.millis()).unwrap_or(0);
        return Ok(Next::Wait(Duration::from_millis(ms)));
    }
    let task = &t.view.id;
    if effect != Effect::StartUnit {
        e.decide(
            e.proj(),
            task,
            Actor::Engine,
            crate::Decision::new("retry", Command::RetryDue),
            |_| Ok(()),
        )?;
        return Ok(Next::Idle);
    }
    if !slot_free(e) {
        return Ok(Next::Idle);
    }
    let (a, from) = if t.needs_reviewer() {
        let head = t.view.head.clone().unwrap_or_default();
        (reviewer_assignment(e, p, t, &head)?, None)
    } else {
        let a = builder_assignment(e, p, t, "Retry after a transient failure.".into()).await?;
        let from = preserved_workspace(e, &t.view.id, &a.base);
        (a, from)
    };
    start_run(
        e,
        task,
        Actor::Engine,
        crate::Decision::new("retry", Command::Start(a.clone())),
        &a,
        from,
        |_| Ok(()),
    )?;
    Ok(Next::Idle)
}
