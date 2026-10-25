//! Mirror snapshots on startup and authoritative changes, with periodic repair. GitHub observations never enter projections.
use crate::{AppState, Fail};
use dvb_core::api::GoalState;
use dvb_core::forge::{BoardState, ForgeError, GoalMirror, Repo, TaskMirror, Visibility};
use dvb_core::{Actor, EventKind, GoalId, ProjectId, TaskId, TaskState, Timestamp};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum MirrorKey {
    Project(ProjectId),
    Goal(GoalId),
    Task(TaskId),
}

#[derive(Clone, PartialEq, Eq)]
enum MirrorInput {
    Project(Repo),
    Goal(Repo, GoalMirror),
    Task(Repo, TaskMirror),
}

struct Retry {
    input: MirrorInput,
    delay_ms: i64,
    next: Timestamp,
}

#[derive(PartialEq, Eq)]
struct FinishedGoal {
    input: MirrorInput,
    tasks: Vec<(TaskId, TaskMirror)>,
}

/// In-memory effect retries; startup still repairs every mirror once.
#[derive(Default)]
pub(crate) struct Retries {
    pending: BTreeMap<MirrorKey, Retry>,
    completed: BTreeMap<GoalId, FinishedGoal>,
}

impl Retries {
    fn due(&mut self, key: &MirrorKey, input: &MirrorInput, now: Timestamp) -> bool {
        if self
            .pending
            .get(key)
            .is_some_and(|retry| retry.input != *input)
        {
            self.pending.remove(key);
        }
        self.pending.get(key).is_none_or(|retry| now >= retry.next)
    }

    fn finished(
        &mut self,
        key: MirrorKey,
        input: MirrorInput,
        success: bool,
        now: Timestamp,
        rate_limit: Option<dvb_core::forge::RateLimit>,
    ) {
        if success {
            self.pending.remove(&key);
        } else {
            let delay_ms = self.pending.get(&key).map_or(60_000, |retry| {
                if rate_limit.is_some_and(|limit| limit.reset.is_some()) {
                    retry.delay_ms
                } else {
                    dvb_core::state::backoff_millis(2, retry.delay_ms, 3_600_000)
                }
            });
            self.pending.insert(
                key,
                Retry {
                    input,
                    delay_ms,
                    next: rate_limit.map_or_else(
                        || now.plus_millis(delay_ms),
                        |limit| limit.not_before(now, delay_ms),
                    ),
                },
            );
        }
    }
}

pub fn board_state(state: &TaskState) -> BoardState {
    match state {
        TaskState::Planned => BoardState::Planned,
        TaskState::Preparing
        | TaskState::Running
        | TaskState::Collecting
        | TaskState::Publishing => BoardState::Working,
        TaskState::InReview | TaskState::AwaitingChecks => BoardState::InReview,
        TaskState::Merging => BoardState::ReadyToMerge,
        TaskState::Done => BoardState::Done,
        TaskState::Stopped { .. } => BoardState::Stopped,
        TaskState::Cancelled => BoardState::Cancelled,
        TaskState::Failed { .. } => BoardState::Failed,
        TaskState::Unknown { .. } => BoardState::Unknown,
        TaskState::RetryPending { effect, .. } => match effect {
            dvb_core::state::Effect::StartUnit
            | dvb_core::state::Effect::StopUnit
            | dvb_core::state::Effect::Push
            | dvb_core::state::Effect::OpenPr => BoardState::Working,
            dvb_core::state::Effect::ReadChecks => BoardState::InReview,
            dvb_core::state::Effect::Merge
            | dvb_core::state::Effect::CloseIssue
            | dvb_core::state::Effect::DeleteBranch => BoardState::ReadyToMerge,
        },
    }
}

type Observations = BTreeMap<(ProjectId, GoalId, Option<TaskId>), (bool, Option<String>)>;

/// Each repository's visibility, read once per pass.
type Visibilities = BTreeMap<Repo, Result<Visibility, ForgeError>>;

fn retryable(error: &crate::InternalError) -> bool {
    matches!(error, crate::InternalError::Forge(error) if error.retryable)
}

