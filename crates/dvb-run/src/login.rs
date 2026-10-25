//! A login run: the vendor's own login command inside the vendor's
//! unit, as the worker user, writing into the shared login directory that the unit
//! mounts. The run talks to the engine through files in its run directory, so it
//! needs no run-socket registration:
//!
//! - `login.json` `{url, prompt}`, once the command has printed what the owner needs;
//! - `code`, written by the engine when the owner pastes the vendor's code (Claude);
//!   read once and deleted;
//! - `login-result`, `ok` or `failed: <why>`, when the command has ended.
//!
//! `claude auth login` prints a URL and takes the pasted code on stdin;
//! `codex login --device-auth` prints a URL and a device code and waits for the
//! page. Neither ends up in an event or a log: the code file is deleted and the
//! output is only kept in memory.

use crate::child::KillOnDrop;
use crate::exec::RunEnv;
use dvb_core::io::write_handoff;
use dvb_core::{RunOutcome, Vendor};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long each wait lasts.
#[derive(Debug, Clone, Copy)]
pub struct Waits {
    /// For the command to print its URL.
    pub url: Duration,
    /// For the owner to finish (paste the code, or use the device page).
    pub owner: Duration,
}

impl Default for Waits {
    fn default() -> Self {
        Self {
            url: Duration::from_secs(45),
            owner: Duration::from_secs(15 * 60),
        }
    }
}

/// The first `https://` word.
pub fn find_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|w| w.starts_with("https://"))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_owned())
}

/// A device code such as `ABCD-1234`.
pub fn find_device_code(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|w| {
        let (a, b) = w.split_once('-')?;
        let ok = |s: &str, min: usize| {
            s.len() >= min
                && s.bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        };
        (ok(a, 4) && ok(b, 4) && a.len() <= 6 && b.len() <= 8).then(|| w.to_owned())
    })
}

/// Writes the result, then removes a code that arrived too late to be read. The
/// engine, which checks for the result after writing a code, removes it too.
fn write_result(dir: &Path, text: &str) {
    let _ = write_handoff(&dir.join("login-result"), text);
    let _ = std::fs::remove_file(dir.join("code"));
}

/// The login run. Exit codes: 0 logged in, 2 setup failed, 4 the login failed.
pub fn run(env: &RunEnv, waits: Waits) -> RunOutcome {
    match login(env, waits) {
        Ok(()) => {
            write_result(&env.dir, "ok");
            RunOutcome::Completed
        }
        Err(e) => {
            eprintln!("login: {e}");
            write_result(&env.dir, &format!("failed: {e}"));
            RunOutcome::SessionFailed
        }
    }
}

