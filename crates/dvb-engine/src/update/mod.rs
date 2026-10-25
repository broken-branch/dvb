//! Self-update. `mod.rs`: the release check and the flags the rest of
//! the engine reads; `fetch`: download and verify; `handover`: the files, the
//! recovery at start (the crash table) and the steps before `exec`; `protocol`:
//! `POST /update` end to end.

pub mod fetch;
pub mod handover;
pub mod local;
pub mod protocol;

use crate::InternalError;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Atomic};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;

/// The repository releases come from.
pub const REPO: &str = "broken-branch/dvb";

/// Where public releases are listed. Only a private fork's releases need the
/// owner's token (`github_token`).
pub fn releases_url() -> String {
    format!("{}/repos/{REPO}/releases", api_base())
}

/// The minisign public key every release must be signed with, compiled in.
const RELEASE_PUBLIC_KEY: &str = include_str!("release.pub");

/// What the update protocol tells the rest of the engine during drain and handover.
pub struct State {
    /// Admission closed: no new run starts; `admit` waits.
    admission: watch::Sender<bool>,
    effects: AtomicUsize,
    /// Quiesced: the store takes no append.
    quiesced: AtomicBool,
    /// `LISTEN_FDNAMES` when systemd gave us the sockets.
    listen: std::sync::Mutex<Option<String>>,
    /// The newest release the last check found.
    available: std::sync::Mutex<Option<String>>,
    /// The version an update is heading for, while one is under way.
    target: std::sync::Mutex<Option<String>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            admission: watch::channel(false).0,
            effects: AtomicUsize::new(0),
            quiesced: AtomicBool::new(false),
            listen: std::sync::Mutex::new(None),
            available: std::sync::Mutex::new(None),
            target: std::sync::Mutex::new(None),
        }
    }
}

/// An effect section in progress (see `State::effect`).
pub struct EffectGuard<'a>(&'a State);

impl Drop for EffectGuard<'_> {
    fn drop(&mut self) {
        self.0.effects.fetch_sub(1, Atomic::SeqCst);
    }
}

impl State {
    /// A run about to start its unit waits here while admission is closed.
    pub async fn admit(&self) {
        let mut closed = self.admission.subscribe();
        while *closed.borrow_and_update() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }

    pub fn close_admission(&self) {
        self.admission.send_replace(true);
    }

    pub fn reopen_admission(&self) {
        self.admission.send_replace(false);
    }

    pub fn quiesce(&self) {
        self.quiesced.store(true, Atomic::SeqCst);
    }

    pub fn lift_quiesce(&self) {
        self.quiesced.store(false, Atomic::SeqCst);
    }

    pub fn quiesced(&self) -> bool {
        self.quiesced.load(Atomic::SeqCst)
    }

    pub fn set_listen(&self, names: String) {
        *self.listen.lock().unwrap_or_else(dvb_core::sync::recover) = Some(names);
    }

    pub fn listen(&self) -> Option<String> {
        self.listen
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }

    pub fn set_available(&self, v: Option<String>) {
        *self
            .available
            .lock()
            .unwrap_or_else(dvb_core::sync::recover) = v;
    }

    /// What the last check found, newer than this build.
    pub fn available(&self) -> Option<String> {
        self.available
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }

