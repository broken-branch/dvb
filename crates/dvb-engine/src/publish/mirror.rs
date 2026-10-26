//! Mirror snapshots on startup and authoritative changes, with periodic passes.
//! A milestone or card is sent only when it differs from its last receipt
//! (`github.mirror_sent`), so manual board edits stay until the item changes.
//! GitHub observations never enter projections.
use super::without_run_ids;
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

/// In-memory effect retries; the receipts of what was sent are durable events.
#[derive(Default)]
pub(crate) struct Retries {
    pending: BTreeMap<MirrorKey, Retry>,
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

/// A goal's milestone, or one of its task's board items.
type Item = (ProjectId, GoalId, Option<TaskId>);

/// What GitHub confirmed for an item: its digest, and a goal's milestone.
struct Receipt {
    digest: String,
    milestone: Option<u64>,
}

#[derive(Default)]
struct Observations {
    items: BTreeMap<Item, (bool, Option<String>)>,
    /// Projects told that GitHub reaches no repository by their saved name.
    unreachable: BTreeSet<ProjectId>,
    /// The confirmed receipts; an item whose last send failed has none.
    sent: BTreeMap<Item, Receipt>,
}

/// The digest of the fields an item sends. The repository's name is left
/// out: a repoint keeps the same repository and moves its mirror with it.
fn digest(fields: &[&str]) -> String {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    for field in fields {
        hash.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn goal_digest(goal: &GoalMirror) -> String {
    digest(&[
        "goal",
        &goal.key,
        &goal.title,
        &goal.description,
        if goal.done { "closed" } else { "open" },
    ])
}

fn task_digest(task: &TaskMirror) -> String {
    digest(&[
        "task",
        &task.key,
        &task.title,
        &task.role,
        &task.model,
        task.state.label(),
        &task.pr.map(|pr| pr.to_string()).unwrap_or_default(),
        &task.milestone.to_string(),
    ])
}

/// Records what GitHub confirmed for the item, or, after a failed send, that
/// its last receipt no longer holds.
fn receipt(
    e: &AppState,
    observations: &mut Observations,
    item: Item,
    sent: Option<Receipt>,
) -> Result<(), Fail> {
    if sent.is_none() && !observations.sent.contains_key(&item) {
        return Ok(());
    }
    let (project, goal, task) = item.clone();
    e.append_checked(Some(&project), Actor::Engine, |_, _| {
        Ok(vec![EventKind::GithubMirrorSent {
            goal: goal.clone(),
            task: task.clone(),
            digest: sent.as_ref().map(|r| r.digest.clone()),
            milestone: sent.as_ref().and_then(|r| r.milestone),
        }])
    })?;
    match sent {
        Some(sent) => observations.sent.insert(item, sent),
        None => observations.sent.remove(&item),
    };
    Ok(())
}

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
    // GitHub refuses the token on every call until the project is repointed.
    if matches!(result, Err(crate::InternalError::Forge(error)) if error.unreachable) {
        if observations.unreachable.insert(project.clone()) {
            e.append_checked(Some(project), Actor::Engine, |proj, _| {
                Ok(proj
                    .projects
                    .get(project)
                    .map(|p| EventKind::ProjectRepoUnreachable {
                        repo: p.view.repo.clone(),
                    })
                    .into_iter()
                    .collect())
            })?;
        }
        return Ok(());
    }
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
            .items
            .iter()
            .any(|((other, ..), seen)| other == project && *seen == observation)
    } else {
        // The first write after the notice records that it reached GitHub again.
        observations.items.get(&key) == Some(&observation)
            && !(mirrored && observations.unreachable.contains(project))
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
        observations.items.insert(key, observation);
        if mirrored {
            observations.unreachable.remove(project);
        }
    }
    Ok(())
}

/// The project is still there under the snapshot's repository and not paused.
/// Checked under the mirror lock, so a project removed or repointed after the
/// snapshot gets no receipt or event.
fn mirrorable(e: &AppState, project: &ProjectId, repo: &Repo) -> bool {
    e.proj()
        .projects
        .get(project)
        .is_some_and(|p| !p.view.paused && p.view.repo == repo.0)
}

struct Job<'a> {
    e: &'a AppState,
    retries: tokio::sync::MutexGuard<'a, Retries>,
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        // Clear the project before releasing the lock to the next job.
        if let Some(pass) = self
            .e
            .mirror_pass
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .as_mut()
        {
            pass.project = None;
        }
    }
}

