//! Run units: `SystemdLauncher` starts `dvb-run-<vendor>@<run>.service`
//! over systemd's D-Bus API, allowed for user `dvb` by the polkit rule;
//! `ProcessLauncher` runs `dvb run exec <run>` as a child process in dev mode and
//! tests. Each also tells the engine which run a peer process belongs to.

use async_trait::async_trait;
use dvb_core::launcher::{LaunchError, Launcher, UnitId, UnitState};
use dvb_core::{RunId, Vendor};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use zbus::{OwnedObjectPath, OwnedVariant};

const SYSTEMD: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

fn err(e: impl std::fmt::Display) -> LaunchError {
    LaunchError(e.to_string())
}

/// Units over the system bus. The engine is a plain system service with no login
/// session, so polkit applies the rule for user `dvb`.
#[derive(Default)]
pub struct SystemdLauncher {
    conn: tokio::sync::OnceCell<zbus::Connection>,
}

impl SystemdLauncher {
    async fn conn(&self) -> Result<&zbus::Connection, LaunchError> {
        self.conn
            .get_or_try_init(zbus::Connection::system)
            .await
            .map_err(err)
    }

    async fn manager<B, R>(&self, method: &str, body: &B) -> Result<R, LaunchError>
    where
        B: serde::Serialize + zbus::DynamicType,
        R: for<'d> zbus::wire::DynamicDeserialize<'d>,
    {
        let msg = self
            .conn()
            .await?
            .call_method(Some(SYSTEMD), MANAGER_PATH, Some(MANAGER), method, body)
            .await
            .map_err(err)?;
        msg.body().deserialize().map_err(err)
    }

    async fn property(
        &self,
        path: &OwnedObjectPath,
        iface: &str,
        name: &str,
    ) -> Result<OwnedVariant, LaunchError> {
        let msg = self
            .conn()
            .await?
            .call_method(Some(SYSTEMD), path, Some(PROPERTIES), "Get", &(iface, name))
            .await
            .map_err(err)?;
        msg.body().deserialize().map_err(err)
    }
}

#[async_trait]
impl Launcher for SystemdLauncher {
    fn run_of_pid(&self, pid: i32) -> Option<RunId> {
        run_of_cgroup(pid)
    }
    /// Starts the unit and waits for its start job to finish, so a unit that is
    /// inactive afterwards has run and ended.
    async fn start(&self, unit: &UnitId) -> Result<(), LaunchError> {
        let job: OwnedObjectPath = self
            .manager("StartUnit", &(unit.unit_name(), "replace"))
            .await?;
        for _ in 0..600 {
            if self
                .property(&job, "org.freedesktop.systemd1.Job", "State")
                .await
                .is_err()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(LaunchError(format!(
            "{}: start job did not finish",
            unit.unit_name()
        )))
    }

    async fn stop(&self, unit: &UnitId) -> Result<(), LaunchError> {
        let _job: OwnedObjectPath = self
            .manager("StopUnit", &(unit.unit_name(), "replace"))
            .await?;
        Ok(())
    }

    async fn state(&self, unit: &UnitId) -> Result<UnitState, LaunchError> {
        let path: OwnedObjectPath = self.manager("LoadUnit", &(unit.unit_name(),)).await?;
        let v = self
            .property(&path, "org.freedesktop.systemd1.Unit", "ActiveState")
            .await?;
        let s = String::try_from(v).map_err(err)?;
        Ok(match s.as_str() {
            "inactive" => UnitState::Inactive,
            "failed" => UnitState::Failed,
            // active, activating, deactivating, reloading, maintenance, refreshing
            _ => UnitState::Active,
        })
    }
}

/// The run whose unit a process is in, from `/proc/<pid>/cgroup`, the cgroup
/// systemd itself placed it in.
pub fn run_of_cgroup(pid: i32) -> Option<RunId> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup_run(&text)
}

fn cgroup_run(text: &str) -> Option<RunId> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    unit_run(path.rsplit('/').next()?)
}

/// `dvb-run-<vendor>@<run>.service` -> `<run>`.
fn unit_run(unit: &str) -> Option<RunId> {
    let rest = unit.strip_prefix("dvb-run-")?.strip_suffix(".service")?;
    let (_vendor, run) = rest.split_once('@')?;
    (!run.is_empty()).then(|| RunId::new(run))
}

