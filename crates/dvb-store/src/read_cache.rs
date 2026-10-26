//! Rebuildable read-route cache, advanced transactionally from typed events.
use crate::{StoreError, event_row};
use dvb_core::api::ProjectActivity;
use dvb_core::{Actor, Event, EventId, EventKind, GoalId, TaskId, TaskState};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
struct Goal {
    approved: bool,
    failed: bool,
    tasks: BTreeMap<TaskId, TaskState>,
}

#[derive(Clone, Default)]
pub(crate) struct ReadCache {
    pub cursor: u64,
    pub activity: BTreeMap<String, ProjectActivity>,
    goals: BTreeMap<GoalId, Goal>,
    tasks: BTreeMap<TaskId, GoalId>,
    #[cfg(test)]
    pub visited: usize,
}

impl ReadCache {
    pub fn prepare(&self, conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS dvb_read_latest (key TEXT PRIMARY KEY, id INTEGER NOT \
                NULL UNIQUE);
            CREATE TEMP TABLE IF NOT EXISTS dvb_read_runs (run TEXT PRIMARY KEY, role \
                TEXT NOT NULL, task TEXT);
            CREATE INDEX IF NOT EXISTS dvb_read_runs_role ON dvb_read_runs(role);
            CREATE INDEX IF NOT EXISTS dvb_read_runs_task ON dvb_read_runs(task);
            CREATE TEMP TABLE IF NOT EXISTS dvb_read_tasks (task TEXT PRIMARY KEY, role \
                TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS dvb_read_tasks_role ON dvb_read_tasks(role);
            CREATE TEMP TABLE IF NOT EXISTS dvb_read_events (id INTEGER PRIMARY KEY, \
                task TEXT, run TEXT, actor_run TEXT, decision INTEGER NOT NULL);
            CREATE TEMP TABLE IF NOT EXISTS dvb_read_event_tasks (id INTEGER NOT NULL, \
                task TEXT NOT NULL, PRIMARY KEY(id, task));
            CREATE INDEX IF NOT EXISTS dvb_read_event_tasks_task ON \
                dvb_read_event_tasks(task, id);
            CREATE TEMP TABLE IF NOT EXISTS dvb_read_event_roles (id INTEGER NOT NULL, \
                role TEXT NOT NULL, PRIMARY KEY(id, role));
            CREATE INDEX IF NOT EXISTS dvb_read_event_roles_role ON \
                dvb_read_event_roles(role, id)",
        )?;
        Ok(())
    }

    pub fn advance(&mut self, conn: &Connection, cursor: u64) -> Result<(), StoreError> {
        self.prepare(conn)?;
        if self.cursor == cursor {
            return Ok(());
        }
        // Neither memory nor temporary SQL indexes move until the entire advance succeeds.
        let mut next = self.clone();
        let tx = conn.unchecked_transaction()?;
        next.fold(&tx, cursor)?;
        tx.commit()?;
        *self = next;
        Ok(())
    }

