//! Orchestrator wake selection, admission, retries and planning holds.
use crate::engine::{AppState, Fail};
use crate::launch::{fake_script, launch};
use crate::proj::{DROP_GOAL, PLAN_AGAIN, Projections, QuestionKind};
use crate::spawn;
use dvb_core::event::EndReason;
use dvb_core::{Actor, Event, EventId, EventKind, ProjectId, RunAssignment, RunId, TaskState};
use std::future::Future;

const TASK_RECOVERY_PREFIX: &str = "Task recovery: ";

/// Why the orchestrator is woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Plan the stated goal and ask the owner.
    Goal,
    /// Start approved tasks or recover a task problem within the approved goal.
    Approved,
    /// The owner answered a question: act on the reply in the prompt.
    Answer,
    /// Every task is done: recommend the next goal.
    GoalDone,
}

impl Wake {
    fn script(self) -> &'static str {
        match self {
            Wake::Goal => include_str!("fake_scripts/orchestrator-goal.json"),
            Wake::Approved => include_str!("fake_scripts/orchestrator-approved.json"),
            Wake::Answer => include_str!("fake_scripts/orchestrator-answer.json"),
            Wake::GoalDone => include_str!("fake_scripts/orchestrator-done.json"),
        }
    }

    /// The name a dev-mode test's own script has (`fake_script`).
    fn name(self) -> &'static str {
        match self {
            Wake::Goal => "orchestrator-goal",
            Wake::Approved => "orchestrator-approved",
            Wake::Answer => "orchestrator-answer",
            Wake::GoalDone => "orchestrator-done",
        }
    }

    /// The wake recorded in a run's reason.
    pub(crate) fn from_reason(reason: &str) -> Option<Wake> {
        if reason.starts_with(TASK_RECOVERY_PREFIX) {
            return Some(Wake::Approved);
        }
        [Wake::Goal, Wake::Approved, Wake::Answer, Wake::GoalDone]
            .into_iter()
            .find(|w| w.reason() == reason)
    }

    fn reason(self) -> &'static str {
        match self {
            Wake::Goal => "A goal was stated.",
            Wake::Approved => "The owner approved the plan.",
            Wake::Answer => "The owner answered a question.",
            Wake::GoalDone => "The goal is done.",
        }
    }
}

