//! Download a release, verify it against the key compiled into
//! this binary, and unpack it to `versions/<v>`. Any failure is a refusal that
//! the owner is shown; nothing under `versions/` changes until the unpacked
//! release has passed every check.

use super::handover::Layout;
use crate::InternalError;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::process::Command;

/// Where release files are, `<base>/v<version>/<file>`.
pub const RELEASE_BASE: &str = "https://github.com/broken-branch/dvb/releases/download";

/// Where an update reads a release's files from.
#[derive(Debug, Clone)]
pub enum Origin {
    /// `<base>/v<version>/<file>`: a mirror, a `file://` directory, or the public
    /// download URL.
    Base(String),
    /// The GitHub release API with the owner's token, needed only for a private
    /// fork's releases (its browser download URL answers 404).
    Api { api: String, token: String },
}

impl Origin {
    /// `DVB_RELEASE_BASE` if set; else the API when the owner supplied a token;
    /// else the public download URL.
    pub fn from_env() -> Self {
        if let Ok(base) = std::env::var("DVB_RELEASE_BASE") {
            return Origin::Base(base);
        }
        match super::github_token() {
            Some(token) => Origin::Api {
                api: super::api_base(),
                token,
            },
            None => Origin::Base(RELEASE_BASE.to_owned()),
        }
    }

    /// Downloads `name` of `v<version>` to `dest`.
    async fn get(
        &self,
        version: &str,
        name: &str,
        dest: &Path,
        release: &mut Option<serde_json::Value>,
    ) -> Result<(), InternalError> {
        match self {
            Origin::Base(base) => super::curl(
                &format!("{base}/v{version}/{name}"),
                None,
                "*/*",
                Some(dest),
            )
            .await
            .map(|_| ()),
            Origin::Api { api, token } => {
                if release.is_none() {
                    let url = format!("{api}/repos/{}/releases/tags/v{version}", super::REPO);
                    let body =
                        super::curl(&url, Some(token), "application/vnd.github+json", None).await?;
                    *release = Some(
                        serde_json::from_slice(&body)
                            .map_err(|e| InternalError::context(url, e))?,
                    );
                }
                let url = release
                    .as_ref()
                    .and_then(|r| r["assets"].as_array())
                    .and_then(|assets| assets.iter().find(|a| a["name"] == name))
                    .and_then(|a| a["url"].as_str())
                    .ok_or_else(|| {
                        InternalError::from(format!("release v{version} has no asset {name}"))
                    })?;
                super::curl(url, Some(token), "application/octet-stream", Some(dest))
                    .await
                    .map(|_| ())
            }
        }
    }
}

/// Checks `SHA256SUMS` against its signature with `key` (minisign's two-line
/// public key file, as in `release.pub`).
pub fn verify_signature(key: &str, sums: &[u8], sig: &str) -> Result<(), InternalError> {
    let line = key
        .lines()
        .nth(1)
        .ok_or("the pinned key is not a minisign public key")?;
    let key = minisign_verify::PublicKey::from_base64(line.trim())
        .map_err(|e| InternalError::context("the pinned key", e))?;
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| InternalError::context("the signature", e))?;
    key.verify(sums, &sig, false).map_err(|error| {
        InternalError::context("the release is not signed with dvb's release key", error)
    })
}

/// The sum `SHA256SUMS` lists for `file`.
fn listed_sum<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let (sum, name) = l.split_once("  ")?;
        (name == file).then_some(sum)
    })
}