fn observed(
    e: &AppState,
    observations: &mut Observations,
    project: &ProjectId,
    goal: &GoalId,
    task: Option<&TaskId>,
    result: Result<(), &crate::InternalError>,
) -> Result<(), Fail> {
    let outage = result.is_err_and(retryable);
    let (mirrored, detail) = match result {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error.to_string())),
    };
    let key = (project.clone(), goal.clone(), task.cloned());
    let observation = (mirrored, detail.clone());
    // One item records an outage for the whole project, and later its recovery.
    let seen = if outage {
        observations
            .iter()
            .any(|((other, ..), seen)| other == project && *seen == observation)
    } else {
        observations.get(&key) == Some(&observation)
    };
    if !seen {
        e.append_checked(Some(project), Actor::Engine, |_, _| {
            Ok(vec![EventKind::GithubMirrorObserved {
                goal: goal.clone(),
                task: task.cloned(),
                mirrored,
                detail,
            }])
        })?;
        observations.insert(key, observation);
    }
    Ok(())
}

fn unpaused(e: &AppState, project: &ProjectId) -> bool {
    e.proj()
        .projects
        .get(project)
        .is_some_and(|p| !p.view.paused)
}

#[cfg(any(test, feature = "test-support"))]
pub async fn sync(e: &AppState) -> Result<bool, Fail> {
    sync_projects(e, None).await
}

async fn sync_projects(e: &AppState, affected: Option<&BTreeSet<ProjectId>>) -> Result<bool, Fail> {
    let _effect = e.update.effect().await;
    let Some(forge) = e.forge() else {
        return Ok(true);
    };
    // Serialize snapshots so event wakes cannot duplicate an in-flight retry.
    let mut retries = e.mirror_retries.lock().await;
    let mut observations = Observations::new();
    for event in e.store.select(
        dvb_core::EventId(0),
        dvb_store::Select::Kinds(&["github.mirror_observed"]),
    )? {
        if let (
            Some(project),
            EventKind::GithubMirrorObserved {
                goal,
                task,
                mirrored,
                detail,
            },
        ) = (event.project, event.kind)
        {
            observations.insert((project, goal, task), (mirrored, detail));
        }
    }
    let mut healthy = project_jobs(e, forge.as_ref(), &mut retries, affected).await;
    let mut visibilities = Visibilities::new();
    for goal in snapshot(e, affected) {
        healthy &= goal_job(
            e,
            forge.as_ref(),
            &mut retries,
            (&mut observations, &mut visibilities),
            goal,
        )
        .await?;
    }
    drop(retries);
    // The same pass reads the issues of a project waiting for new work.
    let projects: Vec<ProjectId> = e
        .proj()
        .projects
        .keys()
        .filter(|p| affected.is_none_or(|ids| ids.contains(p)))
        .cloned()
        .collect();
    for project in projects {
        if let Err(error) = crate::wake::on_new_issue(e, &project).await {
            eprintln!("dvb engine: reading new issues of {project}: {error}");
        }
    }
    Ok(healthy)
}

/// The goal's text travels apart from its mirror, whose description holds
/// only the task lines until the repository's visibility is known.
type GoalSnapshot = (
    ProjectId,
    GoalId,
    Repo,
    String,
    GoalMirror,
    Vec<(TaskId, TaskMirror)>,
);