/// Runs that ended without finishing. Their wake is retried, so what they were
/// woken for (an owner's answer) counts as pending.
pub(crate) fn failed_runs(events: &[Event]) -> std::collections::HashSet<dvb_core::RunId> {
    events
        .iter()
        .filter_map(|ev| {
            if let EventKind::RunEnded { run, reason, .. } = &ev.kind
                && !matches!(reason, EndReason::Completed | EndReason::Stopped)
            {
                Some(run.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Answers after the latest orchestrator start belong to its successor's
/// prompt. The log keeps this wake recoverable across pauses and restarts.
pub(crate) fn pending_answer(proj: &Projections, project: &ProjectId) -> Option<Wake> {
    let p = proj.projects.get(project)?;
    let since = p.orchestrator_start;
    let pending = p.answers.iter().any(|(id, at)| {
        if *at <= since {
            return false;
        }
        let Some(q) = proj.questions.get(id) else {
            return true;
        };
        if matches!(
            q.kind,
            crate::proj::QuestionKind::PlanningHold { .. }
                | crate::proj::QuestionKind::Completion { .. }
        ) || q.main_ci.is_some()
            || q.no_effect()
            || q.parks_plan()
        {
            return false;
        }
        if let dvb_core::event::LapsesOn::GoalVersion(v) = q.lapses_on
            && proj
                .goal_versions
                .get(&v)
                .is_some_and(|id| proj.dropped_goals.contains(id))
        {
            return false;
        }
        !proj.goals.values().any(|g| {
            g.planning_held
                && !((q.orchestrator || q.plan_question) && proj.answer_releases_hold(g))
                && q.lapses_on == dvb_core::event::LapsesOn::GoalVersion(g.version)
        })
    });
    pending.then(|| {
        if p.approvals
            .iter()
            .any(|(goal, at)| *at > since && !proj.dropped_goals.contains(goal))
        {
            Wake::Approved
        } else {
            Wake::Answer
        }
    })
}

/// The wait before orchestrator retry `failed` (1-based).
fn orchestrator_backoff(failed: u8) -> i64 {
    dvb_core::state::backoff_millis(failed, 2_000, 300_000)
}

/// The newest completion (or proposal request), when no admitted, goal-bound
/// orchestrator prompt has included it yet and no other goal of the project is
/// stated or in progress: that goal's own runs carry on, so nothing asks what
/// is next. Recovery retries failed deliveries after backoff;
/// explicit retry admission has already waited, and passes no recovery time.
pub(crate) fn pending_goal_done(
    proj: &Projections,
    project: &ProjectId,
    recovery_time: Option<dvb_core::Timestamp>,
) -> Option<dvb_core::GoalId> {
    let retry_failed = recovery_time.is_none_or(|now| {
        let (failed, latest) = proj
            .projects
            .get(project)
            .map_or((0, None), |p| p.orchestrator_failures);
        failed < dvb_core::caps::ATTEMPTS_PER_TASK
            && latest.is_some_and(|at| now >= at.plus_millis(orchestrator_backoff(failed)))
    });
    let mut delivered = std::collections::BTreeMap::new();
    for run in proj.runs.values().filter(|r| {
        &r.project == project
            && r.assignment.is_orchestrator()
            && (!retry_failed || !proj.failed_runs.contains(&r.assignment.run))
    }) {
        if let Some(version) = run.delivered_goal_version {
            let start = delivered.entry(version).or_insert(EventId(0));
            *start = (*start).max(run.started);
        }
    }
    if proj.goal_under_way(project).is_some() {
        return None;
    }
    proj.goals_in_order(project)
        .into_iter()
        .filter(|g| g.state == dvb_core::api::GoalState::Done)
        .filter_map(|g| g.finished.map(|finished| (g, finished)))
        .max_by_key(|(_, finished)| *finished)
        .filter(|(g, finished)| {
            delivered
                .get(&g.version)
                .is_none_or(|start| start < finished)
        })
        .map(|(g, _)| g.id.clone())
}

/// Whether the next goal is being proposed: a completion or proposal request
/// waits for its run, or such a run is live or its question is open.
pub(crate) fn proposing(proj: &Projections, project: &ProjectId) -> bool {
    let done = |version: EventId| {
        proj.goals.values().any(|g| {
            &g.project == project
                && g.version == version
                && g.state == dvb_core::api::GoalState::Done
        })
    };
    pending_goal_done(proj, project, None).is_some()
        || proj.live_runs().any(|r| {
            &r.project == project
                && r.assignment.is_orchestrator()
                && r.goal_version.is_some_and(done)
        })
        || proj.questions.values().any(|q| {
            q.view.open
                && q.orchestrator
                && matches!(q.lapses_on, dvb_core::event::LapsesOn::GoalVersion(v) if done(v))
        })
}

/// When the run that asked the project's last what-next question started,
/// while the project waits for new work: the owner answered that question
/// without stating a goal, and nothing is stated, under way, being proposed or
/// answered since. The open issues in that run's prompt were read after it
/// started.
fn waiting_for_issue(proj: &Projections, project: &ProjectId) -> Option<dvb_core::Timestamp> {
    if proj.projects.get(project)?.view.paused
        || proj.goal_under_way(project).is_some()
        || proposing(proj, project)
        || pending_answer(proj, project).is_some()
        || proj
            .live_runs()
            .any(|r| &r.project == project && r.assignment.is_orchestrator())
    {
        return None;
    }
    let goals = proj.goals_in_order(project);
    let last = proj
        .questions
        .values()
        .filter(|q| {
            q.orchestrator
                && matches!(q.lapses_on, dvb_core::event::LapsesOn::GoalVersion(v)
                if goals.iter().any(|g| {
                    g.version == v && g.state == dvb_core::api::GoalState::Done
                }))
        })
        .max_by_key(|q| q.opened_at)?;
    // A goal or proposal request since then (its own what-next question
    // comes later) ends the wait for this question.
    let asker = last.asked_by.as_ref().and_then(|run| proj.runs.get(run))?;
    (last.answered_at.is_some() && goals.iter().all(|g| g.stated < last.opened_at))
        .then_some(asker.started_at)
}

/// Wake a project waiting for new work (`waiting_for_issue`) once when a
/// trusted issue was opened after the run that asked its last what-next
/// question started: the orchestrator proposes it with the usual what-next
/// question. The request is logged after that question, so the same issue
/// never wakes again.
pub async fn on_new_issue(e: &AppState, project: &ProjectId) -> Result<(), Fail> {
    let Some(since) = waiting_for_issue(&e.proj(), project) else {
        return Ok(());
    };
    let repo = crate::issues::repo_of(e, project)?;
    if !crate::issues::trusted_opened_after(e, &repo, since).await? {
        return Ok(());
    }
    // The issues were read without the lock: the owner may have acted.
    request_proposal(e, project, Actor::Engine, |proj| {
        Ok(waiting_for_issue(proj, project) == Some(since))
    })
}

/// Log a proposal request when `allowed` holds under the log's lock, then
/// wake the orchestrator to propose the next goal.
pub(crate) fn request_proposal(
    e: &AppState,
    project: &ProjectId,
    actor: Actor,
    allowed: impl FnOnce(&Projections) -> Result<bool, Fail>,
) -> Result<(), Fail> {
    let appended = e.append_checked(Some(project), actor, |proj, _| {
        Ok(if allowed(proj)? {
            vec![EventKind::ProposalRequested {}]
        } else {
            vec![]
        })
    })?;
    if !appended.is_empty() {
        spawn(
            "wake the orchestrator",
            wake(e.clone(), project.clone(), Wake::GoalDone, 0, 0),
        );
    }
    Ok(())
}

/// Select input without consuming it; admission repeats this under its guard.
fn wake_goal<'a>(
    proj: &'a Projections,
    project: &ProjectId,
    wake: &mut Wake,
    pool: &dvb_core::ModelPool,
) -> Option<(&'a crate::proj::Goal, String)> {
    let pending = pending_answer(proj, project);
    let answering = pending.is_some();
    if let Some(pending) = pending {
        *wake = pending;
    } else if *wake == Wake::Answer {
        return None;
    }
    if *wake == Wake::GoalDone
        && proj
            .goals_in_order(project)
            .iter()
            .any(|g| proj.plan_wake_ready(g))
    {
        *wake = Wake::Goal;
    }
    let completed = if *wake == Wake::GoalDone {
        Some(pending_goal_done(proj, project, None)?)
    } else {
        None
    };
    // Approval replies remain bound to the answered goal, even after its
    // tasks start. Every goal-bound wake stays on its own goal; a pending
    // task recovery takes only an approval wake, so it cannot swallow input
    // for another goal. Recovery approvals retain unanswered-work eligibility.
    let goal_wake = if answering { Wake::Answer } else { *wake };
    let mut recovery_reason = None;
    let goal = if *wake == Wake::GoalDone {
        completed.as_ref().and_then(|id| proj.goals.get(id))
    } else if goal_wake == Wake::Goal {
        proj.goals_in_order(project)
            .into_iter()
            .find(|g| proj.plan_wake_ready(g))
    } else if goal_wake == Wake::Approved {
        let recovery = proj.pending_task_recovery(project);
        recovery_reason = recovery.and_then(|g| proj.task_recovery_reason(g));
        recovery.or_else(|| proj.goal_for_wake(project, Some(goal_wake), pool))
    } else {
        proj.goal_for_wake(project, Some(goal_wake), pool)
    }?;
    // All wake types respect holds; a newer orchestrator or saved-plan reply
    // can consume the owner release for an attempt already in flight.
    if proj.waits_for(goal).is_some() {
        return None;
    }
    let owner_release = answering && proj.answer_releases_hold(goal);
    if (proj.orchestrator_question_open(goal) || goal.planning_held) && !owner_release {
        return None;
    }
    let reason = recovery_reason.map_or_else(
        || wake.reason().to_owned(),
        |reason| format!("{TASK_RECOVERY_PREFIX}{reason}"),
    );
    Some((goal, reason))
}

/// Wake the orchestrator with its retry budgets (both zero for new input).
/// Boxed to break the launch, run-end and wake scheduling cycle.
pub fn wake(
    e: AppState,
    project: ProjectId,
    mut wake: Wake,
    failed: u8,
    rate_limit_retries: u8,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), Fail>> + Send>> {
    Box::pin(async move {
        let p = e
            .proj()
            .projects
            .get(&project)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("project {project}")))?;
        let role = p
            .role_by_slug(dvb_core::roles::ORCHESTRATOR)
            .cloned()
            .ok_or_else(|| Fail::internal("the project has no orchestrator"))?;
        let hire = p
            .hire(&role.id, None)
            .cloned()
            .ok_or_else(|| Fail::internal("the orchestrator has no hire"))?;
        let vendor = e
            .hire_vendor(&hire.model)
            .map_err(|error| e.refuse(Some(&project), Actor::Engine, "orchestrator wake", error))?;
        let selected = {
            let proj = e.proj();
            let mut selected_wake = wake;
            let Some((goal, _)) = wake_goal(&proj, &project, &mut selected_wake, &e.pool) else {
                return Ok(());
            };
            (goal.id.clone(), goal.version, goal.planning_run.clone())
        };
        let mirror = e.mirror(&p.view.name);
        let forge = e.forge().ok_or_else(|| Fail::internal("no forge"))?;
        let fetched = async {
            let _fetch = e.lock_mirror(&mirror).await;
            // A paused project starts no run; unpausing gives the wake back.
            // Git now yields: another queued wake can have started or been held
            // while this one waited for the mirror. Avoid a redundant fetch.
            let eligible = {
                let proj = e.proj();
                proj.projects.get(&project).is_some_and(|p| !p.view.paused)
                    && !proj
                        .live_runs()
                        .any(|r| r.project == project && r.assignment.is_orchestrator())
                    && !(failed > 0 && held_in(&proj, &project))
            };
            if !eligible {
                return Ok(None);
            }
            forge
                .fetch(
                    &dvb_core::forge::Repo(p.view.repo.clone()),
                    &mirror,
                    &p.view.default_branch,
                )
                .await
                .map(Some)
        }
        .await;
        let base =
            match fetched {
                Ok(Some(base)) => base,
                Ok(None) => return Ok(()),
                Err(err) => {
                    if let Some(limit) = err.rate_limit {
                        let now = e.clock.now();
                        let wait = limit
                            .not_before(
                                now,
                                orchestrator_backoff(
                                    failed.saturating_add(rate_limit_retries).saturating_add(1),
                                ),
                            )
                            .millis()
                            .saturating_sub(now.millis());
                        retry_orchestrator_after(
                            &e,
                            &project,
                            Some(wake),
                            failed,
                            rate_limit_retries.saturating_add(u8::from(limit.reset.is_none())),
                            wait,
                        );
                        return Ok(());
                    }
                    let failed = failed.saturating_add(1);
                    if err.retryable && failed < dvb_core::caps::ATTEMPTS_PER_TASK {
                        retry_orchestrator_after(
                            &e,
                            &project,
                            Some(wake),
                            failed,
                            rate_limit_retries,
                            orchestrator_backoff(failed),
                        );
                    } else {
                        let attempt = RunId::new(format!("fetch-{}", e.new_id()));
                        e.append_checked(Some(&project), Actor::Engine, |proj, _now| {
                            let Some(g) = proj.goals.get(&selected.0).filter(|g| {
                                g.version == selected.1 && g.planning_run == selected.2
                            }) else {
                                return Ok(vec![]);
                            };
                            let mut current_wake = wake;
                            if wake_goal(proj, &project, &mut current_wake, &e.pool)
                                .is_none_or(|(current, _)| current.id != g.id)
                                || proj.projects.get(&project).is_none_or(|p| p.view.paused)
                                || proj
                                    .live_runs()
                                    .any(|r| r.project == project && r.assignment.is_orchestrator())
                            {
                                return Ok(vec![]);
                            }
                            // No run started. Its unique fetch attempt uses the same
                            // durable planning question and owner actions as an empty run.
                            Ok(vec![planning_hold_question(
                                g,
                                &attempt,
                                &format!("planning could not start: {}", err.detail),
                                true,
                            )])
                        })?;
                    }
                    return Ok(());
                }
            };
        let mut a = RunAssignment {
            review_stage: None,
            goal_version: None,
            run: RunId::new(e.new_id()),
            task: None,
            role: role.id.clone(),
            hire: hire.id.clone(),
            model: hire.model.clone(),
            vendor,
            effort: hire.effort,
            reason: wake.reason().into(),
            reviews: None,
            base: Some(base),
            routine: false,
        };

        let mut vars = Vec::new();
        loop {
            crate::dispatch::wait_launch(&e, &project, &a).await?;
            e.update.admit().await;
            e.expire_only(None)?;
            // Check and record admission together: another wake may have started while
            // we fetched the base or waited. A recorded run also covers unit startup.
            let mut waiting = false;
            let admitted = e.append_checked(Some(&project), Actor::Engine, |proj, now| {
                let launch = crate::dispatch::decide_launch(&e, proj, &project, &a, now)?;
                let crate::dispatch::Launch::Ready(reroute) = launch else {
                    waiting = true;
                    return Ok(vec![]);
                };
                if (failed > 0 && held_in(proj, &project))
                    || proj.projects.get(&project).is_none_or(|p| p.view.paused)
                    || proj
                        .live_runs()
                        .any(|r| r.project == project && r.assignment.is_orchestrator())
                {
                    return Ok(vec![]);
                }
                if u64::try_from(proj.live_runs().count()).unwrap_or(u64::MAX) >= proj.max_runs() {
                    return Ok(vec![]);
                }
                let Some((goal, reason)) = wake_goal(proj, &project, &mut wake, &e.pool) else {
                    return Ok(vec![]);
                };
                a.reason = reason;
                // Persist the binding and consume its release with the start.
                a.goal_version = Some(goal.version);
                vars.push(("goal_id", goal.id.to_string()));
                vars.push(("goal_version", goal.version.to_string()));
                vars.push(("goal_state", format!("{:?}", goal.state)));
                vars.push((
                    "other_waiting_goals",
                    proj.goals_in_order(&project)
                        .into_iter()
                        .filter(|g| g.id != goal.id && Projections::needs_plan(g))
                        .map(|g| g.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
                vars.push(("goal_text", goal.text.clone()));
                if wake == Wake::Approved || wake == Wake::Answer {
                    // The first planned task of the bound goal, and who builds it.
                    let task = goal
                        .tasks
                        .iter()
                        .filter_map(|t| proj.tasks.get(t))
                        .find(|t| t.view.state == TaskState::Planned);
                    if let Some(t) = task {
                        let role = p.role(&t.view.role);
                        vars.push(("task", t.view.id.to_string()));
                        vars.push((
                            "task_role",
                            role.map(|r| r.slug.clone()).unwrap_or_default(),
                        ));
                    }
                }
                let mut kinds = vec![EventKind::RunStarted(a.clone())];
                kinds.extend(crate::dispatch::launch_events(proj, &a, reroute));
                Ok(kinds)
            })?;
            if waiting {
                continue;
            }
            if admitted.is_empty() {
                return Ok(());
            }
            if let Err(err) = launch(
                e.clone(),
                a.run.clone(),
                fake_script(&e, wake.name(), wake.script()),
                vars,
                None,
            )
            .await
            {
                eprintln!("dvb engine: run {}: {err}", a.run);
                crate::dispatch::run_ended(&e, &a.run, EndReason::Crashed).await?;
            }
            return Ok(());
        }
    })
}
/// Retry failed orchestrator runs with backoff up to the attempt cap, then
/// hold automatic wakes until the owner answers the failure question.
pub(crate) fn orchestrator_failed(e: &AppState, project: &ProjectId, wake: Option<Wake>) {
    let (failed, _) = e
        .proj()
        .projects
        .get(project)
        .map_or((0, None), |p| p.orchestrator_failures);
    if failed < dvb_core::caps::ATTEMPTS_PER_TASK {
        retry_orchestrator(e, project, wake, failed);
    } else if failed == dvb_core::caps::ATTEMPTS_PER_TASK
        && let Err(err) = ask_orchestrator_failed(e, project, failed)
    {
        eprintln!("dvb engine: ask about the failed orchestrator: {err}");
    }
}

/// The shared delayed wake for failed launches and pre-launch fetches.
pub(crate) fn retry_orchestrator(
    e: &AppState,
    project: &ProjectId,
    wake: Option<Wake>,
    failed: u8,
) {
    retry_orchestrator_after(e, project, wake, failed, 0, orchestrator_backoff(failed));
}

pub(crate) fn retry_orchestrator_after(
    e: &AppState,
    project: &ProjectId,
    wake: Option<Wake>,
    failed: u8,
    rate_limit_retries: u8,
    wait: i64,
) {
    let (e, project) = (e.clone(), project.clone());
    spawn("retry the orchestrator", async move {
        tokio::time::sleep(std::time::Duration::from_millis(wait.unsigned_abs())).await;
        if held(&e, &project) {
            return Ok(());
        }
        // What failed, with any newer input first; keep the attempt budget
        // when recovery selects the wake instead of the original fallback.
        match missed(&e, &project, true) {
            Some(Missed::Wake(reason)) => {
                crate::wake::wake(e, project, reason, failed, rate_limit_retries).await?
            }
            Some(Missed::Hold(_) | Missed::Ask(_)) => missed_wake(&e, &project, true),
            None => {
                if let Some(wake) = wake {
                    crate::wake::wake(e, project, wake, failed, rate_limit_retries).await?;
                }
            }
        }
        Ok(())
    });
}

/// An open orchestrator-failure question holds every automatic wake (a retry, a
/// restart, an unpause) until it is answered or a new goal is stated.
fn held(e: &AppState, project: &ProjectId) -> bool {
    held_in(&e.proj(), project)
}

pub(crate) fn held_in(proj: &Projections, project: &ProjectId) -> bool {
    proj.questions.values().any(|q| {
        &q.view.project == project && q.view.open && q.kind == QuestionKind::OrchestratorFailed
    })
}

/// The owner is asked what to do about an orchestrator that keeps failing;
/// answering wakes it again, and until then nothing runs on its own.
fn ask_orchestrator_failed(e: &AppState, project: &ProjectId, attempts: u8) -> Result<(), Fail> {
    let format = e.proj().settings.question_format();
    let question = dvb_core::QuestionId::new(e.new_id());
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        let Some(g) = proj.goal_for_wake(project, None, &e.pool) else {
            return Ok(vec![]);
        };
        let option = dvb_core::event::QuestionOption::owner;
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(QuestionKind::OrchestratorFailed),
            main_ci: None,
            task: None,
            question: question.clone(),
            subject: dvb_core::wording::render(
                "question.orchestrator-failed-question",
                &serde_json::json!({"attempts": attempts.to_string(), "goal": g.text.to_string()}),
            ),
            format,
            options: vec![option(
                1,
                dvb_core::wording::text("question.orchestrator-retry"),
                dvb_core::wording::text("question.orchestrator-retry-effect"),
            )],
            recommended: 1,
            lapses_on: dvb_core::event::LapsesOn::GoalVersion(g.version),
        }])
    })?;
    Ok(())
}

