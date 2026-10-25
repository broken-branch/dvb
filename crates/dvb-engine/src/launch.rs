//! Run directory and environment preparation, unit start and observation.
use crate::dispatch::BUILD_DIR;
use crate::engine::{AppState, Fail};
use crate::prompt::{render, template};
use crate::spawn;
use crate::units::mount_target;
use dvb_core::event::EndReason;
use dvb_core::launcher::UnitState;
use dvb_core::state::Observed;
use dvb_core::{Actor, Command, EventKind, RunId, Vendor};
use std::future::Future;
use std::time::Duration;
/// In a run's directory: its workspace passed preflight, so it is whole and a
/// resumed run may carry on in it. Moved with the workspace.
pub(crate) const PREPARED: &str = "prepared";
/// In a run's directory: a link to its project's build cache, which the run unit
/// (`BindPaths=-/var/lib/dvb/work/%i/cache`) mounts at the cache's own path.
const CACHE_LINK: &str = "cache";
/// In a run's directory: a link to its project's mirror, which the run unit
/// (`BindReadOnlyPaths=-/var/lib/dvb/work/%i/mirror`) mounts at the mirror's own
/// path under the hidden mirrors directory.
const MIRROR_LINK: &str = "mirror";

pub(crate) const BUILDER_SCRIPT: &str = include_str!("fake_scripts/builder.json");
pub(crate) const REVIEWER_SCRIPT: &str = include_str!("fake_scripts/reviewer.json");

/// The Fake provider's script for a kind of run. In dev mode a test may put its
/// own at `<dir>/fake/<name>.json` (`builder`, `builder-resume`, `reviewer`,
/// `orchestrator-goal`, `orchestrator-approved`, `orchestrator-answer`, `orchestrator-done`).
/// The Fake forge reads a repo's issues from `<dir>/fake/issues/<repo-name>.json`.
pub(crate) fn fake_script(e: &AppState, name: &str, default: &str) -> String {
    let own = e
        .cfg
        .paths
        .state
        .parent()
        .filter(|_| e.cfg.dev)
        .map(|root| root.join("fake").join(format!("{name}.json")))
        .and_then(|f| std::fs::read_to_string(f).ok());
    own.unwrap_or_else(|| default.to_owned())
}

