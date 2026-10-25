//! One first-start addition to Omarchy's user menu; the owner owns it afterwards.

use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

const MENU: &str = include_str!("omarchy-menu.json");

pub fn at_start() -> std::io::Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    install(
        &home.join(".config/omarchy/extensions/omarchy-menu.jsonc"),
        &state.join("dvb/omarchy-menu-attempted"),
    )
}

fn install(menu: &Path, marker: &Path) -> std::io::Result<()> {
    if !menu.parent().is_some_and(Path::is_dir) {
        return Ok(());
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Claim the attempt before touching the menu: even a crash must not restore
    // a block the owner may have removed in the meantime.
    match std::fs::File::create_new(marker) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    }
    match std::fs::symlink_metadata(menu) {
        Err(e) if e.kind() == ErrorKind::NotFound => return std::fs::write(menu, MENU),
        Err(e) => return Err(e),
        Ok(meta) if !meta.is_file() => return Err(add_by_hand(menu)),
        Ok(_) => {}
    }
    let mut source = std::fs::read_to_string(menu)?;
    // Only a menu of comments, like Omarchy's template, takes a text insertion;
    // the owner's own entries are theirs to merge.
    let at = closing_brace(&source).ok_or_else(|| add_by_hand(menu))?;
    source.insert_str(
        at,
        MENU.trim().trim_start_matches('{').trim_end_matches('}'),
    );
    std::fs::write(menu, source)
}

fn add_by_hand(menu: &Path) -> Error {
    Error::new(
        ErrorKind::InvalidData,
        format!("add this block to {}:\n{MENU}", menu.display()),
    )
}