    /// Waits while admission is closed, then enters an effect section: a forge
    /// operation and the evidence it appends. An update waits for every section
    /// that has begun (`effects_inflight`) before it snapshots; one that has not
    /// begun waits here, queued, and goes on if the update is undone (after an
    /// `exec` the next start's reconcile derives it again from the log).
    pub async fn effect(&self) -> EffectGuard<'_> {
        let mut closed = self.admission.subscribe();
        loop {
            self.effects.fetch_add(1, Atomic::SeqCst);
            if !*closed.borrow_and_update() {
                return EffectGuard(self);
            }
            self.effects.fetch_sub(1, Atomic::SeqCst);
            while *closed.borrow_and_update() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    /// Effect sections running now.
    pub fn effects_inflight(&self) -> usize {
        self.effects.load(Atomic::SeqCst)
    }

    /// Marks an update as under way; false when one already is.
    pub fn begin(&self, to: &str) -> bool {
        let mut t = self.target.lock().unwrap_or_else(dvb_core::sync::recover);
        if t.is_some() {
            return false;
        }
        *t = Some(to.to_owned());
        true
    }

    pub fn end(&self) {
        *self.target.lock().unwrap_or_else(dvb_core::sync::recover) = None;
    }

    pub fn target(&self) -> Option<String> {
        self.target
            .lock()
            .unwrap_or_else(dvb_core::sync::recover)
            .clone()
    }
}

/// The committed minisign release key.
pub fn release_public_key() -> &'static str {
    RELEASE_PUBLIC_KEY.trim()
}

/// Reads `1.2.3`, `v1.2.3` or `1.2.3-rc.1` (SemVer 2.0; a pre-release sorts
/// below its release); anything else is `None`.
pub fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.strip_prefix('v').unwrap_or(s)).ok()
}

/// The highest published release in GitHub's `releases` list (drafts and tags
/// that are not versions are ignored; pre-releases count), as `1.2.3`.
pub fn newest_release(json: &str) -> Result<Option<String>, InternalError> {
    let list: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| InternalError::context("the release list is not JSON", e))?;
    let list = list.as_array().ok_or("the release list is not a list")?;
    let newest = list
        .iter()
        .filter(|r| r["draft"].as_bool() != Some(true))
        .filter_map(|r| r["tag_name"].as_str())
        .filter_map(parse_version)
        .max();
    Ok(newest.map(|version| version.to_string()))
}

/// The GitHub API root; `DVB_API_BASE` replaces it (tests, an enterprise host).
pub fn api_base() -> String {
    std::env::var("DVB_API_BASE").unwrap_or_else(|_| "https://api.github.com".to_owned())
}

/// An optional credential, needed only for a private fork's releases:
/// `DVB_GITHUB_TOKEN`, or the first line of the file `DVB_GITHUB_TOKEN_FILE`
/// names (the file must be readable by the engine's user). It is passed to curl
/// on stdin only, never in an argument, and never logged.
pub fn github_token() -> Option<String> {
    if let Ok(t) = std::env::var("DVB_GITHUB_TOKEN") {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_owned());
        }
    }
    let file = std::env::var_os("DVB_GITHUB_TOKEN_FILE")?;
    let text = std::fs::read_to_string(file).ok()?;
    let t = text.lines().next()?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// `curl` for one URL: follows redirects, sends `Accept`, and puts the token in
/// a header read from stdin (curl drops it on a redirect to another host). With
/// `dest` the body is written there; otherwise it is returned.
pub async fn curl(
    url: &str,
    token: Option<&str>,
    accept: &str,
    dest: Option<&std::path::Path>,
) -> Result<Vec<u8>, InternalError> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "600", "-H"])
        .arg(format!("Accept: {accept}"))
        .args(["-H", "User-Agent: dvb", "--config", "-"]);
    if let Some(d) = dest {
        cmd.arg("-o").arg(d);
    }
    cmd.arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| InternalError::context("curl", e))?;
    if let Some(mut stdin) = child.stdin.take() {
        let config = token
            .map(|t| format!("header = \"Authorization: Bearer {t}\"\n"))
            .unwrap_or_default();
        stdin
            .write_all(config.as_bytes())
            .await
            .map_err(|e| InternalError::context("curl", e))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| InternalError::context("curl", e))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("cannot read {url}: {}", why.trim()).into());
    }
    Ok(out.stdout)
}