    fn fold(&mut self, conn: &Connection, cursor: u64) -> Result<(), StoreError> {
        let mut changed_keys = BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT id, at, actor, project, kind, payload, evidence FROM events WHERE id > ?1 AND \
                id <= ?2 ORDER BY id",
        )?;
        let mut rows = stmt.query(params![
            i64::try_from(self.cursor).unwrap_or(i64::MAX),
            i64::try_from(cursor).unwrap_or(i64::MAX)
        ])?;
        while let Some(row) = rows.next()? {
            let event = event_row(row)?;
            #[cfg(test)]
            {
                self.visited += 1;
            }
            if let Some(project) = &event.project {
                let a = self
                    .activity
                    .entry(project.0.clone())
                    .or_insert_with(|| ProjectActivity {
                        project: project.clone(),
                        latest: EventId(0),
                        fresh: 0,
                        goals: Vec::new(),
                    });
                if !matches!(
                    event.kind,
                    EventKind::ProjectSeen {}
                        | EventKind::TaskRetryTextSet { .. }
                        | EventKind::TaskStateChanged { .. }
                        | EventKind::RunUsage { .. }
                ) {
                    a.latest = event.id;
                }
                if matches!(event.kind, EventKind::ProjectSeen {}) {
                    a.fresh = 0;
                }
                if matches!(
                    event.kind,
                    EventKind::QuestionOpened { .. }
                        | EventKind::TaskDone { .. }
                        | EventKind::TaskFailed { .. }
                        | EventKind::GoalDone { .. }
                ) {
                    a.fresh += 1;
                }
            }
            let mut changed_goal = None;
            if let EventKind::GoalStated { goal, .. }
            | EventKind::GoalApproved { goal }
            | EventKind::GoalDone { goal }
            | EventKind::GoalDropped { goal } = &event.kind
            {
                self.goals.entry(goal.clone()).or_default().approved =
                    matches!(event.kind, EventKind::GoalApproved { .. });
                changed_goal = Some(goal.clone());
            } else if let EventKind::PlanProposed { goal, tasks } = &event.kind {
                for entry in tasks {
                    if let Some(old) = self.tasks.insert(entry.task.clone(), goal.clone()) {
                        self.goals.entry(old).or_default().tasks.remove(&entry.task);
                    }
                    self.goals
                        .entry(goal.clone())
                        .or_default()
                        .tasks
                        .insert(entry.task.clone(), TaskState::Planned);
                    conn.execute(
                        "INSERT INTO dvb_read_tasks (task, role) VALUES (?1, ?2) \
                         ON CONFLICT(task) DO UPDATE SET role = excluded.role",
                        params![entry.task.0, entry.role.0],
                    )?;
                }
                changed_goal = Some(goal.clone());
            } else if let EventKind::TaskStateChanged {
                task, to: state, ..
            }
            | EventKind::EngineReconciled { task, state } = &event.kind
                && let Some(goal) = self.tasks.get(task)
            {
                self.goals
                    .entry(goal.clone())
                    .or_default()
                    .tasks
                    .insert(task.clone(), state.clone());
                changed_goal = Some(goal.clone());
            }
            if let Some(goal) = changed_goal {
                let g = self.goals.entry(goal).or_default();
                let failed = dvb_core::state::goal_failed(g.approved, g.tasks.values());
                if failed
                    && !g.failed
                    && let Some(project) = &event.project
                    && let Some(a) = self.activity.get_mut(&project.0)
                {
                    a.fresh += 1;
                }
                g.failed = failed;
            }
            let scope = index_event(conn, &event)?;
            let key = serde_json::json!([
                event.project,
                event.kind.kind(),
                scope.task,
                scope.goal,
                matches!(&event.kind, EventKind::RunStarted(a) if a.reviews.is_some())
            ])
            .to_string();
            changed_keys.insert(key, i64::try_from(event.id.0).unwrap_or(i64::MAX));
        }
        for (key, id) in changed_keys {
            conn.execute(
                "INSERT INTO dvb_read_latest (key, id) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET id = excluded.id",
                params![key, id],
            )?;
        }
        self.cursor = cursor;
        Ok(())
    }
}

#[derive(Default)]
struct Scope<'a> {
    task: Option<&'a TaskId>,
    goal: Option<&'a GoalId>,
}

