//! `dvb run exec <run>`: prepare, preflight, exec and collect, in one process
//! inside the run's unit, each reporting to the engine on the run
//! socket. The engine wrote `env` in the run directory before starting the unit;
//! systemd (or the dev launcher) passes it in as the environment, and the unit's
//! working directory is the run directory.

use crate::claude::{self, Claude};
use crate::client;
use crate::codex::Codex;
use crate::fake::Fake;
use crate::stream::StderrTail;
use dvb_core::event::{OutputItem, ServedModel};
use dvb_core::git::Git;
use dvb_core::provider::{Provider, ProviderError, RunItem, Session};
use dvb_core::run_verbs::{CollectedBody, ModelBody, OutputBody, RateLimitBody, UsageBody};
use dvb_core::{Effort, RunOutcome, Vendor};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The worker's commits are never published; this identity keeps git from
/// reading an identity from elsewhere.
pub const WORKER_NAME: &str = "dvb worker";
pub const WORKER_EMAIL: &str = "worker@dvb.invalid";

/// The `env` file's keys, plus `DVB_DEV` for dev mode.
#[derive(Debug, Clone)]
pub struct RunEnv {
    pub run: String,
    pub vendor: Vendor,
    pub model: String,
    pub effort: Effort,
    pub base: String,
    pub mirror: PathBuf,
    pub run_sock: PathBuf,
    pub prompt: Option<PathBuf>,
    pub fake_script: Option<PathBuf>,
    pub dev: bool,
    /// A login run (`dvb login`): the vendor's login command, nothing else.
    pub login: bool,
    /// The vendor CLI to use instead of the one on `PATH` (`DVB_CLI_BIN`).
    pub cli_bin: Option<PathBuf>,
    /// The run directory (the working directory).
    pub dir: PathBuf,
}

fn var(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("{key} is not set"))
}

fn parse_effort(value: &str) -> Result<Effort, String> {
    if value.is_empty() {
        Ok(Effort::Medium)
    } else {
        value.parse()
    }
}

impl RunEnv {
    pub fn from_env() -> Result<Self, String> {
        let vendor = var("DVB_VENDOR")?.parse()?;
        let effort = parse_effort(&var("DVB_EFFORT").unwrap_or_default())?;
        Ok(Self {
            run: var("DVB_RUN_ID")?,
            vendor,
            model: var("DVB_MODEL")?,
            effort,
            base: var("DVB_BASE")?,
            mirror: var("DVB_MIRROR")?.into(),
            run_sock: var("DVB_RUN_SOCK")?.into(),
            prompt: std::env::var("DVB_PROMPT").ok().map(PathBuf::from),
            fake_script: std::env::var("DVB_FAKE_SCRIPT").ok().map(PathBuf::from),
            dev: std::env::var("DVB_DEV").is_ok_and(|v| v == "1"),
            login: std::env::var("DVB_LOGIN").is_ok_and(|v| v == "1"),
            cli_bin: std::env::var("DVB_CLI_BIN").ok().map(PathBuf::from),
            dir: std::env::current_dir().map_err(|e| e.to_string())?,
        })
    }

    pub fn ws(&self) -> PathBuf {
        self.dir.join("ws")
    }

    pub fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    /// `<root>/work/<run>` -> `<root>/state` (the same layout in an install and
    /// in dev mode).
    pub fn state_dir(&self) -> Option<PathBuf> {
        Some(self.dir.parent()?.parent()?.join("state"))
    }

    pub(crate) fn post<T: Serialize>(&self, verb: &str, body: &T) -> Result<String, String> {
        let body = serde_json::to_string(body).map_err(|e| e.to_string())?;
        let reply = client::post(&self.run_sock, &format!("/run/{verb}"), &body)
            .map_err(|e| format!("run socket: {e}"))?;
        if reply.ok() {
            Ok(reply.body)
        } else {
            Err(format!("/run/{verb}: {} {}", reply.status, reply.body))
        }
    }
}

/// `DVB_RESUME=1`: the engine moved the task's preserved workspace into this
/// run's directory; the run carries on in it.
pub fn resuming() -> bool {
    std::env::var("DVB_RESUME").is_ok_and(|v| v == "1")
}

/// An executable `name` in a directory of `path`.
pub(crate) fn on_path(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|bin| {
            bin.is_file()
                && bin
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        })
}

/// Prepare the worker-owned project cache and its child environment.
/// `rustup_home` is a RUSTUP_HOME the run already has (from its unit, or passed on
/// by the engine), which is kept.
pub fn prepare_build_cache(
    command: &mut Command,
    cache: &Path,
    run_dir: &Path,
    path: &std::ffi::OsStr,
    rustup_home: Option<&std::ffi::OsStr>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(cache.join("cargo")).map_err(|e| cache_error(cache, e))?;
    if rustup_home.is_none() {
        // Shared by the project's runs: rustup installs a pinned toolchain
        // (rust-toolchain.toml) there on first use, and stable for a project
        // that pins none.
        let home = cache.join("rustup");
        std::fs::create_dir_all(&home)?;
        let settings = home.join("settings.toml");
        if !settings.exists() {
            std::fs::write(
                settings,
                "version = \"12\"\ndefault_toolchain = \"stable\"\n",
            )?;
        }
        command.env("RUSTUP_HOME", home);
    }
    if let Some(sccache) = on_path(path, "sccache") {
        std::fs::create_dir_all(cache.join("sccache"))?;
        let build = run_dir.join("build");
        std::fs::create_dir_all(&build)?;
        command
            .env("RUSTC_WRAPPER", std::fs::canonicalize(sccache)?)
            .env("SCCACHE_DIR", cache.join("sccache"))
            // A server in an isolated unit cannot see another run's workspace.
            .env("SCCACHE_SERVER_UDS", build.join("sccache.sock"))
            .env("CARGO_INCREMENTAL", "0");
    }
    if let Some(mise) = on_path(path, "mise") {
        // Node's commands go through mise, which installs the version a project
        // pins (.nvmrc, .node-version) into the project cache on first use and
        // runs the system's Node where none is pinned.
        let data = cache.join("mise");
        let shims = data.join("shims");
        std::fs::create_dir_all(&shims)?;
        let mise = std::fs::canonicalize(mise)?;
        for name in ["node", "npm", "npx", "corepack"] {
            // Made anew every run rather than trusted from the cache, and swapped
            // in by a rename, so a concurrent run of the project never misses one.
            let fresh = shims.join(format!(".{name}.{}", std::process::id()));
            if let Err(e) = std::fs::remove_file(&fresh)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(e);
            }
            std::os::unix::fs::symlink(&mise, &fresh)?;
            std::fs::rename(&fresh, shims.join(name))?;
        }
        let mut search = shims.into_os_string();
        search.push(":");
        search.push(path);
        command
            .env("MISE_DATA_DIR", data)
            .env("MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS", "node")
            .env("PATH", search);
    }
    Ok(())
}