/// Takes the mirror lock for one job and marks the pass as on its project.
async fn job<'a>(e: &'a AppState, project: &ProjectId) -> Job<'a> {
    let retries = e.mirror_retries.lock().await;
    if let Some(pass) = e
        .mirror_pass
        .lock()
        .unwrap_or_else(dvb_core::sync::recover)
        .as_mut()
    {
        pass.project = Some(project.clone());
    }
    Job { e, retries }
}

/// How long a project's remove or repoint waits for the running job; well
/// inside the CLI's request timeout.
const PROJECT_CHANGE_WAIT: Duration = Duration::from_secs(10);

/// The mirror lock for removing or repointing a project, or a refusal that
/// names the pass it waited for: nothing has changed yet when it is refused.
pub(crate) async fn lock_for_change<'a>(
    e: &'a AppState,
    project: &ProjectId,
    command: &str,
) -> Result<tokio::sync::MutexGuard<'a, Retries>, Fail> {
    if let Ok(retries) = tokio::time::timeout(PROJECT_CHANGE_WAIT, e.mirror_retries.lock()).await {
        return Ok(retries);
    }
    let pass = e
        .mirror_pass
        .lock()
        .unwrap_or_else(dvb_core::sync::recover)
        .clone();
    let detail = match pass {
        Some(pass) => {
            let started = chrono::DateTime::from_timestamp_millis(pass.started.millis())
                .map_or_else(
                    || pass.started.millis().to_string(),
                    |time| time.format("%H:%M UTC").to_string(),
                );
            let on = pass
                .project
                .and_then(|id| e.proj().projects.get(&id).map(|p| p.view.name.clone()))
                .map(|name| format!(", now on project {name}"))
                .unwrap_or_default();
            format!(
                "waiting for the GitHub mirror pass, started {started}{on}; nothing was changed, try again"
            )
        }
        None => "waiting for the GitHub mirror; nothing was changed, try again".into(),
    };
    Err(e.refuse(
        Some(project),
        Actor::Owner,
        command,
        dvb_core::Refusal::new(dvb_core::RefusalCode::MirrorPassRunning, detail),
    ))
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
    let started = e.clock.now();
    let scope = affected.map_or_else(
        || "every project".to_owned(),
        |ids| {
            let proj = e.proj();
            ids.iter()
                .map(|id| proj.projects.get(id).map_or(id.as_str(), |p| &p.view.name))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    eprintln!("dvb engine: GitHub mirror pass started for {scope}");
    *e.mirror_pass.lock().unwrap_or_else(dvb_core::sync::recover) =
        Some(dvb_core::api::MirrorPass {
            started,
            project: None,
        });
    let result = pass(e, forge.as_ref(), affected).await;
    *e.mirror_pass.lock().unwrap_or_else(dvb_core::sync::recover) = None;
    let seconds = e.clock.now().millis().saturating_sub(started.millis()) / 1000;
    eprintln!("dvb engine: GitHub mirror pass for {scope} finished in {seconds} s");
    result
}

async fn pass(
    e: &AppState,
    forge: &dyn dvb_core::forge::Forge,
    affected: Option<&BTreeSet<ProjectId>>,
) -> Result<bool, Fail> {
    let mut observations = Observations::default();
    for event in e.store.select(
        dvb_core::EventId(0),
        dvb_store::Select::Kinds(&[
            "github.mirror_observed",
            "github.mirror_sent",
            "project.repo_unreachable",
            "project.repo_changed",
        ]),
    )? {
        let Some(project) = event.project else {
            continue;
        };
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "the store selects only these four kinds"
        )]
        match event.kind {
            EventKind::GithubMirrorObserved {
                goal,
                task,
                mirrored,
                detail,
            } => {
                if mirrored {
                    observations.unreachable.remove(&project);
                }
                observations
                    .items
                    .insert((project, goal, task), (mirrored, detail));
            }
            EventKind::GithubMirrorSent {
                goal,
                task,
                digest,
                milestone,
            } => {
                let item = (project, goal, task);
                match digest {
                    Some(digest) => observations
                        .sent
                        .insert(item, Receipt { digest, milestone }),
                    None => observations.sent.remove(&item),
                };
            }
            EventKind::ProjectRepoUnreachable { .. } => {
                observations.unreachable.insert(project);
            }
            EventKind::ProjectRepoChanged { .. } => {
                observations.unreachable.remove(&project);
            }
            _ => {}
        }
    }
    let mut healthy = project_jobs(e, forge, affected).await;
    let mut visibilities = Visibilities::new();
    for goal in snapshot(e, affected) {
        healthy &= goal_job(e, forge, (&mut observations, &mut visibilities), goal).await?;
    }
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
                                title: without_run_ids(&t.view.title).into_owned(),
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
                    without_run_ids(&g.text).into_owned(),
                    GoalMirror {
                        key: g.id.to_string(),
                        title: without_run_ids(g.text.lines().next().unwrap_or("Goal"))
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
        let mut job = job(e, &project).await;
        if !mirrorable(e, &project, &repo) {
            continue;
        }
        let key = MirrorKey::Project(project.clone());
        let input = MirrorInput::Project(repo.clone());
        if !job.retries.due(&key, &input, e.clock.now()) {
            healthy = false;
            continue;
        }
        let result = scan_and_mirror(e, forge, &mut job.retries, key, input).await;
        drop(job);
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
    (observations, visibilities): (&mut Observations, &mut Visibilities),
    snapshot: GoalSnapshot,
) -> Result<bool, Fail> {
    let (project, goal, repo, text, mut mirror, tasks) = snapshot;
    let mut job = job(e, &project).await;
    // Recheck after every awaited forge operation: an in-flight write may
    // finish during a pause, but it must not start the next one.
    if !mirrorable(e, &project, &repo) {
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
    if !mirrorable(e, &project, &repo) {
        return Ok(true);
    }
    let item = (project.clone(), goal.clone(), None);
    let digest = goal_digest(&mirror);
    let unchanged = observations
        .sent
        .get(&item)
        .filter(|sent| sent.digest == digest)
        .and_then(|sent| sent.milestone);
    let milestone = if let Some(number) = unchanged {
        observed(e, observations, &project, &goal, None, Ok(()))?;
        number
    } else {
        let key = MirrorKey::Goal(goal.clone());
        let input = MirrorInput::Goal(repo.clone(), mirror);
        if !job.retries.due(&key, &input, e.clock.now()) {
            return Ok(false);
        }
        match scan_and_mirror(e, forge, &mut job.retries, key, input).await {
            Ok(number) => {
                observed(e, observations, &project, &goal, None, Ok(()))?;
                let sent = Receipt {
                    digest,
                    milestone: Some(number),
                };
                receipt(e, observations, item, Some(sent))?;
                number
            }
            Err(error) => {
                observed(e, observations, &project, &goal, None, Err(&error))?;
                receipt(e, observations, item, None)?;
                for (id, _) in tasks {
                    observed(e, observations, &project, &goal, Some(&id), Err(&error))?;
                }
                return Ok(false);
            }
        }
    };
    let all_mirrored = task_jobs(
        e,
        forge,
        &mut job.retries,
        observations,
        (&project, &goal, &repo),
        milestone,
        tasks,
    )
    .await?;
    drop(job);
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
        if !mirrorable(e, project, repo) {
            all_mirrored = false;
            break;
        }
        task.milestone = milestone;
        let item = (project.clone(), goal.clone(), Some(id.clone()));
        let digest = task_digest(&task);
        if observations
            .sent
            .get(&item)
            .is_some_and(|sent| sent.digest == digest)
        {
            observed(e, observations, project, goal, Some(&id), Ok(()))?;
            continue;
        }
        let key = MirrorKey::Task(id.clone());
        let input = MirrorInput::Task(repo.clone(), task);
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
        let sent = result.is_ok().then_some(Receipt {
            digest,
            milestone: None,
        });
        receipt(e, observations, item, sent)?;
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
            | EventKind::ProjectRepoChanged { .. }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn status(e: &AppState) -> dvb_core::api::Status {
        let response = crate::api::router(e.clone())
            .oneshot(Request::get("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success());
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn status_has_no_project_between_mirror_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        let started = Timestamp(1_791_990_000_000);
        *e.mirror_pass.lock().unwrap() = Some(dvb_core::api::MirrorPass {
            started,
            project: None,
        });
        for id in ["first", "second"] {
            let project = ProjectId::new(id);
            let held = job(&e, &project).await;
            let running = status(&e).await;
            assert_eq!(running.mirror_pass.unwrap().project, Some(project));
            drop(held);
            assert!(e.mirror_retries.try_lock().is_ok());
            let between = status(&e).await;
            assert_eq!(
                between.mirror_pass,
                Some(dvb_core::api::MirrorPass {
                    started,
                    project: None
                })
            );
        }
    }
}