/// Gives the orchestrator a wake it missed (a pause, a restart), or asks the
/// owner about a plan the orchestrator saved but did not get to ask about.
pub fn wake_if_missed(e: &AppState, project: &ProjectId) {
    missed_wake(e, project, true);
}

/// `restart`: also resume approved Custom tasks the orchestrator never started.
/// Dropped wakes, unchosen Auto tasks and missing hires recover after run ends
/// too, including deferred tasks another goal's slot release can unblock.
pub(crate) fn missed_wake(e: &AppState, project: &ProjectId, restart: bool) {
    // A restored question starts no run, so the next outstanding goal is
    // looked at too, until a run is scheduled or nothing remains.
    while let Some(missed) = missed(e, project, restart) {
        match missed {
            Missed::Wake(wake) => {
                spawn(
                    "wake the orchestrator",
                    crate::wake::wake(e.clone(), project.clone(), wake, 0, 0),
                );
                return;
            }
            Missed::Hold(goal) => {
                if let Err(err) = ask_empty_plan(e, project, &goal) {
                    eprintln!("dvb engine: ask about empty planning: {err}");
                    return;
                }
            }
            Missed::Ask(goal) => {
                if let Err(err) = ask_saved_plan(e, project, &goal) {
                    eprintln!("dvb engine: ask about the saved plan: {err}");
                    return;
                }
            }
        }
    }
}