/// The run units mount the project cache back only through the engine's cache
/// link; under an engine older than the installed units, which makes none, the
/// cache is on their read-only tmpfs. Other errors are kept as they are.
fn cache_error(cache: &Path, e: std::io::Error) -> std::io::Error {
    if e.kind() != std::io::ErrorKind::ReadOnlyFilesystem {
        return e;
    }
    std::io::Error::new(
        e.kind(),
        format!(
            "cannot write the project cache {} ({e}): the engine and the installed run units must come from the same release; re-run its install.sh",
            cache.display()
        ),
    )
}

/// How long a run waits for another run of its project to finish installing a
/// toolchain; a stalled one would else hold every other run until its time limit.
const TOOLCHAIN_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Install a Rust workspace's toolchain (its rust-toolchain.toml pin, or rustup's
/// default) before the session, one run at a time: rustup does not guard
/// concurrent installs into the RUSTUP_HOME a project's runs share.
/// A run holding the lock longer than `wait` fails this step, not the run, with
/// `ErrorKind::WouldBlock`: that run may still be installing.
fn install_toolchain(
    ws: &Path,
    path: &std::ffi::OsStr,
    rustup_home: Option<&std::ffi::OsStr>,
    wait: std::time::Duration,
) -> std::io::Result<()> {
    let rust = ["rust-toolchain.toml", "rust-toolchain", "Cargo.toml"]
        .iter()
        .any(|file| ws.join(file).exists());
    let (Some(home), Some(_), true) = (rustup_home, on_path(path, "rustup"), rust) else {
        return Ok(());
    };
    let lock_path = Path::new(home).join("dvb-install.lock");
    let lock = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        // Runs cannot install into a RUSTUP_HOME they cannot write (a system one).
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut remaining = wait;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "gave up after {}s waiting for another run of this project to install the toolchain (it holds {})",
                    wait.as_secs(),
                    lock_path.display()
                ),
            ));
        }
        let step = remaining.min(std::time::Duration::from_secs(1));
        std::thread::sleep(step);
        remaining -= step;
    }
    // The rustc proxy installs the toolchain the workspace selects.
    let output = crate::child::output(
        Command::new("rustc")
            .arg("--version")
            .current_dir(ws)
            .env("PATH", path)
            .env("RUSTUP_HOME", home),
        std::time::Duration::from_secs(900),
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

/// The whole run. Exit codes: 0 done, 2 setup failed, 3 preflight refused,
/// 4 the session failed, 5 the served model is not the pinned one, 6 the CLI's
/// output could not be parsed. Codes 5 and 6 also write `reason` in the run
/// directory (`mismatch`, `parse_error`) for the engine to read.
pub async fn exec(run: &str) -> RunOutcome {
    let env = match RunEnv::from_env() {
        Ok(env) if env.run == run => env,
        Ok(env) => {
            eprintln!("run {run}: the env file is for {}", env.run);
            return RunOutcome::SetupFailed;
        }
        Err(e) => {
            eprintln!("run {run}: {e}");
            return RunOutcome::SetupFailed;
        }
    };
    let stopped = share_on_stop(&env.dir);
    let outcome = if env.login {
        crate::login::run(&env, crate::login::Waits::default())
    } else {
        whole_run(&env, run).await
    };
    share_run(&env.dir);
    // The session may end first, cut short by the same stop.
    if stopped.load(std::sync::atomic::Ordering::SeqCst) {
        RunOutcome::Completed
    } else {
        outcome
    }
}

/// Shares the run's directories on SIGTERM and exits 0 so the engine can
/// distinguish a stop from a crash. The flag is set once the stop is seen.
fn share_on_stop(dir: &Path) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    use tokio::signal::unix::{SignalKind, signal};
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            eprintln!("SIGTERM: {e}");
            return stopped;
        }
    };
    let (dir, seen) = (dir.to_owned(), stopped.clone());
    tokio::spawn(async move {
        if term.recv().await.is_some() {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
            share_run(&dir);
            std::process::exit(0);
        }
    });
    stopped
}

async fn whole_run(env: &RunEnv, run: &str) -> RunOutcome {
    if let Some(cache) = std::env::var_os("DVB_BUILD_CACHE") {
        // Re-exec keeps the worker PID and gives every descendant the build
        // environment without mutating a multithreaded process environment.
        use std::os::unix::process::CommandExt;
        let setup = || -> Result<(), String> {
            let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command
                .args(["run", "exec", run])
                .env_remove("DVB_BUILD_CACHE");
            prepare_build_cache(
                &mut command,
                Path::new(&cache),
                &env.dir,
                &std::env::var_os("PATH").unwrap_or_default(),
                std::env::var_os("RUSTUP_HOME").as_deref(),
            )
            .map_err(|e| e.to_string())?;
            Err(command.exec().to_string())
        };
        if let Err(e) = setup() {
            return failed(
                &env.dir,
                RunOutcome::SetupFailed,
                &format!("build cache: {e}"),
            );
        }
    }
    if let Err(e) = prepare(env).await {
        return failed(&env.dir, RunOutcome::SetupFailed, &format!("prepare: {e}"));
    }
    let mut toolchain_installs = true;
    // The Fake provider builds nothing.
    if env.vendor != Vendor::Fake
        && let Err(e) = install_toolchain(
            &env.ws(),
            &std::env::var_os("PATH").unwrap_or_default(),
            std::env::var_os("RUSTUP_HOME").as_deref(),
            TOOLCHAIN_LOCK_WAIT,
        )
    {
        // The session's first cargo or rustc call installs it instead, unless
        // another run may still be installing it into the shared RUSTUP_HOME.
        eprintln!("rust toolchain: {e}");
        toolchain_installs = e.kind() != std::io::ErrorKind::WouldBlock;
    }
    let body = crate::preflight::run(env).await;
    let refused = body.checks.iter().any(|c| !c.ok);
    if let Err(e) = env.post("preflight", &body) {
        return failed(
            &env.dir,
            RunOutcome::SetupFailed,
            &format!("preflight: {e}"),
        );
    }
    if refused {
        eprintln!("preflight refused: {:?}", body.checks);
        return RunOutcome::PreflightRefused;
    }
    let stderr = StderrTail::default();
    if let Err(failure) = session(env, toolchain_installs, &stderr) {
        let outcome = outcome_for(&failure);
        write_reason(&env.dir, outcome);
        write_stderr(&env.dir, &stderr.text());
        return failed(&env.dir, outcome, &format!("exec: {failure}"));
    }
    match collect(env).await {
        Ok(_) => RunOutcome::Completed,
        Err(e) => failed(
            &env.dir,
            RunOutcome::SessionFailed,
            &format!("collect: {e}"),
        ),
    }
}

