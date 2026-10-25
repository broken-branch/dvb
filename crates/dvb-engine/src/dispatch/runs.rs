use super::{kick, read, slot_free};
use crate::engine::{AppState, Fail};
use crate::proj::{Project, Projections, Task};
use crate::{dispatch, reconcile};
use crate::{launch, spawn};
use dvb_core::api::GoalState;
use dvb_core::event::EndReason;
use dvb_core::event::ResourceWait;
use dvb_core::forge::Repo;
use dvb_core::model::fallback_model;
use dvb_core::roles::{HireRecord, ReviewKind};
use dvb_core::state::Effect;
use dvb_core::{
    Actor, Command, Event, EventId, EventKind, ModelPool, ProjectId, Refusal, RefusalCode,
    RunAssignment, RunId, TaskId, TaskState,
};
use std::time::Duration;

use crate::wake::{missed_wake, orchestrator_failed};

// --- Runs ----------------------------------------------------------------------

/// A builder run of the task, on `round_hire`; on the publication head
/// once there is one, else where the last build started, else the default
/// branch now.
pub(super) async fn builder_assignment(
    e: &AppState,
    p: &Project,
    t: &Task,
    reason: String,
) -> Result<RunAssignment, Fail> {
    let prev = t
        .facts
        .builder
        .as_ref()
        .and_then(|r| e.proj().runs.get(r).cloned());
    let hire = round_hire(p, t, prev.as_ref().map(|r| &r.assignment.hire))
        .cloned()
        .ok_or_else(|| Fail::internal("the task's role has no hire"))?;
    let base = match (t.view.head.clone(), prev.and_then(|r| r.assignment.base)) {
        (Some(head), _) => head,
        (None, Some(base)) => base,
        (None, None) => {
            let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
            e.fetch(
                forge.as_ref(),
                &Repo(p.view.repo.clone()),
                &e.mirror(&p.view.name),
                &p.view.default_branch,
            )
            .await
            .map_err(Fail::internal)?
        }
    };
    Ok(RunAssignment {
        review_stage: None,
        goal_version: None,
        run: RunId::new(e.new_id()),
        task: Some(t.view.id.clone()),
        role: t.view.role.clone(),
        hire: hire.id.clone(),
        model: hire.model.clone(),
        vendor: e.hire_vendor(&hire.model)?,
        effort: hire.effort,
        reason,
        reviews: None,
        base: Some(base),
        routine: t.view.routine,
    })
}

/// The hire for the task's next builder round. The role's routine hire takes
/// a routine task's rounds and every fix round on an open PR, until
/// `caps::ROUTINE_SEND_BACKS` of its publications were sent back; then, or
/// with no routine hire, the hire of the last build if active (one builder
/// vendor per branch), else the role's default hire.
pub(crate) fn round_hire<'a>(
    p: &'a Project,
    t: &Task,
    last: Option<&dvb_core::HireId>,
) -> Option<&'a HireRecord> {
    let escalated = t.routine_sent_back >= dvb_core::caps::ROUTINE_SEND_BACKS;
    match p.routine_hire(&t.view.role) {
        Some(h) if !escalated && (t.view.routine || t.view.pr.is_some()) => Some(h),
        Some(_) | None => last
            .and_then(|id| {
                p.hires.iter().find(|h| {
                    &h.id == id
                        && h.status == dvb_core::roles::HireStatus::Active
                        && !(escalated && h.routine)
                })
            })
            .or_else(|| p.hire(&t.view.role, None)),
    }
}

/// The head's accountable reviewer, pinned to the head.
pub(super) fn reviewer_assignment(
    e: &AppState,
    p: &Project,
    t: &Task,
    head: &str,
) -> Result<RunAssignment, Fail> {
    let stage = t.next_review();
    let reviewer = t
        .reviewer(p, stage)
        .ok_or_else(|| Fail::internal("the project has no reviewer role"))?;
    let hire = p
        .hire(&reviewer, None)
        .cloned()
        .ok_or_else(|| Fail::internal("the reviewer has no hire"))?;
    Ok(RunAssignment {
        review_stage: Some(stage),
        goal_version: None,
        run: RunId::new(e.new_id()),
        task: Some(t.view.id.clone()),
        role: reviewer,
        hire: hire.id.clone(),
        model: hire.model.clone(),
        vendor: e.hire_vendor(&hire.model).map_err(|error| {
            e.refuse(
                Some(&p.view.id),
                Actor::Engine,
                "reviewer assignment",
                error,
            )
        })?,
        effort: hire.effort,
        reason: format!(
            "{} review {head}.",
            match stage {
                ReviewKind::Code => "Code",
                ReviewKind::Design => "Design",
            }
        ),
        reviews: Some(head.to_owned()),
        base: Some(head.to_owned()),
        routine: false,
    })
}

