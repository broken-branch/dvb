use super::{Next, read};
use crate::engine::{AppState, Fail};
use crate::launch;
use crate::proj::{Project, Projections, Task};
use crate::publish;
use dvb_core::event::EndReason;
use dvb_core::forge::Repo;
use dvb_core::git::Git;
use dvb_core::launcher::{UnitId, UnitState};
use dvb_core::state::Effect;
use dvb_core::{
    Actor, Command, EventId, EventKind, Refusal, RefusalCode, RunAssignment, RunId, TaskId,
    TaskState, caps,
};
use std::time::Duration;

use super::runs::{
    builder_assignment, launch_run, preserved_workspace, queue, reviewer_assignment, start_run,
};

// --- Task control --------------------------------------------------------

/// Records the stop, then takes the task's lock (an effect in flight lands
/// first) and stops the task's unit: a run still queued is cancelled and ended
/// at once; a running unit is stopped and waited for until it is inactive
/// (within the stop timeout), then its end is recorded. A stop that cannot be
/// confirmed is an error, recorded as a retry the dispatcher keeps making; the
/// workspace is not reused until the unit is gone.
pub async fn stop(e: &AppState, task: &TaskId, actor: Actor) -> Result<TaskState, Fail> {
    stop_with(e, task, actor, None, |_| Ok(())).await
}

/// `stop`, with the reason shown with the stopped task, recorded only when
/// `allowed` holds in the same decision.
pub(crate) async fn stop_with(
    e: &AppState,
    task: &TaskId,
    actor: Actor,
    reason: Option<String>,
    allowed: impl FnOnce(&Projections) -> Result<(), Refusal>,
) -> Result<TaskState, Fail> {
    stop_decision(
        e,
        task,
        actor,
        crate::Decision::new("stop", Command::Stop(reason)),
        allowed,
    )
    .await
}

async fn stop_decision(
    e: &AppState,
    task: &TaskId,
    actor: Actor,
    decision: crate::Decision<'_>,
    allowed: impl FnOnce(&Projections) -> Result<(), Refusal>,
) -> Result<TaskState, Fail> {
    e.remember();
    e.decide(e.proj(), task, actor, decision, allowed)?;
    let _lock = e.lock(task).await;
    let (t, _) = read(e, task)?;
    let Some(r) = live_run(e, &t) else {
        return Ok(t.view.state);
    };
    if queue(e).remove(&r.assignment.run).is_some() {
        e.end_run(&r.assignment.run, EndReason::Stopped, None)?;
        return Ok(read(e, task)?.0.view.state);
    }
    fail::fail_point!("effect.stop_unit.before");
    match stop_unit(e, &r, EndReason::Stopped).await {
        Ok(()) => Ok(read(e, task)?.0.view.state),
        Err(err) => {
            stop_retry(e, &t);
            Err(match err {
                Fail::Internal(source) => Fail::context(
                    format!(
                        "the run {} did not stop; dvb keeps trying",
                        r.assignment.run
                    ),
                    source,
                ),
                err @ (Fail::Refused(_) | Fail::NotFound(_)) => err,
            })
        }
    }
}

/// At the drain's bound every run still going is stopped with its workspace
/// and branch kept, so no run spans the update. A run doing its task's work
/// marks the task for one resume after handover or rollback; any other run
/// (an orchestrator's, a reviewer past
/// its verdict) has only its unit stopped and ended. A stop that cannot be
/// confirmed in time is left to the target: its restart reconciliation stops a
/// unit that outlived its task's stop or its durable taskless stop request.
pub async fn stop_for_update(e: &AppState, to: &str) {
    let runs: Vec<crate::proj::Run> = {
        let proj = e.proj();
        let queued = queue(e);
        proj.live_runs()
            .filter(|r| !queued.contains_key(&r.assignment.run))
            .cloned()
            .collect()
    };
    let stops = runs.into_iter().map(|r| {
        let reason = format!("stopped for the update to dvb {to}; resumes after the restart");
        async move {
            let works = task_to_stop_for_update(&e.proj(), &r);
            let stopped = match works {
                Some(task) => stop_decision(
                    e,
                    &task,
                    Actor::Engine,
                    crate::Decision {
                        update_stop: true,
                        ..crate::Decision::new("stop for update", Command::Stop(Some(reason)))
                    },
                    |proj| {
                        if task_to_stop_for_update(proj, &r).as_ref() == Some(&task) {
                            Ok(())
                        } else {
                            Err(Refusal::new(
                                RefusalCode::ValueChanged,
                                "the task changed before the update stop",
                            ))
                        }
                    },
                )
                .await
                .map(|_| ()),
                None => stop_unit(e, &r, EndReason::Stopped).await,
            };
            if let Err(err) = stopped {
                eprintln!(
                    "dvb engine: stop {} for the update: {err}",
                    r.assignment.run
                );
            }
        }
    });
    futures_util::future::join_all(stops).await;
}