/// Logs why the run failed and writes it, after its exit code, as the run
/// directory's one-line `cause`: the engine's `run.ended` carries it.
fn failed(dir: &Path, outcome: RunOutcome, cause: &str) -> RunOutcome {
    eprintln!("{cause}");
    let line = cause.lines().next().unwrap_or_default();
    let line = format!("exit {}: {line}", outcome.exit_code());
    if let Err(e) = std::fs::write(dir.join("cause"), line) {
        eprintln!("cause: {e}");
    }
    outcome
}

/// Writes a failed session's last stderr lines as the run directory's
/// `stderr`: the engine's `run.ended` carries them.
fn write_stderr(dir: &Path, tail: &str) {
    if tail.is_empty() {
        return;
    }
    if let Err(e) = std::fs::write(dir.join("stderr"), tail) {
        eprintln!("stderr: {e}");
    }
}

/// Writes a failed session's `reason` for the engine.
fn write_reason(dir: &Path, outcome: RunOutcome) {
    if let Some(reason) = outcome.driver_reason() {
        let _ = std::fs::write(dir.join("reason"), reason);
    }
}

/// Gives the group every directory the run left under its run directory `dir`
/// and in its project's cache, so the engine (in the run's group) can remove
/// either once nothing needs it: tools can make directories without group write
/// whatever the umask. sccache's server daemonizes with umask 0027, so every
/// directory it makes in the cache is 0750.
pub fn share_run(dir: &Path) {
    share_tree(dir);
    // The engine's link to the cache, which the unit mounts at the cache's own
    // path; a login run has none.
    if let Ok(cache) = std::fs::canonicalize(dir.join("cache")) {
        share_tree(&cache);
    }
}

/// Gives the group every directory under `dir`. Another mount inside, such as
/// the vendor's login directory on the managed home, is left alone. Best
/// effort: the first failure is logged.
fn share_tree(dir: &Path) {
    use rustix::fs::{AtFlags, StatxFlags};
    let mount = match rustix::fs::statx(
        rustix::fs::CWD,
        dir,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::MNT_ID,
    ) {
        Ok(st) => st.stx_mnt_id,
        Err(e) => {
            eprintln!("share {}: {e}", dir.display());
            return;
        }
    };
    let mut failed = 0_usize;
    let mut todo = vec![dir.to_owned()];
    while let Some(dir) = todo.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                if failed == 0 {
                    eprintln!("share {}: {e}", dir.display());
                }
                failed += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = entry.path();
            match share_dir(&path, mount) {
                Ok(true) => todo.push(path),
                Ok(false) => {}
                Err(e) => {
                    if failed == 0 {
                        eprintln!("share {}: {e}", path.display());
                    }
                    failed += 1;
                }
            }
        }
    }
    if failed > 1 {
        eprintln!("share: {failed} directories in all");
    }
}

/// Adds group read, write and search to the directory, opened without
/// following a link, unless it is on another mount; whether to look inside.
fn share_dir(path: &Path, mount: u64) -> std::io::Result<bool> {
    use rustix::fs::{AtFlags, Mode, OFlags, StatxFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let st = rustix::fs::statx(
        &fd,
        "",
        AtFlags::EMPTY_PATH,
        StatxFlags::MODE | StatxFlags::MNT_ID,
    )?;
    if st.stx_mnt_id != mount {
        return Ok(false);
    }
    let mode = u32::from(st.stx_mode) & 0o7777;
    if mode & 0o070 != 0o070 {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode | 0o070))?;
    }
    Ok(true)
}

/// Clone the mirror at the base head into `ws/`; make the managed home.
pub async fn prepare(env: &RunEnv) -> Result<(), String> {
    std::fs::create_dir_all(env.home()).map_err(|e| format!("home: {e}"))?;
    let ws = env.ws();
    let mirror = env.mirror.to_string_lossy();
    // A resume keeps the preserved workspace with its tracked and untracked
    // edits. It fetches the mirror's branches and tags, so release comparisons
    // and commits the engine asks it to merge are available in the workspace.
    if resuming() && ws.join(".git").exists() {
        Git::new(&ws)
            .run(
                &[
                    "-c",
                    "safe.directory=*",
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--upload-pack",
                    "git -c safe.directory='*' upload-pack",
                    &mirror,
                    "+refs/heads/*:refs/remotes/origin/*",
                    "+refs/tags/*:refs/tags/*",
                ],
                &[],
            )
            .await
            .map_err(|e| e.to_string())?;
        return write_vendor_files(env);
    }
    let ws_s = ws.to_string_lossy();
    // The mirror belongs to the engine's user; the run only reads it.
    Git::new(&env.dir)
        .run(
            &[
                "-c",
                "safe.directory=*",
                "clone",
                "--quiet",
                "--no-hardlinks",
                "--no-checkout",
                // A local clone's upload-pack drops `-c` options (git 2.43 on Ubuntu 24.04), so
                // it must get the exception itself, or it refuses the engine-owned mirror.
                "--upload-pack",
                "git -c safe.directory='*' upload-pack",
                &mirror,
                &ws_s,
            ],
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    Git::new(&ws)
        .run(&["checkout", "--quiet", "--detach", &env.base], &[])
        .await
        .map_err(|e| e.to_string())?;
    Git::new(&ws)
        .run(&["config", "user.name", WORKER_NAME], &[])
        .await
        .map_err(|e| e.to_string())?;
    Git::new(&ws)
        .run(&["config", "user.email", WORKER_EMAIL], &[])
        .await
        .map_err(|e| e.to_string())?;
    write_vendor_files(env)?;
    Ok(())
}

/// What the real providers need beside the workspace: `bin/dvb` so the model
/// can call `dvb run <verb>`; Claude's settings (sandbox off, memory off, pin);
/// for Codex, `memories` and `fast_mode` switched off in the shared home.
pub fn write_vendor_files(env: &RunEnv) -> Result<(), String> {
    if env.vendor != Vendor::Fake {
        let bin = env.dir.join("bin");
        std::fs::create_dir_all(&bin).map_err(|e| format!("bin: {e}"))?;
        let link = bin.join("dvb");
        let _ = std::fs::remove_file(&link);
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(me, &link).map_err(|e| format!("bin/dvb: {e}"))?;
    }
    match env.vendor {
        Vendor::Fake => Ok(()),
        Vendor::Claude => std::fs::write(
            env.dir.join(claude::SETTINGS_FILE),
            claude::settings_json().to_string(),
        )
        .map_err(|e| format!("claude settings: {e}")),
        Vendor::Codex => {
            // Preflight reports failed commands through the existing check codes.
            if let Some(cli) = crate::preflight::find_cli(env) {
                for &feature in crate::codex::FEATURES_OFF {
                    let _ = crate::child::output(
                        Command::new(&cli)
                            .args(["features", "disable", feature])
                            .env("HOME", env.home()),
                        crate::child::PROBE_TIMEOUT,
                    );
                }
            }
            Ok(())
        }
    }
}

/// Why a session failed.
#[derive(Debug)]
enum Failure {
    Mismatch(String),
    Provider(ProviderError),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Mismatch(m)
            | Failure::Provider(ProviderError::Parse(m) | ProviderError::Other(m)) => {
                f.write_str(m)
            }
        }
    }
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Provider(ProviderError::Other(s))
    }
}