/// Preparation markers exclude incomplete clones and queued runs from resume selection.
pub(super) fn preserved_workspace(
    e: &AppState,
    task: &TaskId,
    base: &Option<String>,
) -> Option<RunId> {
    let proj = e.proj();
    proj.runs
        .values()
        .rev()
        .find(|r| {
            if r.assignment.task.as_ref() != Some(task)
                || r.assignment.reviews.is_some()
                || &r.assignment.base != base
            {
                return false;
            }
            launch::prepared(&e.cfg.paths.work.join(r.assignment.run.as_str()))
        })
        .map(|r| r.assignment.run.clone())
}

/// Decides a run start (`Start`, or `Resume` with the run) and, if `decide`
/// started it, launches it. The caller holds the task's lock.
pub(super) fn start_run(
    e: &AppState,
    task: &TaskId,
    actor: Actor,
    command: &str,
    cmd: Command,
    a: &RunAssignment,
    from: Option<RunId>,
) -> Result<TaskState, Fail> {
    let state = e.decide(
        e.proj(),
        task,
        actor,
        crate::Decision::new(command, cmd),
        |_| Ok(()),
    )?;
    if e.proj().runs.contains_key(&a.run) {
        launch_run(e, a, from);
    }
    Ok(state)
}

/// Starts a recorded run's unit, in the background: queued (`Engine::queued`)
/// until `decide_launch` lets it start (`wait_launch`, waited for outside the
/// task's lock), then started under the lock only if the task wants that
/// run. A stop cancels a queued run; a pause holds it until unpaused.
pub(super) fn launch_run(e: &AppState, a: &RunAssignment, from: Option<RunId>) {
    queue(e).insert(a.run.clone(), crate::engine::Queued { from, held: false });
    spawn("launch the run", launch_later(e.clone(), a.clone()));
}

pub(super) fn queue(
    e: &AppState,
) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<RunId, crate::engine::Queued>> {
    e.queued.lock().unwrap_or_else(dvb_core::sync::recover)
}

/// Releases a run held by a pause and returns whether it held the run.
pub(super) fn unhold(e: &AppState, run: &RunId) -> bool {
    match queue(e).get_mut(run) {
        Some(q) if q.held => {
            q.held = false;
            true
        }
        Some(_) | None => false,
    }
}

/// A run starts only with this much free on the work directory's filesystem.
const MIN_FREE_BYTES: u64 = 25 * GB;
const GB: u64 = 1024 * 1024 * 1024;

/// One run's 8 GB limit plus 2 GB headroom.
const MIN_AVAILABLE_MEMORY_BYTES: u64 = 10 * GB;

/// How often a run held for resources looks again, besides when a run ends.
const RESOURCE_POLL: Duration = Duration::from_secs(30);

/// The free space, when it is below `MIN_FREE_BYTES`.
fn short_of_disk(e: &AppState) -> Option<u64> {
    e.free_bytes().filter(|free| *free < MIN_FREE_BYTES)
}

fn short_of_memory(e: &AppState) -> Option<u64> {
    e.available_memory_bytes()
        .filter(|available| *available < MIN_AVAILABLE_MEMORY_BYTES)
}

fn overloaded(e: &AppState) -> Option<(f64, usize)> {
    e.host_load_reading()
        .filter(|(load, cpus)| *load > *cpus as f64)
}

/// What to do with a run about to launch: the one decision,
/// taken before waiting and again under the task's lock.
pub(crate) enum Launch {
    Ready(Option<Box<EventKind>>),
    Wait,
}

/// The shared start rule for task runs and taskless orchestrators. Under the
/// switch only the effective vendor gates admission, and usage fallback is off.
/// Other launches use the orchestrator vendor gate or task fallback.
pub(crate) fn decide_launch(
    e: &AppState,
    proj: &crate::proj::Projections,
    project: &ProjectId,
    a: &RunAssignment,
    now: dvb_core::Timestamp,
) -> Result<Launch, Fail> {
    if short_of_disk(e).is_some() || short_of_memory(e).is_some() || overloaded(e).is_some() {
        return Ok(Launch::Wait);
    }
    if let Some((_, only)) = proj.only.as_ref().filter(|(_, only)| only.ends_at > now) {
        let (model, effort) = dvb_core::model::only_model(&a.model, a.effort, only, &e.pool)
            .ok_or_else(|| Fail::internal("the model switch has no supported model/effort"))?;
        let vendor = e
            .pool
            .find(model)
            .ok_or_else(|| Fail::internal("switch model missing"))?
            .vendor;
        if e.vendors.is_limited(vendor) {
            return Ok(Launch::Wait);
        }
        let moved = model != a.model || effort != a.effort;
        return Ok(Launch::Ready(moved.then(|| {
            Box::new(EventKind::RunRerouted {
                run: a.run.clone(),
                task: a.task.clone(),
                role: a.role.clone(),
                from_model: a.model.clone(),
                to_model: model.into(),
                vendor,
                effort,
                limited: None,
                resets_at: None,
            })
        })));
    }
    if a.task.is_none() {
        return Ok(if e.vendors.is_limited(a.vendor) {
            Launch::Wait
        } else {
            Launch::Ready(None)
        });
    }
    let Some(p) = proj.projects.get(project) else {
        return Ok(Launch::Ready(None));
    };
    let orchestrator = p
        .role_by_slug(dvb_core::roles::ORCHESTRATOR)
        .and_then(|r| p.hire(&r.id, None))
        .cloned();
    let role_hires: Vec<_> = p
        .hires
        .iter()
        .filter(|h| h.role == a.role)
        .cloned()
        .collect();
    let orchestrator_vendor = orchestrator
        .as_ref()
        .and_then(|o| e.pool.find(&o.model))
        .map(|m| m.vendor);
    if orchestrator_vendor.is_some_and(|v| e.vendors.is_limited(v)) {
        return Ok(Launch::Wait);
    }
    if !e.vendors.is_limited(a.vendor) {
        return Ok(Launch::Ready(None));
    }
    let on = proj.settings.vendor_fallback();
    let pool = ModelPool {
        models: e.selected_models(proj).into_iter().cloned().collect(),
    };
    let target = orchestrator
        .filter(|_| on)
        .and_then(|o| fallback_model(&role_hires, &o, &pool));
    let Some((to_model, effort)) = target else {
        return Ok(Launch::Wait);
    };
    let Some(vendor) = pool.find(&to_model).map(|m| m.vendor) else {
        return Ok(Launch::Wait);
    };
    if vendor == a.vendor || e.vendors.is_limited(vendor) {
        return Ok(Launch::Wait);
    }
    Ok(Launch::Ready(Some(Box::new(EventKind::RunRerouted {
        run: a.run.clone(),
        task: a.task.clone(),
        role: a.role.clone(),
        from_model: a.model.clone(),
        to_model,
        vendor,
        effort,
        limited: Some(a.vendor),
        resets_at: e.vendors.resets_at(a.vendor),
    }))))
}