fn index_event<'a>(conn: &Connection, e: &'a Event) -> Result<Scope<'a>, StoreError> {
    use EventKind::*;
    let mut scope = Scope::default();
    let mut run = None;
    let mut role = None;
    match &e.kind {
        EngineReconciled { task, .. }
        | QuestionClosed { task, .. }
        | TaskWaiting { task, .. }
        | TaskStopped { task, .. }
        | TaskResumed { task, .. }
        | TaskCancelled { task, .. }
        | TaskDone { task, .. }
        | TaskFailed { task, .. }
        | TaskRetryTextSet { task, .. }
        | TaskStateChanged { task, .. }
        | BranchPublished { task, .. }
        | BranchUpdated { task, .. }
        | ScanHit { task, .. }
        | PrOpened { task, .. }
        | ChecksObserved { task, .. }
        | ChecksPending { task, .. }
        | ChecksStuck { task, .. }
        | MergeRefused { task, .. }
        | PrMerged { task, .. }
        | EffectRetry { task, .. } => scope.task = Some(task),
        TaskStarted { task, run: r, .. } | TaskReasonChanged { task, run: r, .. } => {
            scope.task = Some(task);
            run = Some(r);
        }
        RunStarted(a) => {
            scope.task = a.task.as_ref();
            run = Some(&a.run);
            role = Some(&a.role);
            conn.execute(
                "INSERT INTO dvb_read_runs (run, role, task) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(run) DO UPDATE SET role = excluded.role, task = excluded.task",
                params![a.run.0, a.role.0, a.task.as_ref().map(|t| &t.0)],
            )?;
        }
        RunPreflight { run: r, .. }
        | RunModel { run: r, .. }
        | RunOutput { run: r, .. }
        | RunUsage { run: r, .. }
        | RunCollected { run: r, .. }
        | RunStopRequested { run: r }
        | RunSurvivedRestart { run: r }
        | RunLaunched { run: r }
        | RunEnded { run: r, .. } => run = Some(r),
        RunTimeLimitReached { run: r, task, .. } => {
            run = Some(r);
            scope.task = task.as_ref();
        }
        RunRerouted {
            run: r,
            task,
            role: by,
            ..
        } => {
            run = Some(r);
            scope.task = task.as_ref();
            role = Some(by);
        }
        VerdictRecorded {
            task,
            run: r,
            role: by,
            ..
        } => {
            scope.task = Some(task);
            run = Some(r);
            role = Some(by);
        }
        RoleAdded(r) | RoleChanged { role: r, .. } | RoleRemoved(r) => role = Some(&r.id),
        HireAdded(h) | HireChanged { hire: h, .. } | HireRetired(h) => role = Some(&h.role),
        PlanProposed { goal, tasks } => {
            scope.goal = Some(goal);
            for t in tasks {
                conn.execute(
                    "INSERT OR IGNORE INTO dvb_read_event_tasks (id, task) VALUES (?1, ?2)",
                    params![i64::try_from(e.id.0).unwrap_or(i64::MAX), t.task.0],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO dvb_read_event_roles (id, role) VALUES (?1, ?2)",
                    params![i64::try_from(e.id.0).unwrap_or(i64::MAX), t.role.0],
                )?;
            }
        }
        GoalStated { goal, .. }
        | PlanCancelled { goal, .. }
        | GoalApproved { goal }
        | GoalDone { goal }
        | GoalDropped { goal }
        | GoalAfterSet { goal, .. }
        | GoalIssueClosed { goal, .. } => scope.goal = Some(goal),
        GithubMirrorObserved { goal, task, .. } | GithubMirrorSent { goal, task, .. } => {
            scope.goal = Some(goal);
            scope.task = task.as_ref();
        }
        QuestionOpened { task, .. } => scope.task = task.as_ref(),
        EngineStarted { .. }
        | EngineUpdated { .. }
        | EngineUpdateFailed { .. }
        | ProjectAdded { .. }
        | RepositoryCreated { .. }
        | RepositorySetupPrepared { .. }
        | ProjectPaused {}
        | ProjectResumed {}
        | ProjectSeen {}
        | ProjectBriefSet { .. }
        | ProjectIssuesFound(_)
        | ProjectRulesetMissing {}
        | ProjectRulesetInstalled {}
        | ProjectRepoChanged { .. }
        | ProjectRepoUnreachable { .. }
        | ProjectRemoved { .. }
        | ProposalRequested {}
        | SettingsChanged { .. }
        | QuestionAnswered { .. }
        | DecisionRecorded { .. }
        | CommandRefused(_)
        | OnlyStarted(_)
        | OnlyEnded { .. }
        | VendorLimited { .. }
        | VendorResumed { .. } => {}
    }
    let actor_run = match &e.actor {
        Actor::Run(run) => Some(&run.0),
        Actor::Owner | Actor::Engine => None,
    };
    conn.execute(
        "INSERT INTO dvb_read_events (id, task, run, actor_run, decision) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            i64::try_from(e.id.0).unwrap_or(i64::MAX),
            scope.task.map(|t| &t.0),
            run.map(|r| &r.0),
            actor_run,
            matches!(e.kind, DecisionRecorded { .. })
        ],
    )?;
    if let Some(task) = scope.task {
        conn.execute(
            "INSERT OR IGNORE INTO dvb_read_event_tasks (id, task) VALUES (?1, ?2)",
            params![i64::try_from(e.id.0).unwrap_or(i64::MAX), task.0],
        )?;
    }
    if let Some(role) = role {
        conn.execute(
            "INSERT OR IGNORE INTO dvb_read_event_roles (id, role) VALUES (?1, ?2)",
            params![i64::try_from(e.id.0).unwrap_or(i64::MAX), role.0],
        )?;
    }
    Ok(scope)
}