/// The exit code and `reason` file text for a failed session.
fn outcome_for(f: &Failure) -> RunOutcome {
    match f {
        Failure::Mismatch(_) => RunOutcome::Mismatch,
        Failure::Provider(ProviderError::Parse(_)) => RunOutcome::ParseError,
        Failure::Provider(ProviderError::Other(_)) => RunOutcome::SessionFailed,
    }
}

impl From<ProviderError> for Failure {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

fn provider(
    env: &RunEnv,
    toolchain_installs: bool,
    stderr: &StderrTail,
) -> Result<Box<dyn Provider>, String> {
    let cli = || {
        crate::preflight::find_cli(env)
            .ok_or_else(|| format!("{} is not on PATH", env.vendor.as_str()))
    };
    Ok(match env.vendor {
        Vendor::Fake => Box::new(Fake {
            script: env
                .fake_script
                .clone()
                .ok_or("DVB_FAKE_SCRIPT is not set")?,
            dvb: std::env::current_exe().map_err(|e| e.to_string())?,
            stderr: stderr.clone(),
        }),
        Vendor::Claude => Box::new(Claude {
            bin: cli()?,
            run_dir: env.dir.clone(),
            toolchain_installs,
            stderr: stderr.clone(),
        }),
        Vendor::Codex => Box::new(Codex {
            bin: cli()?,
            run_dir: env.dir.clone(),
            toolchain_installs,
            stderr: stderr.clone(),
        }),
    })
}

/// Plays the provider's session, reporting each item to the engine. The CLI's
/// stderr goes to `stderr`.
fn session(env: &RunEnv, toolchain_installs: bool, stderr: &StderrTail) -> Result<(), Failure> {
    let provider = provider(env, toolchain_installs, stderr)?;
    let prompt = env
        .prompt
        .as_ref()
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|e| format!("prompt: {e}"))?
        .unwrap_or_default();
    // Startup talks to the CLI too (Codex initialises its thread here), so its
    // parse failures keep their classification.
    let mut s = provider.start(&env.ws(), &env.home(), &prompt, &env.model, env.effort)?;
    let result = report(env, s.as_mut());
    // On every exit, a failed report too: a live vendor must not keep editing.
    s.stop();
    result
}

/// Reports each item to the engine until the session ends.
fn report(env: &RunEnv, s: &mut dyn Session) -> Result<(), Failure> {
    let mut ended = None;
    while let Some(item) = s.next() {
        match item {
            RunItem::ModelServed(served) => {
                let mismatch = match &served {
                    ServedModel::Mismatch(id) => Some(id.clone()),
                    ServedModel::Confirmed(_) | ServedModel::Unconfirmed => None,
                };
                if mismatch.is_some() {
                    // Before telling the engine, which stops the unit.
                    let _ = std::fs::write(
                        env.dir.join("reason"),
                        RunOutcome::Mismatch.driver_reason().unwrap_or_default(),
                    );
                }
                env.post(
                    "model",
                    &ModelBody {
                        requested: env.model.clone(),
                        served,
                    },
                )?;
                if let Some(id) = mismatch {
                    return Err(Failure::Mismatch(format!(
                        "asked for {}, the vendor served {id}",
                        env.model
                    )));
                }
            }
            RunItem::Text(text) => {
                env.post("output", &output(OutputItem::Text(text)))?;
            }
            RunItem::ToolCall { name, .. } => {
                env.post("output", &output(OutputItem::ToolCall { name }))?;
            }
            RunItem::Usage {
                uncached_in,
                cached_in,
                out,
            } => {
                env.post(
                    "usage",
                    &UsageBody {
                        uncached_in,
                        cached_in,
                        out,
                    },
                )?;
            }
            RunItem::RateLimit {
                used_pct,
                window,
                resets_at,
            } => {
                // The window is a bonus: a refusal must not fail the run.
                let _ = env.post(
                    "ratelimit",
                    &RateLimitBody {
                        used_pct,
                        window,
                        resets_at,
                    },
                );
            }
            RunItem::Ended(status) => {
                ended = Some(status.code);
                break;
            }
            RunItem::Error(e) => return Err(e.into()),
        };
    }
    match ended {
        Some(Some(0)) | None => Ok(()),
        Some(Some(code)) => Err(format!("the session exited with code {code}").into()),
        Some(None) => Err("a signal ended the session".to_owned().into()),
    }
}

fn output(item: OutputItem) -> OutputBody {
    OutputBody { item }
}