/// Writes the run directory (`env`, the prompt, the Fake script) and starts the
/// run's unit; then watches it until it ends. A resumed builder (`from`, the
/// task's previous builder run) carries on in that run's workspace, moved into
/// its own directory, with `DVB_RESUME=1`. An error means the
/// unit did not start; the caller records how the run ended. The caller has
/// decided the run may start (`dispatch::decide_launch`).
///
/// Boxed with an explicit `Send` so the cycle launch -> watch -> dispatch ->
/// launch type-checks.
pub fn launch(
    e: AppState,
    run: RunId,
    fake_script: String,
    mut vars: Vec<(&'static str, String)>,
    from: Option<RunId>,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move {
        let (r, p, task) = {
            let proj = e.proj();
            let r = proj
                .runs
                .get(&run)
                .cloned()
                .ok_or_else(|| format!("no run {run}"))?;
            let p = proj
                .projects
                .get(&r.project)
                .cloned()
                .ok_or_else(|| format!("no project {}", r.project))?;
            let task = r
                .assignment
                .task
                .as_ref()
                .and_then(|t| proj.tasks.get(t))
                .cloned();
            drop(proj);
            (r, p, task)
        };
        let base = r.assignment.base.clone().ok_or("the run has no base")?;
        let role = p.role(&r.assignment.role).cloned();
        vars.extend(
            crate::prompt::context(&e, &r, &p)
                .await
                .map_err(|f| f.to_string())?,
        );
        vars.push(("project_name", p.view.name.clone()));
        vars.push((
            "role_name",
            role.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
        ));
        vars.push((
            "role_job",
            role.as_ref().map(|r| r.job.clone()).unwrap_or_default(),
        ));
        vars.push((
            "role_lane",
            role.as_ref().map(|r| r.lane.clone()).unwrap_or_default(),
        ));
        if let Some(t) = &task {
            vars.push(("task", t.view.id.to_string()));
            vars.push(("task_title", t.view.title.clone()));
        }

        let dir = e.cfg.paths.work.join(run.as_str());
        let mirror = e.mirror(&p.view.name);
        let cache = e.cfg.paths.mirrors.join(".cache").join(p.view.id.as_str());
        let preserved = from
            .map(|old| e.cfg.paths.work.join(old.as_str()))
            .filter(|old| prepared(old));
        let setup = || -> std::io::Result<()> {
            create_shared_dir(&dir)?;
            mount_target(&dir, r.assignment.vendor)?;
            link_project_cache(&dir, &cache)?;
            replace_link(&dir.join(MIRROR_LINK), &mirror)?;
            if let Some(old) = &preserved {
                // The marker goes first, then the workspace, then the old marker
                // is dropped: whatever instant the engine dies at, one of the two
                // directories has both, so the next start finds a prepared
                // workspace rather than cloning afresh. The marker is made, not
                // copied: a copy would follow a link the old run left in its place.
                write_run_file(&dir.join(PREPARED), "")?;
                std::fs::rename(old.join("ws"), dir.join("ws"))?;
                fail::fail_point!("flow.workspace.moved");
                let _ = std::fs::remove_file(old.join(PREPARED));
            }
            let slug = if r.assignment.task.is_none() {
                dvb_core::roles::ORCHESTRATOR
            } else if r.assignment.reviews.is_some() {
                match r.assignment.review_stage {
                    Some(dvb_core::roles::ReviewKind::Code) => dvb_core::roles::CODE_REVIEWER,
                    Some(dvb_core::roles::ReviewKind::Design) => dvb_core::roles::DESIGN_REVIEWER,
                    None => {
                        if role
                            .as_ref()
                            .is_some_and(|r| r.slug == dvb_core::roles::DESIGN_REVIEWER)
                        {
                            dvb_core::roles::DESIGN_REVIEWER
                        } else {
                            dvb_core::roles::CODE_REVIEWER
                        }
                    }
                }
            } else {
                role.as_ref().map_or("builder", |r| r.slug.as_str())
            };
            write_run_file(&dir.join("prompt.md"), render(template(slug), &vars, false))?;
            if r.assignment.vendor == Vendor::Fake {
                write_run_file(
                    &dir.join("fake-script.json"),
                    render(&fake_script, &vars, true),
                )?;
            }
            let mut env = vec![
                ("DVB_RUN_ID", run.to_string()),
                (
                    "DVB_TASK_ID",
                    r.assignment
                        .task
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ),
                ("DVB_PROJECT", p.view.id.to_string()),
                (
                    "DVB_ROLE",
                    role.as_ref().map(|r| r.slug.clone()).unwrap_or_default(),
                ),
                ("DVB_VENDOR", r.assignment.vendor.as_str().to_owned()),
                ("DVB_MODEL", r.assignment.model.clone()),
                ("DVB_EFFORT", r.assignment.effort.as_str().to_owned()),
                ("DVB_BASE", base.clone()),
                ("DVB_MIRROR", mirror.to_string_lossy().into_owned()),
                (
                    "DVB_RUN_SOCK",
                    e.cfg.paths.run_sock.to_string_lossy().into_owned(),
                ),
                (
                    "DVB_RESUME",
                    if preserved.is_some() { "1" } else { "0" }.to_owned(),
                ),
                (
                    "DVB_PROMPT",
                    dir.join("prompt.md").to_string_lossy().into_owned(),
                ),
                ("DVB_BUILD_CACHE", cache.to_string_lossy().into_owned()),
                (
                    "CARGO_HOME",
                    cache.join("cargo").to_string_lossy().into_owned(),
                ),
                // Build output lives beside the workspace, so the engine can drop it
                // when the run ends without touching the work.
                (
                    "CARGO_TARGET_DIR",
                    dir.join(BUILD_DIR).to_string_lossy().into_owned(),
                ),
            ];
            // A RUSTUP_HOME the run unit sets is not overridden from here.
            if let Some(home) = &e.cfg.rustup_home {
                env.push(("RUSTUP_HOME", home.clone()));
            }
            if r.assignment.vendor == Vendor::Fake {
                env.push((
                    "DVB_FAKE_SCRIPT",
                    dir.join("fake-script.json").to_string_lossy().into_owned(),
                ));
            }
            let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
            write_run_file(&dir.join("env"), text)
        };
        setup().map_err(|err| format!("run directory: {err}"))?;
        // Its time limit counts from here, not from a wait in the queue.
        e.append_checked(Some(&r.project), Actor::Engine, |proj, _now| {
            Ok(proj
                .runs
                .get(&run)
                .filter(|live| live.ended.is_none())
                .map(|_| vec![EventKind::RunLaunched { run: run.clone() }])
                .unwrap_or_default())
        })
        .map_err(|err| err.to_string())?;
        e.start_unit(r.assignment.vendor, &run)
            .await
            .map_err(|err| err.to_string())?;
        fail::fail_point!("effect.start_unit.sent");
        spawn("watch run", watch(e, run, r.assignment.vendor));
        Ok(())
    })
}
/// Makes the directory `path` if it is missing (its parent must exist) and
/// shares it with runs.
pub(crate) fn create_shared_dir(path: &std::path::Path) -> std::io::Result<()> {
    create_shared_dir_at(rustix::fs::CWD, path).map(drop)
}