/// Keep reroute evidence and same-vendor review warnings on the feed
/// and task output paths, comparing actual assignments rather than hire vendors.
pub(crate) fn launch_events(
    proj: &crate::proj::Projections,
    a: &RunAssignment,
    reroute: Option<Box<EventKind>>,
) -> Vec<EventKind> {
    let vendor = if let Some(EventKind::RunRerouted { vendor, .. }) = reroute.as_deref() {
        *vendor
    } else {
        a.vendor
    };
    let flag = (proj.only.is_some() || reroute.is_some())
        && a.reviews.is_some()
        && a.task
            .as_ref()
            .and_then(|id| proj.tasks.get(id))
            .and_then(|t| t.facts.published_by.as_ref().or(t.facts.builder.as_ref()))
            .and_then(|id| proj.runs.get(id))
            .is_some_and(|r| r.assignment.vendor == vendor);
    let mut kinds: Vec<_> = reroute.into_iter().map(|kind| *kind).collect();
    kinds.extend(resource_reason_event(proj, a, None));
    if proj.resource_wait(&a.run).is_some() {
        kinds.push(EventKind::RunOutput {
            resource: Some(ResourceWait::Available),
            run: a.run.clone(),
            item: dvb_core::event::OutputItem::Note(String::new()),
        });
    }
    if flag {
        kinds.push(EventKind::RunOutput {
            resource: None,
            run: a.run.clone(),
            item: dvb_core::event::OutputItem::Note(format!(
                "Same-vendor review: builder and reviewer both run on {}",
                vendor.as_str()
            )),
        });
    }
    kinds
}

/// A waiter subscribes before checking, so replacement/clear/expiry can wake it
/// immediately even if its previously chosen vendor stays limited.
pub(crate) async fn wait_launch(
    e: &AppState,
    project: &ProjectId,
    a: &RunAssignment,
) -> Result<(), Fail> {
    loop {
        let mut changes = e.vendors.subscribe();
        let changed = e.only_changed.notified();
        let freed = e.disk_freed.notified();
        tokio::pin!(changed, freed);
        changed.as_mut().enable();
        freed.as_mut().enable();
        e.expire_only(None)?;
        if !matches!(
            decide_launch(e, &e.proj(), project, a, e.clock.now())?,
            Launch::Wait
        ) {
            // Clear before handing off to any remaining admission guard.
            refresh_resource_reason(e, project, a, None)?;
            return Ok(());
        }
        let mut waiting = None;
        if let Some(free) = short_of_disk(e) {
            tell_waiting_for_resource(
                e,
                project,
                a,
                ResourceWait::Disk,
                &format!("{} GB free, needs {} GB", free / GB, MIN_FREE_BYTES / GB),
            )?;
            waiting = Some(ResourceWait::Disk);
        }
        if let Some(available) = short_of_memory(e) {
            tell_waiting_for_resource(
                e,
                project,
                a,
                ResourceWait::Memory,
                &format!(
                    "{} GB available, needs {} GB",
                    available / GB,
                    MIN_AVAILABLE_MEMORY_BYTES / GB
                ),
            )?;
            waiting = Some(ResourceWait::Memory);
        }
        if let Some((load, cpus)) = overloaded(e) {
            tell_waiting_for_resource(
                e,
                project,
                a,
                ResourceWait::HostLoad,
                &format!("{load:.2} (1-minute average), {cpus} CPUs"),
            )?;
            waiting = Some(ResourceWait::HostLoad);
        }
        refresh_resource_reason(e, project, a, waiting)?;
        tokio::select! {
            _ = changes.changed() => {}
            () = &mut changed => {}
            () = &mut freed => {}
            () = tokio::time::sleep(RESOURCE_POLL) => {}
        }
    }
}

