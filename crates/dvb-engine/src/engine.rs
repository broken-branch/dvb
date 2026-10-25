//! The engine's shared state, handed to every handler as `State<AppState>`: the
//! store, the projections, the clock, the launcher and the forge.

use crate::keyed_lock::{KeyedGuard, KeyedLocks};
use crate::proj::{Projections, Task};
use crate::units::{ProcessLauncher, SystemdLauncher};
use dvb_core::event::EndReason;
use dvb_core::event::RefusedCommand;
use dvb_core::forge::Forge;
use dvb_core::launcher::{LaunchError, Launcher, UnitId, UnitState};
use dvb_core::state::Effect;
use dvb_core::{
    Actor, Clock, Command, Context, Event, EventKind, Evidence, ModelPool, ProjectId, Refusal,
    RefusalCode, RunAssignment, RunId, SystemClock, TaskId, TaskState, Vendor, decide,
};
use dvb_forge::FakeForge;
use dvb_store::{NewEvent, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::broadcast;

/// Where an engine keeps its files. An install and dev mode use the
/// same layout under a different root.
#[derive(Debug, Clone)]
pub struct Paths {
    pub state: PathBuf,
    pub mirrors: PathBuf,
    pub work: PathBuf,
    pub engine_sock: PathBuf,
    pub run_sock: PathBuf,
}

impl Paths {
    pub fn install() -> Self {
        let root = Path::new("/var/lib/dvb");
        Self {
            state: root.join("state"),
            mirrors: root.join("mirrors"),
            work: root.join("work"),
            engine_sock: dvb_core::io::ENGINE_SOCKET.into(),
            run_sock: "/run/dvb/run.sock".into(),
        }
    }

    /// `dvb engine --dev <dir>`: everything under `<dir>`.
    pub fn dev(dir: &Path) -> Self {
        Self {
            state: dir.join("state"),
            mirrors: dir.join("mirrors"),
            work: dir.join("work"),
            engine_sock: dir.join("engine.sock"),
            run_sock: dir.join("run.sock"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Paths,
    /// Dev mode: runs are child processes (`Launcher::Process`), only the Fake
    /// provider and Fake forge are allowed.
    pub dev: bool,
    /// The Fake provider and Fake forge (dev mode, and the e2e job on an install).
    pub fake: bool,
    /// The uid the owner API accepts besides the engine's own.
    pub owner_uid: u32,
    /// The `dvb` binary dev-mode runs execute.
    pub dvb_bin: PathBuf,
    /// The engine's own `RUSTUP_HOME`, passed on to runs; without one, a run's
    /// unit setting or else its project's cache holds the toolchains.
    pub rustup_home: Option<String>,
    /// The run units' version stamp install.sh writes (`launch::UNITS_VERSION`);
    /// none in dev mode, which has no units.
    pub units_version: Option<PathBuf>,
    /// systemctl for live slice settings; absent for an engine without installed units.
    pub systemctl: Option<PathBuf>,
    /// Linux RAM information for the total/half RAM metadata on `/settings`;
    /// the engine's memory hold reads `/proc/meminfo` itself.
    pub meminfo: PathBuf,
}

impl Config {
    pub fn dev(dir: &Path, dvb_bin: PathBuf) -> Self {
        Self {
            paths: Paths::dev(dir),
            dev: true,
            fake: true,
            owner_uid: own_uid(),
            dvb_bin,
            rustup_home: None,
            units_version: None,
            systemctl: None,
            meminfo: "/proc/meminfo".into(),
        }
    }

    /// An install: the owner's uid is recorded at install in `state/owner`. The
    /// Fake provider and forge only when `DVB_FAKE=1` is set on the service (the
    /// e2e job). Otherwise the engine loads the configured GitHub forge and
    /// launches the selected provider CLI.
    pub fn install() -> Result<Self, InternalError> {
        let paths = Paths::install();
        let owner = paths.state.join("owner");
        let owner_uid = std::fs::read_to_string(&owner)
            .map_err(|source| InternalError::Io {
                context: owner.display().to_string(),
                source,
            })?
            .trim()
            .parse()
            .map_err(|source| InternalError::OwnerUid {
                path: owner,
                source,
            })?;
        Ok(Self {
            units_version: Some(paths.state.join("units-version")),
            paths,
            dev: false,
            fake: std::env::var("DVB_FAKE").is_ok_and(|v| v == "1"),
            owner_uid,
            dvb_bin: "/opt/dvb/current/dvb".into(),
            rustup_home: std::env::var("RUSTUP_HOME").ok(),
            systemctl: Some("systemctl".into()),
            meminfo: "/proc/meminfo".into(),
        })
    }
}

/// This process's uid.
pub use dvb_core::io::own_uid;

pub type HostLoadReading = Arc<dyn Fn() -> Option<(f64, usize)> + Send + Sync>;

pub type ResourceReading = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Dependencies and resource readings supplied at engine construction.
/// Omitted dependencies use the configured installed/dev implementations.
#[derive(Default)]
pub struct EngineOptions {
    pub clock: Option<Arc<dyn Clock>>,
    pub forge: Option<Arc<dyn Forge>>,
    pub launcher: Option<Arc<dyn Launcher>>,
    pub disk_reading: Option<ResourceReading>,
    pub memory_reading: Option<ResourceReading>,
    pub host_load: Option<HostLoadReading>,
    pub pool: Option<ModelPool>,
}

/// Events accepted atomically with a command, and questions/refusals conditional on its outcome.
pub struct Decision<'a> {
    pub command: &'a str,
    pub cmd: Command,
    pub pre: Vec<EventKind>,
    pub waiting: Option<EventKind>,
    pub question: Option<EventKind>,
    /// Mark a successful stop as belonging to the update, atomically.
    pub update_stop: bool,
}

impl<'a> Decision<'a> {
    pub fn new(command: &'a str, cmd: Command) -> Self {
        Self {
            command,
            cmd,
            pre: Vec::new(),
            waiting: None,
            question: None,
            update_stop: false,
        }
    }
}

/// A run recorded as started whose unit is not started yet: waiting for its
/// vendor to reopen, or held while its project is paused.
#[derive(Debug, Clone)]
pub struct Queued {
    /// The run whose directory holds the workspace the run resumes in.
    pub from: Option<RunId>,
    /// Held by a pause; unpausing launches it.
    pub held: bool,
}

/// Why a handler did not succeed: a coded refusal, or an internal error.
#[derive(Debug, thiserror::Error)]
pub enum Fail {
    #[error("{0}")]
    Refused(#[from] Refusal),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("internal: {0}")]
    Internal(#[from] InternalError),
}

/// Internal failures retain their original source, including forge rate limits.
#[derive(Debug, thiserror::Error)]
pub enum InternalError {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("{0}")]
    ParseInt(#[from] std::num::ParseIntError),
    #[error("{0}")]
    Signature(#[from] minisign_verify::Error),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Store(#[from] dvb_store::StoreError),
    #[error("{0}")]
    Forge(#[from] dvb_core::forge::ForgeError),
    #[error("{0}")]
    Git(#[from] dvb_core::git::GitError),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Launch(#[from] LaunchError),
    #[error("{0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("{0}")]
    Handover(#[from] crate::update::handover::HandoverError),
    #[error("{0}")]
    Publication(#[from] crate::publish::PublicationError),
    #[error("{0}")]
    Identity(#[from] crate::publish::IdentityError),
    #[error("{path}: {source}")]
    OwnerUid {
        path: PathBuf,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<InternalError>,
    },
}

impl InternalError {
    pub fn context(context: impl Into<String>, source: impl Into<InternalError>) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(source.into()),
        }
    }
}
impl From<String> for InternalError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}
impl From<&str> for InternalError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}
impl From<std::io::Error> for InternalError {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            context: "I/O".into(),
            source,
        }
    }
}
impl From<dvb_store::StoreError> for Fail {
    fn from(source: dvb_store::StoreError) -> Self {
        Self::internal(source)
    }
}
impl Fail {
    pub fn internal(error: impl Into<InternalError>) -> Self {
        Self::Internal(error.into())
    }
    pub fn context(context: impl Into<String>, source: impl Into<InternalError>) -> Self {
        Self::internal(InternalError::context(context, source))
    }
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::internal(InternalError::Io {
            context: context.into(),
            source,
        })
    }
}

pub struct Engine {
    pub cfg: Config,
    pub store: Store,
    pub clock: Arc<dyn Clock>,
    pub units: Arc<dyn Launcher>,
    /// The current forge; `None` until one is configured.
    pub forge: Mutex<Option<Arc<dyn Forge>>>,
    pub pool: ModelPool,
    /// Vendor windows and the usage-limit pause.
    pub vendors: crate::vendors::Vendors,
    pub(crate) publication: Mutex<crate::publish::PublicationSettings>,
    /// What the update protocol tells the rest of the engine.
    pub update: crate::update::State,
    pub(crate) github_setup: tokio::sync::Mutex<()>,
    pub(crate) github_wait: Arc<dvb_forge::Wait>,
    pub(crate) github_setup_client: Mutex<Option<(String, dvb_forge::github_setup::Client)>>,
    pub(crate) mirror_retries: tokio::sync::Mutex<crate::publish::mirror::Retries>,
    proj: Mutex<Projections>,
    feed: broadcast::Sender<Event>,
    /// Each task's lock: an effect is checked, sent and recorded under it.
    locks: KeyedLocks<TaskId>,
    pub(crate) starting: tokio::sync::Mutex<()>,
    /// Owner CI choices serialize with check observations and final merge effects.
    project_checks: KeyedLocks<ProjectId>,
    /// Branch refreshes share refs even when their callers hold different task locks.
    mirror_fetches: KeyedLocks<PathBuf>,
    /// The tasks whose dispatcher is running (`dispatch::kick`).
    pub(crate) dispatching: Mutex<BTreeMap<TaskId, crate::dispatch::Running>>,
    /// Runs recorded as started whose units are not started yet.
    pub(crate) queued: Mutex<BTreeMap<RunId, Queued>>,
    /// A failed `dispatch::start_waiting` pass has its next pass scheduled.
    pub(crate) waiting_again: std::sync::atomic::AtomicBool,
    disk_reading: ResourceReading,
    memory_reading: ResourceReading,
    host_load: HostLoadReading,
    /// Woken when any run ends, so resource-held runs across projects look again.
    pub disk_freed: tokio::sync::Notify,
    pub(crate) only_changed: tokio::sync::Notify,
    /// The engine's own `Arc`, once the dispatcher has been used, to wake
    /// dispatchers from a decision (set lazily, so a fresh engine is
    /// unshared).
    me: std::sync::OnceLock<Weak<Engine>>,
}

pub type AppState = Arc<Engine>;

impl Engine {
    /// Bytes free on the filesystem holding the work directory; `None` when it
    /// cannot be read (nothing is held for that).
    pub fn free_bytes(&self) -> Option<u64> {
        (self.disk_reading)()
    }

    /// Linux MemAvailable, or the supplied resource reading.
    pub fn available_memory_bytes(&self) -> Option<u64> {
        (self.memory_reading)()
    }

    /// Unreadable host readings do not hold runs, as with memory and disk.
    pub fn host_load_reading(&self) -> Option<(f64, usize)> {
        (self.host_load)()
    }

    /// Opens the store, rebuilds the projections and records `engine.started`.
    pub fn open(cfg: Config, options: EngineOptions) -> Result<AppState, Fail> {
        let EngineOptions {
            clock,
            forge,
            launcher,
            disk_reading,
            memory_reading,
            host_load,
            pool,
        } = options;
        let clock = clock.unwrap_or_else(|| Arc::new(SystemClock));
        if cfg.dev {
            for dir in [&cfg.paths.state, &cfg.paths.mirrors, &cfg.paths.work] {
                std::fs::create_dir_all(dir).map_err(|e| Fail::io(dir.display().to_string(), e))?;
            }
        }
        // Mirrors are already engine-writable on installs predating shared caches.
        // Only this subtree grants workers write access; state stays private.
        let caches = cfg.paths.mirrors.join(".cache");
        match std::fs::create_dir(&caches) {
            Ok(()) => crate::launch::share_dir(&caches)
                .map_err(|e| Fail::io(caches.display().to_string(), e))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Fail::io(caches.display().to_string(), e)),
        }
        // Neither stops the engine: a mirror git cannot read fails at its next fetch.
        if let Err(e) = crate::publish::clean_mirrors(&cfg.paths.mirrors) {
            eprintln!(
                "dvb engine: clean the mirrors in {}: {e}",
                cfg.paths.mirrors.display()
            );
        }
        let store = Store::open(&cfg.paths.state.join("dvb.sqlite"))?;
        let units: Arc<dyn Launcher> = launcher.unwrap_or_else(|| {
            if cfg.dev {
                Arc::new(ProcessLauncher::new(
                    cfg.dvb_bin.clone(),
                    cfg.paths.work.clone(),
                ))
            } else {
                Arc::new(SystemdLauncher::default())
            }
        });
        let fake_forge = cfg
            .fake
            .then(|| {
                let fake = FakeForge::local_git();
                // Dev mode: a test seeds a repo's issues as it does Fake scripts (`fake_script`).
                Arc::new(match cfg.paths.state.parent().filter(|_| cfg.dev) {
                    Some(root) => fake.with_issue_files(root.join("fake").join("issues")),
                    None => fake,
                })
            })
            .filter(|_| forge.is_none());
        let publication =
            crate::publish::PublicationSettings::load(&cfg.paths.state).map_err(Fail::internal)?;
        publication.validate().map_err(Fail::internal)?;
        let github_wait = Arc::new(dvb_forge::Wait::new(clock.clone()));
        let forge: Option<Arc<dyn Forge>> = if let Some(given) = forge {
            Some(given)
        } else if let Some(fake) = &fake_forge {
            Some(fake.clone())
        } else if cfg.paths.state.join("github-app.json").exists() {
            if publication.owner_name.is_empty() || publication.owner_email.is_empty() {
                return Err(Fail::internal(
                    "GitHub publication requires owner_name and owner_email in publication settings",
                ));
            }
            Some(Arc::new(
                dvb_forge::GitHub::from_state(&cfg.paths.state, github_wait.clone())
                    .map_err(Fail::internal)?,
            ))
        } else {
            None
        };
        let pool = pool.unwrap_or_else(|| crate::usage::prices::model_pool(cfg.fake));
        let (feed, _) = broadcast::channel(1024);
        let work = cfg.paths.work.clone();
        let disk_reading = disk_reading.unwrap_or_else(|| {
            if cfg.fake || cfg.dev {
                Arc::new(|| Some(u64::MAX))
            } else {
                Arc::new(move || {
                    let s = rustix::fs::statvfs(&work).ok()?;
                    Some(s.f_bavail.saturating_mul(s.f_frsize))
                })
            }
        });
        let controlled_memory = cfg.fake || cfg.dev;
        let memory_reading = memory_reading.unwrap_or_else(|| {
            Arc::new(move || {
                if controlled_memory {
                    Some(u64::MAX)
                } else {
                    mem_available(&std::fs::read_to_string("/proc/meminfo").ok()?)
                }
            })
        });
        // Folding one event at a time avoids leaving the decoded history's
        // freed allocations resident in the allocator.
        let mut projections = Projections::default();
        store.each(dvb_core::EventId(0), dvb_store::Select::All, |e| {
            projections.apply(&e);
        })?;
        let engine = Arc::new(Engine {
            memory_reading,
            disk_reading,
            host_load: host_load.unwrap_or_else(|| {
                if cfg.fake || cfg.dev {
                    Arc::new(|| Some((0.0, 1)))
                } else {
                    Arc::new(host_load_reading)
                }
            }),
            cfg,
            store,
            clock,
            units,
            forge: Mutex::new(forge),
            pool,
            vendors: crate::vendors::Vendors::default(),
            publication: Mutex::new(publication),
            update: crate::update::State::default(),
            github_setup: tokio::sync::Mutex::new(()),
            github_wait,
            github_setup_client: Mutex::new(None),
            mirror_retries: tokio::sync::Mutex::new(crate::publish::mirror::Retries::default()),
            proj: Mutex::new(projections),
            feed,
            locks: KeyedLocks::default(),
            starting: tokio::sync::Mutex::new(()),
            project_checks: KeyedLocks::default(),
            mirror_fetches: KeyedLocks::default(),
            dispatching: Mutex::new(BTreeMap::new()),
            queued: Mutex::new(BTreeMap::new()),
            waiting_again: std::sync::atomic::AtomicBool::new(false),
            disk_freed: tokio::sync::Notify::new(),
            only_changed: tokio::sync::Notify::new(),
            me: std::sync::OnceLock::new(),
        });
        engine.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::EngineStarted {
                version: dvb_core::VERSION.into(),
            }])
        })?;
        engine.expire_only(None)?;
        Ok(engine)
    }

    /// Quiesces the store for an update. The flag is set while
    /// holding the lock every append takes for its check and its commit, so a
    /// writer already past the check has committed before this returns, and every
    /// later one is refused.
    pub fn quiesce(&self) {
        let _writers = self.proj();
        self.update.quiesce();
    }

    pub fn forge(&self) -> Option<Arc<dyn Forge>> {
        self.forge
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }
    pub fn publication(&self) -> crate::publish::PublicationSettings {
        self.publication
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }
    pub fn launcher(&self) -> &dyn Launcher {
        self.units.as_ref()
    }

    /// The projections, read under the lock.
    pub fn proj(&self) -> MutexGuard<'_, Projections> {
        self.proj.lock().unwrap_or_else(dvb_core::sync::recover)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.feed.subscribe()
    }

    /// A new ULID, from the engine's clock.
    pub fn new_id(&self) -> String {
        self.new_id_at(self.clock.now())
    }

    pub(crate) fn new_id_at(&self, now: dvb_core::Timestamp) -> String {
        let ms = u64::try_from(now.0).unwrap_or(0);
        ulid::Ulid::from_parts(ms, rand::random::<u128>()).to_string()
    }

    fn commit(
        &self,
        proj: &mut Projections,
        project: Option<&ProjectId>,
        actor: Actor,
        mut kinds: Vec<EventKind>,
        now: dvb_core::Timestamp,
    ) -> Result<Vec<Event>, Fail> {
        // Quiesced for an update: nothing is appended from here to the exec.
        if self.update.quiesced() {
            return Err(Fail::Refused(Refusal::new(
                RefusalCode::EngineUpdating,
                "dvb is updating",
            )));
        }
        // Resolve attention in the same transaction as completion or an accepted retry.
        let resolved: std::collections::BTreeSet<_> = kinds
            .iter()
            .filter_map(|kind| {
                if let EventKind::TaskStateChanged {
                    task,
                    to: TaskState::Done,
                    ..
                }
                | EventKind::TaskRetryTextSet { task, .. } = kind
                {
                    Some(task.clone())
                } else {
                    None
                }
            })
            .collect();
        let redirected: std::collections::BTreeSet<_> = kinds
            .iter()
            .filter_map(|kind| {
                if let EventKind::GoalStated { goal, .. } | EventKind::GoalDropped { goal } = kind {
                    proj.goals.contains_key(goal).then_some(goal)
                } else {
                    None
                }
            })
            .collect();
        let closed: Vec<_> = proj
            .questions
            .values()
            .filter(|q| q.view.open)
            .filter_map(|q| {
                let finished = q
                    .failure_task
                    .as_ref()
                    .filter(|task| resolved.contains(*task))
                    .or_else(|| {
                        q.view
                            .options
                            .iter()
                            .filter_map(|o| o.retries_task.as_ref())
                            .find(|task| resolved.contains(*task))
                    });
                let stopped = q.failure_task.as_ref().filter(|task| {
                    q.view.leaves_task_stopped()
                        && proj.tasks.get(*task).is_some_and(|t| {
                            redirected.contains(&t.view.goal)
                                && matches!(t.view.state, TaskState::Stopped { .. })
                        })
                });
                finished.or(stopped).map(|task| EventKind::QuestionClosed {
                    question: q.view.id.clone(),
                    task: task.clone(),
                })
            })
            .collect();
        kinds.extend(closed);
        // Crash injection: the engine dies before this batch is written.
        #[cfg(feature = "failpoints")]
        for k in &kinds {
            fail::fail_point!(&format!("append.{}", k.kind()));
        }
        let batch = kinds
            .into_iter()
            .map(move |kind| NewEvent {
                actor: actor.clone(),
                project: project.cloned(),
                evidence: evidence_of(&kind),
                kind,
            })
            .collect();
        let held_goals: Vec<_> = proj
            .goals
            .values()
            .filter(|g| {
                (g.planning_held || proj.waits_for(g).is_some()) && Some(&g.project) == project
            })
            .map(|g| g.id.clone())
            .collect();
        let max_runs = proj.max_runs();
        let events = self.store.append(now, batch)?;
        for e in &events {
            proj.apply(e);
            if matches!(
                e.kind,
                EventKind::OnlyStarted(_) | EventKind::OnlyEnded { .. }
            ) {
                self.only_changed.notify_waiters();
            }
            // No subscriber is fine.
            let _ = self.feed.send(e.clone());
        }
        crate::dispatch::drop_finished_runs(proj, &self.cfg.paths.work, &events);
        // Recovery follows the durable release, including owner drops and
        // terminal effect failures after the worker's run-end recovery pass.
        // Spawn only; this fold owns the projection lock. Admission
        // rechecks eligibility under that lock and consumes one release.
        if held_goals
            .iter()
            .any(|id| proj.goals.get(id).is_some_and(|g| proj.plan_wake_ready(g)))
            && tokio::runtime::Handle::try_current().is_ok()
            && let Some(e) = self.me.get().and_then(Weak::upgrade)
            && let Some(project) = project.cloned()
        {
            crate::spawn("recover released planning", async move {
                crate::wake::wake_if_missed(&e, &project);
                Ok(())
            });
        }
        if proj.max_runs() > max_runs
            && tokio::runtime::Handle::try_current().is_ok()
            && let Some(e) = self.me.get().and_then(Weak::upgrade)
        {
            // Admission rechecks the new limit after this fold releases its lock.
            crate::spawn("start work after run limit increase", async move {
                crate::dispatch::kick_all(&e, None);
                crate::dispatch::start_waiting(e).await
            });
        }
        Ok(events)
    }

    /// Rewrites notes from committed events under the engine writer lock.
    pub fn sync_notes(&self, project: &ProjectId) -> Result<(), Fail> {
        let _writer = self.proj();
        let events = self
            .store
            .select(dvb_core::EventId(0), dvb_store::Select::WithoutTelemetry)?;
        let text = crate::prompt::notes_text(&events, project);
        crate::prompt::write_notes(&self.cfg.paths.state, project, &text)
            .map_err(|err| Fail::io("NOTES.md", err))
    }

    /// A timed decision and its durable timestamp share one Clock reading.
    pub fn append_checked(
        &self,
        project: Option<&ProjectId>,
        actor: Actor,
        build: impl FnOnce(&Projections, dvb_core::Timestamp) -> Result<Vec<EventKind>, Fail>,
    ) -> Result<Vec<Event>, Fail> {
        let mut proj = self.proj();
        let now = self.clock.now();
        let kinds = build(&proj, now)?;
        if kinds.is_empty() {
            return Ok(vec![]);
        }
        let result = self.commit(&mut proj, project, actor, kinds, now);
        drop(proj);
        result
    }

    /// Record a refused validation once, after its atomic check releases the writer lock.
    pub fn refuse(
        &self,
        project: Option<&ProjectId>,
        actor: Actor,
        command: &str,
        error: impl Into<Fail>,
    ) -> Fail {
        match error.into() {
            Fail::Refused(r) => {
                let kind = EventKind::CommandRefused(RefusedCommand {
                    command: command.into(),
                    refusal: r.clone().into(),
                });
                match self.append_checked(project, actor, |_, _| Ok(vec![kind])) {
                    Ok(_) => Fail::Refused(r),
                    Err(error) => error,
                }
            }
            other @ (Fail::NotFound(_) | Fail::Internal(_)) => other,
        }
    }

    /// Fetch all branches and resolve the requested head under one mirror lock.
    /// Wakes, admission and different task effects can otherwise race Git's
    /// old-head check while updating the same refs. Distinct mirrors stay independent.
    pub async fn fetch(
        &self,
        forge: &dyn Forge,
        repo: &dvb_core::forge::Repo,
        mirror: &Path,
        branch: &str,
    ) -> Result<String, dvb_core::forge::ForgeError> {
        let _fetch = self.lock_mirror(mirror).await;
        forge.fetch(repo, mirror, branch).await
    }

    /// Planning rechecks eligibility after waiting for the same fetch lock.
    pub(crate) async fn lock_mirror(&self, mirror: &Path) -> KeyedGuard<PathBuf> {
        self.mirror_fetches.lock(mirror.to_path_buf()).await
    }

    /// The task's lock. Effects take it to check the state, send and
    /// record the result; a stop takes it after recording itself, so it returns
    /// only once an effect in flight is recorded. Always taken before the
    /// projections' lock, never while holding it.
    pub async fn lock(&self, task: &TaskId) -> KeyedGuard<TaskId> {
        self.locks.lock(task.clone()).await
    }

    /// Keep the saved CI choice fixed from observation through the merge reply.
    /// Effects take the task lock first; owner writes take only this lock.
    /// Take it before reading projections, never while holding their mutex.
    pub(crate) async fn lock_project_checks(&self, project: &ProjectId) -> KeyedGuard<ProjectId> {
        self.project_checks.lock(project.clone()).await
    }

    /// Auto hiring and its start decision share the writer lock. A competing
    /// same-model hire is reused; otherwise every active hire is replaced.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "The projection guard is consumed by decide to keep hire admission and its decision atomic"
    )]
    pub(crate) fn admit_task_hiring(
        &self,
        task: &TaskId,
        actor: Actor,
        mut assignment: RunAssignment,
        hire: Option<dvb_core::roles::HireRecord>,
        waiting: Option<EventKind>,
    ) -> Result<RunAssignment, Fail> {
        let proj = self.proj();
        let t = proj
            .tasks
            .get(task)
            .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        let project = &proj.projects[&t.view.project];
        let mut pre = Vec::new();
        if let Some(hire) = hire {
            if let Some(existing) = project.hire(&hire.role, Some(&hire.model)) {
                assignment.hire = existing.id.clone();
            } else {
                pre.extend(
                    crate::proj::hires::retiring_hires(project, &hire.role, hire.routine)
                        .into_iter()
                        .map(EventKind::HireRetired),
                );
                pre.push(EventKind::HireAdded(hire));
            }
        }
        self.decide(
            proj,
            task,
            actor,
            Decision {
                pre,
                waiting,
                ..Decision::new("task start", Command::Start(assignment.clone()))
            },
            |_| Ok(()),
        )?;
        Ok(assignment)
    }

    /// Decide under the writer lock, with `waiting` recorded with a refusal for the cap on
    /// concurrent runs, before the lock is released, and the owner's `question`
    /// recorded with a decision that stops the task (the review-round cap).
    pub fn decide(
        &self,
        mut proj: MutexGuard<'_, Projections>,
        task: &TaskId,
        actor: Actor,
        decision: Decision<'_>,
        allowed: impl FnOnce(&Projections) -> Result<(), Refusal>,
    ) -> Result<TaskState, Fail> {
        let Decision {
            command,
            cmd,
            pre,
            waiting,
            question,
            update_stop,
        } = decision;
        if let Err(r) = allowed(&proj) {
            let project = proj.tasks.get(task).map(|t| t.view.project.clone());
            let refused = EventKind::CommandRefused(RefusedCommand {
                command: command.into(),
                refusal: r.clone().into(),
            });
            self.commit(
                &mut proj,
                project.as_ref(),
                actor,
                vec![refused],
                self.clock.now(),
            )?;
            return Err(Fail::Refused(r));
        }
        let t = proj
            .tasks
            .get(task)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        let project = t.view.project.clone();
        let ctx = self.context(&proj, &t, &cmd, self.clock.now())?;
        match decide(&t.view.state, cmd, &ctx) {
            Ok(mut kinds) => {
                if update_stop {
                    for kind in &mut kinds {
                        if let EventKind::TaskStopped { update, .. } = kind {
                            *update = true;
                        }
                    }
                }
                let stops = kinds
                    .iter()
                    .any(|k| matches!(k, EventKind::TaskStopped { .. }));
                let mut all = pre;
                all.extend(kinds);
                all.extend(question.filter(|_| stops));
                if !all.is_empty() {
                    self.commit(&mut proj, Some(&project), actor, all, ctx.now)?;
                }
                let now = proj
                    .tasks
                    .get(task)
                    .map_or(TaskState::Planned, |t| t.view.state.clone());
                drop(proj);
                if now != t.view.state {
                    self.kick(task);
                    if matches!(now, TaskState::Failed { .. } | TaskState::Stopped { .. })
                        && let Some(e) = self.me.get().and_then(Weak::upgrade)
                    {
                        crate::wake::wake_if_missed(&e, &project);
                    }
                }
                Ok(now)
            }
            Err(r) => {
                let capped = RefusalCode::CapReached {
                    which: dvb_core::Cap::ConcurrentRuns,
                };
                let refused = EventKind::CommandRefused(RefusedCommand {
                    command: command.into(),
                    refusal: r.clone().into(),
                });
                let mut kinds = vec![refused];
                kinds.extend(waiting.filter(|_| {
                    r.code == capped || matches!(r.code, RefusalCode::DependencyNotMerged { .. })
                }));
                self.commit(&mut proj, Some(&project), actor, kinds, ctx.now)?;
                Err(Fail::Refused(r))
            }
        }
    }

    /// A terminal effect failure and its owner question, recorded together.
    /// If the effect no longer applies (for example, the owner stopped the
    /// task while checks were read), neither the failure nor question is saved.
    pub(crate) fn fail_and_ask(
        &self,
        task: &TaskId,
        effect: Effect,
        detail: String,
        question: EventKind,
    ) -> Result<(), Fail> {
        let project = self
            .proj()
            .tasks
            .get(task)
            .map(|t| t.view.project.clone())
            .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        self.append_checked(Some(&project), Actor::Engine, |proj, now| {
            let t = proj
                .tasks
                .get(task)
                .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
            let cmd = Command::EffectFailed {
                rate_limit: None,
                effect,
                transient: false,
                detail,
            };
            let ctx = self.context(proj, t, &cmd, now)?;
            let mut kinds = decide(&t.view.state, cmd, &ctx)?;
            if kinds.iter().any(|k| {
                matches!(
                    k,
                    EventKind::TaskStateChanged {
                        to: TaskState::Failed { .. },
                        ..
                    }
                )
            }) {
                kinds.push(question);
            }
            Ok(kinds)
        })?;
        Ok(())
    }

    /// Empty collection is a terminal task outcome, not a failed Push effect.
    /// Save its retry question with the decision; repeated dispatch/recovery
    /// cannot create another question once the task has ended.
    pub(crate) fn no_changes(&self, task: &TaskId) -> Result<(), Fail> {
        let project = self
            .proj()
            .tasks
            .get(task)
            .map(|t| t.view.project.clone())
            .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        self.append_checked(Some(&project), Actor::Engine, |proj, now| {
            let t = &proj.tasks[task];
            if t.view.state == TaskState::Publishing
                && t.view.outcome != dvb_core::run_verbs::BuilderOutcome::Complete
            {
                let cmd = Command::Stop(Some(
                    t.view
                        .outcome
                        .reason()
                        .unwrap_or("Incomplete work")
                        .to_owned(),
                ));
                let ctx = self.context(proj, t, &cmd, now)?;
                return Ok(decide(&t.view.state, cmd, &ctx)?);
            }
            let cmd = Command::NoChanges;
            let ctx = self.context(proj, t, &cmd, now)?;
            let mut kinds = decide(&t.view.state, cmd, &ctx)?;
            if kinds.is_empty() {
                return Ok(kinds);
            }
            let goal = proj
                .goals
                .get(&t.view.goal)
                .ok_or_else(|| Fail::NotFound(format!("goal {}", t.view.goal)))?;
            if goal.state == dvb_core::api::GoalState::Dropped {
                return Ok(kinds);
            }
            // An already open failed-task question retains its identity and choices.
            let existing = proj.questions.values().find(|q| {
                q.view.open
                    && q.view.project == project
                    && q.lapses_on == dvb_core::event::LapsesOn::GoalVersion(goal.version)
                    && q.view
                        .options
                        .iter()
                        .any(|o| o.retries_task.as_ref() == Some(task))
            });
            kinds.push(EventKind::QuestionOpened {
                kind: Some(dvb_core::event::QuestionKind::Ordinary),
                main_ci: None,
                task: Some(task.clone()),
                question: existing
                    .map(|q| q.view.id.clone())
                    .unwrap_or_else(|| dvb_core::QuestionId::new(self.new_id_at(now))),
                subject: dvb_core::wording::render(
                    "question.no-changes-question",
                    &serde_json::json!({"title": (t.view.title).to_string()}),
                ),
                format: existing.map_or(dvb_core::event::QuestionFormat::QuickCompare, |q| {
                    q.view.format
                }),
                options: existing.map(|q| q.view.options.clone()).unwrap_or_else(|| {
                    vec![
                        dvb_core::event::QuestionOption::owner(
                            1,
                            dvb_core::wording::text("question.leave-failed"),
                            dvb_core::wording::text("question.no-changes-kept"),
                        ),
                        dvb_core::event::QuestionOption {
                            retries_task: Some(task.clone()),
                            ..dvb_core::event::QuestionOption::owner(
                                2,
                                dvb_core::wording::text("question.retry-task"),
                                dvb_core::wording::text("question.retry-effect"),
                            )
                        },
                    ]
                }),
                recommended: existing.map_or(1, |q| q.view.recommended),
                lapses_on: dvb_core::event::LapsesOn::GoalVersion(goal.version),
            });
            Ok(kinds)
        })?;
        Ok(())
    }

    /// An engine-started fix round: the task fails with `detail` (the effect
    /// `effect` failed) and its retry starts `run`. Both are decided under one
    /// projections' lock and recorded in one transaction after `pre`, or
    /// nothing is (the refusal is returned), so the task is never left failed
    /// without spending the planned fix round.
    pub(crate) fn fail_and_retry(
        &self,
        task: &TaskId,
        effect: Effect,
        detail: String,
        pre: Vec<EventKind>,
        run: RunAssignment,
    ) -> Result<TaskState, Fail> {
        let mut proj = self.proj();
        let t = proj
            .tasks
            .get(task)
            .cloned()
            .ok_or_else(|| Fail::NotFound(format!("task {task}")))?;
        let fail = Command::EffectFailed {
            rate_limit: None,
            effect,
            transient: false,
            detail,
        };
        let ctx = self.context(&proj, &t, &fail, self.clock.now())?;
        let mut kinds = decide(&t.view.state, fail, &ctx).map_err(Fail::Refused)?;
        let failed = kinds.iter().find_map(|k| {
            if let EventKind::TaskStateChanged { to, .. } = k
                && matches!(to, TaskState::Failed { .. })
            {
                Some(to.clone())
            } else {
                None
            }
        });
        let Some(failed) = failed else {
            return Err(Fail::Refused(Refusal::new(
                RefusalCode::ValueChanged,
                "the task did not fail",
            )));
        };
        let retry = Command::Retry(Some(run));
        let ctx = self.context(&proj, &t, &retry, ctx.now)?;
        kinds.extend(decide(&failed, retry, &ctx).map_err(Fail::Refused)?);
        let kinds = pre.into_iter().chain(kinds).collect();
        self.commit(
            &mut proj,
            Some(&t.view.project),
            Actor::Engine,
            kinds,
            ctx.now,
        )?;
        let now = proj
            .tasks
            .get(task)
            .map_or(TaskState::Planned, |t| t.view.state.clone());
        drop(proj);
        self.kick(task);
        Ok(now)
    }

    /// Decide only for the current live run, retaining review-stage validation under the writer lock.
    pub fn run_command(
        &self,
        run: &RunId,
        decision: Decision<'_>,
    ) -> Result<Option<TaskState>, Fail> {
        let cmd = &decision.cmd;
        let proj = self.proj();
        let Some(task) = current_task(&proj, run) else {
            return Ok(None);
        };
        if matches!(cmd, Command::VerdictRecorded { .. }) {
            let t = &proj.tasks[&task];
            let r = &proj.runs[run];
            let stage = t.next_review();
            let p = &proj.projects[&t.view.project];
            let expected = t.reviewer(p, stage);
            if t.view.review == dvb_core::roles::ReviewKind::Design
                && (r.verdict_given
                    || r.started <= t.facts.review_since
                    || r.assignment.review_stage != Some(stage)
                    || expected != Some(r.assignment.role.clone()))
            {
                return Ok(None);
            }
        }
        let actor = Actor::Run(run.clone());
        self.decide(proj, &task, actor, decision, |_| Ok(()))
            .map(Some)
    }

    /// A run's unit ended: `run.ended`, and for the task's current run the end
    /// decided, in one transaction, so the end of a run replaced meanwhile never
    /// moves its successor. A reviewer that gave its verdict is not "ended
    /// without a verdict". `driver` is the driver's own account, read before.
    /// Recorded once: a run that already ended is left alone.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "The guard is moved into the atomic decision, or held through the append, before slot admission"
    )]
    pub fn end_run(
        &self,
        run: &RunId,
        reason: EndReason,
        driver: Option<EndReason>,
    ) -> Result<(), Fail> {
        // Read before the projections are held.
        let dir = self.cfg.paths.work.join(run.as_str());
        let cause = crate::launch::driver_cause(&dir);
        let stderr = crate::launch::driver_stderr(&dir);
        let proj = self.proj();
        let Some(r) = proj.runs.get(run).cloned() else {
            return Err(Fail::NotFound(format!("run {run}")));
        };
        if r.ended.is_some() {
            return Ok(());
        }
        let goal = r
            .assignment
            .task
            .as_ref()
            .and_then(|t| proj.tasks.get(t))
            .map(|t| t.view.goal.clone())
            .or_else(|| {
                proj.goals
                    .values()
                    .find(|g| g.project == r.project && Some(g.version) == r.goal_version)
                    .map(|g| g.id.clone())
            });
        let current = current_task(&proj, run);
        let t = current.as_ref().and_then(|t| proj.tasks.get(t));
        let verdict_given = r.verdict_given;
        let stopped = t.is_some_and(|t| {
            let state = if let TaskState::Unknown { last } = &t.view.state {
                last.as_ref()
            } else {
                &t.view.state
            };
            matches!(state, TaskState::Stopped { .. })
        });
        // A stop the engine made (the task's, an update's, the time limit's or
        // a finished reviewer's) also ends the vendor CLI, whose cut-off
        // stream the driver may report before it sees the stop.
        let driver = driver.filter(|why| {
            *why != EndReason::ParseError
                || !(stopped
                    || r.stop_requested
                    || r.time_limited
                    || r.finished_review(self.clock.now()))
        });
        // The driver's own account (a refused preflight, a model mismatch, an
        // unreadable stream) over the unit's exit; then a unit stopped because
        // the task state before the end.
        let reason = match (r.preflight_failed, driver, reason) {
            (Some(code), _, EndReason::Completed | EndReason::Crashed) => {
                EndReason::Preflight(code)
            }
            (None, Some(why), EndReason::Completed | EndReason::Crashed) => why,
            (None, None, EndReason::Completed | EndReason::Crashed) if stopped => {
                EndReason::Stopped
            }
            (_, _, reason) => reason,
        };
        // Durable update-stop intent cannot certify a natural completion,
        // even if a watcher races its confirmation after restart.
        let reason =
            if r.assignment.task.is_none() && r.stop_requested && reason == EndReason::Completed {
                EndReason::Interrupted
            } else {
                reason
            };
        // A unit stopped at its time limit ended for that, however its end is
        // seen (the stop, the watcher or a restart's look), unless its task
        // was stopped meanwhile.
        let reason = if r.time_limited && !stopped {
            EndReason::TimeLimit
        } else {
            reason
        };
        let crashed = reason == EndReason::Crashed;
        let detail = cause.filter(|_| crashed);
        let stderr = stderr.filter(|_| crashed);
        let ended = EventKind::RunEnded {
            run: run.clone(),
            reason: reason.clone(),
            detail: detail.clone(),
            stderr: stderr.clone(),
        };
        let result = match current.filter(|_| !verdict_given) {
            Some(task) => {
                let cmd = Command::UnitEnded {
                    reason,
                    detail,
                    stderr,
                };
                self.decide(
                    proj,
                    &task,
                    Actor::Engine,
                    Decision {
                        pre: vec![ended],
                        ..Decision::new("unit_ended", cmd)
                    },
                    |_| Ok(()),
                )
                .map(|_| ())
            }
            None => {
                let mut proj = proj;
                self.commit(
                    &mut proj,
                    Some(&r.project),
                    Actor::Engine,
                    vec![ended],
                    self.clock.now(),
                )
                .map(|_| ())
            }
        };
        if result.is_ok() {
            self.disk_freed.notify_waiters();
            self.slot_freed(goal);
        }
        result
    }

    /// A run ended, so a run slot is free: a task whose start waited for one
    /// starts now, followed by never-started tasks of the ended run's goal,
    /// then of every other approved goal.
    fn slot_freed(&self, goal: Option<dvb_core::GoalId>) {
        if tokio::runtime::Handle::try_current().is_ok()
            && let Some(e) = self.me.get().and_then(Weak::upgrade)
        {
            crate::spawn(
                "start tasks after run end",
                crate::dispatch::start_approved_goals(e, goal),
            );
        }
    }

    /// The PR's head moved: the observed head becomes the
    /// task's publication (`branch.published` in the same round, which voids
    /// the old head's verdict and CI evidence) and review re-enters
    /// (`merge.refused`), in one decision. A no-op unless the task is in
    /// `state` on `old_head` (a stop or another observer came first).
    pub(crate) fn publication_moved(
        &self,
        task: &TaskId,
        state: &TaskState,
        old_head: &str,
        branch: String,
        head: String,
    ) -> Result<bool, Fail> {
        let proj = self.proj();
        let Some(t) = proj.tasks.get(task) else {
            return Err(Fail::NotFound(format!("task {task}")));
        };
        if &t.view.state != state || t.view.head.as_deref() != Some(old_head) {
            return Ok(false);
        }
        let moved = EventKind::BranchPublished {
            task: task.clone(),
            round: t.facts.round,
            branch,
            head,
        };
        let cmd = Command::MergeRefused("the PR's head changed".into());
        self.decide(
            proj,
            task,
            Actor::Engine,
            Decision {
                pre: vec![moved],
                ..Decision::new("merge_refused", cmd)
            },
            |_| Ok(()),
        )
        .map(|now| &now != state)
    }

    /// The task's branch advances to its base (`branch.updated`,
    /// seen again after a lost push reply or a crash). With `carried`, the
    /// copy of the old head's clean verdict, the task keeps its state and CI is
    /// read on the new head;
    /// without, review re-enters (`merge.refused`). A no-op unless the task is
    /// in `state` on `old_head`.
    pub(crate) fn branch_updated(
        &self,
        task: &TaskId,
        state: &TaskState,
        old_head: &str,
        updated: EventKind,
        carried: Vec<EventKind>,
    ) -> Result<(), Fail> {
        let mut proj = self.proj();
        let Some(t) = proj.tasks.get(task) else {
            return Err(Fail::NotFound(format!("task {task}")));
        };
        if &t.view.state != state || t.view.head.as_deref() != Some(old_head) {
            return Ok(());
        }
        let project = t.view.project.clone();
        if !carried.is_empty() {
            let mut events = vec![updated];
            events.extend(carried);
            self.commit(
                &mut proj,
                Some(&project),
                Actor::Engine,
                events,
                self.clock.now(),
            )
            .map(|_| ())
        } else {
            let why =
                "the branch was brought up to date; its change differs, so it is reviewed again";
            let cmd = Command::MergeRefused(why.into());
            self.decide(
                proj,
                task,
                Actor::Engine,
                Decision {
                    pre: vec![updated],
                    ..Decision::new("merge_refused", cmd)
                },
                |_| Ok(()),
            )
            .map(|_| ())
        }
    }

    /// Goal completion rests on the durable merge records, not on task
    /// cleanup: when every task of an approved goal has a recorded merge
    /// (cancelled tasks aside), `goal.done`, once, under the projections' lock.
    pub(crate) fn record_goal_completion(&self, goal: &dvb_core::GoalId) -> Result<bool, Fail> {
        let mut proj = self.proj();
        let Some(g) = proj.goals.get(goal).cloned() else {
            return Err(Fail::NotFound(format!("goal {goal}")));
        };
        if g.state != dvb_core::api::GoalState::Approved || g.tasks.is_empty() {
            return Ok(false);
        }
        let all_merged = g.tasks.iter().all(|t| {
            proj.tasks
                .get(t)
                .is_some_and(|t| t.merged.is_some() || t.view.state == TaskState::Cancelled)
        });
        if !all_merged {
            return Ok(false);
        }
        self.commit(
            &mut proj,
            Some(&g.project),
            Actor::Engine,
            vec![EventKind::GoalDone { goal: goal.clone() }],
            self.clock.now(),
        )?;
        drop(proj);
        Ok(true)
    }

    /// What `decide` may read for this command, with one
    /// clock read.
    fn context(
        &self,
        proj: &Projections,
        t: &Task,
        cmd: &Command,
        now: dvb_core::Timestamp,
    ) -> Result<Context, Fail> {
        let project = proj
            .projects
            .get(&t.view.project)
            .ok_or_else(|| Fail::NotFound(format!("project {}", t.view.project)))?;
        let goal_approved = t.approved;
        Ok(Context {
            waiting_for: proj.unmet_dependencies(t),
            task: t.view.id.clone(),
            head: t.facts.head.clone(),
            goal_approved,
            project_paused: project.view.paused,
            max_runs: proj.max_runs(),
            caps: t.facts.caps(
                cmd,
                u64::try_from(proj.live_runs().count()).unwrap_or(u64::MAX),
            ),
            more_review: t.facts.more_review(cmd, t.view.review),
            ci: project.view.ci.clone(),
            branch_repair: t.facts.branch_repair,
            retried: t.facts.retried,
            reviewed: t.reviews_approved(),
            open_findings: t.facts.open_findings(),
            now,
        })
    }

    /// Wakes the task's dispatcher, when there is a runtime to run it on and
    /// the dispatcher is in use.
    pub fn kick(&self, task: &TaskId) {
        if tokio::runtime::Handle::try_current().is_ok()
            && let Some(e) = self.me.get().and_then(Weak::upgrade)
        {
            crate::dispatch::kick(&e, task);
        }
    }

    /// Keep decision wakes disabled for fixtures that drive dispatcher steps themselves.
    /// Set this before any operation enables the dispatcher.
    #[cfg(any(test, feature = "test-support"))]
    pub fn manual_dispatch(&self) {
        let _ = self.me.set(Weak::new());
    }

    pub(crate) fn dispatch_enabled(&self) -> bool {
        self.me.get().is_none_or(|me| me.strong_count() > 0)
    }

    /// Lets decisions wake dispatchers from now on.
    pub fn remember(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
    }

    pub async fn start_unit(&self, vendor: Vendor, run: &RunId) -> Result<(), LaunchError> {
        self.launcher()
            .start(&UnitId {
                vendor,
                run: run.clone(),
            })
            .await
    }

    pub async fn unit_state(&self, vendor: Vendor, run: &RunId) -> Result<UnitState, LaunchError> {
        self.launcher()
            .state(&UnitId {
                vendor,
                run: run.clone(),
            })
            .await
    }
}