/// What a pause or a restart cut short.
enum Missed {
    Wake(Wake),
    /// A saved plan with no owner question.
    Ask(dvb_core::GoalId),
    Hold(dvb_core::GoalId),
}

fn missed(e: &AppState, project: &ProjectId, restart: bool) -> Option<Missed> {
    if held(e, project) {
        return None;
    }
    let proj = e.proj();
    // Owner visibility is independent of answer/run admission. A reply
    // arriving during the empty attempt must not hide its hold question,
    // nor may work on another goal keep that question from being recovered.
    if let Some(g) = proj.goals_in_order(project).into_iter().find(|g| {
        g.planning_held
            && Projections::planning_question(g).is_some_and(|id| !proj.questions.contains_key(&id))
    }) {
        return Some(Missed::Hold(g.id.clone()));
    }
    if proj.projects.get(project).is_none_or(|p| p.view.paused)
        || proj
            .live_runs()
            .any(|r| &r.project == project && r.assignment.is_orchestrator())
    {
        return None;
    }
    // An answer goes first, to its own goal: any orchestrator start consumes
    // the project's pending replies. A recovery counts only on a goal that
    // can take a wake now, so it hides no other goal's work.
    if pending_answer(&proj, project).is_some() {
        return Some(Missed::Wake(Wake::Answer));
    }
    if proj.pending_task_recovery(project).is_some() {
        return Some(Missed::Wake(Wake::Approved));
    }
    // Goals in the order stated: the first with something outstanding.
    proj.goals_in_order(project)
        .into_iter()
        .filter(|g| proj.waits_for(g).is_none())
        .find_map(|g| {
            if Projections::needs_plan(g) {
                if proj.plan_wake_ready(g) {
                    Some(Missed::Wake(Wake::Goal))
                } else {
                    None
                }
            } else if proj.plan_unapproved(g)
                && !proj.goal_question_open(g)
                && proj.plan_parked(g).is_none()
            {
                // A saved plan nobody asked about (a run cut short, or a
                // written reply that saved no new plan). "Not yet" parks it.
                Some(Missed::Ask(g.id.clone()))
            } else if proj.has_waiting_tasks(g, &e.pool)
                && (restart || proj.goal_needs_orchestrator(g, &e.pool))
                && !proj.goal_question_open(g)
            {
                Some(Missed::Wake(Wake::Approved))
            } else {
                None
            }
        })
        .or_else(|| {
            pending_goal_done(&proj, project, Some(e.clock.now()))
                .map(|_| Missed::Wake(Wake::GoalDone))
        })
}