/// Only the current held run may change the task's projected status.
fn resource_reason_event(
    proj: &crate::proj::Projections,
    a: &RunAssignment,
    resource: Option<ResourceWait>,
) -> Option<EventKind> {
    let r = proj.runs.get(&a.run).filter(|r| r.ended.is_none())?;
    let task = r.assignment.task.as_ref()?;
    let t = proj.tasks.get(task).filter(|t| {
        t.facts.run.as_ref() == Some(&a.run)
            && matches!(t.view.state, TaskState::Preparing | TaskState::InReview)
    })?;
    if resource.is_none() && proj.resource_wait(&a.run).is_none() {
        return None;
    }
    let reason = resource
        .map(|resource| dvb_core::wording::resource_words(resource).into())
        .or_else(|| {
            r.assignment.reviews.as_ref().map(|_| {
                dvb_core::wording::review_reason(
                    r.assignment.review_stage.unwrap_or(ReviewKind::Code),
                )
                .into()
            })
        });
    (t.view.reason != reason || proj.resource_wait(&a.run) != resource).then(|| {
        EventKind::TaskReasonChanged {
            resource: Some(resource.unwrap_or(ResourceWait::Available)),
            task: task.clone(),
            run: a.run.clone(),
            reason,
        }
    })
}

/// Task reason events refresh live snapshots.
fn refresh_resource_reason(
    e: &AppState,
    project: &ProjectId,
    a: &RunAssignment,
    resource: Option<ResourceWait>,
) -> Result<(), Fail> {
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        Ok(resource_reason_event(proj, a, resource)
            .into_iter()
            .collect())
    })?;
    Ok(())
}

/// One owner notice per resource/run, including taskless orchestrators and
/// repeated waits after update admission.
fn tell_waiting_for_resource(
    e: &AppState,
    project: &ProjectId,
    a: &RunAssignment,
    resource: ResourceWait,
    detail: &str,
) -> Result<(), Fail> {
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        Ok(if proj.resource_notice_given(&a.run, resource) {
            vec![]
        } else {
            let mut kinds = vec![EventKind::RunOutput {
                resource: Some(resource),
                run: a.run.clone(),
                item: dvb_core::event::OutputItem::Note(detail.into()),
            }];
            kinds.extend(resource_reason_event(proj, a, Some(resource)));
            kinds
        })
    })?;
    Ok(())
}

pub(super) async fn launch_later(e: AppState, a: RunAssignment) -> Result<(), Fail> {
    let task = a
        .task
        .clone()
        .ok_or_else(|| Fail::internal("a task run without a task"))?;
    loop {
        let project = e
            .proj()
            .tasks
            .get(&task)
            .map(|t| t.view.project.clone())
            .ok_or_else(|| Fail::internal("no task for queued run"))?;
        wait_launch(&e, &project, &a).await?;
        // An update in progress holds new runs until it has finished.
        e.update.admit().await;
        let _lock = e.lock(&task).await;
        // Cancelled by a stop while it waited.
        let Some(from) = queue(&e).get(&a.run).map(|q| q.from.clone()) else {
            return Ok(());
        };
        let (t, p) = read(&e, &task)?;
        if p.view.paused {
            if let Some(q) = queue(&e).get_mut(&a.run) {
                q.held = true;
            }
            return Ok(());
        }
        // Decided again now that the lock is held: a vendor may have closed meanwhile.
        // Waiting keeps the queue entry and gives the lock back.
        let wanted = match &t.view.state {
            TaskState::Preparing | TaskState::InReview => t.facts.run.as_ref() == Some(&a.run),
            TaskState::Planned
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Stopped { .. }
            | TaskState::Unknown { .. }
            | TaskState::Done
            | TaskState::Cancelled
            | TaskState::Failed { .. } => false,
        };
        if !wanted {
            // Moved on while it waited: never started.
            return e.end_run(&a.run, EndReason::Stopped, None);
        }
        e.expire_only(None)?;
        let mut launch = Launch::Wait;
        e.append_checked(Some(&project), Actor::Engine, |proj, now| {
            launch = decide_launch(&e, proj, &project, &a, now)?;
            Ok(match &launch {
                Launch::Ready(reroute) => launch_events(proj, &a, reroute.clone()),
                Launch::Wait => vec![],
            })
        })?;
        if matches!(launch, Launch::Wait) {
            continue;
        }
        queue(&e).remove(&a.run);
        let (name, default, vars) = match &a.reviews {
            Some(head) => (
                "reviewer",
                launch::REVIEWER_SCRIPT,
                vec![("head", head.clone())],
            ),
            None if from.is_some() => ("builder-resume", launch::BUILDER_SCRIPT, Vec::new()),
            None => ("builder", launch::BUILDER_SCRIPT, Vec::new()),
        };
        let script = launch::fake_script(&e, name, default);
        return match launch::launch(e.clone(), a.run.clone(), script, vars, from).await {
            Ok(()) => {
                if a.reviews.is_none() {
                    e.run_command(
                        &a.run,
                        crate::Decision::new("unit_active", Command::UnitActive),
                    )?;
                }
                Ok(())
            }
            // The run's end decides the task's retry or failure.
            Err(err) => {
                eprintln!("dvb engine: run {}: {err}", a.run);
                e.end_run(&a.run, EndReason::Crashed, None)
            }
        };
    }
}