/// Makes the directory `name` in the directory `parent` if it is missing, shares
/// it with runs and returns it opened. Made under an opened parent, a child of a
/// run-writable directory cannot land elsewhere through a link swapped in for
/// that parent.
pub(crate) fn create_shared_dir_at(
    parent: impl rustix::fd::AsFd,
    name: &std::path::Path,
) -> std::io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::Mode;
    match rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o2770)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(err) => return Err(err.into()),
    }
    share_dir_at(parent, name)
}

/// Gives the directory `path` to group dvb-worker's runs (mode 2770; the group
/// comes from the setgid parent).
pub(crate) fn share_dir(path: &std::path::Path) -> std::io::Result<()> {
    share_dir_at(rustix::fs::CWD, path).map(drop)
}

/// The mode is set on the directory opened without following a link, never by
/// path: a run can swap a fresh entry in a worker-writable parent for a link
/// before the chmod, which would then change the link's target.
fn share_dir_at(
    parent: impl rustix::fd::AsFd,
    name: &std::path::Path,
) -> std::io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags};
    let opened = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    rustix::fs::fchmod(&opened, Mode::from_raw_mode(0o2770))?;
    Ok(opened)
}

/// Whether the run directory `dir` holds a workspace that passed preflight. Its
/// entries are looked at, not followed: the run could have left a link in
/// place of either.
pub(crate) fn prepared(dir: &std::path::Path) -> bool {
    std::fs::symlink_metadata(dir.join("ws")).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(dir.join(PREPARED)).is_ok_and(|m| m.is_file())
}

/// The most the engine reads of a file a run wrote.
const RUN_FILE_MAX: u64 = 64 * 1024;

/// Reads a file in a run-writable directory: never through a link (a run could
/// point one at the engine's database or key), only a regular file (a FIFO
/// would hold the engine), and at most `RUN_FILE_MAX` bytes.
pub(crate) fn read_run_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    open_run_file(path)?
        .take(RUN_FILE_MAX)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Opens a file in a run-writable directory as `read_run_file` reads it: not
/// through a link, and only a regular file.
pub(crate) fn open_run_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{FileType, Mode, OFlags};
    let opened = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if FileType::from_raw_mode(rustix::fs::fstat(&opened)?.st_mode) != FileType::RegularFile {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{}: not a regular file", path.display()),
        ));
    }
    Ok(opened.into())
}

/// Creates the file `path` in a run-writable directory afresh. Whatever entry a
/// run left there is removed first, and `create_new` never follows a link: one
/// planted in between fails the create instead of redirecting it.
pub(crate) fn create_run_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Writes `contents` to `path` as `create_run_file` creates it.
pub(crate) fn write_run_file(
    path: &std::path::Path,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    use std::io::Write;
    create_run_file(path)?.write_all(contents.as_ref())
}