/// The task whose current, live run this is.
fn current_task(proj: &Projections, run: &RunId) -> Option<TaskId> {
    let r = proj.runs.get(run).filter(|r| r.ended.is_none())?;
    let task = r.assignment.task.clone()?;
    (proj.tasks.get(&task)?.facts.run.as_ref() == Some(run)).then_some(task)
}

/// The evidence an event rests on, from its own payload.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "only the kinds that carry evidence have an arm"
)]
fn evidence_of(kind: &EventKind) -> Option<Evidence> {
    let (head, pr, verdict) = match kind {
        EventKind::RunCollected { head, .. } => (head.clone(), None, None),
        EventKind::BranchPublished { head, .. }
        | EventKind::BranchUpdated { head, .. }
        | EventKind::ChecksObserved { head, .. } => (Some(head.clone()), None, None),
        EventKind::PrOpened { pr, .. } => (None, Some(*pr), None),
        EventKind::VerdictRecorded { head, verdict, .. } => {
            (Some(head.clone()), None, Some(verdict.clone()))
        }
        EventKind::PrMerged { pr, sha, .. } => (Some(sha.clone()), Some(*pr), None),
        _ => return None,
    };
    Some(Evidence { head, pr, verdict })
}

impl axum::response::IntoResponse for Fail {
    fn into_response(self) -> axum::response::Response {
        match self {
            Fail::Refused(r) => crate::api::refusal(r),
            Fail::NotFound(what) => crate::api::refusal(Refusal::new(RefusalCode::NotFound, what)),
            Fail::Internal(e) => {
                eprintln!("dvb engine: {e}");
                crate::api::refusal(Refusal::new(RefusalCode::InternalError, e.to_string()))
            }
        }
    }
}