/// A task start decided under the task's lock, then launched (tests).
#[cfg(any(test, feature = "test-support"))]
pub async fn start_task(e: &AppState, a: RunAssignment, actor: Actor) -> Result<RunId, Fail> {
    let task = a
        .task
        .clone()
        .ok_or_else(|| Fail::internal("a task start without a task"))?;
    let _lock = e.lock(&task).await;
    start_locked(e, a, actor, None, None, None)
}

/// A task start, with the task's lock held by the caller. `hire` replaces
/// active hires with an Auto hire in the accepted start transaction (concurrent
/// same-model hiring reuses the active record). `waiting` (`task.waiting`) is
/// recorded with a refusal for the run cap. `approved_version` restricts
/// automatic admission to a never-started task of that approved goal version,
/// checked with the start decision.
pub(crate) fn start_locked(
    e: &AppState,
    a: RunAssignment,
    actor: Actor,
    hire: Option<HireRecord>,
    waiting: Option<EventKind>,
    approved_version: Option<EventId>,
) -> Result<RunId, Fail> {
    e.remember();
    let task = a
        .task
        .clone()
        .ok_or_else(|| Fail::internal("a task start without a task"))?;
    let run = a.run.clone();
    let cmd = Command::Start(a.clone());
    if let Some(version) = approved_version {
        // Automatic admission uses the active hire, never creating a hire.
        debug_assert!(hire.is_none());
        e.decide(
            e.proj(),
            &task,
            actor,
            crate::Decision::new("task start", cmd),
            |proj| {
                let t = proj.tasks.get(&task).ok_or_else(|| {
                    Refusal::new(RefusalCode::ValueChanged, "the task disappeared")
                })?;
                if t.facts.run.is_some()
                    || proj.needs_orchestrator(t, &e.pool)
                    || t.waiting
                        .as_ref()
                        .is_some_and(|w| w.approved_version != Some(version))
                    || t.view.state != TaskState::Planned
                {
                    return Err(Refusal::new(
                        RefusalCode::ValueChanged,
                        "the task is no longer eligible for automatic admission",
                    ));
                }
                if !t.approved
                    || !proj
                        .goals
                        .get(&t.view.goal)
                        .is_some_and(|g| g.version == version && g.state == GoalState::Approved)
                {
                    return Err(Refusal::new(
                        RefusalCode::GoalNotApproved,
                        "the goal version is no longer approved",
                    ));
                }
                Ok(())
            },
        )?;
        launch_run(e, &a, None);
        return Ok(run);
    }
    let a = e.admit_task_hiring(&task, actor, a, hire, waiting)?;
    if e.proj().runs.contains_key(&a.run) {
        launch_run(e, &a, None);
    }
    Ok(run)
}

/// How soon a `start_waiting` pass that failed (the forge could not be read)
/// runs again.
const WAITING_AGAIN: std::time::Duration = std::time::Duration::from_secs(30);

/// A slot may be free (`Engine::end_run`, an unpause, the engine's start): the
/// engine starts deferred requests (`task.waiting`), explicit requests first,
/// then automatic admissions, oldest first, while a slot is free. Only planned
/// tasks of an approved plan in an unpaused project are started; `decide` checks
/// approval, pause and caps again, and the run waits for its vendor like any
/// other. One pass at a time; a pass with nothing to do does nothing. A start
/// refused for another reason stays waiting for the next run end; one that
/// failed is tried again in `WAITING_AGAIN`, with or without a run end.
pub async fn start_waiting(e: AppState) -> Result<(), Fail> {
    let _one = e.starting.lock().await;
    let mut tried = Vec::new();
    let mut failed = false;
    while slot_free(&e) {
        let next = {
            let proj = e.proj();
            proj.tasks
                .values()
                .filter(|t| t.approved && t.view.state == TaskState::Planned)
                .filter(|t| proj.unmet_dependencies(t).is_empty())
                .filter(|t| !proj.needs_orchestrator(t, &e.pool))
                .filter(|t| !tried.contains(&t.view.id))
                .filter_map(|t| t.waiting.as_ref().map(|w| (t, w)))
                .filter_map(|(t, w)| {
                    let g = proj.goals.get(&t.view.goal)?;
                    if g.state != GoalState::Approved
                        || w.approved_version.is_some_and(|v| v != g.version)
                    {
                        return None;
                    }
                    let p = proj
                        .projects
                        .get(&t.view.project)
                        .filter(|p| !p.view.paused)?;
                    let body = dvb_core::run_verbs::TaskStartBody {
                        task: t.view.id.clone(),
                        role: p.role(&t.view.role)?.slug.clone(),
                        model: w.model.clone(),
                        effort: w.effort,
                        reason: w.reason.clone(),
                        routine: w.routine,
                    };
                    Some((w.approved_version, w.since, t.view.project.clone(), body))
                })
                .min_by_key(|(version, since, _, _)| (version.is_some(), *since))
        };
        let Some((version, _, project, body)) = next else {
            break;
        };
        tried.push(body.task.clone());
        let start =
            crate::dispatch::start_as_asked(&e, &project, Actor::Engine, body, None, version);
        match start.await {
            Err(Fail::Refused(r))
                if r.code
                    == (RefusalCode::CapReached {
                        which: dvb_core::Cap::ConcurrentRuns,
                    }) =>
            {
                break;
            }
            Ok(_) | Err(Fail::Refused(_)) => {}
            Err(err @ (Fail::NotFound(_) | Fail::Internal(_))) => {
                eprintln!("dvb engine: start a waiting task: {err}; again in {WAITING_AGAIN:?}");
                failed = true;
            }
        }
    }
    if failed {
        start_waiting_again(&e);
    }
    // Missing hires and unchosen Auto tasks remain recoverable even at the
    // cap. Any goal's slot release must revisit their orchestrator wake too.
    let projects: Vec<_> = e.proj().projects.keys().cloned().collect();
    for project in projects {
        missed_wake(&e, &project, false);
    }
    Ok(())
}

