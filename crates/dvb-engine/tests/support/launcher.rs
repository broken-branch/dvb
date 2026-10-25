use dvb_core::RunId;
use dvb_core::launcher::{LaunchError, Launcher, UnitId, UnitState};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};

type StartHook = Arc<dyn Fn(&UnitId) -> Result<(), LaunchError> + Send + Sync>;

/// In-memory units with explicit samples, failures and start/stop observation.
pub struct FakeLauncher {
    pub delegate: Option<Arc<dyn Launcher>>,
    pub initial: UnitState,
    /// None preserves externally supplied states when a start only records a call.
    pub start_state: Option<UnitState>,
    pub states: Mutex<BTreeMap<RunId, UnitState>>,
    pub stopping: Mutex<BTreeMap<RunId, u32>>,
    pub units: Mutex<Vec<UnitId>>,
    pub starts: AtomicUsize,
    pub stops: AtomicUsize,
    pub queries: AtomicU32,
    pub failing: AtomicU32,
    pub fail_stops: AtomicBool,
    pub linger: AtomicU32,
    pub block_start: AtomicBool,
    pub release: Semaphore,
    pub starting: Semaphore,
    pub ready: Semaphore,
    pub started: Notify,
    pub start_hook: Option<StartHook>,
    pub forbid_start: Option<&'static str>,
    pub forbid_stop: Option<&'static str>,
    pub forbid_state: Option<&'static str>,
    pub stop_changes_state: bool,
    pub readings: super::engine::Readings,
}

impl Default for FakeLauncher {
    fn default() -> Self {
        Self::new(UnitState::Inactive)
    }
}

impl FakeLauncher {
    pub fn new(initial: UnitState) -> Self {
        Self {
            delegate: None,
            initial,
            start_state: Some(UnitState::Active),
            states: Mutex::new(BTreeMap::new()),
            stopping: Mutex::new(BTreeMap::new()),
            units: Mutex::new(Vec::new()),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            queries: AtomicU32::new(0),
            failing: AtomicU32::new(0),
            fail_stops: AtomicBool::new(false),
            linger: AtomicU32::new(0),
            block_start: AtomicBool::new(false),
            release: Semaphore::new(0),
            starting: Semaphore::new(0),
            ready: Semaphore::new(0),
            started: Notify::new(),
            start_hook: None,
            forbid_start: None,
            forbid_stop: None,
            forbid_state: None,
            stop_changes_state: true,
            readings: Default::default(),
        }
    }

    pub fn forwarding(delegate: Arc<dyn Launcher>) -> Self {
        Self {
            delegate: Some(delegate),
            ..Self::default()
        }
    }

    pub fn running(&self, run: &str) {
        self.states
            .lock()
            .unwrap()
            .insert(RunId::new(run), UnitState::Active);
    }

    pub fn end(&self, run: &RunId) {
        self.states
            .lock()
            .unwrap()
            .insert(run.clone(), UnitState::Inactive);
    }

    pub fn is_active(&self, run: &RunId) -> bool {
        self.states
            .lock()
            .unwrap()
            .get(run)
            .unwrap_or(&self.initial)
            == &UnitState::Active
    }
}

#[async_trait::async_trait]
impl Launcher for FakeLauncher {
    fn run_of_pid(&self, pid: i32) -> Option<RunId> {
        PEERS.lock().unwrap().get(&pid).cloned().or_else(|| {
            self.delegate
                .as_ref()
                .and_then(|delegate| delegate.run_of_pid(pid))
        })
    }
    async fn start(&self, unit: &UnitId) -> Result<(), LaunchError> {
        if let Some(delegate) = &self.delegate {
            return delegate.start(unit).await;
        }
        if let Some(message) = self.forbid_start {
            panic!("{message}");
        }
        if let Some(hook) = &self.start_hook {
            hook(unit)?;
        }
        self.units.lock().unwrap().push(unit.clone());
        if let Some(state) = self.start_state {
            self.states.lock().unwrap().insert(unit.run.clone(), state);
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.starting.add_permits(1);
        if self.block_start.load(Ordering::SeqCst) {
            self.release.acquire().await.unwrap().forget();
        }
        self.ready.add_permits(1);
        self.started.notify_one();
        Ok(())
    }
    async fn stop(&self, unit: &UnitId) -> Result<(), LaunchError> {
        if let Some(delegate) = &self.delegate {
            return delegate.stop(unit).await;
        }
        if let Some(message) = self.forbid_stop {
            panic!("{message}");
        }
        if self.fail_stops.load(Ordering::SeqCst) {
            return Err(LaunchError("the stop job failed".into()));
        }
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stop_changes_state {
            let linger = *self
                .stopping
                .lock()
                .unwrap()
                .entry(unit.run.clone())
                .or_insert_with(|| self.linger.load(Ordering::SeqCst));
            if linger == 0 {
                self.end(&unit.run);
            }
        }
        Ok(())
    }
    async fn state(&self, unit: &UnitId) -> Result<UnitState, LaunchError> {
        if let Some(delegate) = &self.delegate {
            return delegate.state(unit).await;
        }
        if let Some(message) = self.forbid_state {
            panic!("{message}");
        }
        self.queries.fetch_add(1, Ordering::SeqCst);
        if self
            .failing
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(LaunchError("org.freedesktop.DBus.Error.NoReply".into()));
        }
        if let Some(left) = self.stopping.lock().unwrap().get_mut(&unit.run) {
            if *left > 0 {
                *left -= 1;
                return Ok(UnitState::Active);
            }
            self.end(&unit.run);
        }
        Ok(*self
            .states
            .lock()
            .unwrap()
            .get(&unit.run)
            .unwrap_or(&self.initial))
    }
}

// Synthetic peer ids avoid process-global caller identity when router calls race.
static PEERS: Mutex<BTreeMap<i32, RunId>> = Mutex::new(BTreeMap::new());
static NEXT_PEER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

pub fn register_peer(run: RunId) -> i32 {
    let pid = NEXT_PEER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PEERS.lock().unwrap().insert(pid, run);
    pid
}

pub fn unregister_peer(pid: i32) {
    PEERS.lock().unwrap().remove(&pid);
}
