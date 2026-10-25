fn values<'a>(unit: &'a str, key: &str) -> Vec<&'a str> {
    unit.lines()
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .flat_map(str::split_whitespace)
        .collect()
}

#[test]
fn run_units_hide_siblings_and_protect_launch_metadata() {
    let common = include_str!("../../../install/dvb-run-.service.d/10-common.conf");
    for (vendor, unit) in [
        (
            "claude",
            include_str!("../../../install/dvb-run-claude@.service"),
        ),
        (
            "codex",
            include_str!("../../../install/dvb-run-codex@.service"),
        ),
        (
            "fake",
            include_str!("../../../install/dvb-run-fake@.service"),
        ),
    ] {
        let effective = format!("{unit}\n{common}");
        let unit = effective.as_str();
        assert_eq!(values(unit, "User"), ["dvb-worker"], "{vendor}");
        assert_eq!(values(unit, "PrivateUsers"), ["yes"], "{vendor}");
        assert_eq!(
            values(unit, "TemporaryFileSystem"),
            ["/var/lib/dvb/work:ro", "/var/lib/dvb/mirrors:ro"],
            "{vendor}"
        );
        let mut writable = Vec::new();
        if vendor != "fake" {
            writable.push(format!(
                "/var/lib/dvb/logins/{vendor}:/var/lib/dvb/work/%i/home/.{vendor}"
            ));
        }
        writable.push("/var/lib/dvb/work/%i".to_owned());
        writable.push("-/var/lib/dvb/work/%i/cache".to_owned());
        assert_eq!(values(unit, "BindPaths"), writable, "{vendor}");
        let mut metadata = Vec::new();
        if vendor == "fake" {
            metadata.push("/var/lib/dvb/work/%i/fake-script.json");
        }
        // Login runs do not have a project mirror.
        metadata.push("-/var/lib/dvb/work/%i/mirror");
        metadata.push("/var/lib/dvb/work/%i/env");
        // Login runs do not have a rendered prompt.
        metadata.push("-/var/lib/dvb/work/%i/prompt.md");
        assert_eq!(values(unit, "BindReadOnlyPaths"), metadata, "{vendor}");
        assert_eq!(
            values(unit, "EnvironmentFile"),
            ["/var/lib/dvb/work/%i/env"],
            "{vendor}"
        );
        assert_eq!(
            values(unit, "InaccessiblePaths"),
            ["/var/lib/dvb/logins"],
            "{vendor}"
        );
        assert!(values(unit, "ReadWritePaths").is_empty(), "{vendor}");
    }
}

#[test]
fn invalid_effort_returns_the_run_configuration_error_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dvb"))
        .args(["run", "exec", "effort-test"])
        .current_dir(dir.path())
        .env_clear()
        .env("DVB_VENDOR", "fake")
        .env("DVB_EFFORT", "medum")
        .env("DVB_RUN_ID", "effort-test")
        .env("DVB_MODEL", "fake-1")
        .env("DVB_BASE", "base")
        .env("DVB_MIRROR", dir.path())
        .env("DVB_RUN_SOCK", dir.path().join("missing.sock"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown effort medum"),
        "{out:?}"
    );
    assert!(!dir.path().join("ws").exists());
}

#[path = "../../../tests/support/executable.rs"]
mod executable;

#[test]
fn a_run_stopped_with_sigterm_leaves_its_directories_removable() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work/r1");
    std::fs::create_dir_all(&dir).unwrap();
    // A login that makes a private directory, as a browser does, then waits
    // for the owner's code.
    let cli = tmp.path().join("claude");
    executable::write_executable(
        &cli,
        "#!/bin/sh\nmkdir -p \"$HOME/.cache/dconf\" && chmod 2700 \"$HOME/.cache/dconf\"\n\
         echo 'Visit https://claude.example/authorize to sign in'\nexec sleep 60\n",
    );
    let mut run = std::process::Command::new(env!("CARGO_BIN_EXE_dvb"))
        .args(["run", "exec", "r1"])
        .current_dir(&dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DVB_VENDOR", "claude")
        .env("DVB_RUN_ID", "r1")
        .env("DVB_MODEL", "")
        .env("DVB_BASE", "")
        .env("DVB_MIRROR", "")
        .env("DVB_RUN_SOCK", tmp.path().join("missing.sock"))
        .env("DVB_LOGIN", "1")
        .env("DVB_CLI_BIN", &cli)
        // The whole group is signalled, as a unit's stop signals its cgroup.
        .process_group(0)
        .spawn()
        .unwrap();
    let waiting = (0..400).any(|_| {
        if dir.join("login.json").exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        false
    });
    assert!(waiting, "the login never waited for its code");
    let private = dir.join("home/.cache/dconf");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&private), 0o2700);
    let group = format!("-{}", run.id());
    let kill = std::process::Command::new("kill")
        .args(["-TERM", "--", &group])
        .status()
        .unwrap();
    assert!(kill.success());
    // A clean exit, as a death by SIGTERM is to systemd.
    assert_eq!(run.wait().unwrap().code(), Some(0));
    assert_eq!(mode(&private), 0o2770);
    std::fs::remove_dir_all(&dir).unwrap();
}