/// A merge or a run end advances the same approved goal. Explicit waiting
/// requests take priority; the remaining
/// never-started Custom tasks use their role's active hire and its effort.
/// Auto tasks without a recorded choice return to the orchestrator. The shared
/// start path rechecks approval, pause and caps under the task lock, and queues
/// the run behind the usual vendor, disk, memory and update guards.
pub(crate) async fn start_approved(e: AppState, goal: dvb_core::GoalId) -> Result<(), Fail> {
    e.remember();
    start_waiting(e.clone()).await?;
    let tasks = {
        let proj = e.proj();
        proj.goals
            .get(&goal)
            .filter(|g| g.state == GoalState::Approved)
            .map(|g| g.tasks.clone())
            .unwrap_or_default()
    };
    let mut capped = false;
    for task in tasks {
        let next = {
            let proj = e.proj();
            proj.tasks
                .get(&task)
                .filter(|t| {
                    t.approved
                        && t.facts.run.is_none()
                        && t.waiting.is_none()
                        && t.view.state == TaskState::Planned
                })
                .filter(|t| !proj.needs_orchestrator(t, &e.pool))
                .and_then(|t| {
                    let g = proj
                        .goals
                        .get(&goal)
                        .filter(|g| g.state == GoalState::Approved)?;
                    let p = proj.projects.get(&g.project).filter(|p| !p.view.paused)?;
                    Some((
                        p.view.id.clone(),
                        g.version,
                        dvb_core::run_verbs::TaskStartBody {
                            task: task.clone(),
                            role: p.role(&t.view.role)?.slug.clone(),
                            model: None,
                            effort: None,
                            reason: "Continue the approved plan".into(),
                            routine: false,
                        },
                    ))
                })
        };
        let Some((project, version, body)) = next else {
            continue;
        };
        if !slot_free(&e) {
            retain_approved(&e, &project, &goal, version, &body)?;
            capped = true;
            continue;
        }
        match crate::dispatch::start_as_asked(
            &e,
            &project,
            Actor::Engine,
            body.clone(),
            None,
            Some(version),
        )
        .await
        {
            Err(Fail::Refused(r))
                if r.code
                    == (RefusalCode::CapReached {
                        which: dvb_core::Cap::ConcurrentRuns,
                    }) =>
            {
                retain_approved(&e, &project, &goal, version, &body)?;
                capped = true;
            }
            Ok(_) | Err(Fail::Refused(_)) => {}
            Err(err @ (Fail::NotFound(_) | Fail::Internal(_))) => {
                retain_approved(&e, &project, &goal, version, &body)?;
                eprintln!("dvb engine: start an approved task: {err}; again in {WAITING_AGAIN:?}");
                start_waiting_again(&e);
            }
        }
    }
    if capped {
        // A run may have ended between the cap check/refusal and retaining
        // the request. Revisit now so that release cannot be lost.
        start_waiting(e.clone()).await?;
    }
    // A mode/hire change during preparation can make the atomic start check
    // return the task to orchestration after the initial waiting pass.
    let project = e.proj().goals.get(&goal).map(|g| g.project.clone());
    if let Some(project) = project {
        missed_wake(&e, &project, false);
    }
    Ok(())
}