/// Dev mode: each run is a child process of the engine, in its run directory,
/// with only the `env` file's variables (plus `PATH`, `HOME` and `DVB_DEV`).
/// Its pid is kept in `work/<run>/pid`, so that like a unit it outlives an
/// engine that dies: a restarted engine sees it, stops it and takes its calls.
pub struct ProcessLauncher {
    dvb: PathBuf,
    /// The engine's `work/` directory; a run's directory is `work/<run>`.
    work: PathBuf,
    children: Mutex<BTreeMap<RunId, Child>>,
}

struct Child {
    pid: i32,
    proc: std::process::Child,
}

impl ProcessLauncher {
    pub fn new(dvb: PathBuf, work: PathBuf) -> Self {
        Self {
            dvb,
            work,
            children: Mutex::new(BTreeMap::new()),
        }
    }

    fn children(&self) -> std::sync::MutexGuard<'_, BTreeMap<RunId, Child>> {
        self.children.lock().unwrap_or_else(dvb_core::sync::recover)
    }

    /// The run whose process `pid` is, or descends from: a child of this
    /// engine, or a run an earlier engine started that is still running.
    pub fn run_of_pid(&self, pid: i32) -> Option<RunId> {
        let mut by_pid: BTreeMap<i32, RunId> = self
            .children()
            .iter()
            .map(|(run, c)| (c.pid, run.clone()))
            .collect();
        for entry in std::fs::read_dir(&self.work)
            .into_iter()
            .flatten()
            .flatten()
        {
            let run = RunId::new(entry.file_name().to_string_lossy());
            if let Some(pid) = self.orphan(&run) {
                by_pid.entry(pid).or_insert(run);
            }
        }
        let mut p = pid;
        for _ in 0..64 {
            if let Some(run) = by_pid.get(&p) {
                return Some(run.clone());
            }
            p = parent_pid(p)?;
            if p <= 1 {
                return None;
            }
        }
        None
    }

    fn run_dir(&self, unit: &UnitId) -> PathBuf {
        self.work.join(unit.run.as_str())
    }

    /// The live process of a run this engine did not start (from its pid file),
    /// if `dvb run exec <run>` is still running there.
    fn orphan(&self, run: &RunId) -> Option<i32> {
        if self.children().contains_key(run) {
            return None;
        }
        let pid: i32 = crate::launch::read_run_file(&self.work.join(run.as_str()).join("pid"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let ours = args.windows(3).any(|w| {
            w[0] == b"run".as_slice()
                && w[1] == b"exec".as_slice()
                && w[2] == run.as_str().as_bytes()
        });
        ours.then_some(pid)
    }
}

fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid ...; comm may hold spaces and parentheses.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Reads `KEY=value` lines.
pub fn read_env(path: &std::path::Path) -> Result<Vec<(String, String)>, LaunchError> {
    let text =
        crate::launch::read_run_file(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

#[async_trait]
impl Launcher for ProcessLauncher {
    fn run_of_pid(&self, pid: i32) -> Option<RunId> {
        ProcessLauncher::run_of_pid(self, pid)
    }
    async fn start(&self, unit: &UnitId) -> Result<(), LaunchError> {
        let dir = self.run_dir(unit);
        let env = read_env(&dir.join("env"))?;
        let log = crate::launch::create_run_file(&dir.join("run.log")).map_err(err)?;
        let proc = std::process::Command::new(&self.dvb)
            .args(["run", "exec", unit.run.as_str()])
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir.join("home"))
            .env("DVB_DEV", "1")
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(err)?)
            .stderr(log)
            .spawn()
            .map_err(|e| err(format!("{}: {e}", self.dvb.display())))?;
        let pid = i32::try_from(proc.id()).map_err(err)?;
        crate::launch::write_run_file(&dir.join("pid"), format!("{pid}\n")).map_err(err)?;
        self.children()
            .insert(unit.run.clone(), Child { pid, proc });
        Ok(())
    }

    async fn stop(&self, unit: &UnitId) -> Result<(), LaunchError> {
        if let Some(c) = self.children().get_mut(&unit.run) {
            c.proc.kill().map_err(err)?;
        } else if let Some(pid) = self.orphan(&unit.run) {
            std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status()
                .map_err(err)?;
        }
        Ok(())
    }

    async fn state(&self, unit: &UnitId) -> Result<UnitState, LaunchError> {
        if self.orphan(&unit.run).is_some() {
            return Ok(UnitState::Active);
        }
        let mut children = self.children();
        let Some(c) = children.get_mut(&unit.run) else {
            return Ok(UnitState::Inactive);
        };
        let state = match c.proc.try_wait().map_err(err)? {
            None => UnitState::Active,
            Some(status) if status.success() => UnitState::Inactive,
            Some(_) => UnitState::Failed,
        };
        drop(children);
        Ok(state)
    }
}

/// The run's managed home and the vendor's login mount target inside it. The unit
/// bind-mounts the vendor's login directory onto `home/.claude` or `home/.codex`
/// and systemd cannot mount onto a path that does not exist, so the
/// engine makes it before `StartUnit`. The home is group-writable for the worker.
pub(crate) fn mount_target(dir: &std::path::Path, vendor: Vendor) -> std::io::Result<()> {
    let target = match vendor {
        Vendor::Claude => ".claude",
        Vendor::Codex => ".codex",
        Vendor::Fake => return Ok(()),
    };
    let home = crate::launch::create_shared_dir_at(rustix::fs::CWD, &dir.join("home"))?;
    crate::launch::create_shared_dir_at(home, std::path::Path::new(target)).map(drop)
}

#[cfg(test)]
mod tests {
    #[test]
    fn systemd_replies_decode_object_paths_and_variant_states() {
        let call = zbus::Message::method_call(MANAGER_PATH, "StartUnit")
            .destination(SYSTEMD)
            .interface(MANAGER)
            .build(&("dvb-run-fake@test.service", "replace"))
            .unwrap();
        assert_eq!(call.body().signature(), &zbus::signature!("ss"));
        let path = OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/1").unwrap();
        let reply = zbus::Message::method_return(&call.header())
            .build(&path)
            .unwrap();
        assert_eq!(reply.body().signature(), &zbus::signature!("o"));
        assert_eq!(reply.body().deserialize::<OwnedObjectPath>().unwrap(), path);

        let call = zbus::Message::method_call(MANAGER_PATH, "Get")
            .destination(SYSTEMD)
            .interface(PROPERTIES)
            .build(&("org.freedesktop.systemd1.Unit", "ActiveState"))
            .unwrap();
        for state in ["active", "inactive", "failed"] {
            let reply = zbus::Message::method_return(&call.header())
                .build(&zbus::Variant::from(state))
                .unwrap();
            assert_eq!(reply.body().signature(), &zbus::signature!("v"));
            let value: OwnedVariant = reply.body().deserialize().unwrap();
            assert_eq!(String::try_from(value).unwrap(), state);
        }
    }

    #[test]
    fn a_vendor_run_gets_its_login_mount_target_before_start() {
        for (vendor, target) in [(Vendor::Claude, ".claude"), (Vendor::Codex, ".codex")] {
            let dir = tempfile::tempdir().unwrap();
            mount_target(dir.path(), vendor).unwrap();
            assert!(dir.path().join("home").join(target).is_dir());
        }
        let dir = tempfile::tempdir().unwrap();
        mount_target(dir.path(), Vendor::Fake).unwrap();
        assert!(!dir.path().join("home").exists());
    }

    use super::*;

    #[test]
    fn a_unit_name_gives_its_run() {
        assert_eq!(
            unit_run("dvb-run-fake@01JABC.service"),
            Some(RunId::new("01JABC"))
        );
        assert_eq!(unit_run("dvb.service"), None);
        assert_eq!(unit_run("dvb-run-fake@.service"), None);
    }

    #[test]
    fn cgroup_membership_identifies_runs() {
        // A builder runs these tests inside its own dvb run unit. Use fixed
        // memberships so both ordinary services and workers are covered.
        for text in [
            "0::/system.slice/dvb.service\n",
            "0::/user.slice/user-1000.slice/session-1.scope\n",
            "0::/system.slice/dvb-run-fake@.service\n",
            "1:name=systemd:/system.slice/dvb-run-fake@01JABC.service\n",
            "",
        ] {
            assert_eq!(cgroup_run(text), None, "{text:?}");
        }
        for vendor in ["fake", "claude", "codex"] {
            let text = format!(
                "1:cpu:/other\n0::/system.slice/system-dvb\\x2drun\\x2d{vendor}.slice/dvb-run-{vendor}@01JABC.service\n"
            );
            assert_eq!(cgroup_run(&text), Some(RunId::new("01JABC")));
        }
    }

    #[test]
    fn a_missing_process_has_no_run() {
        assert_eq!(run_of_cgroup(-1), None);
    }

    #[test]
    fn parent_pid_reads_proc() {
        let me = i32::try_from(std::process::id()).unwrap();
        assert!(parent_pid(me).is_some_and(|p| p > 0));
    }
}