/// /proc/meminfo reports kB as 1024-byte units.
fn mem_available(meminfo: &str) -> Option<u64> {
    let mut fields = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace();
    let kb: u64 = fields.next()?.parse().ok()?;
    (fields.next()? == "kB").then_some(())?;
    kb.checked_mul(1024)
}

impl Engine {
    /// A project's mirror: `mirrors/<name>.git`.
    pub fn mirror(&self, name: &str) -> PathBuf {
        self.cfg.paths.mirrors.join(format!("{name}.git"))
    }

    /// Exact install model ids selected for new work.
    pub(crate) fn selected_models(&self, proj: &Projections) -> Vec<&dvb_core::PoolModel> {
        self.pool
            .models
            .iter()
            .filter(|m| proj.settings.model_selected(&m.id))
            .collect()
    }

    /// The model every default hire of a new project takes.
    pub(crate) fn default_hire_model(&self) -> Option<dvb_core::PoolModel> {
        self.selected_models(&self.proj()).first().copied().cloned()
    }

    /// The switch can use an unselected hire;
    /// the named targets, rather than the hire, were checked against selection.
    pub(crate) fn hire_vendor(&self, model: &str) -> Result<Vendor, Fail> {
        if self.only_models()?.is_some() {
            self.pool
                .find(model)
                .map(|m| m.vendor)
                .ok_or_else(|| Fail::internal("hired model is outside the install pool"))
        } else {
            self.vendor_of(model)
        }
    }