/// Where an object holding only comments and whitespace closes.
fn closing_brace(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut code = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"//") {
            at += bytes[at..]
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(bytes.len() - at);
        } else if bytes[at..].starts_with(b"/*") {
            at += bytes[at + 2..].windows(2).position(|pair| pair == b"*/")? + 4;
        } else {
            if !bytes[at].is_ascii_whitespace() {
                code.push(at);
            }
            at += 1;
        }
    }
    match code[..] {
        [open, close] if bytes[open] == b'{' && bytes[close] == b'}' => Some(close),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    const FIXTURE: &str = include_str!("fixtures/omarchy-menu.jsonc");
    const FIRST: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const SECOND: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

    #[test]
    fn inserts_without_changing_any_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("omarchy-menu.jsonc");
        let marker = dir.path().join("state/omarchy-menu");
        for original in [FIXTURE, "{}", "{\n  // Empty\n}\n", "{} // }"] {
            std::fs::write(&menu, original).unwrap();
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
            install(&menu, &marker).unwrap();
            let changed = std::fs::read_to_string(&menu).unwrap();
            let at = closing_brace(original).unwrap();
            assert!(changed.starts_with(&original[..at]));
            assert!(changed.ends_with(&original[at..]));
            assert!(changed.contains("\"dvb\":"));
            assert!(marker.exists());
        }
    }

    #[test]
    fn missing_file_contains_only_the_dvb_block() {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("omarchy-menu.jsonc");
        install(&menu, &dir.path().join("state/omarchy-menu")).unwrap();
        assert_eq!(std::fs::read_to_string(menu).unwrap(), MENU);
    }

    #[test]
    fn menus_with_entries_are_untouched_and_attempted_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("omarchy-menu.jsonc");
        let marker = dir.path().join("state/omarchy-menu");
        for original in [
            "{\n// no last brace",
            "[]",
            "{ /* unclosed }",
            "{\"work\": {\"label\": \"Work\"}}",
            "{\"items\":{\"work\":{\"label\":\"Work\",\"action\":\"true\"}}}",
            "{\n  \"dvb\" : {\"label\": \"My dvb\"}\n}\n",
            MENU,
        ] {
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
            std::fs::write(&menu, original).unwrap();
            let error = install(&menu, &marker).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert_eq!(
                error.to_string(),
                format!("add this block to {}:\n{MENU}", menu.display())
            );
            assert_eq!(std::fs::read_to_string(&menu).unwrap(), original);
            assert!(marker.exists());
            install(&menu, &marker).unwrap();
            assert_eq!(std::fs::read_to_string(&menu).unwrap(), original);
        }
    }

    #[test]
    fn owner_deletion_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("omarchy-menu.jsonc");
        let marker = dir.path().join("state/omarchy-menu");
        std::fs::write(&menu, FIXTURE).unwrap();
        install(&menu, &marker).unwrap();
        std::fs::write(&menu, FIXTURE).unwrap();
        install(&menu, &marker).unwrap();
        assert_eq!(std::fs::read_to_string(&menu).unwrap(), FIXTURE);
        std::fs::remove_file(&menu).unwrap();
        install(&menu, &marker).unwrap();
        assert!(!menu.exists());
    }

    #[test]
    fn off_omarchy_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("absent/omarchy-menu.jsonc");
        let marker = dir.path().join("state/omarchy-menu");
        install(&menu, &marker).unwrap();
        assert!(!menu.exists());
        assert!(!marker.exists());
    }

    fn executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn action(id: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let menu = dir.path().join("menu.jsonc");
        install(&menu, &dir.path().join("marker")).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(menu).unwrap()).unwrap();
        value[id]["action"].as_str().unwrap().to_owned()
    }

    fn run_action(
        id: &str,
        projects: &str,
        status: &str,
        pick: usize,
        cancel: &str,
    ) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        executable(
            &dir.path().join("dvb"),
            "#!/bin/bash\nprintf '%s\\0' \"$@\" >> \"$CALLS\"\ncase \"$1\" in\nproject) printf '%s' \"$PROJECTS\";;\nstatus) printf '%s' \"$STATUS\";;\nanswer|goal) if [[ $CANCEL == command ]]; then printf '%s\\n' 'Command refused' >&2; exit 1; fi;;\nesac\n",
        );
        executable(
            &dir.path().join("notify-send"),
            "#!/bin/bash\nprintf '%s\\0' notify \"$@\" >> \"$CALLS\"\n",
        );
        executable(
            &dir.path().join("omarchy-menu-select"),
            "#!/bin/bash\nprintf '%s\\0' select \"$@\" >> \"$CALLS\"\n[[ $CANCEL != select ]] || exit 1\nshift\nprintf '%s' \"${!PICK}\"\n",
        );
        executable(
            &dir.path().join("omarchy-menu-input"),
            "#!/bin/bash\nprintf '%s\\0' input \"$@\" >> \"$CALLS\"\n[[ $CANCEL != input ]] || exit 1\nprintf '%s' \"$TEXT\"\n",
        );
        executable(
            &dir.path()
                .join("omarchy-launch-floating-terminal-with-presentation"),
            "#!/bin/bash\nprintf '%s\\0' floating \"$@\" >> \"$CALLS\"\n\"$@\"\n",
        );
        let text =
            "--dictated 'words' \"quoted\"; $(touch injected) `touch injected`\nnext line \\ end";
        let calls = dir.path().join("calls");
        let result = Command::new("timeout")
            .args(["10", "bash", "-c", &action(id)])
            .current_dir(dir.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("CALLS", &calls)
            .env("PROJECTS", projects)
            .env("STATUS", status)
            .env("PICK", pick.to_string())
            .env("CANCEL", cancel)
            .env("TEXT", text)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
        assert!(!dir.path().join("injected").exists());
        std::fs::read_to_string(calls)
            .unwrap()
            .split_terminator('\0')
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn open_and_status_run_existing_commands() {
        assert_eq!(run_action("dvb.open", "", "", 1, ""), ["app"]);
        assert_eq!(
            run_action("dvb.status", "", "Running", 1, ""),
            ["floating", "dvb", "status", "status"]
        );
    }

    #[test]
    fn goal_picks_a_project_and_quotes_dictation() {
        let one = format!("alpha  {FIRST}  org/alpha\n");
        let two = format!("{one}beta  {SECOND}  org/beta\n");
        let text =
            "--dictated 'words' \"quoted\"; $(touch injected) `touch injected`\nnext line \\ end";
        assert_eq!(
            run_action("dvb.goal", &one, "", 1, ""),
            [
                "project", "list", "input", "New goal", "goal", FIRST, "--", text
            ]
        );
        let calls = run_action("dvb.goal", &two, "", 2, "");
        assert_eq!(&calls[..3], ["project", "list", "select"]);
        assert_eq!(&calls[calls.len() - 4..], ["goal", SECOND, "--", text]);
        let refused = run_action("dvb.goal", &one, "", 1, "command");
        assert_eq!(
            &refused[refused.len() - 3..],
            ["notify", "dvb", "Command refused"]
        );
        assert_eq!(run_action("dvb.goal", "", "", 1, ""), ["project", "list"]);
        assert!(
            !run_action("dvb.goal", &two, "", 1, "select")
                .iter()
                .any(|v| v == "goal")
        );
        assert!(
            !run_action("dvb.goal", &one, "", 1, "input")
                .iter()
                .any(|v| v == "goal")
        );
    }

    #[test]
    fn answer_picks_only_question_lines_and_quotes_reply() {
        let question = format!("Needs you: Ship (after review)? ({FIRST})");
        let other = format!("Needs you: Another question ({SECOND})");
        let task = "Needs you: Held (work) (held) - Stopped: dvb resume held";
        let status = format!("Running: 1\n{task}\n{question}\n  1. Yes - ship\n{other}\n");
        let calls = run_action("dvb.answer", "", &status, 2, "");
        assert_eq!(
            &calls[..5],
            ["status", "select", "Answer a question", &question, &other]
        );
        assert_eq!(
            &calls[5..9],
            ["input", "Answer a question", "answer", SECOND]
        );
        assert_eq!(
            calls[9],
            "--reply=--dictated 'words' \"quoted\"; $(touch injected) `touch injected`\nnext line \\ end"
        );
        let refused = run_action("dvb.answer", "", &status, 1, "command");
        assert_eq!(
            &refused[refused.len() - 3..],
            ["notify", "dvb", "Command refused"]
        );
        assert_eq!(
            run_action("dvb.answer", "", "Running: 1\n", 1, ""),
            ["status"]
        );
        assert!(
            !run_action("dvb.answer", "", &status, 1, "select")
                .iter()
                .any(|v| v == "answer")
        );
        assert!(
            !run_action("dvb.answer", "", &status, 1, "input")
                .iter()
                .any(|v| v == "answer")
        );
    }

    #[test]
    fn pause_and_resume_visit_each_project() {
        let projects = format!("alpha  {FIRST}  org/alpha\nbeta  {SECOND}  org/beta\n");
        for verb in ["pause", "resume"] {
            assert_eq!(
                run_action(&format!("dvb.{verb}"), &projects, "", 1, ""),
                ["project", "list", verb, FIRST, verb, SECOND]
            );
            assert_eq!(
                run_action(&format!("dvb.{verb}"), "", "", 1, ""),
                ["project", "list"]
            );
        }
    }
}
