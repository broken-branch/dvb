//! SQLite event log, migrations and snapshots.
//!
//! One file, WAL, append-only `events` table; refinery migrations. Writes go
//! through one connection behind a lock, so there is one writer. Projections
//! stay in the engine's memory and are rebuilt from the log at startup.

use dvb_core::{Actor, Event, EventId, EventKind, Evidence, ProjectId, RunId, Timestamp};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;

mod read_cache;

mod embedded {
    refinery::embed_migrations!("migrations");
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("store: {path}: {source}")]
    Open {
        path: std::path::PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("store: {0}")]
    Json(#[from] serde_json::Error),
    #[error("store migration: {0}")]
    Migration(#[from] refinery::Error),
    #[error("store: {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("store: negative event id {0}")]
    NegativeId(i64),
    #[error("store: bad time {value}: {source}")]
    Time {
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

/// One event to append; the store gives it its id and the batch's time.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub actor: Actor,
    pub project: Option<ProjectId>,
    pub kind: EventKind,
    pub evidence: Option<Evidence>,
}

/// The events a read (`Store::each`, `Store::select`) decodes.
#[derive(Debug, Clone, Copy)]
pub enum Select<'a> {
    All,
    /// Events of these dotted kinds.
    Kinds(&'a [&'a str]),
    /// One run's `run.output`.
    RunOutput(&'a RunId),
    /// Every event but run telemetry, most of the log: `run.output` text and
    /// tool calls (notes stay), `run.usage`, `run.preflight`, `run.model`,
    /// `run.launched` and `task.state`. Prompts, plans, notes and record
    /// histories read the rest, `run.collected` summaries included.
    WithoutTelemetry,
}

/// The newest migration this build knows: two builds read the same database
/// exactly when this is equal (`dvb update --to`).
pub fn schema_version() -> u32 {
    embedded::migrations::runner()
        .get_migrations()
        .iter()
        .map(refinery::Migration::version)
        .max()
        .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

pub struct Store {
    conn: Mutex<Connection>,
    read_cache: Mutex<read_cache::ReadCache>,
}

impl Store {
    /// Opens (or creates) the database and applies the migrations.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::init(Connection::open(path).map_err(|source| StoreError::Open {
            path: path.into(),
            source,
        })?)
    }

    /// Opens an existing database without applying migrations, to append to its
    /// log (the `events` table is the same in every schema). The update recovery
    /// uses it after restoring a snapshot: the binary doing the restore may be
    /// newer than the schema it restored.
    pub fn open_unmigrated(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: Mutex::new(Connection::open(path).map_err(|source| StoreError::Open {
                path: path.into(),
                source,
            })?),
            read_cache: Mutex::default(),
        })
    }

    /// An in-memory store for tests.
    pub fn memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        // All pending migrations commit together or not at all.
        embedded::migrations::runner()
            .set_grouped(true)
            .run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            read_cache: Mutex::default(),
        })
    }

    /// A consistent copy of the database at `to` (SQLite's online backup), synced
    /// to disk; `to` must not exist.
    pub fn snapshot(&self, to: &Path) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(dvb_core::sync::recover);
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(to)
                .map_err(|source| StoreError::Io {
                    path: to.into(),
                    source,
                })?;
        }
        let mut dst = Connection::open(to)?;
        {
            let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
            backup.run_to_completion(1024, std::time::Duration::ZERO, None)?;
        }
        drop(conn);
        // A snapshot is one file, whatever journal mode the copy ended in.
        dst.pragma_update(None, "journal_mode", "DELETE")?;
        drop(dst);
        std::fs::File::open(to)
            .and_then(|f| f.sync_all())
            .map_err(|source| StoreError::Io {
                path: to.into(),
                source,
            })
    }

    /// Appends a batch in one transaction, all at `at`; returns the stored events.
    pub fn append(&self, at: Timestamp, batch: Vec<NewEvent>) -> Result<Vec<Event>, StoreError> {
        let mut conn = self.conn.lock().unwrap_or_else(dvb_core::sync::recover);
        let tx = conn.transaction()?;
        let mut out = Vec::with_capacity(batch.len());
        for e in batch {
            let (kind, payload) = split(&e.kind)?;
            tx.execute(
                "INSERT INTO events (at, actor, project, kind, payload, evidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    at.0.to_string(),
                    serde_json::to_string(&e.actor)?,
                    e.project.as_ref().map(|p| p.0.clone()),
                    kind,
                    payload,
                    e.evidence.as_ref().map(serde_json::to_string).transpose()?,
                ],
            )?;
            let id = u64::try_from(tx.last_insert_rowid())
                .map_err(|_| StoreError::NegativeId(tx.last_insert_rowid()))?;
            out.push(Event {
                id: EventId(id),
                at,
                actor: e.actor,
                project: e.project,
                kind: e.kind,
                evidence: e.evidence,
            });
        }
        tx.commit()?;
        drop(conn);
        Ok(out)
    }

    /// Every event after `after`, in id order.
    pub fn after(&self, after: EventId) -> Result<Vec<Event>, StoreError> {
        self.select(after, Select::All)
    }

    /// The events after `after` that `select` picks, in id order.
    pub fn select(&self, after: EventId, select: Select<'_>) -> Result<Vec<Event>, StoreError> {
        let mut out = Vec::new();
        self.each(after, select, |e| out.push(e))?;
        Ok(out)
    }

    /// Calls `f` with each event after `after` that `select` picks, in id order,
    /// one at a time; SQL skips the other rows without decoding them. Folds use
    /// this to avoid allocating the whole decoded history.
    pub fn each(
        &self,
        after: EventId,
        select: Select<'_>,
        mut f: impl FnMut(Event),
    ) -> Result<(), StoreError> {
        let (filter, arg) = match select {
            Select::All => ("1", None),
            Select::Kinds(kinds) => {
                debug_assert!(kinds.iter().all(|k| EventKind::ALL_KINDS.contains(k)));
                (
                    "instr(',' || ?2 || ',', ',' || kind || ',') > 0",
                    Some(kinds.join(",")),
                )
            }
            Select::RunOutput(run) => (
                "kind = 'run.output' AND json_extract(payload, '$.run') = ?2",
                Some(run.0.clone()),
            ),
            Select::WithoutTelemetry => (
                "kind NOT IN ('run.usage', 'run.preflight', 'run.model', 'run.launched', \
                    'task.state') AND (kind != 'run.output' OR \
                    json_extract(payload, '$.item.note') IS NOT NULL)",
                None,
            ),
        };
        let conn = self.conn.lock().unwrap_or_else(dvb_core::sync::recover);
        let mut stmt = conn.prepare(&format!(
            "SELECT id, at, actor, project, kind, payload, evidence
             FROM events WHERE id > ?1 AND ({filter}) ORDER BY id"
        ))?;
        let after = i64::try_from(after.0).unwrap_or(i64::MAX);
        let mut rows = match arg {
            Some(arg) => stmt.query(params![after, arg])?,
            None => stmt.query(params![after])?,
        };
        while let Some(row) = rows.next()? {
            f(event_row(row)?);
        }
        drop(rows);
        drop(stmt);
        drop(conn);
        Ok(())
    }

    /// Bounded pages with cursor-keyed summary/evidence caches; no repeated replay.
    pub fn event_page(
        &self,
        q: &dvb_core::api::EventPageQuery,
    ) -> Result<dvb_core::api::EventPage, StoreError> {
        use dvb_core::api::EventPage;
        let mut cache = self
            .read_cache
            .lock()
            .unwrap_or_else(dvb_core::sync::recover);
        let conn = self.conn.lock().unwrap_or_else(dvb_core::sync::recover);
        let cursor: i64 =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?;
        let limit = q.limit.unwrap_or(50).clamp(1, 200);
        if q.summary.unwrap_or(false)
            || q.latest.unwrap_or(false)
            || q.task.is_some()
            || q.role.is_some()
        {
            cache.advance(&conn, u64::try_from(cursor).unwrap_or(0))?;
        }
        cache.prepare(&conn)?;
        let sql = concat!(
            "SELECT e.id, e.at, e.actor, e.project, e.kind, e.payload, e.evidence FROM ",
            "{event_source}\n",
            "             WHERE e.id < ?1 AND (?2 IS NULL OR project = ?2)\n",
            "             AND (?3 IS NULL OR instr(',' || ?3 || ',', ',' || kind || ',') > 0)\n",
            "             AND (?4 IS NULL OR EXISTS (SELECT 1 FROM dvb_read_event_tasks t WHERE \
                t.id = e.id AND t.task = ?4)\n",
            "                 OR EXISTS (SELECT 1 FROM dvb_read_runs r JOIN dvb_read_events c ON c.id = e.id\n",
            "                    WHERE r.task = ?4 AND (r.run = c.run OR (c.decision AND r.run = \
                c.actor_run))))\n",
            "             AND (?5 IS NULL OR EXISTS (SELECT 1 FROM dvb_read_event_roles r WHERE \
                r.id = e.id AND r.role = ?5)\n",
            "                 OR EXISTS (SELECT 1 FROM dvb_read_tasks t JOIN dvb_read_events c ON c.id = e.id\n",
            "                    WHERE t.role = ?5 AND t.task = c.task)\n",
            "                 OR EXISTS (SELECT 1 FROM dvb_read_runs r JOIN dvb_read_events c ON c.id = e.id\n",
            "                    WHERE r.role = ?5 AND (r.run = c.run OR r.run = c.actor_run)))\n",
            "             ORDER BY e.id DESC LIMIT ?6",
        );
        let event_source = if q.latest.unwrap_or(false) {
            "dvb_read_latest l CROSS JOIN events e ON e.id = l.id"
        } else {
            "events e"
        };
        let mut stmt = conn.prepare(&sql.replace("{event_source}", event_source))?;
        let mut rows = stmt.query(params![
            q.before
                .map_or(i64::MAX, |id| i64::try_from(id.0).unwrap_or(i64::MAX)),
            q.project.as_ref().map(|p| &p.0),
            q.kinds,
            q.task.as_ref().map(|t| &t.0),
            q.role.as_ref().map(|r| &r.0),
            limit + 1
        ])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(event_row(row)?);
        }
        drop(rows);
        drop(stmt);
        drop(conn);
        let more = events.len() > limit as usize;
        events.truncate(limit as usize);
        let before = if more {
            events.last().map(|e| e.id)
        } else {
            None
        };
        let activity = if q.summary.unwrap_or(false) {
            cache.activity.values().cloned().collect()
        } else {
            Vec::new()
        };
        drop(cache);
        Ok(EventPage {
            cursor: EventId(u64::try_from(cursor).unwrap_or(0)),
            events,
            before,
            activity,
        })
    }

    /// The newest event's id (0 when empty).
    pub fn last_id(&self) -> Result<EventId, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(dvb_core::sync::recover);
        let id: Option<i64> = conn.query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))?;
        drop(conn);
        Ok(EventId(id.and_then(|i| u64::try_from(i).ok()).unwrap_or(0)))
    }
}