/// A run end or a start-up revisits every approved goal, the ended run's
/// first, so a plan approved while a run for another goal was live, or
/// before a restart, does not stay Planned.
pub(crate) async fn start_approved_goals(
    e: AppState,
    first: Option<dvb_core::GoalId>,
) -> Result<(), Fail> {
    let mut goals: Vec<_> = e
        .proj()
        .goals
        .values()
        .filter(|g| g.state == GoalState::Approved)
        .map(|g| g.id.clone())
        .collect();
    if goals.is_empty() {
        return start_waiting(e).await;
    }
    goals.sort_by_key(|g| Some(g) != first.as_ref());
    for goal in goals {
        start_approved(e.clone(), goal).await?;
    }
    Ok(())
}

/// Retain an automatic request atomically with its eligibility check. An
/// explicit request or start racing preparation wins without being replaced.
fn retain_approved(
    e: &AppState,
    project: &dvb_core::ProjectId,
    goal: &dvb_core::GoalId,
    version: EventId,
    body: &dvb_core::run_verbs::TaskStartBody,
) -> Result<(), Fail> {
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        let eligible = proj
            .goals
            .get(goal)
            .is_some_and(|g| g.version == version && g.state == GoalState::Approved)
            && proj.tasks.get(&body.task).is_some_and(|t| {
                &t.view.goal == goal
                    && t.approved
                    && t.facts.run.is_none()
                    && t.waiting.is_none()
                    && t.view.state == TaskState::Planned
            });
        Ok(if eligible {
            vec![EventKind::TaskWaiting {
                task: body.task.clone(),
                model: body.model.clone(),
                effort: body.effort,
                reason: body.reason.clone(),
                routine: body.routine,
            }]
        } else {
            Vec::new()
        })
    })?;
    Ok(())
}

/// One more `start_waiting` pass in `WAITING_AGAIN`, unless one is due already.
fn start_waiting_again(e: &AppState) {
    use std::sync::atomic::Ordering;
    if e.waiting_again.swap(true, Ordering::SeqCst) {
        return;
    }
    let e = e.clone();
    spawn("start waiting tasks again", async move {
        tokio::time::sleep(WAITING_AGAIN).await;
        e.waiting_again.store(false, Ordering::SeqCst);
        Box::pin(start_waiting(e)).await
    });
}

/// A run's unit ended: `run.ended` and, for the task's current run, the end
/// decided in the same transaction (`Engine::end_run`). A slot is free, so
/// every waiting task is kicked.
pub async fn run_ended(e: &AppState, run: &RunId, reason: EndReason) -> Result<(), Fail> {
    e.remember();
    reconcile::recover_collection(e, run, &reason).await?;
    let driver = launch::driver_reason(&e.cfg.paths.work.join(run.as_str()));
    let completed = reason == EndReason::Completed;
    e.end_run(run, reason, driver)?;
    // The unit is gone and its work collected: the build output is not needed,
    // nor the whole directory of an orchestrator or of a task done or dropped
    // meanwhile.
    let dir = e.cfg.paths.work.join(run.as_str());
    let finished = finished_with(&e.proj(), run);
    drop(tokio::task::spawn_blocking(move || {
        if finished {
            remove_dir(&dir, "run directory");
        } else {
            dispatch::drop_build_output(&dir);
        }
    }));
    // Recover wakes deferred by this run through the
    // same rule as a restart. A run that failed is not retried here.
    let orchestrator = e
        .proj()
        .runs
        .get(run)
        .filter(|r| r.assignment.is_orchestrator())
        .map(|r| (r.project.clone(), r.ended.clone(), r.wake));
    if let Some((project, ended, reason)) = orchestrator {
        if completed && ended != Some(EndReason::TimeLimit) {
            missed_wake(e, &project, false);
        } else if !matches!(ended, Some(EndReason::Stopped)) {
            orchestrator_failed(e, &project, reason);
        }
    }
    let now = e.clock.now();
    let tasks = {
        let proj = e.proj();
        let own = proj.runs.get(run).and_then(|r| r.assignment.task.as_ref());
        proj.tasks
            .values()
            .filter(|task| {
                own == Some(&task.view.id)
                    || match &task.view.state {
                        TaskState::InReview => !proj.runs.values().any(|run| {
                            run.assignment.task.as_ref() == Some(&task.view.id)
                                && run.assignment.reviews.is_some()
                                && run.assignment.reviews == task.view.head
                                && run.ended.is_none()
                        }),
                        // A publication can be waiting to send a scan hit
                        // back to its builder when a run slot becomes free.
                        TaskState::Publishing => true,
                        TaskState::AwaitingChecks => task.checks_waiting_for_slot,
                        TaskState::RetryPending {
                            effect: Effect::StartUnit,
                            ..
                        } => task.retry_at.is_none_or(|at| at <= now),
                        TaskState::Planned
                        | TaskState::Preparing
                        | TaskState::Running
                        | TaskState::Collecting
                        | TaskState::Merging
                        | TaskState::RetryPending { .. }
                        | TaskState::Stopped { .. }
                        | TaskState::Unknown { .. }
                        | TaskState::Done
                        | TaskState::Cancelled
                        | TaskState::Failed { .. } => false,
                    }
            })
            .map(|task| task.view.id.clone())
            .collect::<Vec<_>>()
    };
    for task in tasks {
        kick(e, &task);
    }
    Ok(())
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod resource_tests;

/// The run's build output directory, inside its run directory and outside `ws/`.
pub(crate) const BUILD_DIR: &str = "build";

/// Removes a finished run's build output, `<run dir>/build`; the workspace is
/// never touched (`ws/target` may hold the project's own files). Best
/// effort: a failure is logged only. The engine can delete the worker's files
/// because run directories are setgid `dvb-worker`, the engine user is in that
/// group and workers run with `UMask=0002` and leave their directories
/// group-writable when they end, as for moving a workspace on resume.
pub(crate) fn drop_build_output(dir: &std::path::Path) {
    remove_dir(&dir.join(BUILD_DIR), "build output");
}

fn remove_dir(dir: &std::path::Path, what: &str) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("dvb engine: {what} {}: {err}", dir.display()),
    }
}