/// The run unit hides every project's cache and mounts back the one the run
/// directory's link names, so a run cannot reach another project's toolchains.
fn link_project_cache(dir: &std::path::Path, cache: &std::path::Path) -> std::io::Result<()> {
    // `.cache` is worker-writable, and systemd follows a bind source on the host:
    // a run under older units could have swapped this entry for a link to another
    // project's cache, so only a real directory is kept.
    match std::fs::symlink_metadata(cache) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            if let Err(err) = std::fs::remove_file(cache)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(err);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    match std::fs::create_dir(cache) {
        Ok(()) => share_dir(cache)?,
        Err(err)
            if err.kind() == std::io::ErrorKind::AlreadyExists
                && std::fs::symlink_metadata(cache).is_ok_and(|meta| meta.is_dir()) => {}
        Err(err) => return Err(err),
    }
    replace_link(&dir.join(CACHE_LINK), cache)
}

fn replace_link(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    if let Err(err) = std::fs::remove_file(link)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(err);
    }
    std::os::unix::fs::symlink(target, link)
}

/// The run units' version: install.sh writes it to `Config::units_version`
/// with the unit files. Raise both when the units change in a way the engine
/// relies on; `dvb update` replaces the binary, never the unit files.
pub(crate) const UNITS_VERSION: &str = "2";

pub(crate) fn install_warnings(cfg: &crate::Config) -> Vec<dvb_core::api::InstallWarning> {
    let Some(stamp) = &cfg.units_version else {
        return Vec::new();
    };
    match std::fs::read_to_string(stamp) {
        Ok(version) if version.trim() == UNITS_VERSION => Vec::new(),
        Ok(_) | Err(_) => vec![dvb_core::api::InstallWarning::CacheIsolationMissing],
    }
}

/// The driver's exit code and one-line cause of a failed run, if it left a
/// `cause` file, cut to `CAUSE_MAX` characters.
pub(crate) fn driver_cause(dir: &std::path::Path) -> Option<String> {
    let text = read_run_file(&dir.join("cause")).ok()?;
    let line: String = text
        .lines()
        .next()?
        .trim()
        .chars()
        .take(CAUSE_MAX)
        .collect();
    (!line.is_empty()).then_some(line)
}

const CAUSE_MAX: usize = 300;

/// Why the run's driver stopped itself, if it left a `reason` file: a model
/// mismatch or an unparseable CLI stream.
pub(crate) fn driver_reason(dir: &std::path::Path) -> Option<EndReason> {
    dvb_core::RunOutcome::from_driver_reason(read_run_file(&dir.join("reason")).ok()?.trim())
        .map(dvb_core::RunOutcome::end_reason)
}

/// Why the engine ends a run whose unit is still active, if it should now:
/// its time limit ran out (`EndReason::TimeLimit`), or it is a reviewer that
/// finished, verdict given and quiet past the grace (`EndReason::Completed`).
/// Both are read from recorded times, so they hold across a restart.
fn due_end(r: &crate::proj::Run, now: dvb_core::Timestamp) -> Option<EndReason> {
    if r.ended.is_some() {
        None
    } else if now >= r.time_limit_at() {
        Some(EndReason::TimeLimit)
    } else if r.finished_review(now) {
        Some(EndReason::Completed)
    } else {
        None
    }
}

/// Ends a live run through the unit stop path. A time limit is recorded
/// first, naming the run, its task, kind and limit.
async fn end_due(e: &AppState, r: &crate::proj::Run, reason: EndReason) -> Result<(), Fail> {
    if reason == EndReason::TimeLimit {
        let limit = dvb_core::caps::time_limit(r.assignment.kind());
        e.append_checked(Some(&r.project), Actor::Engine, |proj, _now| {
            Ok(proj
                .runs
                .get(&r.assignment.run)
                .filter(|live| live.ended.is_none() && !live.time_limited)
                .map(|_| {
                    vec![EventKind::RunTimeLimitReached {
                        run: r.assignment.run.clone(),
                        task: r.assignment.task.clone(),
                        kind: r.assignment.kind(),
                        limit_minutes: limit.as_secs() / 60,
                    }]
                })
                .unwrap_or_default())
        })?;
    }
    crate::dispatch::stop_unit(e, r, reason).await
}