fn login(env: &RunEnv, waits: Waits) -> Result<(), String> {
    std::fs::create_dir_all(env.home()).map_err(|e| format!("home: {e}"))?;
    let (args, pastes): (&[&str], bool) = match env.vendor {
        Vendor::Claude => (&["auth", "login"], true),
        Vendor::Codex => (&["login", "--device-auth"], false),
        Vendor::Fake => return Err("the fake vendor has no login".into()),
    };
    let bin = crate::preflight::find_cli(env)
        .ok_or_else(|| format!("{} is not on PATH", env.vendor.as_str()))?;
    let mut child = KillOnDrop(
        Command::new(bin)
            .args(args)
            .current_dir(&env.dir)
            .env("HOME", env.home())
            .env("BROWSER", "false")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", env.vendor.as_str()))?,
    );
    let mut stdin = child.stdin.take();
    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(dvb_core::text::plain(&line)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    // The URL, and for a device login the code that goes with it.
    let (mut url, mut device) = (None, None);
    let mut last = String::new();
    // Counted, not timed: the clock is dvb_core::Clock's alone.
    let mut polls = waits.url.as_millis() / 200;
    while polls > 0 && !(url.is_some() && (pastes || device.is_some())) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                url = url.or_else(|| find_url(&line));
                device = device.or_else(|| find_device_code(&line));
                if !line.trim().is_empty() {
                    last = line;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => polls -= 1,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let Some(url) = url else {
        return Err(format!("the login printed no URL (last line: {last:?})"));
    };
    let prompt = match (&device, pastes) {
        (Some(code), _) => format!("Open the link, sign in and enter the code {code}."),
        (None, true) => "Open the link, sign in, then paste the code the page shows.".to_owned(),
        (None, false) => "Open the link and sign in.".to_owned(),
    };
    write_handoff(
        &env.dir.join("login.json"),
        &serde_json::json!({ "url": url, "prompt": prompt }).to_string(),
    )
    .map_err(|e| format!("login.json: {e}"))?;

    if pastes {
        let code_file = env.dir.join("code");
        let mut polls = waits.owner.as_millis() / 100;
        let code = loop {
            if let Ok(c) = std::fs::read_to_string(&code_file) {
                let _ = std::fs::remove_file(&code_file);
                break c.trim().to_owned();
            }
            polls = polls.saturating_sub(1);
            if polls == 0 {
                return Err("no code was pasted in time".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if let Some(mut w) = stdin.take() {
            writeln!(w, "{code}").map_err(|e| format!("the login closed early: {e}"))?;
        }
    }
    drop(stdin);

    // Wait for the command to end, remembering what it last said.
    let mut polls = waits.owner.as_millis() / 100;
    loop {
        while let Ok(line) = rx.try_recv() {
            if !line.trim().is_empty() {
                last = line;
            }
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => {
                return Err(format!(
                    "the login ended with {status} (last line: {last:?})"
                ));
            }
            None => {}
        }
        polls = polls.saturating_sub(1);
        if polls == 0 {
            return Err("the login did not finish in time".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vendor: Vendor, stub_body: &str) -> (tempfile::TempDir, RunEnv) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let stub = dir.join("stub");
        crate::testutil::write_executable(&stub, &format!("#!/bin/sh\n{stub_body}\n"));
        let mut env = crate::testutil::run_env(&dir, vendor, "login");
        env.login = true;
        env.cli_bin = Some(stub);
        (tmp, env)
    }

    fn wait_for(path: &Path) -> String {
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(path) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{} never appeared", path.display());
    }

    #[test]
    fn output_lines_do_not_consume_the_url_wait() {
        let (_tmp, env) = env(
            Vendor::Codex,
            "i=0; while [ $i -lt 100 ]; do echo progress; i=$((i+1)); done\nprintf \
                'https://auth.example/device\\nABCD-12345\\n'",
        );
        assert_eq!(
            run(
                &env,
                Waits {
                    url: Duration::from_secs(2),
                    owner: Duration::from_secs(3)
                }
            ),
            RunOutcome::Completed
        );
    }

    #[test]
    fn a_claude_login_prints_the_url_then_takes_the_pasted_code() {
        let (_tmp, env) = env(
            Vendor::Claude,
            "echo 'Opening browser to sign in...'\n\
             echo '\x1b[1mVisit https://claude.example/oauth/authorize?code=true&stat\
                e=s1 to sign in\x1b[0m'\n\
             printf 'Paste code here: '\n\
             read code\n\
             echo \"$code\" > \"$HOME/got\"\n\
             test \"$code\" = good-code",
        );
        let dir = env.dir.clone();
        let t = std::thread::spawn(move || run(&env, Waits::default()));
        let started: serde_json::Value =
            serde_json::from_str(&wait_for(&dir.join("login.json"))).unwrap();
        assert_eq!(
            started["url"],
            "https://claude.example/oauth/authorize?code=true&state=s1"
        );
        assert!(
            started["prompt"]
                .as_str()
                .unwrap()
                .contains("paste the code")
        );
        std::fs::write(dir.join("code"), "good-code\n").unwrap();
        assert_eq!(t.join().unwrap(), RunOutcome::Completed);
        assert_eq!(wait_for(&dir.join("login-result")), "ok");
        assert_eq!(wait_for(&dir.join("home/got")).trim(), "good-code");
        assert!(
            !dir.join("code").exists(),
            "the code file is deleted once read"
        );
    }

    #[test]
    fn a_wrong_code_is_a_failed_login() {
        let (_tmp, env) = env(
            Vendor::Claude,
            "echo 'Visit https://claude.example/x'\nread code\necho 'Invalid code'\nexit 1",
        );
        let dir = env.dir.clone();
        let t = std::thread::spawn(move || run(&env, Waits::default()));
        wait_for(&dir.join("login.json"));
        std::fs::write(dir.join("code"), "nope").unwrap();
        assert_eq!(t.join().unwrap(), RunOutcome::SessionFailed);
        let r = wait_for(&dir.join("login-result"));
        assert!(
            r.starts_with("failed:") && r.contains("Invalid code"),
            "{r}"
        );
    }

    #[test]
    fn a_codex_device_login_shows_the_url_and_the_code_and_waits_for_the_page() {
        let (_tmp, env) = env(
            Vendor::Codex,
            "echo 'Follow these steps to sign in:'\n\
             echo '1. Open this link'\n\
             echo '   https://auth.example/codex/device'\n\
             echo '2. Enter this one-time code'\n\
             echo '   ABCD-12345'\n\
             sleep 0.3",
        );
        let dir = env.dir.clone();
        let t = std::thread::spawn(move || run(&env, Waits::default()));
        let started: serde_json::Value =
            serde_json::from_str(&wait_for(&dir.join("login.json"))).unwrap();
        assert_eq!(started["url"], "https://auth.example/codex/device");
        assert!(started["prompt"].as_str().unwrap().contains("ABCD-12345"));
        assert_eq!(t.join().unwrap(), RunOutcome::Completed);
        assert_eq!(wait_for(&dir.join("login-result")), "ok");
    }

    #[test]
    fn a_login_that_prints_no_url_fails_with_its_last_line() {
        let (_tmp, env) = env(Vendor::Codex, "echo 'command not found'\nexit 3");
        let dir = env.dir.clone();
        let waits = Waits {
            url: Duration::from_secs(3),
            owner: Duration::from_secs(3),
        };
        assert_eq!(run(&env, waits), RunOutcome::SessionFailed);
        let r = wait_for(&dir.join("login-result"));
        assert!(
            r.contains("no URL") && r.contains("command not found"),
            "{r}"
        );
    }

    #[test]
    fn login_errors_and_owner_timeouts_terminate_and_reap_the_child() {
        for (vendor, block_handoff, expected) in [
            (Vendor::Claude, true, "login.json:"),
            (Vendor::Claude, false, "no code was pasted in time"),
            (Vendor::Codex, false, "the login did not finish in time"),
        ] {
            // Device login closes stdin before waiting for completion. Block on
            // a separate FIFO so EOF cannot end the stub before the timeout.
            let (_tmp, env) = env(
                vendor,
                "mkfifo hold\necho $$ > pid\nprintf 'https://auth.example/device\\nABCD-12345\\n'\nread line < hold",
            );
            if block_handoff {
                std::fs::create_dir(env.dir.join("login.json")).unwrap();
            }
            let result = login(
                &env,
                Waits {
                    url: Duration::from_secs(3),
                    owner: Duration::ZERO,
                },
            );
            let error = result.unwrap_err();
            assert!(
                error.contains(expected),
                "{vendor:?}: expected {expected}, got {error}"
            );
            let pid = std::fs::read_to_string(env.dir.join("pid")).unwrap();
            assert!(
                !Path::new("/proc").join(pid.trim()).exists(),
                "child {pid} was not reaped"
            );
        }
    }

    #[test]
    fn a_result_clears_a_code_that_came_too_late() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("code"), "late").unwrap();
        write_result(tmp.path(), "failed: no code was pasted in time");
        assert!(!tmp.path().join("code").exists());
        assert!(tmp.path().join("login-result").exists());
    }

    #[test]
    fn helpers_read_urls_and_codes() {
        assert_eq!(
            find_url("go to https://a.b/c."),
            Some("https://a.b/c".into())
        );
        assert_eq!(find_device_code("   ABCD-12345"), Some("ABCD-12345".into()));
        assert_eq!(find_device_code("one-time code"), None);
        assert_eq!(find_device_code("https://x/y-z"), None);
    }
}