fn snapshot(e: &AppState, affected: Option<&BTreeSet<ProjectId>>) -> Vec<GoalSnapshot> {
    // Never hold a projection lock while contacting a forge.
    {
        let p = e.proj();
        p.goals
            .values()
            .filter(|g| !g.proposal && affected.is_none_or(|ids| ids.contains(&g.project)))
            .filter_map(|g| {
                let project = p.projects.get(&g.project)?;
                let tasks = g
                    .tasks
                    .iter()
                    .filter_map(|id| p.tasks.get(id))
                    .map(|t| {
                        let role = project
                            .role(&t.view.role)
                            .map_or_else(|| t.view.role.to_string(), |r| r.name.clone());
                        let model = t
                            .facts
                            .builder
                            .as_ref()
                            .and_then(|id| p.runs.get(id))
                            .map(|r| r.assignment.model.clone())
                            .or_else(|| project.hire(&t.view.role, None).map(|h| h.model.clone()))
                            .unwrap_or_else(|| "Not hired".into());
                        (
                            t.view.id.clone(),
                            TaskMirror {
                                key: t.view.id.to_string(),
                                title: t.view.title.clone(),
                                role,
                                model,
                                state: board_state(&t.view.state),
                                pr: t.view.pr,
                                milestone: 0,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let mut description = String::new();
                for (_, t) in &tasks {
                    description.push_str(&format!("\n- {}: {}", t.state.label(), t.title));
                }
                Some((
                    g.project.clone(),
                    g.id.clone(),
                    Repo(project.view.repo.clone()),
                    g.text.clone(),
                    GoalMirror {
                        key: g.id.to_string(),
                        title: g
                            .text
                            .lines()
                            .next()
                            .unwrap_or("Goal")
                            .chars()
                            .take(200)
                            .collect(),
                        description,
                        done: matches!(g.state, GoalState::Done | GoalState::Dropped),
                    },
                    tasks,
                ))
            })
            .collect::<Vec<_>>()
    }
}

async fn project_jobs(
    e: &AppState,
    forge: &dyn dvb_core::forge::Forge,
    retries: &mut Retries,
    affected: Option<&BTreeSet<ProjectId>>,
) -> bool {
    let mut healthy = true;
    let repos = e
        .proj()
        .projects
        .values()
        .filter(|p| affected.is_none_or(|ids| ids.contains(&p.view.id)))
        .map(|p| (p.view.id.clone(), Repo(p.view.repo.clone())))
        .collect::<Vec<_>>();
    for (project, repo) in repos {
        if !unpaused(e, &project) {
            continue;
        }
        let key = MirrorKey::Project(project.clone());
        let input = MirrorInput::Project(repo.clone());
        if !retries.due(&key, &input, e.clock.now()) {
            healthy = false;
            continue;
        }
        let result = scan_and_mirror(e, forge, retries, key, input).await;
        if let Err(error) = result {
            healthy = false;
            eprintln!("dvb engine: GitHub project mirror: {error}");
        }
    }
    healthy
}

async fn goal_job(
    e: &AppState,
    forge: &dyn dvb_core::forge::Forge,
    retries: &mut Retries,
    (observations, visibilities): (&mut Observations, &mut Visibilities),
    snapshot: GoalSnapshot,
) -> Result<bool, Fail> {
    let (project, goal, repo, text, mut mirror, tasks) = snapshot;
    // Recheck after every awaited forge operation: an in-flight write may
    // finish during a pause, but it must not start the next one.
    if !unpaused(e, &project) {
        return Ok(true);
    }
    let visibility = match visibilities.get(&repo) {
        Some(read) => read.clone(),
        None => {
            let read = forge.visibility(&repo).await;
            visibilities.insert(repo.clone(), read.clone());
            read
        }
    };
    // Anyone can read a milestone on a public or internal repository, and
    // the goal text can hold the owner's private context.
    match visibility {
        Ok(Visibility::Private) => mirror.description.insert_str(0, &text),
        Ok(Visibility::Public) => {
            mirror.description = mirror.description.trim_start_matches('\n').to_owned();
        }
        Err(error) => {
            let error = crate::InternalError::from(error);
            observed(e, observations, &project, &goal, None, Err(&error))?;
            for (id, _) in tasks {
                observed(e, observations, &project, &goal, Some(&id), Err(&error))?;
            }
            return Ok(false);
        }
    }
    if !unpaused(e, &project) {
        return Ok(true);
    }
    let key = MirrorKey::Goal(goal.clone());
    let input = MirrorInput::Goal(repo.clone(), mirror.clone());
    let finished = mirror.done.then(|| FinishedGoal {
        input: input.clone(),
        tasks: tasks.clone(),
    });
    if finished.is_some() && retries.completed.get(&goal) == finished.as_ref() {
        return Ok(true);
    }
    retries.completed.remove(&goal);
    if !retries.due(&key, &input, e.clock.now()) {
        return Ok(false);
    }
    let result = scan_and_mirror(e, forge, retries, key, input).await;
    let milestone = match result {
        Ok(number) => {
            observed(e, observations, &project, &goal, None, Ok(()))?;
            number
        }
        Err(error) => {
            observed(e, observations, &project, &goal, None, Err(&error))?;
            for (id, _) in tasks {
                observed(e, observations, &project, &goal, Some(&id), Err(&error))?;
            }
            return Ok(false);
        }
    };
    let all_mirrored = task_jobs(
        e,
        forge,
        retries,
        observations,
        (&project, &goal, &repo),
        milestone,
        tasks,
    )
    .await?;
    if all_mirrored && let Some(finished) = finished {
        retries.completed.insert(goal, finished);
    }
    Ok(all_mirrored)
}

async fn task_jobs(
    e: &AppState,
    forge: &dyn dvb_core::forge::Forge,
    retries: &mut Retries,
    observations: &mut Observations,
    context: (&ProjectId, &GoalId, &Repo),
    milestone: u64,
    tasks: Vec<(TaskId, TaskMirror)>,
) -> Result<bool, Fail> {
    let (project, goal, repo) = context;
    let mut all_mirrored = true;
    for (id, mut task) in tasks {
        if !unpaused(e, project) {
            all_mirrored = false;
            break;
        }
        task.milestone = milestone;
        let key = MirrorKey::Task(id.clone());
        let input = MirrorInput::Task(repo.clone(), task.clone());
        if !retries.due(&key, &input, e.clock.now()) {
            all_mirrored = false;
            continue;
        }
        let result = scan_and_mirror(e, forge, retries, key, input)
            .await
            .map(|_| ());
        all_mirrored &= result.is_ok();
        observed(
            e,
            observations,
            project,
            goal,
            Some(&id),
            result.as_ref().copied(),
        )?;
    }
    Ok(all_mirrored)
}

async fn scan_and_mirror(
    e: &AppState,
    forge: &dyn dvb_core::forge::Forge,
    retries: &mut Retries,
    key: MirrorKey,
    input: MirrorInput,
) -> Result<u64, crate::InternalError> {
    let fields = match &input {
        MirrorInput::Project(_) => vec![],
        MirrorInput::Goal(_, goal) => vec![
            ("milestone title", goal.title.as_str()),
            ("milestone description", goal.description.as_str()),
        ],
        MirrorInput::Task(_, task) => vec![
            ("board item title", task.title.as_str()),
            ("board role", task.role.as_str()),
            ("board model", task.model.as_str()),
        ],
    };
    let policy = e.publication().policy();
    let hit = fields
        .into_iter()
        .find_map(|(field, value)| policy.scan(field, value));
    let mut rate_limit = None;
    let result = if let Some(hit) = hit {
        Err(format!("SCAN_HIT: {}:{} ({})", hit.file, hit.line, hit.rule).into())
    } else {
        match &input {
            MirrorInput::Project(repo) => forge.mirror_project(repo).await.map(|()| 0),
            MirrorInput::Goal(repo, goal) => forge.mirror_goal(repo, goal).await,
            MirrorInput::Task(repo, task) => forge.mirror_task(repo, task).await.map(|()| 0),
        }
        .inspect_err(|error| rate_limit = error.rate_limit)
        .map_err(crate::InternalError::from)
    };
    retries.finished(key, input, result.is_ok(), e.clock.now(), rate_limit);
    result
}

fn relevant(event: &dvb_core::Event) -> bool {
    matches!(
        event.kind,
        EventKind::ProjectAdded { .. }
            | EventKind::ProjectResumed { .. }
            | EventKind::GoalStated { .. }
            | EventKind::PlanProposed { .. }
            | EventKind::PlanCancelled { .. }
            | EventKind::GoalApproved { .. }
            | EventKind::GoalDone { .. }
            | EventKind::GoalDropped { .. }
            | EventKind::TaskStateChanged { .. }
            | EventKind::PrOpened { .. }
            | EventKind::RunStarted(_)
            | EventKind::RoleAdded(_)
            | EventKind::RoleChanged { .. }
            | EventKind::HireAdded(_)
            | EventKind::HireChanged { .. }
            | EventKind::HireRetired(_)
            | EventKind::EngineReconciled { .. }
    )
}

fn note(event: &dvb_core::Event, all: &mut bool, affected: &mut BTreeSet<ProjectId>) {
    if relevant(event) {
        match &event.project {
            Some(project) => {
                affected.insert(project.clone());
            }
            None => *all = true,
        }
    }
}

pub async fn watch(e: AppState) {
    let mut events = e.subscribe();
    // Startup and lag repair all projects; ordinary wakes use Event.project.
    let mut all = true;
    let mut affected = BTreeSet::new();
    let mut refresh = tokio::time::interval(Duration::from_secs(300));
    refresh.tick().await;
    let mut retry = tokio::time::interval(Duration::from_secs(60));
    retry.tick().await;
    let mut unhealthy = false;
    loop {
        loop {
            match events.try_recv() {
                Ok(event) => note(&event, &mut all, &mut affected),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => all = true,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
            }
        }
        if all || !affected.is_empty() {
            unhealthy = match sync_projects(&e, (!all).then_some(&affected)).await {
                Ok(healthy) => !healthy,
                Err(error) => {
                    eprintln!("dvb engine: GitHub mirror: {error}");
                    true
                }
            } || (!all && unhealthy);
            all = false;
            affected.clear();
        }
        tokio::select! {
            _ = refresh.tick() => all = true,
            _ = retry.tick(), if unhealthy => all = true,
            event = events.recv() => match event {
                Ok(event) => note(&event, &mut all, &mut affected),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => all = true,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}