fn task_to_stop_for_update(proj: &Projections, run: &crate::proj::Run) -> Option<TaskId> {
    let current = proj.runs.get(&run.assignment.run)?;
    if current.ended.is_some() || current.verdict_given {
        return None;
    }
    let task = run.assignment.task.as_ref()?;
    let t = proj.tasks.get(task)?;
    let state = if let TaskState::Unknown { last } = &t.view.state {
        last.as_ref()
    } else {
        &t.view.state
    };
    (t.facts.run.as_ref() == Some(&run.assignment.run)
        && matches!(
            state,
            TaskState::Preparing | TaskState::Running | TaskState::Collecting | TaskState::InReview
        ))
    .then(|| task.clone())
}

/// The task's current run, if it has not ended.
pub(super) fn live_run(e: &AppState, t: &Task) -> Option<crate::proj::Run> {
    let run = t.facts.run.as_ref()?;
    e.proj()
        .runs
        .get(run)
        .filter(|r| r.ended.is_none())
        .cloned()
}

/// Stops a run's unit and waits until it is no longer active, then records its
/// end with `reason` (once: the watch finds it ended).
pub(crate) async fn stop_unit(
    e: &AppState,
    r: &crate::proj::Run,
    reason: EndReason,
) -> Result<(), Fail> {
    if r.assignment.task.is_none() {
        // Persist before asking the launcher: its watcher can see Inactive
        // before stop returns, and an unconfirmed stop can span handover.
        e.append_checked(Some(&r.project), Actor::Engine, |proj, _now| {
            Ok(proj
                .runs
                .get(&r.assignment.run)
                .filter(|live| live.ended.is_none() && !live.stop_requested)
                .map(|_| {
                    vec![EventKind::RunStopRequested {
                        run: r.assignment.run.clone(),
                    }]
                })
                .unwrap_or_default())
        })?;
    }
    let unit = UnitId {
        vendor: r.assignment.vendor,
        run: r.assignment.run.clone(),
    };
    e.launcher().stop(&unit).await.map_err(Fail::internal)?;
    // The unit's own stop timeout, and a little for systemd to report it.
    let deadline = caps::STOP_TIMEOUT + Duration::from_secs(5);
    let mut waited = Duration::ZERO;
    loop {
        match e.launcher().state(&unit).await {
            Ok(UnitState::Inactive | UnitState::Failed) => break,
            Ok(UnitState::Active) | Err(_) if waited < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += Duration::from_millis(100);
            }
            Ok(UnitState::Active) => {
                return Err(Fail::internal(format!("still active after {deadline:?}")));
            }
            Err(err) => return Err(Fail::internal(err)),
        }
    }
    let driver = launch::driver_reason(&e.cfg.paths.work.join(r.assignment.run.as_str()));
    e.end_run(&r.assignment.run, reason, driver)
}

/// Records that stopping the unit is retried, and when.
fn stop_retry(e: &AppState, t: &Task) {
    let attempt = t
        .facts
        .retries
        .get(&Effect::StopUnit)
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    let at = e
        .clock
        .now()
        .plus_millis(dvb_core::state::backoff_millis(attempt, 2_000, 300_000));
    let retry = EventKind::EffectRetry {
        rate_limit: None,
        task: t.view.id.clone(),
        effect: Effect::StopUnit,
        attempt,
        at,
    };
    if let Err(err) = e.append_checked(Some(&t.view.project), Actor::Engine, |_, _| Ok(vec![retry]))
    {
        eprintln!("dvb engine: {err}");
    }
}