/// Snapshot the final tree, tracked and untracked, as one commit on the base at
/// `refs/dvb/collected`; bundle it, write its id to `head` and tell the engine.
pub async fn collect(env: &RunEnv) -> Result<String, String> {
    let msg = format!("dvb collect {}", env.run);
    let head = snapshot(&env.ws(), &env.dir.join("collect.index"), &env.base, &msg).await?;
    Git::new(&env.ws())
        .run(&["update-ref", "refs/dvb/collected", &head], &[])
        .await
        .map_err(|e| e.to_string())?;
    // The engine reads the result from this file, never from the run's
    // repository, whose configuration and refs are the run's. The engine's
    // mirror has the base.
    let bundle = env.dir.join("collected.bundle");
    Git::new(&env.ws())
        .run(
            &[
                "bundle",
                "create",
                "--quiet",
                &bundle.to_string_lossy(),
                "refs/dvb/collected",
                &format!("^{}", env.base),
            ],
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    std::fs::write(env.dir.join("head"), format!("{head}\n")).map_err(|e| format!("head: {e}"))?;
    if let Err(err) = env.post(
        "collected",
        &CollectedBody {
            outcome: None,
            head: Some(head.clone()),
            summary: None,
        },
    ) {
        // The work is complete and the receipt is written. A successful unit
        // exit lets the engine recover it without another provider session.
        eprintln!("collected report: {err}; the engine can recover head {head}");
    }
    Ok(head)
}

/// The workspace's tree as one commit on `base`. The index starts from the base
/// tree, so files the base tracks stay tracked even when `.gitignore` matches
/// them; then every change, deletion and untracked, unignored file is staged,
/// and so is every file the worker tracked itself (force-added or committed).
pub async fn snapshot(ws: &Path, index: &Path, base: &str, msg: &str) -> Result<String, String> {
    let index_s = index.to_string_lossy().into_owned();
    let idx = [("GIT_INDEX_FILE", index_s.as_str())];
    let _ = std::fs::remove_file(index);
    Git::new(ws)
        .run(&["read-tree", base], &idx)
        .await
        .map_err(|e| e.to_string())?;
    Git::new(ws)
        .run(&["add", "--all", "."], &idx)
        .await
        .map_err(|e| e.to_string())?;
    // Paths in the worker's own index, which `.gitignore` may match; ones it
    // has since deleted are already staged as gone.
    let tracked: Vec<String> = Git::new(ws)
        .run(&["ls-files", "-z"], &[])
        .await
        .map_err(|e| e.to_string())?
        .split('\0')
        .filter(|p| !p.is_empty() && ws.join(p).symlink_metadata().is_ok())
        .map(|p| format!("{p}\0"))
        .collect();
    if !tracked.is_empty() {
        let list = index.with_extension("paths");
        std::fs::write(&list, tracked.concat()).map_err(|e| format!("{}: {e}", list.display()))?;
        let list_s = list.to_string_lossy();
        let added = Git::new(ws)
            .run(
                &[
                    "--literal-pathspecs",
                    "add",
                    "--force",
                    "--pathspec-file-nul",
                    &format!("--pathspec-from-file={list_s}"),
                ],
                &idx,
            )
            .await
            .map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&list);
        added?;
    }
    let tree = Git::new(ws)
        .run(&["write-tree"], &idx)
        .await
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(index);
    let who = [
        ("GIT_AUTHOR_NAME", WORKER_NAME),
        ("GIT_AUTHOR_EMAIL", WORKER_EMAIL),
        ("GIT_COMMITTER_NAME", WORKER_NAME),
        ("GIT_COMMITTER_EMAIL", WORKER_EMAIL),
    ];
    Git::new(ws)
        .run(&["commit-tree", &tree, "-p", base, "-m", msg], &who)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_tags_reach_fresh_and_resumed_workspaces() {
        use dvb_core::forge::{Forge, Repo};
        use dvb_forge::FakeForge;

        if let Some(dir) = std::env::var_os("DVB_TEST_TAG_WORKSPACE") {
            let dir = PathBuf::from(dir);
            let mut env = crate::testutil::run_env(&dir, Vendor::Fake, "fake");
            env.mirror = dir.parent().unwrap().join("mirror.git");
            prepare(&env).await.unwrap();
            return;
        }

        async fn git(dir: &Path, args: &[&str]) -> String {
            Git::new(dir)
                .run(
                    args,
                    &[
                        ("GIT_AUTHOR_NAME", WORKER_NAME),
                        ("GIT_AUTHOR_EMAIL", WORKER_EMAIL),
                        ("GIT_COMMITTER_NAME", WORKER_NAME),
                        ("GIT_COMMITTER_EMAIL", WORKER_EMAIL),
                    ],
                )
                .await
                .unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let project = root.join("project");
        let origin = root.join("origin.git");
        let mirror = root.join("mirror.git");
        git(
            root,
            &["init", "--quiet", "-b", "main", project.to_str().unwrap()],
        )
        .await;
        git(
            root,
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        )
        .await;
        std::fs::write(project.join("a.txt"), "one\n").unwrap();
        git(&project, &["add", "a.txt"]).await;
        git(&project, &["commit", "--quiet", "-m", "base"]).await;
        git(
            &project,
            &["push", "--quiet", origin.to_str().unwrap(), "main"],
        )
        .await;
        let forge = FakeForge::local_git();
        let repo = Repo(origin.to_str().unwrap().into());
        forge.clone_repo(&repo, &mirror).await.unwrap();

        git(&project, &["tag", "-a", "v1", "-m", "release"]).await;
        git(
            &project,
            &["push", "--quiet", origin.to_str().unwrap(), "refs/tags/v1"],
        )
        .await;
        std::fs::write(project.join("a.txt"), "two\n").unwrap();
        git(&project, &["commit", "--quiet", "-am", "change"]).await;
        git(
            &project,
            &["push", "--quiet", origin.to_str().unwrap(), "main"],
        )
        .await;
        let head = forge.fetch(&repo, &mirror, "main").await.unwrap();

        let dir = root.join("run");
        let mut env = crate::testutil::run_env(&dir, Vendor::Fake, "fake");
        env.base = head.clone();
        env.mirror = mirror.clone();
        prepare(&env).await.unwrap();
        assert_eq!(git(&env.ws(), &["log", "--format=%H", "v1.."]).await, head);
        let diff = git(&env.ws(), &["diff", "v1", "--", "a.txt"]).await;
        assert!(diff.contains("-one\n+two"), "{diff}");

        git(&project, &["tag", "-f", "-a", "v1", "-m", "moved release"]).await;
        git(&project, &["tag", "v2"]).await;
        git(
            &project,
            &[
                "push",
                "--quiet",
                origin.to_str().unwrap(),
                "+refs/tags/v1",
                "refs/tags/v2",
            ],
        )
        .await;
        assert_eq!(forge.fetch(&repo, &mirror, "main").await.unwrap(), head);
        let mut fresh = env.clone();
        fresh.dir = root.join("fresh");
        prepare(&fresh).await.unwrap();
        assert_eq!(git(&fresh.ws(), &["rev-parse", "v1^{commit}"]).await, head);

        std::fs::write(env.ws().join("a.txt"), "preserved edit\n").unwrap();
        std::fs::write(env.ws().join("untracked.txt"), "preserved file\n").unwrap();
        // A child process sets the resume environment without racing other tests.
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "exec::tests::project_tags_reach_fresh_and_resumed_workspaces",
                "--nocapture",
            ])
            .env("DVB_TEST_TAG_WORKSPACE", &dir)
            .env("DVB_RESUME", "1")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for tag in ["v1^{commit}", "v2"] {
            assert_eq!(git(&env.ws(), &["rev-parse", tag]).await, head);
        }
        assert_eq!(git(&env.ws(), &["rev-parse", "HEAD"]).await, head);
        assert_eq!(
            std::fs::read_to_string(env.ws().join("a.txt")).unwrap(),
            "preserved edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(env.ws().join("untracked.txt")).unwrap(),
            "preserved file\n"
        );
        assert_eq!(git(&mirror, &["rev-parse", "v1^{commit}"]).await, head);
    }

    #[test]
    fn a_run_leaves_its_directories_group_writable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        let deps = dir.join("build/debug/deps");
        let sealed = dir.join("build/debug/sealed");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::create_dir_all(&sealed).unwrap();
        std::fs::write(deps.join("lib.rlib"), "x").unwrap();
        std::fs::write(sealed.join("out"), "x").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("build/link")).unwrap();
        let set = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        set(&deps.join("lib.rlib"), 0o644);
        set(&deps, 0o755);
        set(&sealed.join("out"), 0o444);
        set(&sealed, 0o555);
        set(&dir.join("build/debug"), 0o700);
        set(&outside, 0o700);
        share_tree(&dir);
        assert_eq!(mode(&dir.join("build/debug")), 0o770);
        assert_eq!(mode(&deps), 0o775);
        assert_eq!(mode(&sealed), 0o575);
        // Files need no group write to be removed; a link is not followed.
        assert_eq!(mode(&deps.join("lib.rlib")), 0o644);
        assert_eq!(mode(&sealed.join("out")), 0o444);
        assert_eq!(mode(&outside), 0o700);
        set(&sealed, 0o755);
    }

    #[tokio::test]
    async fn local_git_yields_to_the_async_executor() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let dir = tempfile::tempdir().unwrap();
        let mut operation = Box::pin(async {
            Git::new(dir.path())
                .run(&["init", "--quiet"], &[])
                .await
                .map_err(|e| e.to_string())
        });
        assert!(matches!(
            operation
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        operation.await.unwrap();
    }

    #[test]
    fn worker_effort_text_keeps_the_empty_default_and_unknown_error() {
        for (text, expected) in [
            ("", Effort::Medium),
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
        ] {
            assert_eq!(parse_effort(text), Ok(expected));
        }
        assert_eq!(parse_effort("medum"), Err("unknown effort medum".into()));
        assert_eq!(
            "other".parse::<Vendor>(),
            Err("unknown vendor other".into())
        );
    }

    #[test]
    fn codex_prepare_disables_features_in_the_worker_home() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let stub = dir.join("codex");
        crate::testutil::write_executable(
            &stub,
            "#!/bin/sh\nprintf '%s %s %s %s\\n' \"$HOME\" \"$1\" \"$2\" \"$3\" >> \"$HOME/features\"\n",
        );
        let mut env = crate::testutil::run_env(dir, Vendor::Codex, "gpt-6.1-sol");
        env.cli_bin = Some(stub);
        std::fs::create_dir_all(env.home()).unwrap();
        write_vendor_files(&env).unwrap();
        let features = std::fs::read_to_string(env.home().join("features")).unwrap();
        let home = env.home();
        let home = home.display();
        assert_eq!(
            features,
            format!("{home} features disable memories\n{home} features disable fast_mode\n")
        );
    }

    #[test]
    fn a_failed_run_leaves_its_exit_code_and_first_line_as_its_cause() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = failed(
            dir.path(),
            RunOutcome::SessionFailed,
            "collect: boom\nthe rest",
        );
        assert_eq!(outcome, RunOutcome::SessionFailed);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cause")).unwrap(),
            "exit 4: collect: boom"
        );
    }

    #[test]
    fn a_crashed_session_says_its_exit_code_and_leaves_its_last_stderr_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let token = format!("sk-ant-{}", "a".repeat(30));
        let stub = dir.join("claude");
        crate::testutil::write_executable(
            &stub,
            &format!(
                "#!/bin/sh\ncat >/dev/null\nfor n in 1 2 3 4 5 6 7; do echo \"line $n\" >&2; done\n\
                 echo 'auth: {token}' >&2\nhead -c 3000 /dev/zero | tr '\\0' y >&2\necho >&2\n\
                 printf '\\033[31mfatal: out of memory\\033[0m\\n' >&2\n\
                 printf '%s\\n' '{{\"type\":\"result\",\"is_error\":false,\"result\":\"done\"}}'\n\
                 exit 1\n"
            ),
        );
        let mut env = crate::testutil::run_env(&dir, Vendor::Claude, "pin");
        env.cli_bin = Some(stub);
        let stderr = StderrTail::default();
        let failure = session(&env, true, &stderr).unwrap_err();
        assert_eq!(failure.to_string(), "the session exited with code 1");
        write_stderr(&dir, &stderr.text());
        let kept = std::fs::read_to_string(dir.join("stderr")).unwrap();
        let lines: Vec<_> = kept.lines().collect();
        assert_eq!(
            lines,
            [
                "line 6",
                "line 7",
                &format!("auth: {token}"),
                &"y".repeat(1024),
                "\u{1b}[31mfatal: out of memory\u{1b}[0m",
            ]
        );
    }

    #[test]
    fn error_words_do_not_select_the_exit_code() {
        let other = Failure::from(ProviderError::Other("parse error: ordinary failure".into()));
        assert_eq!(outcome_for(&other), RunOutcome::SessionFailed);
        assert_eq!(other.to_string(), "parse error: ordinary failure");
        let parse = Failure::from(ProviderError::Parse("malformed message".into()));
        assert_eq!(outcome_for(&parse), RunOutcome::ParseError);
        assert_eq!(parse.to_string(), "malformed message");
    }

    #[test]
    fn malformed_json_while_codex_starts_up_is_a_parse_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let stub = dir.join("codex");
        crate::testutil::write_executable(
            &stub,
            "#!/bin/sh\nread l\necho 'this is not json'\nexec sleep 5\n",
        );
        let mut env = crate::testutil::run_env(&dir, Vendor::Codex, "gpt-6.1-sol");
        env.cli_bin = Some(stub);
        let failure = session(&env, true, &StderrTail::default()).unwrap_err();
        assert!(
            matches!(failure, Failure::Provider(ProviderError::Parse(_))),
            "{failure:?}"
        );
        assert_eq!(outcome_for(&failure), RunOutcome::ParseError);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_vendor_serving_another_model_mid_run_is_stopped_and_both_models_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let msg = |id: &str, model: &str| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\"conte\
                nt\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}"
            )
        };
        let stub = dir.join("claude");
        crate::testutil::write_executable(
            &stub,
            &format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{}' '{}'\nsleep 1\ntouch '{}'\n",
                msg("m1", "claude-sonnet-5-5"),
                msg("m2", "claude-haiku-4-5-20251001"),
                dir.join("survived").display()
            ),
        );
        let listener = tokio::net::UnixListener::bind(dir.join("sock")).unwrap();
        let mut requests = crate::test_http::requests(listener);
        let posts = tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                let (request, response) = requests.recv().await.unwrap();
                assert_eq!(request.method(), axum::http::Method::POST);
                let path = request.uri().path().to_owned();
                let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let mismatch = path == "/run/model" && body["served"].get("mismatch").is_some();
                seen.push((path, body));
                response
                    .send(crate::test_http::json(200, serde_json::json!({})))
                    .unwrap();
                if mismatch {
                    return seen;
                }
            }
        });
        let mut env = crate::testutil::run_env(&dir, Vendor::Claude, "claude-sonnet-5-5");
        env.cli_bin = Some(stub);
        let failure =
            tokio::task::spawn_blocking(move || session(&env, true, &StderrTail::default()))
                .await
                .unwrap()
                .unwrap_err();
        assert!(matches!(failure, Failure::Mismatch(_)), "{failure:?}");
        assert_eq!(outcome_for(&failure), RunOutcome::Mismatch);
        assert_eq!(
            std::fs::read_to_string(dir.join("reason")).unwrap(),
            "mismatch"
        );
        let model = tokio::time::timeout(std::time::Duration::from_secs(10), posts)
            .await
            .expect("the mismatch report was received")
            .unwrap()
            .into_iter()
            .find(|(path, body)| path == "/run/model" && body["served"].get("mismatch").is_some())
            .expect("the engine was told");
        assert_eq!(model.1["requested"], "claude-sonnet-5-5");
        assert_eq!(model.1["served"]["mismatch"], "claude-haiku-4-5-20251001");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert!(
            !dir.join("survived").exists(),
            "the vendor process kept running"
        );
    }

    #[test]
    fn a_failed_report_stops_the_vendor_process() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let stub = dir.join("claude");
        crate::testutil::write_executable(
            &stub,
            &format!(
                "#!/bin/sh\ncat >/dev/null\necho $$ > '{}'\nprintf '%s\\n' '{}'\nexec sleep 30\n",
                dir.join("pid").display(),
                "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"pin\",\"content\":[{\
                \"type\":\"text\",\"text\":\"hi\"}]}}"
            ),
        );
        let mut env = crate::testutil::run_env(&dir, Vendor::Claude, "pin");
        env.cli_bin = Some(stub);
        // No engine listens on the run socket.
        env.run_sock = dir.join("unavailable.sock");
        let failure = session(&env, true, &StderrTail::default()).unwrap_err();
        assert!(failure.to_string().contains("run socket"), "{failure}");
        let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "the vendor process {pid} kept running"
        );
    }

    #[tokio::test]
    async fn a_lost_collected_report_keeps_a_successful_receipt_but_snapshot_errors_fail() {
        for usable in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let ws = tmp.path().join("ws");
            Git::new(tmp.path())
                .run(&["init", "--quiet", ws.to_str().unwrap()], &[])
                .await
                .unwrap();
            let who = [
                ("GIT_AUTHOR_NAME", WORKER_NAME),
                ("GIT_AUTHOR_EMAIL", WORKER_EMAIL),
                ("GIT_COMMITTER_NAME", WORKER_NAME),
                ("GIT_COMMITTER_EMAIL", WORKER_EMAIL),
            ];
            Git::new(&ws)
                .run(&["commit", "--quiet", "--allow-empty", "-m", "base"], &who)
                .await
                .unwrap();
            let base = Git::new(&ws)
                .run(&["rev-parse", "HEAD"], &[])
                .await
                .unwrap();
            std::fs::write(ws.join("work.txt"), "finished\n").unwrap();
            let mut env = crate::testutil::run_env(tmp.path(), Vendor::Fake, "fake-1");
            env.base = if usable { base } else { "missing-base".into() };
            env.run_sock = tmp.path().join("unavailable.sock");
            let result = collect(&env).await;
            if usable {
                let head = result.expect("a lost report must not fail completed work");
                assert_eq!(
                    std::fs::read_to_string(env.dir.join("head"))
                        .unwrap()
                        .trim(),
                    head
                );
                assert_eq!(
                    Git::new(&ws)
                        .run(&["rev-parse", "refs/dvb/collected^{commit}"], &[])
                        .await
                        .unwrap(),
                    head
                );
                assert_eq!(
                    Git::new(&ws)
                        .run(&["show", &format!("{head}:work.txt")], &[])
                        .await
                        .unwrap(),
                    "finished"
                );
                let bundle = env.dir.join("collected.bundle");
                assert_eq!(
                    Git::new(&ws)
                        .run(&["bundle", "list-heads", &bundle.to_string_lossy()], &[])
                        .await
                        .unwrap(),
                    format!("{head} refs/dvb/collected")
                );
            } else {
                assert!(result.is_err());
                assert!(!env.dir.join("head").exists());
                assert!(!env.dir.join("collected.bundle").exists());
                assert!(
                    Git::new(&ws)
                        .run(&["rev-parse", "--verify", "refs/dvb/collected"], &[])
                        .await
                        .is_err()
                );
            }
        }
    }

    #[tokio::test]
    async fn a_snapshot_keeps_tracked_ignored_files_and_the_worker_s_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let who = [
            ("GIT_AUTHOR_NAME", "n"),
            ("GIT_AUTHOR_EMAIL", "n@example.invalid"),
            ("GIT_COMMITTER_NAME", "n"),
            ("GIT_COMMITTER_EMAIL", "n@example.invalid"),
        ];
        Git::new(tmp.path())
            .run(&["init", "--quiet", ws.to_str().unwrap()], &[])
            .await
            .unwrap();
        for (f, t) in [
            (".gitignore", "tracked.txt\n*.log\n"),
            ("tracked.txt", "kept\n"),
            ("keep.txt", "old\n"),
            ("gone.txt", "bye\n"),
        ] {
            std::fs::write(ws.join(f), t).unwrap();
        }
        Git::new(&ws)
            .run(&["add", "--force", "."], &[])
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-m", "base"], &who)
            .await
            .unwrap();
        let base = Git::new(&ws)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();

        std::fs::write(ws.join("keep.txt"), "new\n").unwrap();
        std::fs::remove_file(ws.join("gone.txt")).unwrap();
        std::fs::write(ws.join("added.txt"), "hi\n").unwrap();
        std::fs::write(ws.join("junk.log"), "noise\n").unwrap();

        // The worker force-adds one ignored file and commits another.
        std::fs::write(ws.join("staged.log"), "fixture\n").unwrap();
        std::fs::write(ws.join("committed.log"), "fixture\n").unwrap();
        Git::new(&ws)
            .run(&["add", "--force", "staged.log", "committed.log"], &[])
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-m", "work", "committed.log"], &who)
            .await
            .unwrap();

        let head = snapshot(&ws, &tmp.path().join("i"), &base, "snap")
            .await
            .unwrap();
        let files = Git::new(&ws)
            .run(&["ls-tree", "--name-only", &head], &[])
            .await
            .unwrap();
        assert_eq!(
            files,
            ".gitignore\nadded.txt\ncommitted.log\nkeep.txt\nstaged.log\ntracked.txt"
        );
        assert_eq!(
            Git::new(&ws)
                .run(&["show", &format!("{head}:keep.txt")], &[])
                .await
                .unwrap(),
            "new"
        );
        assert_eq!(
            Git::new(&ws)
                .run(&["rev-parse", &format!("{head}^")], &[])
                .await
                .unwrap(),
            base
        );
    }
}

