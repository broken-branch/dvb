use std::path::Path;

// Synchronous fixture seeding only; exercised engine effects await core Git.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs([
            ("GIT_AUTHOR_NAME", "seed"),
            ("GIT_AUTHOR_EMAIL", "seed@example.invalid"),
            ("GIT_COMMITTER_NAME", "seed"),
            ("GIT_COMMITTER_EMAIL", "seed@example.invalid"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub fn seed_repository(dir: &Path, branch: &str, file: Option<(&str, &str)>, message: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet", "-b", branch]);
    if let Some((name, contents)) = file {
        std::fs::write(dir.join(name), contents).unwrap();
        git(dir, &["add", "."]);
    }
    git(dir, &["commit", "--quiet", "--allow-empty", "-m", message]);
}

/// Collects `head` in the run workspace `ws` as `dvb run exec` does: at
/// `refs/dvb/collected`, bundled into the run directory for the engine.
pub fn collect(ws: &Path, head: &str) {
    git(ws, &["update-ref", "refs/dvb/collected", head]);
    let bundle = ws
        .parent()
        .unwrap()
        .join(dvb_engine::publish::COLLECTED_BUNDLE);
    git(
        ws,
        &[
            "bundle",
            "create",
            "--quiet",
            bundle.to_str().unwrap(),
            "refs/dvb/collected",
        ],
    );
}