/// One durable question for this empty attempt. Concurrent recovery uses the
/// same id, and answers validate its run/version under the append guard.
fn ask_empty_plan(e: &AppState, project: &ProjectId, goal: &dvb_core::GoalId) -> Result<(), Fail> {
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        let Some(g) = proj.goals.get(goal).filter(|g| g.planning_held) else {
            return Ok(vec![]);
        };
        let Some(question) = Projections::planning_question(g) else {
            return Ok(vec![]);
        };
        if proj.questions.contains_key(&question) {
            return Ok(vec![]);
        }
        let Some(run) = &g.planning_run else {
            return Ok(vec![]);
        };
        Ok(vec![planning_hold_question(
            g,
            run,
            dvb_core::wording::text("question.empty-plan"),
            false,
        )])
    })?;
    Ok(())
}

/// Planning hold actions for empty runs and failed fetch attempts.
pub(crate) fn planning_hold_question(
    g: &crate::proj::Goal,
    attempt: &RunId,
    detail: &str,
    fetch: bool,
) -> EventKind {
    EventKind::QuestionOpened {
        kind: Some(QuestionKind::PlanningHold {
            attempt: attempt.clone(),
            fetch,
        }),
        main_ci: None,
        task: None,
        question: dvb_core::QuestionId::new(format!("planning-{}-{attempt}", g.version)),
        subject: dvb_core::wording::render(
            "question.planning-question",
            &serde_json::json!({"goal": g.text.lines().next().unwrap_or("Goal").to_string(), "detail": detail.to_string()}),
        ),
        format: dvb_core::event::QuestionFormat::Simple,
        options: vec![
            dvb_core::event::QuestionOption::owner(
                PLAN_AGAIN,
                dvb_core::wording::text("question.plan-again"),
                dvb_core::wording::text("question.plan-again-effect"),
            ),
            dvb_core::event::QuestionOption::owner(
                DROP_GOAL,
                dvb_core::wording::text("question.drop-goal"),
                dvb_core::wording::text("question.drop-goal-effect"),
            ),
        ],
        recommended: PLAN_AGAIN,
        lapses_on: dvb_core::event::LapsesOn::GoalVersion(g.version),
    }
}

