//! The preflight: every check, run inside the real unit before any
//! model spend. Each check has a code (`PreflightCode`) and a fix hint in
//! `prompts/preflight.md`; a failed check's detail carries its hint. The checks
//! read the unit's actual environment, so drift shows up as a refusal.

use crate::exec::RunEnv;
use crate::{claude, codex};
use dvb_core::Vendor;
use dvb_core::event::{PreflightCheck, PreflightCode};
use dvb_core::git::Git;
use dvb_core::run_verbs::PreflightBody;
use std::path::{Path, PathBuf};
use std::process::Command;

const HINTS: &str = include_str!("../../../prompts/preflight.md");

/// The fix hint for a code, from the table in `prompts/preflight.md`.
pub fn hint(code: PreflightCode) -> Option<&'static str> {
    let name = format!("`{}`", code.as_str());
    HINTS.lines().find_map(|l| {
        let cells: Vec<&str> = l.split('|').map(str::trim).collect();
        (cells.get(1) == Some(&name.as_str()))
            .then(|| cells.get(3).copied())
            .flatten()
    })
}

type Probe = Result<String, String>;

fn check(code: PreflightCode, p: Probe) -> PreflightCheck {
    let (ok, mut detail) = match p {
        Ok(d) => (true, d),
        Err(d) => (false, d),
    };
    if !ok && let Some(h) = hint(code) {
        detail = format!("{detail}. Fix: {h}");
    }
    PreflightCheck { code, ok, detail }
}

pub fn user_name(uid: u32) -> Option<String> {
    std::fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            (f.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_owned())
        })
}

/// This process's cgroup and, when it is a dvb run unit, the unit's name.
pub fn own_unit() -> Option<(String, String)> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.to_owned();
    let name = path.rsplit('/').next()?.to_owned();
    name.starts_with("dvb-run-").then_some((path, name))
}

// The boundary checks, as functions of what they observe, so each can be broken
// in a test without a worker user or a unit.

pub fn uid_probe(dev: bool, user: &str, uid: u32) -> Probe {
    if dev {
        Ok(format!(
            "dev mode: runs as {user} (uid {uid}), the engine's user"
        ))
    } else if user == "dvb-worker" {
        Ok(format!("runs as {user} (uid {uid})"))
    } else {
        Err(format!("runs as {user} (uid {uid}), not dvb-worker"))
    }
}

/// What of the engine's private state this process can reach: the directory
/// listing, and read-only opens of the store and the App key themselves (a worker
/// that cannot list the directory may still open a file whose name it knows).
/// Nothing is read from the files.
pub fn reachable_state(state: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if std::fs::read_dir(state).is_ok() {
        found.push(format!("{} (listing)", state.display()));
    }
    for name in ["dvb.sqlite", "github-app.pem"] {
        let p = state.join(name);
        if std::fs::File::open(&p).is_ok() {
            found.push(p.display().to_string());
        }
    }
    found
}

pub fn store_probe(dev: bool, state: &Path, reachable: &[String]) -> Probe {
    match (reachable.is_empty(), dev) {
        (true, _) => Ok(format!("{} not readable", state.display())),
        (false, true) => Ok(format!(
            "dev mode: {} is readable (no boundary)",
            reachable.join(", ")
        )),
        (false, false) => Err(format!("readable: {}", reachable.join(", "))),
    }
}

pub fn memory_probe(dev: bool, unit: Option<&str>, memory_max: Option<&str>) -> Probe {
    if dev {
        return Ok("dev mode: no unit, no memory cap".into());
    }
    let Some(unit) = unit else {
        return Err("not in a dvb run unit".into());
    };
    Ok(match memory_max {
        Some(raw) => format!("{unit}: memory.max {}", raw.trim()),
        None => format!("{unit}: memory.max cannot be read"),
    })
}

/// What the process sees of its surroundings (faked in tests).
#[derive(Debug, Clone)]
pub struct Ambient {
    pub home: String,
    /// Names of the environment variables that are set.
    pub vars: Vec<String>,
}

impl Ambient {
    pub fn real() -> Self {
        Self {
            home: std::env::var("HOME").unwrap_or_default(),
            vars: std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .collect(),
        }
    }
}

fn home_probe(env: &RunEnv, amb: &Ambient) -> Probe {
    let home = amb.home.clone();
    let managed = std::fs::canonicalize(&home)
        .ok()
        .zip(std::fs::canonicalize(env.home()).ok())
        .is_some_and(|(a, b)| a == b);
    if managed {
        Ok(format!("HOME is {home}"))
    } else {
        Err(format!("HOME is {home}, not {}", env.home().display()))
    }
}