/// Reads the release list from `url` (https, or `file://` in tests) with curl and
/// returns the version newer than `current`, if any.
pub async fn available(
    url: &str,
    token: Option<&str>,
    current: &str,
) -> Result<Option<String>, InternalError> {
    let have = parse_version(current)
        .ok_or_else(|| InternalError::from(format!("this build's version {current}")))?;
    let out = curl(url, token, "application/vnd.github+json", None).await?;
    let list = String::from_utf8_lossy(&out);
    let newest = newest_release(&list)?;
    Ok(newest.filter(|n| parse_version(n).is_some_and(|v| v > have)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        parse_version(s).unwrap()
    }

    #[test]
    fn versions_order_and_reject_junk() {
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("v1.0.0") > v("0.99.99"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("0.1.0") > v("0.1.0-rc1"));
        assert!(v("0.1.0-rc2") > v("0.1.0-rc1"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-rc"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-1"));
        assert!(v("1.0.0") > v("1.0.0-rc.10"));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "a.b.c",
            "1.2.3-",
            "latest",
            "1.2.3-../../../state",
            "1.2.3-rc..1",
            "1.2.3-.rc",
            "1.2.3-rc.",
            "1.2.3-rc_1",
            "1.2.3-é",
        ] {
            assert!(parse_version(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_newest_release_skips_drafts_and_non_versions() {
        let json = r#"[
            {"tag_name": "v0.3.0", "draft": true},
            {"tag_name": "nightly", "draft": false},
            {"tag_name": "v0.2.0", "draft": false, "prerelease": true},
            {"tag_name": "v0.10.0", "draft": false},
            {"tag_name": "v0.9.0", "draft": false}
        ]"#;
        assert_eq!(newest_release(json).unwrap().as_deref(), Some("0.10.0"));
        assert_eq!(newest_release("[]").unwrap(), None);
        let rc = r#"[{"tag_name": "v1.0.0-rc.2"}, {"tag_name": "v1.0.0-rc.10"}]"#;
        assert_eq!(newest_release(rc).unwrap().as_deref(), Some("1.0.0-rc.10"));
        assert!(newest_release("{}").is_err());
        assert!(newest_release("<html>").is_err());
    }

    #[tokio::test]
    async fn a_newer_release_is_offered_and_an_older_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("releases.json");
        std::fs::write(
            &file,
            r#"[{"tag_name": "v0.2.0", "draft": false}, {"tag_name": "v0.1.0", "draft": false}]"#,
        )
        .unwrap();
        let url = format!("file://{}", file.display());
        assert_eq!(
            available(&url, None, "0.1.0").await.unwrap().as_deref(),
            Some("0.2.0")
        );
        assert_eq!(available(&url, None, "0.2.0").await.unwrap(), None);
        assert_eq!(available(&url, Some("t"), "0.3.0").await.unwrap(), None);
        let rc = dir.path().join("rc.json");
        std::fs::write(
            &rc,
            r#"[{"tag_name": "v1.0.0-rc.10"}, {"tag_name": "v1.0.0-rc.2"}]"#,
        )
        .unwrap();
        let url = format!("file://{}", rc.display());
        assert_eq!(
            available(&url, None, "1.0.0-rc.2")
                .await
                .unwrap()
                .as_deref(),
            Some("1.0.0-rc.10")
        );
        assert_eq!(available(&url, None, "1.0.0-rc.10").await.unwrap(), None);
        let err = available("file:///nonexistent/releases.json", None, "0.1.0")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    #[test]
    fn the_pinned_key_is_a_minisign_public_key() {
        let key = release_public_key();
        let lines: Vec<&str> = key.lines().collect();
        assert_eq!(lines.len(), 2, "comment line, then the key");
        assert!(lines[0].starts_with("untrusted comment:"));
        // 2 algorithm bytes, 8 key id, 32 key: 42 bytes, 56 base64 characters, "Ed" first.
        assert_eq!(lines[1].len(), 56);
        assert!(lines[1].starts_with("RW"));
    }
}