    pub fn vendor_of(&self, model: &str) -> Result<Vendor, Fail> {
        let models = self.selected_models(&self.proj());
        models
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.vendor)
            .ok_or_else(|| {
                Fail::Refused(Refusal::new(
                    RefusalCode::ModelNotInPool,
                    format!(
                        "{model} is outside the selected model pool; valid model ids: {}",
                        models
                            .iter()
                            .map(|m| m.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ))
            })
    }
}

fn host_load_reading() -> Option<(f64, usize)> {
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()?;
    let cpus = std::thread::available_parallelism().ok()?.get();
    (load.is_finite() && load >= 0.0).then_some((load, cpus))
}

#[cfg(test)]
mod disk_tests {
    use super::{Config, Engine};

    #[test]
    fn default_disk_readings_follow_mode() {
        for (dev, fake) in [(true, true), (true, false), (false, true), (false, false)] {
            let dir = tempfile::tempdir().unwrap();
            let mut cfg = Config::dev(dir.path(), "unused-dvb".into());
            cfg.dev = dev;
            cfg.fake = fake;
            for path in [&cfg.paths.state, &cfg.paths.mirrors, &cfg.paths.work] {
                std::fs::create_dir_all(path).unwrap();
            }
            let e = Engine::open(cfg.clone(), Default::default()).unwrap();
            if dev || fake {
                assert_eq!(e.free_bytes(), Some(u64::MAX));
            } else {
                assert!(e.free_bytes().is_some_and(|bytes| bytes < u64::MAX));
            }
            std::fs::remove_dir(&cfg.paths.work).unwrap();
            assert_eq!(e.free_bytes(), (dev || fake).then_some(u64::MAX));
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::{Config, Engine};

    #[test]
    fn decisions_share_the_commit_time_and_record_each_refusal_once() {
        use dvb_core::{Actor, Clock, Command, EventKind, GoalId, ProjectId, TaskId, Timestamp};
        use std::sync::{
            Arc,
            atomic::{AtomicI64, Ordering},
        };
        struct CountingClock(AtomicI64);
        impl Clock for CountingClock {
            fn now(&self) -> Timestamp {
                Timestamp(self.0.fetch_add(1, Ordering::SeqCst))
            }
        }
        let clock = Arc::new(CountingClock(AtomicI64::new(1)));
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "unused".into()),
            crate::EngineOptions {
                clock: Some(clock.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let project = ProjectId::new("p");
        let goal = GoalId::new("g");
        let task = TaskId::new("t");
        e.append_checked(Some(&project), Actor::Owner, |_, _| {
            Ok(vec![
                EventKind::ProjectAdded {
                    id: project.clone(),
                    name: "p".into(),
                    repo: "org/repo".into(),
                    default_branch: "main".into(),
                    ci: dvb_core::event::CiMode::NoCi,
                },
                EventKind::GoalStated {
                    goal: goal.clone(),
                    text: "goal".into(),
                    from_issue: None,
                    issues: Vec::new(),
                    after: None,
                },
                EventKind::PlanProposed {
                    goal,
                    tasks: vec![dvb_core::event::PlannedTask {
                        after: vec![],
                        task: task.clone(),
                        title: "task".into(),
                        role: dvb_core::RoleId::new("backend"),
                        review: dvb_core::roles::ReviewKind::Code,
                        summary: "build".into(),
                        routine: false,
                    }],
                },
            ])
        })
        .unwrap();
        for refused in [false, true] {
            let before = e.store.last_id().unwrap();
            let at = clock.0.load(Ordering::SeqCst);
            let result = e.decide(
                e.proj(),
                &task,
                Actor::Owner,
                crate::Decision::new("cancel", Command::Cancel),
                |_| Ok(()),
            );
            assert_eq!(result.is_err(), refused);
            assert_eq!(clock.0.load(Ordering::SeqCst), at + 1);
            let recorded = e.store.after(before).unwrap();
            assert!(recorded.iter().all(|event| event.at == Timestamp(at)));
            assert_eq!(
                recorded
                    .iter()
                    .filter(|event| matches!(event.kind, EventKind::CommandRefused(_)))
                    .count(),
                usize::from(refused)
            );
        }
        let before = e.store.last_id().unwrap();
        let error = e.refuse(
            Some(&project),
            Actor::Owner,
            "other",
            crate::Fail::io("context", std::io::Error::other("source")),
        );
        assert!(matches!(error, crate::Fail::Internal(_)));
        assert_eq!(e.store.last_id().unwrap(), before);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::{Config, Engine, mem_available};

    #[test]
    fn reads_mem_available_in_bytes() {
        assert_eq!(
            mem_available("MemFree: 1 kB\nMemAvailable:   10485760 kB\nSwapFree: 7 kB\n"),
            Some(10 * 1024 * 1024 * 1024)
        );
        for bad in [
            "MemFree: 1 kB",
            "MemAvailable: nope kB",
            "MemAvailable: 2 MB",
            "MemAvailable: 18446744073709551615 kB",
        ] {
            assert_eq!(mem_available(bad), None);
        }
        assert_eq!(mem_available("MemAvailable: 0 kB"), Some(0));
    }
    #[test]
    fn dev_construction_and_restart_keep_fixed_memory_without_fake_forge() {
        let dir = tempfile::tempdir().unwrap();
        for fake in [true, false] {
            for _ in 0..2 {
                let mut cfg = Config::dev(dir.path(), "unused-dvb".into());
                cfg.fake = fake;
                let e = Engine::open(cfg, Default::default()).unwrap();
                assert_eq!((e.memory_reading)(), Some(u64::MAX));
                assert_eq!(e.available_memory_bytes(), Some(u64::MAX));
                assert_eq!(e.host_load_reading(), Some((0.0, 1)));
            }
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[tokio::test]
    async fn keyed_locks_remove_the_last_holder_for_all_keys() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "unused".into()), Default::default()).unwrap();
        drop(e.lock(&TaskId::new("task")).await);
        drop(e.lock_project_checks(&ProjectId::new("project")).await);
        drop(e.lock_mirror(Path::new("mirror")).await);
        assert!(e.locks.is_empty());
        assert!(e.project_checks.is_empty());
        assert!(e.mirror_fetches.is_empty());
    }
}