#[cfg(test)]
mod build_cache_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_worker_path_sccache_sets_a_project_cache_and_disables_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sccache = bin.join("sccache");
        crate::testutil::write_executable(&sccache, "#!/bin/sh\nexec \"$@\"\n");
        let cache = dir.path().join("project");
        let mut command = Command::new("/usr/bin/env");
        command.env_clear();
        prepare_build_cache(&mut command, &cache, dir.path(), bin.as_os_str(), None).unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        let env: std::collections::BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        assert_eq!(env["RUSTC_WRAPPER"], sccache.to_str().unwrap());
        assert_eq!(env["SCCACHE_DIR"], cache.join("sccache").to_str().unwrap());
        assert_eq!(
            env["SCCACHE_SERVER_UDS"],
            dir.path().join("build/sccache.sock").to_str().unwrap()
        );
        assert_eq!(env["CARGO_INCREMENTAL"], "0");
        std::fs::write(cache.join("sccache/writable"), "yes").unwrap();
    }

    fn child_env(command: &mut Command) -> std::collections::BTreeMap<String, String> {
        let output = command.output().unwrap();
        assert!(output.status.success());
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn pinned_toolchains_install_into_the_project_cache() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let mise = bin.join("mise");
        crate::testutil::write_executable(&mise, "#!/bin/sh\n");
        let cache = dir.path().join("project");
        let shims = cache.join("mise/shims");
        let default = "version = \"12\"\ndefault_toolchain = \"stable\"\n";
        let mut kept = "";
        // A second run keeps the toolchain settings rustup wrote and remakes the
        // shims an earlier run of the project could have swapped.
        for _ in 0..2 {
            let mut command = Command::new("/usr/bin/env");
            command.env_clear();
            prepare_build_cache(&mut command, &cache, dir.path(), bin.as_os_str(), None).unwrap();
            let env = child_env(&mut command);
            assert_eq!(
                env["PATH"],
                format!("{}:{}", shims.display(), bin.display())
            );
            assert_eq!(env["MISE_DATA_DIR"], cache.join("mise").to_str().unwrap());
            assert_eq!(env["MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS"], "node");
            assert_eq!(env["RUSTUP_HOME"], cache.join("rustup").to_str().unwrap());
            let mut names: Vec<_> = std::fs::read_dir(&shims)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            assert_eq!(names, ["corepack", "node", "npm", "npx"]);
            for name in &names {
                assert_eq!(std::fs::read_link(shims.join(name)).unwrap(), mise);
            }
            let settings = cache.join("rustup/settings.toml");
            let written = std::fs::read_to_string(&settings).unwrap();
            assert_eq!(written, format!("{default}{kept}"));
            kept = "[overrides]\n";
            std::fs::write(&settings, format!("{default}{kept}")).unwrap();
            std::fs::remove_file(shims.join("node")).unwrap();
            crate::testutil::write_executable(&shims.join("node"), "#!/bin/sh\necho swapped\n");
        }
    }

    #[test]
    fn a_rust_workspace_installs_its_toolchain_holding_the_project_lock() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let home = dir.path().join("rustup");
        std::fs::create_dir(&home).unwrap();
        crate::testutil::write_executable(&bin.join("rustup"), "#!/bin/sh\n");
        // A second run's install would wait here: the lock is taken while rustc runs.
        crate::testutil::write_executable(
            &bin.join("rustc"),
            "#!/bin/sh\nflock -n \"$RUSTUP_HOME/dvb-install.lock\" true || pwd > \"$RUSTUP_HOME/installed\"\n",
        );
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        let path = std::ffi::OsString::from(format!("{}:/usr/bin:/bin", bin.display()));
        install_toolchain(&ws, &path, Some(home.as_os_str()), TOOLCHAIN_LOCK_WAIT).unwrap();
        assert!(!home.join("installed").exists(), "not a Rust workspace");
        std::fs::write(ws.join("rust-toolchain.toml"), "[toolchain]\n").unwrap();
        install_toolchain(&ws, &path, Some(home.as_os_str()), TOOLCHAIN_LOCK_WAIT).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("installed"))
                .unwrap()
                .trim(),
            ws.to_str().unwrap()
        );
        crate::testutil::write_executable(
            &bin.join("rustc"),
            "#!/bin/sh\necho offline >&2\nexit 1\n",
        );
        let error =
            install_toolchain(&ws, &path, Some(home.as_os_str()), TOOLCHAIN_LOCK_WAIT).unwrap_err();
        assert_eq!(error.to_string(), "offline");
        // Another run holds the lock past the wait: this step gives up without rustc.
        let held = std::fs::File::open(home.join("dvb-install.lock")).unwrap();
        held.lock().unwrap();
        let error = install_toolchain(
            &ws,
            &path,
            Some(home.as_os_str()),
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            error.to_string().starts_with(
                "gave up after 0s waiting for another run of this project to install the toolchain"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_rustup_home_the_run_already_has_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("project");
        let mut command = Command::new("/usr/bin/env");
        command.env_clear().env("RUSTUP_HOME", "/opt/rust/rustup");
        prepare_build_cache(
            &mut command,
            &cache,
            dir.path(),
            dir.path().as_os_str(),
            Some(std::ffi::OsStr::new("/opt/rust/rustup")),
        )
        .unwrap();
        assert_eq!(child_env(&mut command)["RUSTUP_HOME"], "/opt/rust/rustup");
        assert!(!cache.join("rustup").exists());
    }

    #[test]
    fn only_a_read_only_cache_blames_the_release_mismatch() {
        let cache = Path::new("/var/lib/dvb/mirrors/.cache/p");
        let text = cache_error(
            cache,
            std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem),
        )
        .to_string();
        assert!(
            text.starts_with("cannot write the project cache /var/lib/dvb/mirrors/.cache/p (")
                && text.ends_with("re-run its install.sh"),
            "{text}"
        );
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull).to_string();
        assert_eq!(
            cache_error(cache, std::io::Error::from(std::io::ErrorKind::StorageFull)).to_string(),
            full
        );
        // Any other write failure in setup shows as it is.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let error = prepare_build_cache(
            &mut Command::new("/usr/bin/env"),
            &file.join("project"),
            dir.path(),
            dir.path().as_os_str(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert!(!error.to_string().contains("install.sh"), "{error}");
    }

    #[test]
    fn without_an_executable_sccache_compiler_behavior_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let decoy = dir.path().join("sccache");
        std::fs::write(&decoy, "not executable").unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cache = dir.path().join("project");
        let mut command = Command::new("/usr/bin/env");
        command
            .env_clear()
            .env("RUSTC_WRAPPER", "original")
            .env("CARGO_INCREMENTAL", "1");
        prepare_build_cache(
            &mut command,
            &cache,
            dir.path(),
            dir.path().as_os_str(),
            None,
        )
        .unwrap();
        let output = command.output().unwrap();
        let env: std::collections::BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        assert_eq!(env["RUSTC_WRAPPER"], "original");
        assert_eq!(env["CARGO_INCREMENTAL"], "1");
        assert!(!env.contains_key("SCCACHE_DIR"));
        assert!(!env.contains_key("SCCACHE_SERVER_UDS"));
        assert!(!env.contains_key("PATH") && !env.contains_key("MISE_DATA_DIR"));
        std::fs::write(cache.join("cargo/writable"), "yes").unwrap();
    }
}