/// The question the orchestrator did not get to ask about its saved plan, asked
/// by the engine (no new plan, no model call): yes starts it, as the
/// orchestrator's own question would.
fn ask_saved_plan(e: &AppState, project: &ProjectId, goal: &dvb_core::GoalId) -> Result<(), Fail> {
    let format = e.proj().settings.question_format();
    let question = dvb_core::QuestionId::new(e.new_id());
    e.append_checked(Some(project), Actor::Engine, |proj, _now| {
        let g = proj
            .goals
            .get(goal)
            .ok_or_else(|| Fail::NotFound(format!("goal {goal}")))?;
        let lapses_on = dvb_core::event::LapsesOn::GoalVersion(g.version);
        // Asked meanwhile, or the goal moved on: nothing to do.
        if proj.goal_question_open(g) || !proj.plan_unapproved(g) || proj.plan_parked(g).is_some() {
            return Ok(vec![]);
        }
        let n = g
            .tasks
            .iter()
            .filter(|t| {
                proj.tasks
                    .get(*t)
                    .is_some_and(|t| t.view.state == TaskState::Planned)
            })
            .count();
        let option =
            |id, text: &str, effect: String, approves_plan| dvb_core::event::QuestionOption {
                approves_plan,
                ..dvb_core::event::QuestionOption::owner(id, text, effect)
            };
        Ok(vec![EventKind::QuestionOpened {
            kind: Some(dvb_core::event::QuestionKind::Ordinary),
            main_ci: None,
            task: None,
            question: question.clone(),
            subject: format!(
                "{}{}",
                dvb_core::wording::render(
                    "question.saved-plan-question",
                    &serde_json::json!({"goal": g.text.to_string()}),
                ),
                proj.plan_wording(g)
            ),
            format,
            options: vec![
                option(
                    1,
                    dvb_core::wording::text("question.start-plan"),
                    dvb_core::wording::render(
                        "question.saved-plan-effect",
                        &serde_json::json!({"n": n.to_string()}),
                    ),
                    true,
                ),
                option(
                    2,
                    dvb_core::wording::text("question.wait-plan"),
                    dvb_core::wording::text("question.wait-plan-effect").into(),
                    false,
                ),
            ],
            recommended: 1,
            lapses_on,
        }])
    })?;
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn every_wake_script_parses_once_rendered() {
        let vars = [("goal_text", "a \"quoted\"\ngoal".to_owned())];
        for wake in [
            super::Wake::Goal,
            super::Wake::Approved,
            super::Wake::Answer,
            super::Wake::GoalDone,
        ] {
            assert_eq!(super::Wake::from_reason(wake.reason()), Some(wake));
            serde_json::from_str::<serde_json::Value>(&crate::prompt::render(
                wake.script(),
                &vars,
                true,
            ))
            .unwrap();
        }
    }

    #[test]
    fn the_what_next_subject_ends_with_the_goal_text_alone() {
        // A goal's text usually ends with its own period.
        let vars = [("goal_text", "Show the build version.".to_owned())];
        let script: serde_json::Value = serde_json::from_str(&crate::prompt::render(
            super::Wake::GoalDone.script(),
            &vars,
            true,
        ))
        .unwrap();
        assert_eq!(
            script[0]["run"][1]["subject"],
            "What next? Done: Show the build version."
        );
    }

    use super::*;

    #[test]
    fn a_capped_completion_retry_remains_pending_only_after_its_backoff() {
        let p = ProjectId::new("p");
        let goal = dvb_core::GoalId::new("g");
        let mut assignment = crate::test_support::assignment::Assignment::new(RunId::new("failed"))
            .goal_version(Some(EventId(2)))
            .role(dvb_core::RoleId::new("orchestrator"))
            .hire(dvb_core::HireId::new("orchestrator"))
            .effort(dvb_core::Effort::Medium)
            .reason(Wake::GoalDone.reason().into())
            .build();
        let mut kinds = vec![
            EventKind::ProjectAdded {
                id: p.clone(),
                name: "p".into(),
                repo: "repo".into(),
                default_branch: "main".into(),
                ci: dvb_core::event::CiMode::NoCi,
            },
            EventKind::GoalStated {
                goal: goal.clone(),
                text: "Finished".into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
            EventKind::GoalDone { goal: goal.clone() },
            EventKind::RunStarted(assignment.clone()),
            EventKind::RunEnded {
                run: assignment.run.clone(),
                reason: EndReason::Crashed,
                detail: None,
                stderr: None,
            },
        ];
        let events = |kinds: &[EventKind]| {
            kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| Event {
                    id: EventId(i as u64 + 1),
                    at: dvb_core::Timestamp(1_000),
                    actor: Actor::Engine,
                    project: Some(p.clone()),
                    kind: kind.clone(),
                    evidence: None,
                })
                .collect::<Vec<_>>()
        };
        let log = events(&kinds);
        let proj = Projections::rebuild(&log);
        let due = 1_000 + orchestrator_backoff(1);
        assert_eq!(
            pending_goal_done(&proj, &p, Some(dvb_core::Timestamp(due - 1))),
            None
        );
        // The scheduled retry reaches a full cap, so it records no new start.
        // That unchanged log remains eligible on later slot release/reopen.
        assert_eq!(pending_goal_done(&proj, &p, None), Some(goal.clone()));
        for now in [due, due + 1_000] {
            let rebuilt = Projections::rebuild(&log);
            assert_eq!(
                pending_goal_done(&rebuilt, &p, Some(dvb_core::Timestamp(now))),
                Some(goal.clone())
            );
        }
        assignment.run = RunId::new("retry");
        kinds.push(EventKind::RunStarted(assignment));
        let log = events(&kinds);
        assert_eq!(
            pending_goal_done(
                &Projections::rebuild(&log),
                &p,
                Some(dvb_core::Timestamp(due))
            ),
            None
        );
    }
}
