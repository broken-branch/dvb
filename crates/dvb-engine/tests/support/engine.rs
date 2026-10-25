use dvb_engine::{AppState, Config, Engine, EngineOptions};
use std::path::Path;

pub fn open(dir: &Path) -> AppState {
    open_with(dir, "dvb", Default::default())
}

pub fn engine() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path());
    (dir, e)
}

/// An engine holding [`super::events::approved_task_events`].
pub fn approved() -> (tempfile::TempDir, AppState) {
    let (dir, e) = engine();
    let p = dvb_core::ProjectId::new("p");
    e.append_checked(Some(&p), dvb_core::Actor::Owner, |_, _| {
        Ok(super::events::approved_task_events())
    })
    .unwrap();
    (dir, e)
}

pub fn open_with(dir: &Path, binary: &str, options: EngineOptions) -> AppState {
    open_config(Config::dev(dir, binary.into()), options)
}

pub fn open_config(config: Config, mut options: EngineOptions) -> AppState {
    let launcher = options.launcher.take().unwrap_or_else(|| {
        if config.dev {
            std::sync::Arc::new(dvb_engine::units::ProcessLauncher::new(
                config.dvb_bin.clone(),
                config.paths.work.clone(),
            ))
        } else {
            std::sync::Arc::new(dvb_engine::units::SystemdLauncher::default())
        }
    });
    options.launcher = Some(std::sync::Arc::new(
        super::launcher::FakeLauncher::forwarding(launcher),
    ));
    Engine::open(config, options).unwrap()
}

/// Mutable samples captured by construction-time readings, shared by resource tests.
#[derive(Clone)]
pub struct Readings {
    pub disk: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    pub memory: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    pub load: std::sync::Arc<std::sync::Mutex<Option<(f64, usize)>>>,
}

impl Default for Readings {
    fn default() -> Self {
        Self {
            disk: std::sync::Arc::new(std::sync::Mutex::new(Some(u64::MAX))),
            memory: std::sync::Arc::new(std::sync::Mutex::new(Some(u64::MAX))),
            load: std::sync::Arc::new(std::sync::Mutex::new(Some((0.0, 1)))),
        }
    }
}

impl Readings {
    pub fn options(&self, mut options: EngineOptions) -> EngineOptions {
        let disk = self.disk.clone();
        let memory = self.memory.clone();
        let load = self.load.clone();
        options.disk_reading = Some(std::sync::Arc::new(move || *disk.lock().unwrap()));
        options.memory_reading = Some(std::sync::Arc::new(move || *memory.lock().unwrap()));
        options.host_load = Some(std::sync::Arc::new(move || *load.lock().unwrap()));
        options
    }
}

pub fn fake_pool() -> dvb_core::ModelPool {
    dvb_core::ModelPool {
        models: ["fake-1", "fake-next"]
            .into_iter()
            .map(|id| dvb_core::PoolModel {
                id: id.into(),
                vendor: dvb_core::Vendor::Fake,
                efforts: vec![dvb_core::Effort::Medium, dvb_core::Effort::High],
            })
            .collect(),
    }
}