/// A lost unit query leaves the task unknown until a confirmed observation.
/// Keep watching even a prolonged outage; it is not evidence that the run ended.
/// A run past its time limit, or a reviewer that finished but did not exit,
/// is stopped (`due_end`) and ends as a unit that exited would.
pub(crate) async fn watch(e: AppState, run: RunId, vendor: Vendor) -> Result<(), Fail> {
    let mut lost = false;
    let reason = loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        match e.unit_state(vendor, &run).await {
            Ok(UnitState::Active) => {
                lost = false;
                // Behind a check of the task in flight (reconcile on start), which
                // decides it from a later look and would else be settled first.
                let task = e
                    .proj()
                    .runs
                    .get(&run)
                    .and_then(|r| r.assignment.task.clone());
                let _check = match &task {
                    Some(task) => Some(e.lock(task).await),
                    None => None,
                };
                let head = e.proj().runs.get(&run).and_then(|r| r.collected.clone());
                e.run_command(
                    &run,
                    crate::Decision::new(
                        "reconcile",
                        Command::Reconcile(Observed {
                            unit: UnitState::Active,
                            head,
                        }),
                    ),
                )?;
                let now = e.clock.now();
                let due = e
                    .proj()
                    .runs
                    .get(&run)
                    .and_then(|r| due_end(r, now).map(|reason| (r.clone(), reason)));
                if let Some((r, reason)) = due {
                    match end_due(&e, &r, reason.clone()).await {
                        Ok(()) => break reason,
                        // Tried again at the next look.
                        Err(err) => eprintln!("dvb engine: end run {run}: {err}"),
                    }
                }
            }
            Ok(UnitState::Inactive) => break EndReason::Completed,
            Ok(UnitState::Failed) => break EndReason::Crashed,
            Err(err) => {
                e.run_command(
                    &run,
                    crate::Decision::new("unit_unknown", Command::Restarted),
                )?;
                if !lost {
                    eprintln!("dvb engine: run {run}: {err}; still observing");
                    lost = true;
                }
            }
        }
    };
    // The end, like the look above, waits for a check of the task in flight.
    let task = e
        .proj()
        .runs
        .get(&run)
        .and_then(|r| r.assignment.task.clone());
    if let Some(task) = &task {
        drop(e.lock(task).await);
    }
    crate::dispatch::run_ended(&e, &run, reason).await?;
    // A reviewer that already gave its verdict has no UnitEnded decision.
    // Settle the task's unknown effect or stop state after a verdict.
    if let Some(task) = task {
        crate::reconcile::task_once(&e, &task).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn the_drivers_reason_file_maps_to_an_end_reason() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(driver_reason(dir.path()), None);
        std::fs::write(dir.path().join("reason"), "mismatch").unwrap();
        assert_eq!(driver_reason(dir.path()), Some(EndReason::Mismatch));
        std::fs::write(dir.path().join("reason"), "parse_error\n").unwrap();
        assert_eq!(driver_reason(dir.path()), Some(EndReason::ParseError));
    }

    #[test]
    fn a_link_planted_for_a_shared_directory_leaves_its_target_alone() {
        let root = tempfile::tempdir().unwrap();
        let mirrors = root.path().join("mirrors");
        std::fs::create_dir(&mirrors).unwrap();
        std::fs::set_permissions(&mirrors, std::fs::Permissions::from_mode(0o2750)).unwrap();
        let mode = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        let fresh = root.path().join("fresh");
        create_shared_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o2770);
        // As if a run swapped the fresh directory for a link before its mode is set.
        std::fs::remove_dir(&fresh).unwrap();
        std::os::unix::fs::symlink(&mirrors, &fresh).unwrap();
        assert!(share_dir(&fresh).is_err());
        assert!(create_shared_dir(&fresh).is_err());
        assert_eq!(mode(&mirrors), 0o2750);
    }

    #[test]
    fn run_files_are_never_read_or_written_through_a_link() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("dvb.sqlite");
        std::fs::write(&outside, "the engine's data").unwrap();
        let run = root.path().join("run");
        std::fs::create_dir(&run).unwrap();
        let file = run.join("head");
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        assert!(read_run_file(&file).is_err());
        write_run_file(&file, "abc").unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the engine's data"
        );
        assert!(std::fs::symlink_metadata(&file).unwrap().is_file());
        assert_eq!(read_run_file(&file).unwrap(), "abc");
        // A FIFO would hold a reader that waits for its writer.
        let fifo = run.join("reason");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        assert!(read_run_file(&fifo).is_err());
        assert_eq!(driver_reason(&run), None);
    }

    #[test]
    fn a_shared_child_is_made_in_the_opened_parent_not_through_a_link() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let home = root.path().join("home");
        let opened = create_shared_dir_at(rustix::fs::CWD, &home).unwrap();
        // As if a run swapped the home for a link before its child is made.
        let kept = root.path().join("kept");
        std::fs::rename(&home, &kept).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &home).unwrap();
        create_shared_dir_at(opened, std::path::Path::new(".claude")).unwrap();
        assert!(!elsewhere.join(".claude").exists());
        let meta = std::fs::symlink_metadata(kept.join(".claude")).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o7777, 0o2770);
        assert!(create_shared_dir(&home).is_err());
    }

    #[test]
    fn a_planted_cache_entry_is_replaced_by_a_real_directory() {
        let root = tempfile::tempdir().unwrap();
        let caches = root.path().join(".cache");
        let other = caches.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("toolchain"), "other's").unwrap();
        let cache = caches.join("p");
        let run = root.path().join("run");
        std::fs::create_dir(&run).unwrap();
        for plant in [
            |cache: &std::path::Path, other: &std::path::Path| {
                std::os::unix::fs::symlink(other, cache)
            },
            |cache: &std::path::Path, _: &std::path::Path| std::fs::write(cache, "not a dir"),
        ] {
            plant(&cache, &other).unwrap();
            link_project_cache(&run, &cache).unwrap();
            let meta = std::fs::symlink_metadata(&cache).unwrap();
            assert!(meta.is_dir());
            assert_eq!(meta.permissions().mode() & 0o7777, 0o2770);
            assert!(!cache.join("toolchain").exists());
            assert_eq!(std::fs::read_link(run.join(CACHE_LINK)).unwrap(), cache);
            assert_eq!(
                std::fs::read_to_string(other.join("toolchain")).unwrap(),
                "other's"
            );
            std::fs::remove_dir(&cache).unwrap();
        }
        // A real directory, and what the project cached in it, is kept.
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("kept"), "").unwrap();
        link_project_cache(&run, &cache).unwrap();
        assert!(cache.join("kept").exists());
    }

    #[test]
    fn run_units_without_the_current_version_stamp_are_stale() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::Config::dev(dir.path(), "dvb".into());
        assert_eq!(install_warnings(&cfg), []);
        let stamp = dir.path().join("units-version");
        cfg.units_version = Some(stamp.clone());
        let stale = [dvb_core::api::InstallWarning::CacheIsolationMissing];
        assert_eq!(install_warnings(&cfg), stale);
        std::fs::write(&stamp, "0\n").unwrap();
        assert_eq!(install_warnings(&cfg), stale);
        let install = include_str!("../../../install/install.sh");
        assert!(install.contains(&format!(
            "printf '{UNITS_VERSION}\\n' > /var/lib/dvb/state/units-version"
        )));
        std::fs::write(&stamp, format!("{UNITS_VERSION}\n")).unwrap();
        assert_eq!(install_warnings(&cfg), []);
    }

    #[test]
    fn every_fake_script_parses_once_rendered() {
        let vars = [("goal_text", "a \"quoted\"\ngoal".to_owned())];
        for s in [BUILDER_SCRIPT, REVIEWER_SCRIPT] {
            serde_json::from_str::<serde_json::Value>(&render(s, &vars, true)).unwrap();
        }
    }
}