/// A stopped task whose unit could not be confirmed stopped: try again when due.
/// Only after a stop recorded that it must be retried; otherwise the stop in
/// progress (or reconcile, after a restart) stops the unit.
pub(super) async fn stop_again(e: &AppState, t: &Task) -> Result<Next, Fail> {
    if !t.facts.retries.contains_key(&Effect::StopUnit) {
        return Ok(Next::Idle);
    }
    let Some(r) = live_run(e, t) else {
        return Ok(Next::Idle);
    };
    if queue(e).contains_key(&r.assignment.run) {
        return Ok(Next::Idle);
    }
    let now = e.clock.now();
    if let Some(at) = t.retry_at
        && at > now
    {
        let ms = u64::try_from(at.millis() - now.millis()).unwrap_or(0);
        return Ok(Next::Wait(Duration::from_millis(ms)));
    }
    if let Err(err) = stop_unit(e, &r, EndReason::Stopped).await {
        eprintln!("dvb engine: stop {}: {err}", r.assignment.run);
        stop_retry(e, t);
        let (t, _) = read(e, &t.view.id)?;
        let ms = t
            .retry_at
            .map_or(0, |at| at.millis() - e.clock.now().millis());
        return Ok(Next::Wait(Duration::from_millis(
            u64::try_from(ms).unwrap_or(0),
        )));
    }
    Ok(Next::Idle)
}

/// Resumes a stopped task by the state it stopped in: a stopped run as a
/// new run (the builder in its preserved workspace, the reviewer on the same
/// head), anything else where it was.
pub async fn resume(e: &AppState, task: &TaskId, actor: Actor) -> Result<TaskState, Fail> {
    e.remember();
    let _lock = e.lock(task).await;
    resume_locked(e, task, actor, None).await
}

/// The caller holds the task lock. Update resumes also check the stop under the writer lock.
pub(super) async fn resume_locked(
    e: &AppState,
    task: &TaskId,
    actor: Actor,
    update_stop: Option<EventId>,
) -> Result<TaskState, Fail> {
    let (t, p) = read(e, task)?;
    let allowed = |proj: &Projections| {
        if let Some(expected) = update_stop
            && proj.tasks.get(task).and_then(|t| t.update_stop) != Some(expected)
        {
            return Err(Refusal::new(
                RefusalCode::ValueChanged,
                "the update stop changed before resume",
            ));
        }
        Ok(())
    };
    // Its workspace is not reused while the stopped unit may still run.
    if matches!(t.view.state, TaskState::Stopped { .. })
        && let Some(r) = live_run(e, &t)
    {
        return Err(e.refuse(
            Some(&t.view.project),
            actor,
            "resume",
            Refusal::new(
                RefusalCode::ValueChanged,
                format!("the stopped run {} has not ended yet", r.assignment.run),
            ),
        ));
    }
    let builder = async |reason: &str| -> Result<(RunAssignment, Option<RunId>), Fail> {
        let a = builder_assignment(e, &p, &t, reason.into()).await?;
        let from = preserved_workspace(e, &t.view.id, &a.base);
        Ok((a, from))
    };
    let run = match &t.view.state {
        TaskState::Stopped { .. } if t.stopped_blocked() || matches!(t.view.outcome, dvb_core::run_verbs::BuilderOutcome::Partial { reason: dvb_core::run_verbs::PartialReason::OutOfRole, .. }) => {
            Some(builder("Resume the approved task in your role; incorporate the merged prerequisites before finishing.").await?)
        }
        TaskState::Stopped { was } => match &**was {
            TaskState::Preparing | TaskState::Running | TaskState::Collecting => {
                Some(builder(if update_stop.is_some() { "Resumed after the dvb update." } else { "Resumed by the owner." }).await?)
            }
            TaskState::InReview => Some((
                reviewer_assignment(e, &p, &t, t.view.head.as_deref().unwrap_or_default())?,
                None,
            )),
            TaskState::RetryPending {
                effect: Effect::StartUnit,
                ..
            } => Some(if t.needs_reviewer() {
                let head = t.view.head.clone().unwrap_or_default();
                (reviewer_assignment(e, &p, &t, &head)?, None)
            } else {
                builder("Resumed by the owner.").await?
            }),
            TaskState::Planned
            | TaskState::Publishing
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Stopped { .. }
            | TaskState::Unknown { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. } => None,
        },
        TaskState::Planned
        | TaskState::Preparing
        | TaskState::Running
        | TaskState::Collecting
        | TaskState::Publishing
        | TaskState::InReview
        | TaskState::AwaitingChecks
        | TaskState::Merging
        | TaskState::RetryPending { .. }
        | TaskState::Unknown { .. }
        | TaskState::Done
        | TaskState::Cancelled
        | TaskState::Failed { .. } => None,
    };
    match run {
        Some((a, from)) => start_run(
            e,
            task,
            actor,
            crate::Decision::new("resume", Command::Resume(Some(a.clone()))),
            &a,
            from,
            allowed,
        ),
        None => {
            let state = e.decide(
                e.proj(),
                task,
                actor,
                crate::Decision::new("resume", Command::Resume(None)),
                allowed,
            )?;
            // Back to Planned, it starts as an approved plan's task does:
            // after its dependencies, in a free slot, with its saved hire choice.
            if matches!(&t.view.state, TaskState::Stopped { was } if **was == TaskState::Planned) {
                crate::spawn(
                    "start approved tasks after resume",
                    super::runs::start_approved(e.clone(), t.view.goal.clone()),
                );
            }
            Ok(state)
        }
    }
}