/// Whether nothing reads the run's directory again: the run ended, and it has
/// no task (an orchestrator) or its task is done or dropped. A task that can
/// still run may resume in its runs' workspace (`preserved_workspace`).
fn finished_with(proj: &Projections, run: &RunId) -> bool {
    let Some(r) = proj.runs.get(run).filter(|r| r.ended.is_some()) else {
        return false;
    };
    r.assignment.task.as_ref().is_none_or(|task| {
        proj.tasks.get(task).is_some_and(|t| match t.view.state {
            TaskState::Done | TaskState::Cancelled => true,
            TaskState::Planned
            | TaskState::Preparing
            | TaskState::Running
            | TaskState::Collecting
            | TaskState::Publishing
            | TaskState::InReview
            | TaskState::AwaitingChecks
            | TaskState::Merging
            | TaskState::RetryPending { .. }
            | TaskState::Stopped { .. }
            | TaskState::Unknown { .. }
            | TaskState::Failed { .. } => false,
        })
    })
}

/// Removes the directories of a task's ended runs, in the background, once
/// it is done or dropped: on a change of its state, or of its goal's when the
/// goal is dropped (a failed task is then cancelled in the projection only).
/// Called with the events just applied.
pub(crate) fn drop_finished_runs(proj: &Projections, work: &std::path::Path, events: &[Event]) {
    let mut tasks = std::collections::BTreeSet::new();
    for ev in events {
        if let EventKind::TaskStateChanged { task, .. } = &ev.kind {
            tasks.insert(task.clone());
        } else if let EventKind::GoalDropped { goal } = &ev.kind {
            tasks.extend(
                proj.tasks
                    .values()
                    .filter(|t| &t.view.goal == goal)
                    .map(|t| t.view.id.clone()),
            );
        }
    }
    if tasks.is_empty() {
        return;
    }
    let finished: Vec<RunId> = proj
        .runs
        .values()
        .filter(|r| {
            r.assignment
                .task
                .as_ref()
                .is_some_and(|t| tasks.contains(t))
        })
        .map(|r| r.assignment.run.clone())
        .filter(|run| finished_with(proj, run))
        .collect();
    if finished.is_empty() {
        return;
    }
    let work = work.to_owned();
    std::thread::spawn(move || {
        for run in finished {
            remove_dir(&work.join(run.as_str()), "run directory");
        }
    });
}

/// At engine start: removes the directory of every run nothing reads again and
/// of every ended login, and drops the build output of every other ended run.
/// Covers an engine that died between `run.ended` and the cleanup, and ends
/// that skip it.
pub(crate) fn sweep_run_dirs(e: &AppState) {
    let (finished, ended, runs): (Vec<RunId>, Vec<RunId>, std::collections::BTreeSet<RunId>) = {
        let proj = e.proj();
        let (finished, ended) = proj
            .runs
            .values()
            .filter(|r| r.ended.is_some())
            .map(|r| r.assignment.run.clone())
            .partition(|run| finished_with(&proj, run));
        (finished, ended, proj.runs.keys().cloned().collect())
    };
    let work = e.cfg.paths.work.clone();
    std::thread::spawn(move || {
        for run in finished {
            remove_dir(&work.join(run.as_str()), "run directory");
        }
        for run in ended {
            drop_build_output(&work.join(run.as_str()));
        }
        for dir in ended_logins(&work, &runs) {
            remove_dir(&dir, "login run directory");
        }
    });
}

/// The directories of login runs (`api::logins`) that wrote their result: no
/// run is recorded for them, and once the result is there nothing reads them.
fn ended_logins(
    work: &std::path::Path,
    runs: &std::collections::BTreeSet<RunId>,
) -> Vec<std::path::PathBuf> {
    let entries = match std::fs::read_dir(work) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("dvb engine: run directories {}: {err}", work.display());
            return Vec::new();
        }
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| {
            let run = RunId::new(entry.file_name().to_string_lossy());
            run.is_ulid() && !runs.contains(&run)
        })
        .map(|entry| entry.path())
        .filter(|dir| {
            launch::read_run_file(&dir.join("env")).is_ok_and(|env| env.contains("DVB_LOGIN=1\n"))
                && launch::read_run_file(&dir.join("login-result")).is_ok()
        })
        .collect()
}