fn cred_probe(amb: &Ambient) -> Probe {
    let visible: Vec<String> = ["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"]
        .iter()
        .filter(|k| amb.vars.iter().any(|v| v == *k))
        .map(|k| format!("${k}"))
        .collect();
    let api_keys: Vec<String> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .filter(|k| amb.vars.iter().any(|v| v == *k))
        .map(|k| format!("${k}"))
        .collect();
    let mut detail = if visible.is_empty() {
        "no GitHub or SSH credential visible".into()
    } else {
        format!("visible: {}", visible.join(", "))
    };
    if !api_keys.is_empty() {
        detail.push_str(&format!("; API keys visible: {}", api_keys.join(", ")));
    }
    if visible.is_empty() {
        Ok(detail)
    } else {
        Err(detail)
    }
}

async fn ws_probe(env: &RunEnv) -> Probe {
    let head = Git::new(&env.ws())
        .run(&["rev-parse", "HEAD"], &[])
        .await
        .unwrap_or_default();
    // A resumed workspace may hold the worker's own commits on the base.
    let resumed_on_base = crate::exec::resuming()
        && Git::new(&env.ws())
            .run(&["merge-base", "--is-ancestor", &env.base, &head], &[])
            .await
            .is_ok();
    if head == env.base || resumed_on_base {
        Ok(format!("workspace at {head}"))
    } else {
        Err(format!("workspace at {head:?}, expected {}", env.base))
    }
}

/// The vendor's CLI: `DVB_CLI_BIN`, else an executable of the vendor's name on `PATH`.
pub fn find_cli(env: &RunEnv) -> Option<PathBuf> {
    if let Some(p) = &env.cli_bin {
        return p.is_file().then(|| p.clone());
    }
    crate::exec::on_path(&std::env::var_os("PATH")?, env.vendor.as_str())
}

fn version_of(bin: &Path) -> Option<String> {
    let out = crate::child::output(
        Command::new(bin).arg("--version"),
        crate::child::PROBE_TIMEOUT,
    )
    .ok()?;
    out.status
        .success()
        .then(|| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_owned()
        })
        .filter(|v| !v.is_empty())
}

fn cli_probe(env: &RunEnv, bin: Option<&Path>, version: Option<&str>) -> Probe {
    let recorded = match env.vendor {
        Vendor::Claude => claude::RECORDED_VERSION,
        Vendor::Codex => codex::RECORDED_VERSION,
        Vendor::Fake => "",
    };
    match (bin, version) {
        (Some(b), Some(v)) if v.contains(recorded) => Ok(format!("{} ({v})", b.display())),
        (Some(b), Some(v)) => Ok(format!(
            "{} ({v}; the parser was recorded against {recorded})",
            b.display()
        )),
        (Some(b), None) => Err(format!("{} does not run", b.display())),
        (None, _) => Err(format!("{} is not on PATH", env.vendor.as_str())),
    }
}

fn login_probe(env: &RunEnv) -> Probe {
    let file = match env.vendor {
        Vendor::Claude => env.home().join(".claude/.credentials.json"),
        Vendor::Codex => env.home().join(".codex/auth.json"),
        Vendor::Fake => return Ok("fake provider".into()),
    };
    match std::fs::metadata(&file) {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(format!("{} exists", file.display())),
        Ok(_) | Err(_) => Err(format!(
            "no {} login ({} is missing)",
            env.vendor.as_str(),
            file.display()
        )),
    }
}

/// A full model id is `claude-...` or `gpt-...`: not an alias such as `opus`.
fn full_id(vendor: Vendor, id: &str) -> bool {
    let prefix = match vendor {
        Vendor::Claude => "claude-",
        Vendor::Codex => "gpt-",
        Vendor::Fake => return true,
    };
    id.starts_with(prefix)
        && id.len() > prefix.len()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

fn model_probe(env: &RunEnv, cli: Option<&Path>, login_ok: bool) -> Probe {
    let id = &env.model;
    if !full_id(env.vendor, id) {
        return Err(format!("{id:?} is not a full model id"));
    }
    match env.vendor {
        Vendor::Claude => Ok(format!("claude will ask for {id} (no fallback model)")),
        Vendor::Codex => match (cli, login_ok) {
            (Some(bin), true) => {
                let listed = codex::list_models(bin, &env.home(), &env.dir)
                    .map_err(|e| format!("model/list failed: {e}"))?;
                if listed.iter().any(|m| m == id) {
                    Ok(format!("{id} is in model/list ({} models)", listed.len()))
                } else {
                    Err(format!("{id} is not in the account's model/list"))
                }
            }
            (_, _) => Ok(format!(
                "{id} is a full id; model/list skipped (no CLI or login)"
            )),
        },
        Vendor::Fake => Ok("fake provider".into()),
    }
}

/// Parses `codex features list`: the named features' enabled flags.
pub fn enabled_features(list: &str, names: &[&str]) -> Vec<String> {
    list.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let name = w.next()?;
            let on = w.last()? == "true";
            (on && names.contains(&name)).then(|| name.to_owned())
        })
        .collect()
}