/// Retries a failed task, with its retry text saved in the same transaction
/// and its branch repair count reset. A head every review approved whose CI
/// has not failed starts no run: it goes back to its checks and merge, which
/// bring the branch up to date first. A head that failed CI or still has open
/// findings gets the builder on the head (one more round, even past the
/// review-round cap); a head that conflicts with the default branch a
/// conflict fix round; an unreviewed head its reviewer; a task with no head
/// its builder in the preserved workspace. An unfinished publication goes
/// back to publishing instead, which opens or edits the PR before any review.
/// A stopped out-of-role task can also resume its builder under the existing
/// scope authorization, and a stopped blocked or other incomplete task with
/// the owner's yes. Any other task stopped with a PR, or in review with one,
/// gets one builder round on its head (even past the review-round cap), then
/// review and CI: a review in progress is stopped first, so the old head is
/// never merged as it was. Any other task stopped with no head and with kept
/// work, retried with text, gets one builder round on that work, then its
/// completion check, publication, review and CI. Any other state is refused.
/// `allowed` (the orchestrator's yes; the owner passes none) is checked
/// atomically with the decision, or with the stop of a review, which then
/// holds for the retry that follows: that stop is the retry's own, so it does
/// not use up the yes.
pub async fn retry_failed(
    e: &AppState,
    task: &TaskId,
    actor: Actor,
    text: Option<String>,
    allowed: impl Fn(&Projections) -> Result<(), Refusal>,
) -> Result<TaskState, Fail> {
    let text = match text {
        Some(text) if text.len() > dvb_core::api::RETRY_TEXT_MAX_BYTES => {
            let project = e.proj().tasks.get(task).map(|t| t.view.project.clone());
            return Err(e.refuse(
                project.as_ref(),
                actor,
                "retry",
                Refusal::new(
                    RefusalCode::InvalidValue,
                    format!(
                        "keep retry text within {} bytes",
                        dvb_core::api::RETRY_TEXT_MAX_BYTES
                    ),
                ),
            ));
        }
        Some(text) if !text.trim().is_empty() => Some(text),
        Some(_) | None => None,
    };
    e.remember();
    let in_review = e
        .proj()
        .tasks
        .get(task)
        .filter(|t| t.view.state == TaskState::InReview && on_its_pr(t))
        .map(|t| t.view.project.clone());
    let stopped_here = in_review.is_some();
    if let Some(project) = in_review {
        let early = allowed(&e.proj());
        if let Err(r) = early {
            return Err(e.refuse(Some(&project), actor, "retry", r));
        }
        let reason = "stopped to send it back to its builder".to_owned();
        stop_with(e, task, actor.clone(), Some(reason), &allowed).await?;
    }
    let _lock = e.lock(task).await;
    let (t, p) = read(e, task)?;
    // Finish an interrupted PR open/body edit before admitting its reviewer.
    let failed = matches!(t.view.state, TaskState::Failed { .. });
    let scope = matches!(t.view.state, TaskState::Stopped { .. })
        && matches!(
            t.view.outcome,
            dvb_core::run_verbs::BuilderOutcome::Partial {
                reason: dvb_core::run_verbs::PartialReason::OutOfRole,
                ..
            }
        );
    // A blocked or other incomplete stop resumes its builder the same way,
    // with the owner's yes.
    let resumes = scope || t.stopped_blocked();
    // Any other stop with a PR: a builder round on its head.
    let on_head = !resumes && matches!(t.view.state, TaskState::Stopped { .. }) && on_its_pr(&t);
    // Any other stop with no head, sent back with text: a builder round on
    // its kept work. Without text it is refused with advice to resume.
    let on_kept = !resumes
        && text.is_some()
        && matches!(t.view.state, TaskState::Stopped { .. })
        && t.view.head.is_none()
        && kept_work(e, &t);
    let pending = t.view.head.is_some() && (t.view.pr.is_none() || t.publication_pending);
    let (cmd, run) = if (!failed && !resumes && !on_head && !on_kept) || (pending && !resumes) {
        (Command::Retry(None), None)
    } else {
        // Its workspace is not reused while its last run may still run.
        if let Some(r) = live_run(e, &t) {
            return Err(e.refuse(
                Some(&t.view.project),
                actor,
                "retry",
                Refusal::new(
                    RefusalCode::ValueChanged,
                    format!("the failed run {} has not ended yet", r.assignment.run),
                ),
            ));
        }
        let who = if scope {
            "the orchestrator within the approved scope"
        } else {
            "the owner"
        };
        let fix = resumes || on_head || on_kept || t.checks_failed() || t.facts.open_findings() > 0;
        // Only its merge failed: the unchanged head needs no builder or review.
        if !fix
            && t.needs_reviewer()
            && t.reviews_approved()
            && conflicting_main(e, &t, &p).await?.is_none()
        {
            (Command::RetryMerge, None)
        } else {
            let (a, from) = retry_run(e, &t, &p, who, fix).await?;
            let cmd = if resumes {
                Command::Resume(Some(a.clone()))
            } else {
                Command::Retry(Some(a.clone()))
            };
            (cmd, Some((a, from)))
        }
    };
    let state = e.decide(
        e.proj(),
        task,
        actor,
        crate::Decision {
            pre: vec![EventKind::TaskRetryTextSet {
                task: task.clone(),
                text,
            }],
            ..crate::Decision::new("retry", cmd)
        },
        |proj| if stopped_here { Ok(()) } else { allowed(proj) },
    )?;
    if let Some((a, from)) = run
        && e.proj().runs.contains_key(&a.run)
    {
        launch_run(e, &a, from);
    }
    Ok(state)
}

