//! The engine: the owner API (axum over the Unix socket, SSE), the run socket,
//! rules, dispatch, reconcile, publish, units, usage and update.

pub mod api;
pub mod dispatch;
mod engine;
pub mod feed;
pub mod issues;
mod keyed_lock;
pub mod launch;
mod only;
pub mod owner;
pub mod proj;
pub mod prompt;
pub mod publish;
pub mod reconcile;
pub mod run_api;
mod runs_memory;
pub mod serve;
pub mod units;
pub mod update;
pub mod usage;
pub mod vendors;
pub mod wake;

pub use engine::{
    AppState, Config, Decision, Engine, EngineOptions, Fail, InternalError, Paths, own_uid,
};

/// `dvb engine [--dev <dir>]`: opens the store, records `engine.started` and
/// starts it.
pub async fn run(cfg: Config) -> Result<(), Fail> {
    // A handover in progress is finished (or undone) before anything else
    // touches the database.
    let layout = update::handover::Layout::of(&cfg);
    let delegated = std::env::var_os("DVB_HANDOVER").is_some();
    match update::handover::recover(&layout, dvb_core::VERSION, delegated, &|db| {
        dvb_store::Store::open(db).map_err(update::handover::HandoverError::from)
    })
    .map_err(Fail::internal)?
    {
        update::handover::Next::Run => {}
        update::handover::Next::Exec(bin) => {
            return Err(Fail::io(
                format!("cannot start {}", bin.display()),
                update::protocol::exec_target(&bin, None),
            ));
        }
    }
    start(Engine::open(cfg, Default::default())?).await
}

/// Starts an opened engine: every task that may have had something in flight is
/// unknown before anything is served, then reconciled in the background;
/// then both sockets are served.
pub async fn start(engine: AppState) -> Result<(), Fail> {
    engine.remember();
    only::restore(&engine)?;
    reconcile::mark(&engine)?;
    // Restore durable completion before dispatch can record a recovered merge.
    // Otherwise restoration can steal completion between PrMerged and TaskDone
    // while startup's missed-wake scan races both actors.
    fail::fail_point!("startup.goal_effects.before");
    publish::reconcile::restore_goal_effects(&engine)?;
    fail::fail_point!("startup.goal_effects.restored");
    spawn("reconcile", reconcile::reconcile(engine.clone()));
    for warning in launch::install_warnings(&engine.cfg) {
        eprintln!("dvb engine: {}", warning.text());
    }
    eprintln!(
        "dvb engine {} on {} and {}",
        dvb_core::VERSION,
        engine.cfg.paths.engine_sock.display(),
        engine.cfg.paths.run_sock.display()
    );
    serve::serve(engine).await.map_err(Fail::internal)
}

#[cfg(test)]
extern crate self as dvb_engine;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

/// Runs a step in the background; a failure is logged, not lost.
pub fn spawn<F>(what: &'static str, f: F)
where
    F: std::future::Future<Output = Result<(), Fail>> + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = f.await {
            eprintln!("dvb engine: {what}: {e}");
        }
    });
}