/// Codex runs only: the run's settings already turn Claude automatic memory off.
fn memory_feature_probe(env: &RunEnv, cli: Option<&Path>) -> Probe {
    let bin = cli.ok_or("no codex CLI to ask")?;
    let out = crate::child::output(
        Command::new(bin)
            .args(["features", "list"])
            .env("HOME", env.home()),
        crate::child::PROBE_TIMEOUT,
    )
    .map_err(|e| format!("codex features list: {e}"))?;
    if !out.status.success() {
        return Err("codex features list failed".into());
    }
    let on = enabled_features(
        &String::from_utf8_lossy(&out.stdout),
        crate::codex::FEATURES_OFF,
    );
    if on.is_empty() {
        Ok("codex memories and fast_mode are off".into())
    } else {
        Err(format!("codex features on: {}", on.join(", ")))
    }
}

/// All checks for this run.
pub async fn run(env: &RunEnv) -> PreflightBody {
    run_in(env, &Ambient::real()).await
}

pub async fn run_in(env: &RunEnv, amb: &Ambient) -> PreflightBody {
    let uid = dvb_core::io::own_uid();
    let user = user_name(uid).unwrap_or_else(|| uid.to_string());
    let unit = own_unit();
    let state = env.state_dir().unwrap_or_default();
    let mut checks = vec![
        check(PreflightCode::PfUid, uid_probe(env.dev, &user, uid)),
        check(PreflightCode::PfHome, home_probe(env, amb)),
        check(PreflightCode::PfCredVisible, cred_probe(amb)),
        check(
            PreflightCode::PfStoreVisible,
            store_probe(env.dev, &state, &reachable_state(&state)),
        ),
        check(PreflightCode::PfWsBase, ws_probe(env).await),
    ];
    let mut cli_version = Some("fake provider".to_owned());
    if env.vendor != Vendor::Fake {
        let bin = find_cli(env);
        let version = bin.as_deref().and_then(version_of);
        cli_version.clone_from(&version);
        let cli = cli_probe(env, bin.as_deref(), version.as_deref());
        let cli_ok = cli.is_ok();
        let login = login_probe(env);
        let login_ok = login.is_ok();
        checks.push(check(PreflightCode::PfCliMissing, cli));
        checks.push(check(PreflightCode::PfLoginMissing, login));
        let usable = bin.as_deref().filter(|_| cli_ok);
        checks.push(check(
            PreflightCode::PfModelUnknown,
            model_probe(env, usable, login_ok),
        ));
        if env.vendor == Vendor::Codex {
            checks.push(check(
                PreflightCode::PfMemoryFeature,
                memory_feature_probe(env, usable),
            ));
        }
    }
    let max = unit
        .as_ref()
        .and_then(|(cg, _)| std::fs::read_to_string(format!("/sys/fs/cgroup{cg}/memory.max")).ok());
    checks.push(check(
        PreflightCode::PfMemoryCap,
        memory_probe(
            env.dev,
            unit.as_ref().map(|(_, n)| n.as_str()),
            max.as_deref(),
        ),
    ));
    let boundary = match (&unit, env.dev) {
        (_, true) => format!("dev mode: a child process of the engine, as {user}"),
        (Some((_, name)), false) => format!("{name} as {user}"),
        (None, false) => format!("no unit, as {user}"),
    };
    PreflightBody {
        checks,
        cli_version,
        boundary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PreflightCode; 10] = [
        PreflightCode::PfUid,
        PreflightCode::PfHome,
        PreflightCode::PfCredVisible,
        PreflightCode::PfStoreVisible,
        PreflightCode::PfWsBase,
        PreflightCode::PfCliMissing,
        PreflightCode::PfLoginMissing,
        PreflightCode::PfModelUnknown,
        PreflightCode::PfMemoryCap,
        PreflightCode::PfMemoryFeature,
    ];

    /// A hermetic run directory: a clean clone at the base, a login, stub CLIs.
    struct Fixture {
        _tmp: tempfile::TempDir,
        env: RunEnv,
    }

    fn script(path: &Path, body: &str) {
        crate::testutil::write_executable(path, &format!("#!/bin/sh\n{body}\n"));
    }

    async fn fixture(vendor: Vendor, model: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir = root.join("work/r1");
        let origin = root.join("origin");
        std::fs::create_dir_all(&dir).unwrap();
        let who = [
            ("GIT_AUTHOR_NAME", "n"),
            ("GIT_AUTHOR_EMAIL", "n@example.invalid"),
            ("GIT_COMMITTER_NAME", "n"),
            ("GIT_COMMITTER_EMAIL", "n@example.invalid"),
        ];
        Git::new(&root)
            .run(&["init", "--quiet", origin.to_str().unwrap()], &[])
            .await
            .unwrap();
        std::fs::write(origin.join("a.txt"), "a").unwrap();
        Git::new(&origin).run(&["add", "."], &[]).await.unwrap();
        Git::new(&origin)
            .run(&["commit", "--quiet", "-m", "base"], &who)
            .await
            .unwrap();
        let base = Git::new(&origin)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        let mut env = crate::testutil::run_env(&dir, vendor, model);
        env.base = base;
        env.mirror = origin;
        crate::exec::prepare(&env).await.unwrap();
        let bin = root.join("stub-bin");
        std::fs::create_dir_all(&bin).unwrap();
        match vendor {
            Vendor::Claude => {
                script(&bin.join("claude"), "echo '2.1.286 (Claude Code)'");
                env.cli_bin = Some(bin.join("claude"));
            }
            Vendor::Codex => {
                script(
                    &bin.join("codex"),
                    "case \"$1\" in --version) echo 'codex-cli 0.159.2';; features) printf 'memories stable \
                false\\nfast_mode stable false\\n';; esac",
                );
                env.cli_bin = Some(bin.join("codex"));
            }
            Vendor::Fake => {}
        }
        Fixture { _tmp: tmp, env }
    }

    /// The unit's view: HOME is the run's home and no credential variable is set.
    async fn run(env: &RunEnv) -> PreflightBody {
        run_in(
            env,
            &Ambient {
                home: env.home().to_string_lossy().into_owned(),
                vars: Vec::new(),
            },
        )
        .await
    }

    fn failing(body: &PreflightBody) -> Vec<PreflightCode> {
        body.checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.code)
            .collect()
    }

    #[test]
    fn every_code_has_a_hint_in_the_table() {
        for code in ALL {
            assert!(hint(code).is_some_and(|h| !h.is_empty()), "{code:?}");
        }
    }

    #[test]
    fn a_failed_check_carries_its_hint() {
        let c = check(PreflightCode::PfLoginMissing, Err("no login".into()));
        assert!(!c.ok);
        assert!(
            c.detail.contains("Fix: `dvb login <vendor>`"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn pf_uid() {
        assert!(uid_probe(false, "dvb-worker", 998).is_ok());
        assert!(uid_probe(false, "example-user", 1000).is_err());
        assert!(uid_probe(true, "example-user", 1000).is_ok());
    }

    #[test]
    fn pf_store_visible() {
        let s = Path::new("/var/lib/dvb/state");
        let seen = vec!["/var/lib/dvb/state/github-app.pem".to_owned()];
        assert!(store_probe(false, s, &[]).is_ok());
        assert!(store_probe(false, s, &seen).is_err());
        assert!(store_probe(true, s, &seen).is_ok());
    }

    #[test]
    fn a_state_directory_that_cannot_be_listed_still_exposes_a_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("github-app.pem"), "not a key").unwrap();
        // Execute but no read: names resolve, listing is denied.
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o311)).unwrap();
        let seen = reachable_state(&state);
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].ends_with("github-app.pem"));
        assert!(store_probe(false, &state, &seen).is_err());

        // Locked down: nothing reachable.
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).unwrap();
        let none = reachable_state(&state);
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(none.is_empty(), "{none:?}");
    }

    #[test]
    fn pf_memory_cap() {
        let u = Some("dvb-run-claude@r1.service");
        for value in ["8589934592", "17179869184", "max"] {
            assert_eq!(
                memory_probe(false, u, Some(&format!("{value}\n"))),
                Ok(format!("dvb-run-claude@r1.service: memory.max {value}"))
            );
        }
        assert_eq!(
            memory_probe(false, u, None),
            Ok("dvb-run-claude@r1.service: memory.max cannot be read".into())
        );
        let c = check(PreflightCode::PfMemoryCap, memory_probe(false, None, None));
        assert!(!c.ok);
        assert_eq!(c.code, PreflightCode::PfMemoryCap);
        assert_eq!(
            c.detail,
            "not in a dvb run unit. Fix: Reinstall the unit files"
        );
        assert!(memory_probe(true, None, None).is_ok());
    }

    fn login(f: &Fixture) {
        let (dir, file) = match f.env.vendor {
            Vendor::Claude => (".claude", ".credentials.json"),
            Vendor::Codex | Vendor::Fake => (".codex", "auth.json"),
        };
        std::fs::create_dir_all(f.env.home().join(dir)).unwrap();
        std::fs::write(f.env.home().join(dir).join(file), "{\"x\":1}").unwrap();
    }

    #[tokio::test]
    async fn a_good_fake_run_passes_all_its_checks() {
        let f = fixture(Vendor::Fake, "fake-1").await;
        let body = run(&f.env).await;
        assert_eq!(failing(&body), vec![]);
        assert_eq!(body.checks.len(), 6);
    }

    #[tokio::test]
    async fn pf_ws_base() {
        let mut f = fixture(Vendor::Fake, "fake-1").await;
        f.env.base = "0".repeat(40);
        assert!(failing(&run(&f.env).await).contains(&PreflightCode::PfWsBase));
    }

    #[tokio::test]
    async fn pf_home() {
        let f = fixture(Vendor::Fake, "fake-1").await;
        let body = run_in(
            &f.env,
            &Ambient {
                home: "/home/someone".into(),
                vars: Vec::new(),
            },
        )
        .await;
        assert_eq!(failing(&body), vec![PreflightCode::PfHome]);
    }

    #[tokio::test]
    async fn api_keys_are_reported_without_failing_preflight() {
        let f = fixture(Vendor::Fake, "fake-1").await;
        for keys in [
            vec!["ANTHROPIC_API_KEY"],
            vec!["OPENAI_API_KEY"],
            vec!["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        ] {
            let body = run_in(
                &f.env,
                &Ambient {
                    home: f.env.home().to_string_lossy().into_owned(),
                    vars: keys
                        .iter()
                        .copied()
                        .chain(["PATH"])
                        .map(str::to_owned)
                        .collect(),
                },
            )
            .await;
            assert_eq!(failing(&body), vec![], "{:?}", body.checks);
            let c = body
                .checks
                .iter()
                .find(|c| c.code == PreflightCode::PfCredVisible)
                .unwrap();
            let names: Vec<_> = keys.iter().map(|k| format!("${k}")).collect();
            assert_eq!(
                c.detail,
                format!(
                    "no GitHub or SSH credential visible; API keys visible: {}",
                    names.join(", ")
                )
            );
        }
    }

    #[tokio::test]
    async fn pf_cred_visible_for_github_tokens_or_an_ssh_agent() {
        let f = fixture(Vendor::Fake, "fake-1").await;
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"] {
            let body = run_in(
                &f.env,
                &Ambient {
                    home: f.env.home().to_string_lossy().into_owned(),
                    vars: vec![
                        name.into(),
                        "ANTHROPIC_API_KEY".into(),
                        "OPENAI_API_KEY".into(),
                        "PATH".into(),
                    ],
                },
            )
            .await;
            assert_eq!(failing(&body), vec![PreflightCode::PfCredVisible]);
            let c = body.checks.iter().find(|c| !c.ok).unwrap();
            assert_eq!(
                c.detail,
                format!(
                    "visible: ${name}; API keys visible: $ANTHROPIC_API_KEY, $OPENAI_API_KEY. \
                     Fix: Remove it from the worker's reach; see the install notes"
                )
            );
        }
    }

    #[tokio::test]
    async fn a_good_claude_run_passes() {
        let f = fixture(Vendor::Claude, "claude-sonnet-5-5").await;
        login(&f);
        let body = run(&f.env).await;
        assert_eq!(failing(&body), vec![], "{:?}", body.checks);
        assert_eq!(body.checks.len(), 9);
        assert_eq!(body.cli_version.as_deref(), Some("2.1.286 (Claude Code)"));
    }

    #[tokio::test]
    async fn claude_pf_cli_missing() {
        let mut f = fixture(Vendor::Claude, "claude-sonnet-5-5").await;
        login(&f);
        f.env.cli_bin = Some(f.env.dir.join("no-such-claude"));
        assert_eq!(
            failing(&run(&f.env).await),
            vec![PreflightCode::PfCliMissing]
        );
    }

    #[tokio::test]
    async fn claude_pf_login_missing() {
        let f = fixture(Vendor::Claude, "claude-sonnet-5-5").await;
        let body = run(&f.env).await;
        assert_eq!(failing(&body), vec![PreflightCode::PfLoginMissing]);
        let c = body
            .checks
            .iter()
            .find(|c| !c.ok && c.code == PreflightCode::PfLoginMissing)
            .unwrap();
        assert!(c.detail.contains("dvb login <vendor>"));
    }

    #[tokio::test]
    async fn claude_pf_model_unknown_for_an_alias() {
        let f = fixture(Vendor::Claude, "opus").await;
        login(&f);
        assert_eq!(
            failing(&run(&f.env).await),
            vec![PreflightCode::PfModelUnknown]
        );
    }

    #[tokio::test]
    async fn codex_good_and_pf_cli_missing_and_login_missing() {
        let mut f = fixture(Vendor::Codex, "gpt-6.1-sol").await;
        // No login yet: only the login fails (model/list is skipped without one).
        let body = run(&f.env).await;
        assert_eq!(
            failing(&body),
            vec![PreflightCode::PfLoginMissing],
            "{:?}",
            body.checks
        );
        f.env.cli_bin = Some(f.env.dir.join("no-such-codex"));
        let body = run(&f.env).await;
        assert!(failing(&body).contains(&PreflightCode::PfCliMissing));
    }

    #[tokio::test]
    async fn codex_pf_memory_feature() {
        let f = fixture(Vendor::Codex, "gpt-6.1-sol").await;
        login(&f);
        let bin = f.env.cli_bin.clone().unwrap();
        for feature in ["memories", "fast_mode"] {
            script(
                &bin,
                &format!(
                    "case \"$1\" in --version) echo 'codex-cli 0.159.2';; \
                     features) printf '{feature} stable true\\n';; \
                     app-server) read l; echo '{{\"id\":1,\"result\":{{}}}}'; read l; read l; \
                     echo '{{\"id\":2,\"result\":{{\"data\":[{{\"id\":\"gpt-6.1-sol\"\
                }}]}}}}'; read l;; esac"
                ),
            );
            let body = run(&f.env).await;
            assert_eq!(failing(&body), vec![PreflightCode::PfMemoryFeature]);
            let c = body
                .checks
                .iter()
                .find(|c| c.code == PreflightCode::PfMemoryFeature)
                .unwrap();
            assert!(c.detail.contains(feature));
        }
    }

    #[tokio::test]
    async fn codex_pf_model_unknown_against_the_model_list() {
        let mut f = fixture(Vendor::Codex, "gpt-6.1-sol").await;
        login(&f);
        // A stub app-server that answers initialize and model/list from the fixture.
        let list = include_str!("../tests/fixtures/codex-model-list.json")
            .trim()
            .to_owned();
        let bin = f.env.cli_bin.clone().unwrap();
        script(
            &bin,
            &format!(
                "case \"$1\" in --version) echo 'codex-cli 0.159.2';; features) printf 'memories stable \
                false\\n';; \
                 app-server) read l; echo '{{\"id\":1,\"result\":{{}}}}'; read l; read \
                l; echo '{{\"id\":2,\"result\":{list}}}'; read l;; esac"
            ),
        );
        assert_eq!(failing(&run(&f.env).await), vec![]);
        f.env.model = "gpt-bogus-9".into();
        assert_eq!(
            failing(&run(&f.env).await),
            vec![PreflightCode::PfModelUnknown]
        );
    }

    #[test]
    fn enabled_features_reads_the_list() {
        let list = "memories    stable    true\nfast_mode  stable  false\napps stable true\n";
        assert_eq!(
            enabled_features(list, &["memories", "fast_mode", "apps"]),
            ["memories", "apps"]
        );
    }

    #[test]
    fn a_full_id_is_not_an_alias() {
        assert!(full_id(Vendor::Claude, "claude-sonnet-5-5"));
        assert!(!full_id(Vendor::Claude, "sonnet"));
        assert!(!full_id(Vendor::Claude, "claude-"));
        assert!(full_id(Vendor::Codex, "gpt-6.1-sol"));
        assert!(!full_id(Vendor::Codex, "sol"));
    }
}