/// The task has a published head with its PR open on it.
fn on_its_pr(t: &Task) -> bool {
    t.view.head.is_some() && t.view.pr.is_some() && !t.publication_pending
}

/// The task's last builder left its workspace for a new builder to carry on in.
fn kept_work(e: &AppState, t: &Task) -> bool {
    let base = t
        .facts
        .builder
        .as_ref()
        .and_then(|run| e.proj().runs.get(run).map(|r| r.assignment.base.clone()));
    base.is_some_and(|base| preserved_workspace(e, &t.view.id, &base).is_some())
}

/// The run a retry of the task starts, and the workspace it continues in.
/// `fix` starts a builder on the head even when it needs a review.
pub(super) async fn retry_run(
    e: &AppState,
    t: &Task,
    p: &Project,
    who: &str,
    fix: bool,
) -> Result<(RunAssignment, Option<RunId>), Fail> {
    // The task's conflict fix round, pinned to the default branch's commit
    // it merges: set when the head conflicts with it, else cleared.
    // A task with no head has no branch to merge into.
    let conflict = conflicting_main(e, t, p).await?;
    if t.view.head.is_some() {
        let merges = publish::merges_ref(t.view.id.as_str());
        let pin = match &conflict {
            Some(main) => vec!["update-ref", &merges, main],
            None => vec!["update-ref", "-d", &merges],
        };
        Git::new(&e.mirror(&p.view.name))
            .run(&pin, &[])
            .await
            .map_err(Fail::internal)?;
    }
    Ok(if conflict.is_some() {
        // A head that conflicts with the default branch would fail its
        // merge again: a fix round on the kept branch merges it instead,
        // in a fresh checkout that has that commit. Its builder started
        // on the head, so a crash's retry or a resume continues the round
        // (`Task::needs_reviewer`).
        let main_branch = &p.view.default_branch;
        let reason = dvb_core::wording::render(
            "question.retry-conflict-reason",
            &serde_json::json!({"who": who.to_string(), "main_branch": main_branch.to_string()}),
        );
        (builder_assignment(e, p, t, reason).await?, None)
    } else if t.needs_reviewer() && !fix {
        let head = t.view.head.clone().unwrap_or_default();
        (reviewer_assignment(e, p, t, &head)?, None)
    } else {
        let a = builder_assignment(e, p, t, format!("Retried by {who}.")).await?;
        let from = preserved_workspace(e, &t.view.id, &a.base);
        (a, from)
    })
}