/// The single decoder for durable rows and rebuildable read-cache input.
fn event_row(row: &rusqlite::Row<'_>) -> Result<Event, StoreError> {
    let id: i64 = row.get(0)?;
    let at: String = row.get(1)?;
    let actor: String = row.get(2)?;
    let project: Option<String> = row.get(3)?;
    let kind: String = row.get(4)?;
    let payload: String = row.get(5)?;
    let evidence: Option<String> = row.get(6)?;
    Ok(Event {
        id: EventId(u64::try_from(id).map_err(|_| StoreError::NegativeId(id))?),
        at: Timestamp(
            at.parse()
                .map_err(|source| StoreError::Time { value: at, source })?,
        ),
        actor: serde_json::from_str(&actor)?,
        project: project.map(ProjectId),
        kind: serde_json::from_value(
            serde_json::json!({"kind": kind, "payload": serde_json::from_str::<serde_json::Value>(&payload)?}),
        )?,
        evidence: evidence.as_deref().map(serde_json::from_str).transpose()?,
    })
}

/// The `kind` column and the `payload` column of an event kind.
fn split(kind: &EventKind) -> Result<(&'static str, String), StoreError> {
    let mut v = serde_json::to_value(kind)?;
    let payload = v
        .get_mut("payload")
        .map(serde_json::Value::take)
        .unwrap_or(serde_json::Value::Object(Default::default()));
    Ok((kind.kind(), serde_json::to_string(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::{GoalId, RunId};

    fn ev(kind: EventKind) -> NewEvent {
        NewEvent {
            actor: Actor::Run(RunId::new("r1")),
            project: Some(ProjectId::new("p")),
            kind,
            evidence: Some(Evidence {
                head: Some("abc".into()),
                ..Evidence::default()
            }),
        }
    }

    #[test]
    fn every_historical_event_kind_loads_from_sqlite_without_wire_changes() {
        let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../dvb-core/tests/fixtures/event-kinds.json"
        ))
        .unwrap();
        let store = Store::memory().unwrap();
        {
            let conn = store.conn.lock().unwrap();
            for json in &fixtures {
                conn.execute(
                    concat!(
                        "INSERT INTO events (at, actor, project, kind, payload, evidence) ",
                        r#"VALUES ('1', '"engine"', NULL, ?1, ?2, NULL)"#
                    ),
                    params![
                        json["kind"].as_str().unwrap(),
                        serde_json::to_string(&json["payload"]).unwrap()
                    ],
                )
                .unwrap();
            }
        }
        let loaded = store.after(EventId(0)).unwrap();
        assert_eq!(loaded.len(), EventKind::ALL_KINDS.len());
        for (event, json) in loaded.iter().zip(fixtures) {
            assert_eq!(event.kind.kind(), json["kind"].as_str().unwrap());
            assert_eq!(serde_json::to_value(&event.kind).unwrap(), json);
        }
    }

    #[test]
    fn events_round_trip_in_order() {
        let store = Store::memory().unwrap();
        let written = store
            .append(
                Timestamp(5),
                vec![
                    ev(EventKind::GoalStated {
                        goal: GoalId::new("g"),
                        text: "x".into(),
                        from_issue: None,
                        issues: Vec::new(),
                        after: None,
                    }),
                    ev(EventKind::ProjectPaused {}),
                ],
            )
            .unwrap();
        assert_eq!(written[0].id, EventId(1));
        assert_eq!(written[1].id, EventId(2));
        assert_eq!(store.after(EventId(0)).unwrap(), written);
        assert_eq!(store.after(EventId(1)).unwrap(), written[1..]);
        assert_eq!(store.last_id().unwrap(), EventId(2));
    }

    #[test]
    fn filtered_reads_skip_run_telemetry_without_decoding_it() {
        use dvb_core::event::OutputItem;
        let store = Store::memory().unwrap();
        let run = RunId::new("r1");
        let output = |run: &RunId, item| {
            ev(EventKind::RunOutput {
                resource: None,
                run: run.clone(),
                item,
            })
        };
        let written = store
            .append(
                Timestamp(1),
                vec![
                    output(&run, OutputItem::Text("a line".into())),
                    output(&run, OutputItem::Note("a note".into())),
                    output(
                        &RunId::new("r2"),
                        OutputItem::ToolCall {
                            name: "Read".into(),
                        },
                    ),
                    ev(EventKind::RunUsage {
                        run: run.clone(),
                        uncached_in: 1,
                        cached_in: 0,
                        out: 1,
                        est_cost: 0.0,
                    }),
                    ev(EventKind::ProjectPaused {}),
                ],
            )
            .unwrap();
        // Rows these reads skip are never decoded: undecodable ones do not fail them.
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch(concat!(
                "INSERT INTO events (at, actor, project, kind, payload, evidence) VALUES ",
                r#"('1', '"engine"', NULL, 'run.output', '{"item":{"text":1},"run":"r2"}', NULL), "#,
                r#"('1', '"engine"', NULL, 'run.usage', '{}', NULL), "#,
                r#"('1', '"engine"', NULL, 'run.preflight', '{}', NULL), "#,
                r#"('1', '"engine"', NULL, 'task.state', '{}', NULL)"#
            ))
            .unwrap();
        assert!(store.after(EventId(0)).is_err());
        assert_eq!(
            store.select(EventId(0), Select::WithoutTelemetry).unwrap(),
            [written[1].clone(), written[4].clone()]
        );
        assert_eq!(
            store.select(EventId(0), Select::RunOutput(&run)).unwrap(),
            written[..2]
        );
        assert_eq!(
            store.select(EventId(1), Select::RunOutput(&run)).unwrap(),
            written[1..2]
        );
        assert_eq!(
            store
                .select(
                    EventId(0),
                    Select::Kinds(&["project.paused", "goal.stated"])
                )
                .unwrap(),
            written[4..]
        );
    }

    #[test]
    fn a_file_store_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dvb.sqlite");
        let first = Store::open(&path)
            .unwrap()
            .append(Timestamp(1), vec![ev(EventKind::ProjectResumed {})])
            .unwrap();
        let again = Store::open(&path).unwrap();
        assert_eq!(again.after(EventId(0)).unwrap(), first);
    }

    #[test]
    fn a_refinery_0_9_database_keeps_its_migration_receipt_and_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dvb.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../migrations/V1__events.sql"))
            .unwrap();
        // Receipt generated by refinery 0.9.2 from the deployed V1 bytes.
        conn.execute_batch(
            "CREATE TABLE refinery_schema_history (
                version INT4 PRIMARY KEY, name VARCHAR(255),
                applied_on VARCHAR(255), checksum VARCHAR(255));
             INSERT INTO refinery_schema_history VALUES
                (1, 'events', '2026-10-01T00:00:00Z', '7276623066132626700');
             INSERT INTO events VALUES
                (1, '5', '\"engine\"', 'p', 'project.paused', '{}', NULL);",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&path).unwrap();
        let events = store.after(EventId(0)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, EventId(1));
        assert_eq!(events[0].at, Timestamp(5));
        assert_eq!(events[0].actor, Actor::Engine);
        assert_eq!(events[0].project, Some(ProjectId::new("p")));
        assert_eq!(events[0].kind, EventKind::ProjectPaused {});
        assert_eq!(events[0].evidence, None);
        let appended = store
            .append(Timestamp(6), vec![ev(EventKind::ProjectResumed {})])
            .unwrap();
        assert_eq!(appended[0].id, EventId(2));
        drop(store);

        let conn = Connection::open(&path).unwrap();
        let receipt: (String, String) = conn
            .query_row(
                "SELECT applied_on, checksum FROM refinery_schema_history WHERE version = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            receipt,
            ("2026-10-01T00:00:00Z".into(), "7276623066132626700".into())
        );
        let payload: String = conn
            .query_row("SELECT payload FROM events WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payload, "{}");
    }

    #[test]
    fn role_event_pages_include_role_records_and_orchestrator_proposed_assignments() {
        use dvb_core::RoleId;
        use dvb_core::api::EventPageQuery;
        let store = Store::memory().unwrap();
        let mut role = dvb_core::roles::default_records(|r| RoleId::new(r.slug))
            .into_iter()
            .find(|r| r.slug == "backend")
            .unwrap();
        let added = EventKind::RoleAdded(role.clone());
        role.name = "Backend team".into();
        let changed = EventKind::RoleChanged {
            role: role.clone(),
            restored_from: None,
        };
        let hire = serde_json::from_value(serde_json::json!({
            "kind": "hire.added",
            "payload": {
                "id": "h",
                "role": "backend",
                "model": "fixture-model",
                "effort": "medium",
                "status": "active"
            }
        }))
        .unwrap();
        let orchestrator = serde_json::from_value(serde_json::json!({
            "kind": "run.started",
            "payload": {
                "run": "r1",
                "task": null,
                "role": "orchestrator",
                "hire": "o",
                "model": "fixture-model",
                "vendor": "codex",
                "effort": "medium",
                "reason": "plan",
                "reviews": null
            }
        }))
        .unwrap();
        let plan = serde_json::from_value(serde_json::json!({
            "kind": "plan.proposed",
            "payload": {
                "goal": "g",
                "tasks": [
                    {
                        "task": "t",
                        "title": "Build",
                        "role": "backend",
                        "review": "code",
                        "summary": "Implement"
                    },
                    {
                        "task": "t2",
                        "title": "UI",
                        "role": "frontend",
                        "review": "design",
                        "summary": "Design"
                    }
                ]
            }
        }))
        .unwrap();
        let unrelated = EventKind::RoleAdded(
            dvb_core::roles::default_records(|r| RoleId::new(r.slug))
                .into_iter()
                .find(|r| r.slug == "frontend")
                .unwrap(),
        );
        let written = store
            .append(
                Timestamp(1),
                vec![
                    ev(added),
                    ev(changed),
                    ev(hire),
                    ev(orchestrator),
                    ev(plan),
                    ev(EventKind::RoleRemoved(role)),
                    ev(unrelated),
                ],
            )
            .unwrap();
        let page = store
            .event_page(&EventPageQuery {
                role: Some(RoleId::new("backend")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            page.events,
            vec![
                written[5].clone(),
                written[4].clone(),
                written[2].clone(),
                written[1].clone(),
                written[0].clone(),
            ]
        );
        let task_page = store
            .event_page(&EventPageQuery {
                task: Some(dvb_core::TaskId::new("t")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(task_page.events, vec![written[4].clone()]);
        let second = store
            .event_page(&EventPageQuery {
                task: Some(dvb_core::TaskId::new("t2")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(second.events, vec![written[4].clone()]);
        let reassignment = EventKind::PlanProposed {
            goal: GoalId::new("g"),
            tasks: vec![dvb_core::event::PlannedTask {
                after: vec![],
                task: dvb_core::TaskId::new("t2"),
                title: "Reassigned".into(),
                role: RoleId::new("backend"),
                review: dvb_core::roles::ReviewKind::Code,
                summary: "Build".into(),
                routine: false,
            }],
        };
        store.append(Timestamp(2), vec![ev(reassignment)]).unwrap();
        let frontend = store
            .event_page(&EventPageQuery {
                role: Some(RoleId::new("frontend")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            frontend.events,
            vec![written[6].clone(), written[4].clone()],
            "plans retain every original role even after task reassignment"
        );
    }

    #[test]
    fn failed_cache_advance_rolls_back_counts_indexes_and_cursor_before_retry() {
        use dvb_core::api::EventPageQuery;
        use dvb_core::event::OutputItem;
        use std::error::Error;
        let store = Store::memory().unwrap();
        let written = store
            .append(
                Timestamp(1),
                vec![
                    ev(EventKind::TaskDone {
                        task: dvb_core::TaskId::new("t"),
                        reason: None,
                    }),
                    ev(EventKind::RunOutput {
                        resource: None,
                        run: RunId::new("r"),
                        item: OutputItem::Text("kept".into()),
                    }),
                ],
            )
            .unwrap();
        let id = i64::try_from(written[1].id.0).unwrap();
        let original: String = store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT payload FROM events WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE events SET payload = 'broken' WHERE id = ?1", [id])
            .unwrap();
        let query = EventPageQuery {
            summary: Some(true),
            ..Default::default()
        };
        let error = store.event_page(&query).unwrap_err();
        assert!(matches!(error, StoreError::Json(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        {
            let cache = store.read_cache.lock().unwrap();
            assert_eq!(cache.cursor, 0);
            assert_eq!(cache.visited, 0);
            assert!(cache.activity.is_empty());
            drop(cache);
        }
        for table in ["dvb_read_events", "dvb_read_event_tasks", "dvb_read_latest"] {
            let count: i64 = store
                .conn
                .lock()
                .unwrap()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE events SET payload = ?1 WHERE id = ?2",
                params![original, id],
            )
            .unwrap();
        let page = store.event_page(&query).unwrap();
        assert_eq!(page.activity[0].fresh, 1);
        assert_eq!(page.events, written.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(store.read_cache.lock().unwrap().visited, 2);
        assert_eq!(store.event_page(&query).unwrap().activity[0].fresh, 1);
    }

    #[test]
    fn read_cache_visits_large_history_once_then_only_new_rows() {
        use dvb_core::api::EventPageQuery;
        use dvb_core::event::OutputItem;
        let store = Store::memory().unwrap();
        let mut history: Vec<_> = (0..450).map(|i| ev(serde_json::from_value(serde_json::json!({
            "kind":"checks.observed", "payload":{"task":format!("t{i}"),"head":"h","status":"success","names":[]}
        })).unwrap())).collect();
        history.extend((0..92_000).map(|_| {
            ev(EventKind::RunOutput {
                resource: None,
                run: RunId::new("r"),
                item: OutputItem::Text("synthetic output".repeat(25)),
            })
        }));
        store.append(Timestamp(1), history).unwrap();
        let summary = EventPageQuery {
            summary: Some(true),
            limit: Some(1),
            ..Default::default()
        };
        let first = store.event_page(&summary).unwrap();
        assert_eq!(first.cursor, EventId(92_450));
        assert_eq!(first.activity[0].latest, first.cursor);
        assert_eq!(first.activity[0].fresh, 0);
        assert_eq!(store.read_cache.lock().unwrap().visited, 92_450);
        let mut query = EventPageQuery {
            latest: Some(true),
            kinds: Some("checks.observed".into()),
            limit: Some(200),
            ..Default::default()
        };
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let page = store.event_page(&query).unwrap();
            for e in page.events {
                assert!(ids.insert(e.id));
            }
            query.before = page.before;
            if query.before.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), 450);
        for _ in 0..5 {
            store.event_page(&summary).unwrap();
        }
        assert_eq!(store.read_cache.lock().unwrap().visited, 92_450);
        store
            .append(
                Timestamp(2),
                vec![ev(EventKind::TaskDone {
                    task: dvb_core::TaskId::new("t"),
                    reason: None,
                })],
            )
            .unwrap();
        let updated = store.event_page(&summary).unwrap();
        assert_eq!(updated.activity[0].fresh, 1);
        assert_eq!(store.read_cache.lock().unwrap().visited, 92_451);
        store
            .append(Timestamp(3), vec![ev(EventKind::ProjectSeen {})])
            .unwrap();
        assert_eq!(store.event_page(&summary).unwrap().activity[0].fresh, 0);
        assert_eq!(store.read_cache.lock().unwrap().visited, 92_452);
        let conn = store.conn.lock().unwrap();
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM events WHERE id > ?1 AND id <= ?2 ORDER BY id",
                params![92_450, 92_452],
                |r| r.get(3),
            )
            .unwrap();
        drop(conn);
        assert!(
            plan.contains("SEARCH events USING INTEGER PRIMARY KEY"),
            "{plan}"
        );
    }
}