/// Downloads `v<version>` from `base`, verifies it with `key` and unpacks it to
/// `versions/<version>`. A build with no pinned key refuses every release.
pub async fn fetch(
    l: &Layout,
    origin: &Origin,
    version: &str,
    key: &str,
    current_schema: u32,
    downgrade: bool,
) -> Result<(), InternalError> {
    let version = super::parse_version(version)
        .ok_or_else(|| InternalError::from(format!("invalid version: {version}")))?
        .to_string();
    let version = version.as_str();
    let stage = l.opt.join(format!(".staging-{version}"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| InternalError::context(format!("{}", stage.display()), e))?;
    let result = fetch_into(&stage, l, origin, version, key, current_schema, downgrade).await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

async fn fetch_into(
    stage: &Path,
    l: &Layout,
    origin: &Origin,
    version: &str,
    key: &str,
    current_schema: u32,
    downgrade: bool,
) -> Result<(), InternalError> {
    let tarball = format!("dvb-{version}-x86_64-linux.tar.gz");
    let mut release = None;
    for file in [tarball.as_str(), "SHA256SUMS", "SHA256SUMS.minisig"] {
        origin
            .get(version, file, &stage.join(file), &mut release)
            .await
            .map_err(|e| {
                if file == "SHA256SUMS.minisig" {
                    InternalError::context("this release is not signed", e)
                } else {
                    e
                }
            })?;
    }
    let sums = std::fs::read(stage.join("SHA256SUMS"))?;
    let sig = std::fs::read_to_string(stage.join("SHA256SUMS.minisig"))?;
    verify_signature(key, &sums, &sig)?;
    let sums = String::from_utf8_lossy(&sums);
    let want = listed_sum(&sums, &tarball)
        .ok_or_else(|| InternalError::from(format!("SHA256SUMS lists no {tarball}")))?;
    let bytes = std::fs::read(stage.join(&tarball))?;
    let got = Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
    if got != want {
        return Err(format!("{tarball} does not match its checksum").into());
    }

    let out = Command::new("tar")
        .arg("-xzf")
        .arg(stage.join(&tarball))
        .arg("-C")
        .arg(stage)
        .output()
        .await
        .map_err(|e| InternalError::context("tar", e))?;
    if !out.status.success() {
        return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let root = stage.join(format!("dvb-{version}"));
    let bin = root.join("dvb");
    let said = Command::new(&bin)
        .arg("--version")
        .output()
        .await
        .map_err(|e| InternalError::context("the release's dvb does not run", e))?;
    let said = String::from_utf8_lossy(&said.stdout).trim().to_owned();
    if said != format!("dvb {version}") {
        return Err(format!("the release is named {version} but its dvb says '{said}'").into());
    }
    let schema: u32 = std::fs::read_to_string(root.join("schema"))
        .map_err(|e| InternalError::context("the release has no schema number", e))?
        .trim()
        .parse::<u32>()
        .map_err(|e| InternalError::context("the release's schema number", e))?;
    if downgrade && schema != current_schema {
        return Err(format!(
            "going back needs the same database schema, and {version} reads schema {schema} while this one is {current_schema}"
        ).into());
    }

    // The service's umask (0027) would make this tree owner-only; the owner and
    // the workers read and run it, and nobody but the engine writes it.
    open_for_reading(&root)?;
    let dest = l.versions().join(version);
    std::fs::create_dir_all(l.versions())?;
    set_mode(&l.versions(), 0o755)?;
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&root, &dest)
        .map_err(|e| InternalError::context(format!("{}", dest.display()), e))?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), InternalError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| InternalError::context(format!("{}", path.display()), e))
}

/// Directories 0755; files 0755 when they were executable, else 0644.
fn open_for_reading(dir: &Path) -> Result<(), InternalError> {
    use std::os::unix::fs::PermissionsExt;
    set_mode(dir, 0o755)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            open_for_reading(&path)?;
        } else if meta.is_file() {
            let mode = if meta.permissions().mode() & 0o100 != 0 {
                0o755
            } else {
                0o644
            };
            set_mode(&path, mode)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn traversal_is_refused_before_creating_or_removing_any_path() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            opt: dir.path().join("opt"),
            state: dir.path().join("state"),
        };
        std::fs::create_dir_all(layout.opt.join(".staging-1.2.3-..")).unwrap();
        std::fs::create_dir_all(&layout.state).unwrap();
        std::fs::write(layout.state.join("keep"), "durable").unwrap();
        let result = fetch(
            &layout,
            &Origin::Base("file:///missing-release".into()),
            "1.2.3-../../../state",
            "unused",
            1,
            false,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("invalid version"));
        assert_eq!(
            std::fs::read_to_string(layout.state.join("keep")).unwrap(),
            "durable"
        );
        assert_eq!(std::fs::read_dir(&layout.opt).unwrap().count(), 1);
        let absent = Layout {
            opt: dir.path().join("absent"),
            state: layout.state,
        };
        assert!(
            fetch(
                &absent,
                &Origin::Base("file:///missing-release".into()),
                "1.2.3-../../../state",
                "unused",
                1,
                false
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid version")
        );
        assert!(!absent.opt.exists());
    }
}