/// The default branch's commit, fetched now, when the failed task's published
/// head conflicts with it; else `None`.
async fn conflicting_main(e: &AppState, t: &Task, p: &Project) -> Result<Option<String>, Fail> {
    let Some(head) = &t.view.head else {
        return Ok(None);
    };
    let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
    let mirror = e.mirror(&p.view.name);
    let main = e
        .fetch(
            forge.as_ref(),
            &Repo(p.view.repo.clone()),
            &mirror,
            &p.view.default_branch,
        )
        .await
        .map_err(Fail::internal)?;
    let conflicts = publish::conflicts(&mirror, head, &main)
        .await
        .map_err(Fail::internal)?;
    Ok(conflicts.then_some(main))
}

/// At the review-round cap the task stops with its PR and findings kept, and
/// the owner gets one question, recorded with that decision: leave it stopped
/// (recommended), or change course. The same
/// choices, with the actual failure, for checks/conflicts.
pub(crate) fn round_cap_question_event(
    e: &AppState,
    t: &Task,
    failure: Option<&str>,
) -> Result<EventKind, Fail> {
    let version = e
        .proj()
        .goals
        .get(&t.view.goal)
        .map(|g| g.version)
        .ok_or_else(|| Fail::NotFound(format!("goal {}", t.view.goal)))?;
    let option = dvb_core::event::QuestionOption::owner;
    let subject = match failure {
        Some(why) => dvb_core::wording::render(
            "question.rounds-failed-question",
            &serde_json::json!({"title": t.view.title.to_string(), "round": t.facts.round.to_string(), "reason": why.strip_suffix("; needs a fix round").unwrap_or(why).to_string()}),
        ),
        None => dvb_core::wording::render(
            "question.rounds-open-question",
            &serde_json::json!({"title": t.view.title.to_string(), "round": t.facts.round.to_string()}),
        ),
    };
    let (leave, kept) = if failure.is_some() {
        (
            dvb_core::wording::text("question.leave-failed"),
            dvb_core::wording::text("question.rounds-failed-kept"),
        )
    } else {
        (
            dvb_core::wording::text("question.leave-stopped"),
            dvb_core::wording::text("question.rounds-stopped-kept"),
        )
    };
    Ok(EventKind::QuestionOpened {
        kind: Some(dvb_core::event::QuestionKind::Ordinary),
        main_ci: None,
        task: Some(t.view.id.clone()),
        question: dvb_core::QuestionId::new(e.new_id()),
        subject,
        format: dvb_core::event::QuestionFormat::QuickCompare,
        options: vec![
            option(1, leave, kept),
            option(
                2,
                dvb_core::wording::text("question.change-course"),
                dvb_core::wording::text("question.redirect-effect"),
            ),
        ],
        recommended: 1,
        lapses_on: dvb_core::event::LapsesOn::GoalVersion(version),
    })
}
