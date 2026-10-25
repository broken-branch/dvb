//! Publishing: the engine's mirror of each project, the publication
//! commit built from a run's collected tree, and the scan on what leaves the box.

use crate::InternalError;
use dvb_core::git::{Git, GitError};
use dvb_core::roles::ReviewKind;
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;

/// The default neutral identity for publication commits.
pub const NEUTRAL_NAME: &str = "dvb";
pub const NEUTRAL_EMAIL: &str = "dvb@dvb.invalid";

/// Clones the project's repo as the engine's bare mirror.
#[cfg(any(test, feature = "test-support"))]
pub async fn create_mirror(repo: &str, mirror: &Path) -> Result<(), InternalError> {
    let parent = mirror.parent().ok_or("mirror has no parent")?;
    Git::new(parent)
        .run(
            &[
                "clone",
                "--quiet",
                "--bare",
                repo,
                &mirror.to_string_lossy(),
            ],
            &[],
        )
        .await
        .map_err(InternalError::from)
        .map(|_| ())
}

/// A collection with no work is distinct from a publication failure.
#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    #[error("the run changed nothing")]
    NoChanges,
    #[error("{0}")]
    Other(#[from] InternalError),
}

impl From<GitError> for CollectionError {
    fn from(error: GitError) -> Self {
        Self::Other(error.into())
    }
}

/// The file in a run's directory where `dvb run exec` bundles its collected
/// snapshot (`refs/dvb/collected`).
pub const COLLECTED_BUNDLE: &str = "collected.bundle";

/// The most of a run's bundle the engine copies.
const BUNDLE_MAX: u64 = 1 << 30;

/// How long fetching a run's bundle may take: the task's lock is held
/// meanwhile.
const FETCH_LIMIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The most memory git allocates at once while fetching a run's bundle, so a small pack cannot inflate an object without bound.
const OBJECT_MAX: &str = "512m";

/// What a missing bundle means: the run's dvb-run wrote none.
const NO_BUNDLE: &str = "collected by an older dvb-run; retry to rebuild in the kept workspace";

/// The first line of a git bundle, either version git writes.
const BUNDLE_SIGNATURES: [&[u8; 16]; 2] = [b"# v2 git bundle\n", b"# v3 git bundle\n"];

/// Fetches the snapshot the run in `dir` bundled into the mirror as
/// `refs/dvb/runs/<run>` and checks that it is the commit `head` the run
/// reported. The engine runs no git in the run's repository: its
/// configuration, alternates and refs would choose what git reads there. The
/// bundle is copied through one no-follow open of a regular file, so a run
/// still alive can change only the bytes, and the result is checked here.
/// Any failure leaves no `refs/dvb/runs/<run>`, so a run ref already at
/// `head` was fetched and checked before (recovery, then publication).
pub async fn fetch_collected(
    mirror: &Path,
    dir: &Path,
    run: &str,
    head: &str,
) -> Result<(), InternalError> {
    let run_ref = format!("refs/dvb/runs/{run}");
    if Git::new(mirror)
        .run(&["rev-parse", "--verify", "--quiet", &run_ref], &[])
        .await
        .is_ok_and(|fetched| fetched == head)
    {
        return Ok(());
    }
    let fetched = fetch_bundle(mirror, &dir.join(COLLECTED_BUNDLE), &run_ref).await;
    let error = match fetched {
        Ok(fetched) if fetched == head => return Ok(()),
        Ok(fetched) => format!("the run's snapshot is {fetched}, the run reported {head}").into(),
        Err(error) => error,
    };
    Git::new(mirror)
        .run(&["update-ref", "-d", &run_ref], &[])
        .await
        .map_err(|deleted| format!("{error}; {run_ref}: {deleted}"))?;
    Err(error)
}

/// Readies the mirrors at engine start, before any fetch: removes the bundle
/// copies a crash left (`.collected.*`, up to the bundle limit
/// each).
pub fn clean_mirrors(mirrors: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(mirrors) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for mirror in entries {
        let mirror = mirror?;
        // Only mirrors: the workers' shared caches are theirs.
        if !mirror.file_type()?.is_dir() || mirror.path().extension() != Some("git".as_ref()) {
            continue;
        }
        for copy in std::fs::read_dir(mirror.path())? {
            let copy = copy?;
            if !copy
                .file_name()
                .as_encoded_bytes()
                .starts_with(b".collected.")
            {
                continue;
            }
            let kind = copy.file_type()?;
            if kind.is_dir() {
                std::fs::remove_dir_all(copy.path())?;
            } else if kind.is_file() {
                std::fs::remove_file(copy.path())?;
            }
        }
    }
    Ok(())
}

/// Copies the bundle at `path` into the mirror's directory and fetches its
/// `refs/dvb/collected` as `run_ref`. git reads a fetched path as a bundle only
/// when it starts with a bundle's first line, and otherwise as a repository,
/// following a gitfile the run wrote; so the copy must start with that line.
async fn fetch_bundle(mirror: &Path, path: &Path, run_ref: &str) -> Result<String, InternalError> {
    let io = |source| InternalError::Io {
        context: path.display().to_string(),
        source,
    };
    let mut file = crate::launch::open_run_file(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            InternalError::Message(format!("{}: {NO_BUNDLE}", path.display()))
        } else {
            io(error)
        }
    })?;
    let size = file.metadata().map_err(io)?.len();
    if size > BUNDLE_MAX {
        return Err(format!("{}: {size} bytes, over {BUNDLE_MAX}", path.display()).into());
    }
    let mut copy = tempfile::Builder::new()
        .prefix(".collected.")
        .tempfile_in(mirror)
        .map_err(io)?;
    let copy = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut start = [0; 16];
        file.read_exact(&mut start)?;
        if !BUNDLE_SIGNATURES.contains(&&start) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a git bundle",
            ));
        }
        copy.write_all(&start)?;
        std::io::copy(&mut file.take(BUNDLE_MAX), &mut copy).map(|_| copy)
    })
    .await
    .map_err(|joined| joined.to_string())?
    .map_err(io)?;
    // At most two threads and `OBJECT_MAX`; nothing the fetch lacks is
    // fetched from anywhere else. A git from 2.46 refuses malformed objects.
    Git::new(mirror)
        .run_within(
            FETCH_LIMIT,
            &[
                "-c",
                "pack.threads=2",
                "-c",
                "fetch.fsckObjects=true",
                "fetch",
                "--quiet",
                "--no-tags",
                &copy.path().to_string_lossy(),
                &format!("+refs/dvb/collected:{run_ref}"),
            ],
            &[("GIT_NO_LAZY_FETCH", "1"), ("GIT_ALLOC_LIMIT", OBJECT_MAX)],
            Stdio::null(),
        )
        .await?;
    Git::new(mirror)
        .run(
            &["rev-parse", "--verify", &format!("{run_ref}^{{commit}}")],
            &[],
        )
        .await
        .map_err(Into::into)
}

/// Fetches the collected snapshot the run in `dir` bundled into the mirror and
/// builds the publication commit: only the snapshot's tree, on the
/// base the engine recorded for the run, with the neutral author and committer
/// and the engine's own message. The snapshot must be the head the run reported
/// in `run.collected`; its parents are never used, so no commit the worker made
/// becomes reachable from what is pushed.
///
/// `(base, merges)`: the publication's parents, the run's recorded base and,
/// for a conflict fix round, the default-branch commit it merged.
pub async fn collected_publication(
    mirror: &Path,
    dir: &Path,
    run: &str,
    collected: &str,
    (base, merges): (&str, Option<&str>),
    message: &str,
    settings: &PublicationSettings,
) -> Result<String, CollectionError> {
    fetch_collected(mirror, dir, run, collected).await?;
    let tree = Git::new(mirror)
        .run(&["rev-parse", &format!("{collected}^{{tree}}")], &[])
        .await?;
    let base = Git::new(mirror)
        .run(
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
            &[],
        )
        .await?;
    // The one place every builder head passes before it is published, live or
    // after a restart: a run that changed nothing has no work to publish.
    let base_tree = Git::new(mirror)
        .run(&["rev-parse", &format!("{base}^{{tree}}")], &[])
        .await?;
    if tree == base_tree {
        return Err(CollectionError::NoChanges);
    }
    // A conflict fix round merged the default branch in: its publication
    // is a merge of that commit too, so the branch is up to date with it.
    let parents: Vec<_> = std::iter::once(base.as_str()).chain(merges).collect();
    neutral_commit(mirror, &tree, &parents, message, settings)
        .await
        .map_err(Into::into)
}

/// Where the mirror keeps the default-branch commit a task's conflict fix
/// round merges: written by the retry, used by every builder run of that
/// round (a crash's retry, a resume) and removed once the round is published.
pub fn merges_ref(task: &str) -> String {
    format!("refs/dvb/merges/{task}")
}

/// The default-branch commit the task's pending conflict fix round merges.
pub async fn merges(mirror: &Path, task: &str) -> Option<String> {
    Git::new(mirror)
        .run(
            &["rev-parse", "--verify", "--quiet", &merges_ref(task)],
            &[],
        )
        .await
        .ok()
        .filter(|c| !c.is_empty())
}

/// The default-branch commit last merged into `head` since `since`, by a
/// conflict fix round or a branch update: the second parent of the newest
/// merge on its first-parent line. Builders' own commits are never merges.
pub async fn merged_since(mirror: &Path, since: &str, head: &str) -> Option<String> {
    let line = Git::new(mirror)
        .run(
            &[
                "rev-list",
                "--first-parent",
                "--merges",
                "--parents",
                "-n",
                "1",
                &format!("{since}..{head}"),
            ],
            &[],
        )
        .await
        .ok()?;
    match line.split_whitespace().collect::<Vec<_>>()[..] {
        [_, _, second] => Some(second.to_owned()),
        _ => None,
    }
}

/// Uses the publication identity (the owner's when publishing as themselves).
async fn neutral_commit(
    mirror: &Path,
    tree: &str,
    parents: &[&str],
    message: &str,
    settings: &PublicationSettings,
) -> Result<String, InternalError> {
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    Git::new(mirror)
        .run(&args, &neutral(settings))
        .await
        .map_err(InternalError::from)
}

fn neutral(settings: &PublicationSettings) -> [(&'static str, &str); 4] {
    [
        ("GIT_AUTHOR_NAME", settings.name.as_str()),
        ("GIT_AUTHOR_EMAIL", settings.email.as_str()),
        ("GIT_COMMITTER_NAME", settings.name.as_str()),
        ("GIT_COMMITTER_EMAIL", settings.email.as_str()),
    ]
}

/// Brings a published `head` up to date with `base`: the engine's own
/// merge commit of the two, neutral author and committer, so the branch moves
/// forward without a force push and never by GitHub's "update branch" (which
/// commits as a person). `None` when they conflict.
pub async fn update_commit(
    mirror: &Path,
    head: &str,
    base: &str,
    message: &str,
    settings: &PublicationSettings,
) -> Result<Option<String>, InternalError> {
    let Some(tree) = merged_tree(mirror, head, base).await? else {
        return Ok(None);
    };
    neutral_commit(mirror, &tree, &[head, base], message, settings)
        .await
        .map(Some)
}

/// Whether merging `base` into `head` conflicts.
pub async fn conflicts(mirror: &Path, head: &str, base: &str) -> Result<bool, InternalError> {
    merged_tree(mirror, head, base)
        .await
        .map(|tree| tree.is_none())
}

/// The tree of `base` merged into `head`; `None` when they conflict.
async fn merged_tree(
    mirror: &Path,
    head: &str,
    base: &str,
) -> Result<Option<String>, InternalError> {
    match Git::new(mirror)
        .run(
            &["merge-tree", "--write-tree", "--no-messages", head, base],
            &[],
        )
        .await
    {
        Ok(tree) => Ok(Some(tree)),
        Err(GitError::Failed { status, .. }) if status.code() == Some(1) => Ok(None),
        Err(error @ (GitError::Failed { .. } | GitError::Io(_) | GitError::TimedOut { .. })) => {
            Err(error.into())
        }
    }
}

/// The patch id (`git patch-id --stable`) of the change from `from` to `to`:
/// equal for the same change on another base. Empty for no change.
pub async fn patch_id(mirror: &Path, from: &str, to: &str) -> Result<String, InternalError> {
    let diff = Git::new(mirror)
        .output(
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                from,
                to,
            ],
            &[],
            None,
        )
        .await?;
    let out = Git::new(mirror)
        .output(&["patch-id", "--stable"], &[], Some(&diff.stdout))
        .await?;
    let id = String::from_utf8_lossy(&out.stdout);
    Ok(id.split_whitespace().next().unwrap_or_default().to_owned())
}

pub mod mirror;
pub mod project;
pub mod reconcile;
pub use dvb_forge::scan::{Hit, ScanPolicy, without_run_ids};

/// Installation settings stay local; no identity values go into a publication
/// unless the owner chose to publish as themself (`as_owner`).
#[derive(Clone, serde::Serialize, Deserialize)]
#[serde(default)]
pub struct PublicationSettings {
    #[serde(alias = "neutral_name")]
    pub name: String,
    #[serde(alias = "neutral_email")]
    pub email: String,
    pub owner_name: String,
    pub owner_email: String,
    /// The commit identity may name the owner as recorded at GitHub setup.
    pub as_owner: bool,
}
impl Default for PublicationSettings {
    fn default() -> Self {
        Self {
            name: NEUTRAL_NAME.into(),
            email: NEUTRAL_EMAIL.into(),
            owner_name: String::new(),
            owner_email: String::new(),
            as_owner: false,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("invalid publication settings")]
    Decode(#[source] serde_json::Error),
    #[error("cannot read publication settings")]
    Read(#[source] std::io::Error),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("The commit author {field} {rule}.")]
pub struct IdentityError {
    field: &'static str,
    rule: String,
}

impl PublicationSettings {
    pub fn load(state: &Path) -> Result<Self, PublicationError> {
        match std::fs::read(state.join("publication.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(PublicationError::Decode),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(PublicationError::Read(error)),
        }
    }
    /// Writes the settings where the next engine start loads them.
    pub fn save(&self, state: &Path) -> Result<(), InternalError> {
        dvb_forge::github_setup::write_private(
            state,
            "publication.json",
            &serde_json::to_vec(self)?,
        )?;
        Ok(())
    }
    pub fn identity(&self) -> dvb_core::api::PublishingIdentity {
        dvb_core::api::PublishingIdentity {
            name: self.name.clone(),
            email: self.email.clone(),
            is_default: self.name == NEUTRAL_NAME && self.email == NEUTRAL_EMAIL,
            as_owner: self.as_owner,
        }
    }
    pub fn policy(&self) -> ScanPolicy {
        ScanPolicy {
            owner_name: self.owner_name.clone(),
            owner_email: self.owner_email.clone(),
        }
    }
    /// The commit identity's first broken rule. `as_owner` lifts only the
    /// owner's name and email.
    pub fn validate(&self) -> Result<(), IdentityError> {
        let policy = self.policy();
        for (field, value) in [("name", &self.name), ("email", &self.email)] {
            let refuse = |rule: String| Err(IdentityError { field, rule });
            if value.is_empty() {
                return refuse("is empty".into());
            }
            if value.contains(['\n', '\r']) {
                return refuse("can't contain a line break".into());
            }
            if value.contains(['<', '>']) {
                return refuse("can't contain '<' or '>'".into());
            }
            for hit in policy.hits("commit identity", value) {
                let rule = match hit.rule.as_str() {
                    "owner-name" | "owner-email" if self.as_owner => continue,
                    "owner-name" => format!(
                        "can't contain your name ({}) unless you publish as yourself",
                        self.owner_name
                    ),
                    "owner-email" => format!(
                        "can't contain your email ({}) unless you publish as yourself",
                        self.owner_email
                    ),
                    rule => format!("looks like a secret (scan rule {rule})"),
                };
                return refuse(rule);
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Default)]
struct Ignores {
    #[serde(default)]
    ignore: Vec<Ignore>,
}
#[derive(Deserialize)]
struct Ignore {
    file: String,
    rule: String,
}

async fn base_ignores(mirror: &Path, parent: &str) -> Result<Ignores, InternalError> {
    let path = ".dvb/scan-ignore.toml";
    if Git::new(mirror)
        .run(&["ls-tree", "--name-only", parent, "--", path], &[])
        .await
        .map_err(InternalError::from)?
        .is_empty()
    {
        return Ok(Ignores::default());
    }
    let source = Git::new(mirror)
        .run(&["show", &format!("{parent}:{path}")], &[])
        .await
        .map_err(InternalError::from)?;
    toml::from_str(&source).map_err(InternalError::from)
}

/// Git's empty tree, the attribute source while scanning.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// Git quotes paths as bytes, including three-digit octal escapes for UTF-8.
fn diff_path(header: &str) -> Result<String, InternalError> {
    // A literal tab delimits the header; tabs in filenames are escaped by Git.
    let header = header.split_once('\t').map_or(header, |(path, _)| path);
    let decoded = if let Some(quoted) = header.strip_prefix('"') {
        let quoted = quoted.strip_suffix('"').ok_or("invalid quoted diff path")?;
        let mut input = quoted.bytes();
        let mut bytes = Vec::new();
        while let Some(byte) = input.next() {
            if byte != b'\\' {
                bytes.push(byte);
                continue;
            }
            let escape = input.next().ok_or("invalid diff path escape")?;
            let byte = match escape {
                b'a' => 7,
                b'b' => 8,
                b't' => b'\t',
                b'n' => b'\n',
                b'v' => 11,
                b'f' => 12,
                b'r' => b'\r',
                b'\\' => b'\\',
                b'"' => b'"',
                b'0'..=b'3' => {
                    let second = input
                        .next()
                        .filter(|b| (b'0'..=b'7').contains(b))
                        .ok_or("invalid octal diff path escape")?;
                    let third = input
                        .next()
                        .filter(|b| (b'0'..=b'7').contains(b))
                        .ok_or("invalid octal diff path escape")?;
                    (escape - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
                }
                _ => return Err("invalid diff path escape".into()),
            };
            bytes.push(byte);
        }
        String::from_utf8(bytes)?
    } else {
        header.to_owned()
    };
    Ok(decoded.strip_prefix("b/").unwrap_or(&decoded).to_owned())
}

/// Scans the lines a commit adds against its parent, with the secret
/// rules: the owner's name and email are checked only in what dvb writes itself
/// (commit identity and message, PR title and body, comments).
pub async fn scan_commit(
    mirror: &Path,
    parent: &str,
    commit: &str,
) -> Result<Option<Hit>, InternalError> {
    let policy = ScanPolicy::default();
    let ignores = base_ignores(mirror, parent).await?;
    let paths = Git::new(mirror)
        .run(
            &[
                "-c",
                "core.quotePath=false",
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                parent,
                commit,
            ],
            &[],
        )
        .await
        .map_err(InternalError::from)?;
    for path in paths.split('\0').filter(|p| !p.is_empty()) {
        if let Some(hit) = policy.scan("file path", path) {
            return Ok(Some(hit));
        }
    }
    // Every changed blob as text: `--text` so Git's binary guess hides nothing;
    // no external diff, textconv or rename pairing; attributes from the empty
    // tree and no attribute files, so nothing the worker wrote changes the diff.
    let attr_source = format!("--attr-source={EMPTY_TREE}");
    let diff = Git::new(mirror)
        .run(
            &[
                &attr_source,
                "-c",
                "core.attributesFile=/dev/null",
                "diff",
                "--text",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--unified=0",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                parent,
                commit,
            ],
            &[("GIT_ATTR_NOSYSTEM", "1")],
        )
        .await
        .map_err(InternalError::from)?;
    let mut file = String::new();
    let mut line = 0u32;
    // Headers only between `diff --git` and the first hunk; inside a hunk every
    // line is content, whatever it starts with.
    let mut in_hunk = false;
    for l in diff.lines() {
        if l.starts_with("diff --git ") {
            in_hunk = false;
        } else if !in_hunk && let Some(f) = l.strip_prefix("+++ ") {
            file = diff_path(f)?;
        } else if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            in_hunk = true;
            line = h
                .split_whitespace()
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|p| p.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if in_hunk && let Some(added) = l.strip_prefix('+') {
            if let Some(mut hit) = policy.hits(&file, added).into_iter().find(|hit| {
                !ignores
                    .ignore
                    .iter()
                    .any(|i| i.file == file && i.rule == hit.rule)
            }) {
                hit.line = line;
                return Ok(Some(hit));
            }
            line += 1;
        }
    }
    Ok(None)
}

/// The base a `stage` review of `head` reads against, shared by the reviewer's
/// prompt and the task diff: the last head that stage gave a verdict on, else
/// the task's first base, so a round no verdict covered (a review stopped by a
/// retry) is reviewed with the next one. A branch brought up to date, or a
/// default-branch merge since, reads against the merged commit.
pub(crate) async fn review_base(
    e: &crate::AppState,
    task: &dvb_core::TaskId,
    head: &str,
    stage: ReviewKind,
) -> Option<String> {
    let (mirror, updated, since) = {
        let proj = e.proj();
        let task = proj.tasks.get(task)?;
        let project = proj.projects.get(&task.view.project)?;
        let since = task
            .reviewed(stage)
            .before(head)
            .or(task.base.as_ref())
            .cloned();
        let result = (
            e.mirror(&project.view.name),
            task.updated_base.clone(),
            since,
        );
        drop(proj);
        result
    };
    match updated {
        Some(base) => Some(base),
        None => {
            let since = since?;
            merged_since(&mirror, &since, head).await.or(Some(since))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_uses_dvb_publication_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = PublicationSettings::load(tmp.path()).unwrap();
        settings.validate().unwrap();
        assert_eq!(settings.name, "dvb");
        assert_eq!(settings.email, "dvb@dvb.invalid");
    }

    #[tokio::test]
    async fn stored_publication_identity_survives_loading_and_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("publication.json");
        let bytes = br#"{"neutral_name":"publisher.example.invalid","neutral_email":"publisher@example.invalid","owner_name":"Example Owner","owner_email":"owner@example.invalid"}"#;
        std::fs::write(&file, bytes).unwrap();
        let settings = PublicationSettings::load(tmp.path()).unwrap();
        assert_eq!(settings.owner_name, "Example Owner");
        assert_eq!(settings.owner_email, "owner@example.invalid");
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        std::fs::write(ws.join("a.txt"), "updated\n").unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-am", "update"], &WHO)
            .await
            .unwrap();
        let collected = Git::new(&ws)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        bundle(&ws, &collected).await;
        let commit = collected_publication(
            &mirror,
            run_dir(&ws),
            "r",
            &collected,
            (&base, None),
            "Task",
            &settings,
        )
        .await
        .unwrap();
        assert_eq!(
            Git::new(&mirror)
                .run(&["log", "-1", "--format=%an <%ae> %cn <%ce>", &commit], &[])
                .await
                .unwrap(),
            "publisher.example.invalid <publisher@example.invalid> publisher.example.invalid <publisher@example.invalid>"
        );
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    fn scan_text(name: &str, text: &str) -> Option<Hit> {
        ScanPolicy::default().scan(name, text)
    }

    #[test]
    fn quoted_diff_paths_decode_git_escapes() {
        assert_eq!(diff_path(r#""b/a\tb.txt""#).unwrap(), "a\tb.txt");
        assert_eq!(diff_path(r#""b/\303\251.txt""#).unwrap(), "é.txt");
        assert_eq!(diff_path(r#""b/\360\237\230\200.txt""#).unwrap(), "😀.txt");
        assert_eq!(diff_path(r#""b/a\n\\\".txt""#).unwrap(), "a\n\\\".txt");
        assert_eq!(
            diff_path("\"b/caf\\303\\251 menu.txt \"\t").unwrap(),
            "café menu.txt "
        );
        assert_eq!(diff_path("b/menu.txt \t").unwrap(), "menu.txt ");
        for invalid in [r#""b/\q""#, r#""b/\40""#, r#""b/\777""#] {
            assert!(diff_path(invalid).is_err());
        }
    }

    #[tokio::test]
    async fn quoted_filenames_scan_content_and_trusted_ignores() {
        for path in [
            "a\tb.txt",
            "é.txt",
            "😀.txt",
            "a\nb.txt",
            "a\\b.txt",
            "a\"b.txt",
            "café menu.txt ",
            "a \tb.txt ",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path();
            Git::new(d).run(&["init", "--quiet"], &[]).await.unwrap();
            std::fs::create_dir(d.join(".dvb")).unwrap();
            // JSON strings use the same escapes needed for these TOML basic strings.
            let ignore = format!(
                "[[ignore]]\nfile = {}\nrule = 'github-token'\n",
                serde_json::to_string(path).unwrap()
            );
            std::fs::write(d.join(".dvb/scan-ignore.toml"), ignore).unwrap();
            Git::new(d).run(&["add", "."], &[]).await.unwrap();
            Git::new(d)
                .run(&["commit", "--quiet", "-m", "trusted ignore"], &WHO)
                .await
                .unwrap();
            let base = Git::new(d).run(&["rev-parse", "HEAD"], &[]).await.unwrap();
            std::fs::write(d.join(path), "harmless content\n").unwrap();
            Git::new(d).run(&["add", "."], &[]).await.unwrap();
            Git::new(d)
                .run(&["commit", "--quiet", "-m", "harmless content"], &WHO)
                .await
                .unwrap();
            assert_eq!(scan_commit(d, &base, "HEAD").await.unwrap(), None);
            std::fs::write(d.join(path), format!("{}\n", token())).unwrap();
            Git::new(d).run(&["add", "."], &[]).await.unwrap();
            Git::new(d)
                .run(&["commit", "--quiet", "-m", "fixture"], &WHO)
                .await
                .unwrap();
            assert_eq!(scan_commit(d, &base, "HEAD").await.unwrap(), None);
            // A different secret rule is still attributed to the actual filename.
            let key = format!("-----BEGIN {} KEY-----", "PRIVATE");
            std::fs::write(d.join(path), format!("{key}\n")).unwrap();
            Git::new(d)
                .run(&["commit", "--quiet", "-am", "unignored secret"], &WHO)
                .await
                .unwrap();
            let hit = scan_commit(d, &base, "HEAD").await.unwrap().unwrap();
            assert_eq!(hit.file, path);
            assert_eq!(hit.rule, "private-key");
            assert_eq!(hit.line, 1);
        }
    }

    #[test]
    fn the_release_rules_load() {
        let marker = format!("-----BEGIN {} KEY-----", "PRIVATE");
        assert_eq!(scan_text("release", &marker).unwrap().rule, "private-key");
    }

    #[test]
    fn a_secret_in_a_message_is_a_hit() {
        let key = format!("ghp_{}", "a".repeat(30));
        let hit = scan_text("message", &format!("ok\nsee {key}")).unwrap();
        assert_eq!((hit.line, hit.rule.as_str()), (2, "github-token"));
        assert_eq!(scan_text("message", "nothing here"), None);
    }

    #[tokio::test]
    async fn an_added_secret_is_found_with_its_line() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let env = [
            ("GIT_AUTHOR_NAME", "n"),
            ("GIT_AUTHOR_EMAIL", "n@example.invalid"),
            ("GIT_COMMITTER_NAME", "n"),
            ("GIT_COMMITTER_EMAIL", "n@example.invalid"),
        ];
        Git::new(d).run(&["init", "--quiet"], &[]).await.unwrap();
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        Git::new(d).run(&["add", "."], &[]).await.unwrap();
        Git::new(d)
            .run(&["commit", "--quiet", "-m", "a"], &env)
            .await
            .unwrap();
        let base = Git::new(d).run(&["rev-parse", "HEAD"], &[]).await.unwrap();
        // Split so no secret scanner flags this file.
        let key = format!("-----BEGIN RSA {} KEY-----", "PRIVATE");
        std::fs::write(d.join("a.txt"), format!("one\ntwo\n{key}\n")).unwrap();
        Git::new(d)
            .run(&["commit", "--quiet", "-am", "b"], &env)
            .await
            .unwrap();
        let hit = scan_commit(d, &base, "HEAD").await.unwrap().unwrap();
        assert_eq!(
            hit,
            Hit {
                file: "a.txt".into(),
                line: 3,
                rule: "private-key".into()
            }
        );
        assert_eq!(scan_commit(d, &base, &base).await.unwrap(), None);
    }

    fn token() -> String {
        format!("ghp_{}", "a".repeat(30))
    }

    const WHO: [(&str, &str); 4] = [
        ("GIT_AUTHOR_NAME", "n"),
        ("GIT_AUTHOR_EMAIL", "n@example.invalid"),
        ("GIT_COMMITTER_NAME", "n"),
        ("GIT_COMMITTER_EMAIL", "n@example.invalid"),
    ];

    /// A bare mirror with one base commit, and a workspace cloned from it.
    async fn mirror_and_ws(d: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let seed = d.join("seed");
        let mirror = d.join("mirror.git");
        let ws = d.join("ws");
        Git::new(d)
            .run(
                &["init", "--quiet", "-b", "main", seed.to_str().unwrap()],
                &[],
            )
            .await
            .unwrap();
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        Git::new(&seed).run(&["add", "."], &[]).await.unwrap();
        Git::new(&seed)
            .run(&["commit", "--quiet", "-m", "base"], &WHO)
            .await
            .unwrap();
        create_mirror(seed.to_str().unwrap(), &mirror)
            .await
            .unwrap();
        Git::new(d)
            .run(
                &[
                    "clone",
                    "--quiet",
                    mirror.to_str().unwrap(),
                    ws.to_str().unwrap(),
                ],
                &[],
            )
            .await
            .unwrap();
        let base = Git::new(&mirror)
            .run(&["rev-parse", "main"], &[])
            .await
            .unwrap();
        (mirror, ws, base)
    }

    /// A changed `a.txt` committed in `ws` as its collected snapshot.
    async fn collected_in(ws: &Path) -> String {
        std::fs::write(ws.join("a.txt"), "changed\n").unwrap();
        Git::new(ws)
            .run(&["commit", "--quiet", "-am", "snapshot"], &WHO)
            .await
            .unwrap();
        let head = Git::new(ws).run(&["rev-parse", "HEAD"], &[]).await.unwrap();
        bundle(ws, &head).await;
        head
    }

    /// Collects `head` in `ws` as `dvb run exec` does: `refs/dvb/collected`,
    /// bundled into the run directory.
    async fn bundle(ws: &Path, head: &str) {
        Git::new(ws)
            .run(&["update-ref", "refs/dvb/collected", head], &[])
            .await
            .unwrap();
        let bundle = run_dir(ws).join(COLLECTED_BUNDLE);
        Git::new(ws)
            .run(
                &[
                    "bundle",
                    "create",
                    "--quiet",
                    &bundle.to_string_lossy(),
                    "refs/dvb/collected",
                ],
                &[],
            )
            .await
            .unwrap();
    }

    fn run_dir(ws: &Path) -> &Path {
        ws.parent().unwrap()
    }

    async fn run_refs(mirror: &Path) -> String {
        Git::new(mirror)
            .run(&["for-each-ref", "refs/dvb/runs"], &[])
            .await
            .unwrap()
    }

    /// A repository the run does not own, whose one commit adds `foreign.txt`.
    async fn foreign(d: &Path) -> (std::path::PathBuf, String) {
        let other = d.join("other");
        Git::new(d)
            .run(
                &["init", "--quiet", "-b", "main", other.to_str().unwrap()],
                &[],
            )
            .await
            .unwrap();
        std::fs::write(other.join("foreign.txt"), "foreign\n").unwrap();
        Git::new(&other).run(&["add", "."], &[]).await.unwrap();
        Git::new(&other)
            .run(&["commit", "--quiet", "-m", "foreign"], &WHO)
            .await
            .unwrap();
        let head = Git::new(&other)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        (other, head)
    }

    #[tokio::test]
    async fn a_workspace_naming_another_repository_imports_nothing_foreign() {
        for way in ["alternates", "commondir", "linked ref"] {
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path();
            let (mirror, ws, _) = mirror_and_ws(d).await;
            let head = collected_in(&ws).await;
            let (other, foreign) = foreign(d).await;
            let git = ws.join(".git");
            match way {
                "alternates" => {
                    std::fs::write(
                        git.join("objects/info/alternates"),
                        format!("{}\n", other.join(".git/objects").display()),
                    )
                    .unwrap();
                    Git::new(&ws)
                        .run(&["update-ref", "refs/dvb/collected", &foreign], &[])
                        .await
                        .unwrap();
                }
                "commondir" => {
                    Git::new(&other)
                        .run(&["update-ref", "refs/dvb/collected", &foreign], &[])
                        .await
                        .unwrap();
                    std::fs::write(
                        git.join("commondir"),
                        format!("{}\n", other.join(".git").display()),
                    )
                    .unwrap();
                }
                _ => {
                    let collected = git.join("refs/dvb/collected");
                    std::fs::remove_file(&collected).unwrap();
                    std::os::unix::fs::symlink(other.join(".git/refs/heads/main"), &collected)
                        .unwrap();
                }
            }
            assert_eq!(
                Git::new(&ws)
                    .run(&["rev-parse", "refs/dvb/collected"], &[])
                    .await
                    .unwrap(),
                foreign,
                "{way}"
            );
            assert!(
                fetch_collected(&mirror, d, "r1", &foreign).await.is_err(),
                "{way}"
            );
            assert_eq!(run_refs(&mirror).await, "", "{way}");
            fetch_collected(&mirror, d, "r1", &head).await.unwrap();
            assert!(
                Git::new(&mirror)
                    .run(&["cat-file", "-e", &foreign], &[])
                    .await
                    .is_err(),
                "{way}"
            );
        }
    }

    #[tokio::test]
    async fn a_gitfile_in_place_of_the_bundle_imports_nothing_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, _) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        let (other, foreign) = foreign(d).await;
        Git::new(&other)
            .run(&["update-ref", "refs/dvb/collected", &foreign], &[])
            .await
            .unwrap();
        let bundle = d.join(COLLECTED_BUNDLE);
        let kept = std::fs::read(&bundle).unwrap();
        std::fs::write(
            &bundle,
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        assert!(fetch_collected(&mirror, d, "r1", &foreign).await.is_err());
        assert_eq!(run_refs(&mirror).await, "");
        assert!(
            Git::new(&mirror)
                .run(&["cat-file", "-e", &foreign], &[])
                .await
                .is_err()
        );
        std::fs::write(&bundle, kept).unwrap();
        fetch_collected(&mirror, d, "r1", &head).await.unwrap();
    }

    #[tokio::test]
    async fn only_a_regular_bundle_within_the_bound_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, _) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        let bundle = d.join(COLLECTED_BUNDLE);
        let elsewhere = d.join("elsewhere.bundle");
        std::fs::rename(&bundle, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &bundle).unwrap();
        assert!(fetch_collected(&mirror, d, "r1", &head).await.is_err());
        std::fs::remove_file(&bundle).unwrap();
        std::fs::create_dir(&bundle).unwrap();
        assert!(fetch_collected(&mirror, d, "r1", &head).await.is_err());
        std::fs::remove_dir(&bundle).unwrap();
        // Sparse: no gigabyte is written.
        std::fs::File::create(&bundle)
            .unwrap()
            .set_len(BUNDLE_MAX + 1)
            .unwrap();
        let err = fetch_collected(&mirror, d, "r1", &head).await.unwrap_err();
        assert!(err.to_string().contains("over"), "{err}");
        assert_eq!(run_refs(&mirror).await, "");
        std::fs::rename(&elsewhere, &bundle).unwrap();
        fetch_collected(&mirror, d, "r1", &head).await.unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_other_than_the_reported_head_leaves_no_run_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, base) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        fetch_collected(&mirror, d, "r1", &head).await.unwrap();
        assert_ne!(run_refs(&mirror).await, "");
        assert!(fetch_collected(&mirror, d, "r1", &base).await.is_err());
        assert_eq!(run_refs(&mirror).await, "");
    }

    /// Malformed trees fsck refuses, each with the fsck message it gives: a
    /// `.git` entry no checkout may write, and a `.gitmodules` whose url git
    /// would pass to a command as an option.
    const MALFORMED: [(&str, &str, &[u8]); 2] = [
        (".git", "hasDotgit", b"x\n"),
        (
            ".gitmodules",
            "gitmodulesUrl",
            b"[submodule \"x\"]\n\tpath = x\n\turl = --upload-pack=touch\n",
        ),
    ];

    /// A commit in `repo` whose tree holds only `name`, with `content`.
    async fn commit_with(repo: &Path, name: &str, content: &[u8]) -> String {
        let blob = Git::new(repo)
            .output(&["hash-object", "-w", "--stdin"], &[], Some(content))
            .await
            .unwrap();
        let blob = String::from_utf8(blob.stdout).unwrap();
        let entry = format!("100644 blob {}\t{name}\n", blob.trim());
        let tree = Git::new(repo)
            .output(&["mktree"], &[], Some(entry.as_bytes()))
            .await
            .unwrap();
        let tree = String::from_utf8(tree.stdout).unwrap();
        Git::new(repo)
            .run(&["commit-tree", tree.trim(), "-m", "snapshot"], &WHO)
            .await
            .unwrap()
    }

    /// Whether this git applies `fetch.fsckObjects` to a fetch from a bundle,
    /// which it does from 2.46.
    async fn fetch_fscks_bundles(dir: &Path) -> bool {
        let version = Git::new(dir).run(&["version"], &[]).await.unwrap();
        let mut parts = version
            .trim_start_matches("git version ")
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (2, 46)
    }

    #[tokio::test]
    async fn a_snapshot_failing_fsck_is_refused_at_fetch_and_leaves_no_run_ref() {
        let tmp = tempfile::tempdir().unwrap();
        if !fetch_fscks_bundles(tmp.path()).await {
            eprintln!("skipped: git before 2.46 does not fsck a fetch from a bundle");
            return;
        }
        for (name, message, content) in MALFORMED {
            let tmp = tempfile::tempdir().unwrap();
            let d = tmp.path();
            let (mirror, ws, _) = mirror_and_ws(d).await;
            let head = commit_with(&ws, name, content).await;
            bundle(&ws, &head).await;
            let err = fetch_collected(&mirror, d, "r1", &head).await.unwrap_err();
            let InternalError::Git(GitError::Failed { args, stderr, .. }) = err else {
                panic!("expected fetch to refuse the snapshot: {err}");
            };
            assert!(args.contains("fetch.fsckObjects=true fetch"), "{args}");
            let stderr = String::from_utf8_lossy(&stderr);
            assert!(stderr.contains(message), "{stderr}");
            assert_eq!(run_refs(&mirror).await, "");
        }
    }

    #[tokio::test]
    async fn a_missing_bundle_says_an_older_dvb_run_collected_it() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, base) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        std::fs::remove_file(d.join(COLLECTED_BUNDLE)).unwrap();
        let err = fetch_collected(&mirror, d, "r1", &head).await.unwrap_err();
        assert!(err.to_string().ends_with(NO_BUNDLE), "{err}");
        let settings = PublicationSettings::default();
        let err = collected_publication(&mirror, d, "r1", &head, (&base, None), "Task", &settings)
            .await
            .unwrap_err();
        assert!(err.to_string().ends_with(NO_BUNDLE), "{err}");
        assert_eq!(run_refs(&mirror).await, "");
    }

    #[tokio::test]
    async fn a_run_already_fetched_at_its_head_is_not_fetched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, base) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        fetch_collected(&mirror, d, "r1", &head).await.unwrap();
        std::fs::write(d.join(COLLECTED_BUNDLE), "not a bundle").unwrap();
        fetch_collected(&mirror, d, "r1", &head).await.unwrap();
        assert!(run_refs(&mirror).await.starts_with(&head));
        // Another head is fetched, and checked, again.
        assert!(fetch_collected(&mirror, d, "r1", &base).await.is_err());
        assert_eq!(run_refs(&mirror).await, "");
    }

    #[tokio::test]
    async fn engine_start_removes_stale_bundle_copies_from_the_mirrors() {
        let tmp = tempfile::tempdir().unwrap();
        let mirrors = tmp.path().join("mirrors");
        let (mirror, _, _) = mirror_and_ws(tmp.path()).await;
        std::fs::create_dir(&mirrors).unwrap();
        let mirror = {
            let moved = mirrors.join("p.git");
            std::fs::rename(&mirror, &moved).unwrap();
            moved
        };
        let stale = [
            mirror.join(".collected.abc"),
            mirror.join(".collected.def/check.pack"),
        ];
        let kept = [
            mirror.join("collected.abc"),
            mirrors.join(".cache/p/.collected.abc"),
        ];
        for file in kept.iter().chain(&stale) {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "copy").unwrap();
        }
        crate::Engine::open(
            crate::Config::dev(tmp.path(), "unused".into()),
            Default::default(),
        )
        .unwrap();
        assert!(!stale[0].exists() && !mirror.join(".collected.def").exists());
        assert!(kept.iter().all(|file| file.exists()));
    }

    #[tokio::test]
    async fn a_workspaces_own_configuration_runs_nothing_as_the_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (mirror, ws, base) = mirror_and_ws(d).await;
        let head = collected_in(&ws).await;
        let settings = PublicationSettings::default();
        // A partial clone whose snapshot lacks its tree: reading it would fetch
        // the tree from the configured remote, whose upload-pack leaves a mark.
        let tree = Git::new(&ws)
            .run(&["rev-parse", &format!("{head}^{{tree}}")], &[])
            .await
            .unwrap();
        let marker = d.join("marker");
        for (key, value) in [
            ("core.repositoryformatversion", "1"),
            ("extensions.partialClone", "origin"),
            ("remote.origin.promisor", "true"),
            ("remote.origin.url", ws.to_str().unwrap()),
            (
                "remote.origin.uploadpack",
                &format!("touch {}; false", marker.display()),
            ),
        ] {
            Git::new(&ws)
                .run(&["config", key, value], &[])
                .await
                .unwrap();
        }
        let objects = ws.join(".git/objects");
        std::fs::remove_file(objects.join(&tree[..2]).join(&tree[2..])).unwrap();
        collected_publication(&mirror, d, "r1", &head, (&base, None), "Task", &settings)
            .await
            .unwrap();
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn no_worker_commit_is_reachable_from_the_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        let worker = [
            ("GIT_AUTHOR_NAME", "dvb worker"),
            ("GIT_AUTHOR_EMAIL", "worker@dvb.invalid"),
            ("GIT_COMMITTER_NAME", "dvb worker"),
            ("GIT_COMMITTER_EMAIL", "worker@dvb.invalid"),
        ];
        // The worker commits a secret, removes it, and hands in a snapshot whose
        // parent is its own commit instead of the base.
        std::fs::write(ws.join("secret.txt"), token()).unwrap();
        Git::new(&ws).run(&["add", "."], &[]).await.unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-m", "mine"], &worker)
            .await
            .unwrap();
        std::fs::remove_file(ws.join("secret.txt")).unwrap();
        std::fs::write(ws.join("a.txt"), "two\n").unwrap();
        Git::new(&ws)
            .run(&["add", "--all", "."], &[])
            .await
            .unwrap();
        let tree = Git::new(&ws).run(&["write-tree"], &[]).await.unwrap();
        let snapshot = Git::new(&ws)
            .run(&["commit-tree", &tree, "-p", "HEAD", "-m", "snap"], &worker)
            .await
            .unwrap();
        bundle(&ws, &snapshot).await;

        let settings = PublicationSettings::default();
        let p = collected_publication(
            &mirror,
            run_dir(&ws),
            "r1",
            &snapshot,
            (&base, None),
            "Task",
            &settings,
        )
        .await
        .unwrap();
        assert_eq!(
            Git::new(&mirror).run(&["rev-list", &p], &[]).await.unwrap(),
            format!("{p}\n{base}")
        );
        let who = Git::new(&mirror)
            .run(&["log", "--format=%ae %ce", &p], &[])
            .await
            .unwrap();
        assert!(!who.contains("worker@dvb.invalid"), "{who}");
        assert_eq!(
            Git::new(&mirror)
                .run(&["show", &format!("{p}:a.txt")], &[])
                .await
                .unwrap(),
            "two"
        );

        // A snapshot other than the reported head is refused.
        let other = collected_publication(
            &mirror,
            run_dir(&ws),
            "r1",
            &base,
            (&base, None),
            "Task",
            &settings,
        )
        .await;
        assert!(other.is_err());
    }

    #[tokio::test]
    async fn a_secret_in_a_binary_file_is_found_whatever_the_attributes_say() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        std::fs::write(ws.join(".gitattributes"), "* -diff binary -text\n").unwrap();
        let mut blob = b"\x00\x01binary\n".to_vec();
        blob.extend_from_slice(token().as_bytes());
        blob.push(b'\n');
        std::fs::write(ws.join("data.bin"), blob).unwrap();
        Git::new(&ws)
            .run(&["add", "--all", "."], &[])
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-m", "bin"], &WHO)
            .await
            .unwrap();
        let head = Git::new(&ws)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["push", "--quiet", "origin", "HEAD:refs/heads/t"], &[])
            .await
            .unwrap();
        let hit = scan_commit(&mirror, &base, &head).await.unwrap().unwrap();
        assert_eq!(
            (hit.file.as_str(), hit.rule.as_str()),
            ("data.bin", "github-token")
        );
    }

    #[tokio::test]
    async fn an_added_line_that_looks_like_a_header_is_still_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        std::fs::write(ws.join("a.txt"), format!("one\n++ {}\n", token())).unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-am", "x"], &WHO)
            .await
            .unwrap();
        let head = Git::new(&ws)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["push", "--quiet", "origin", "HEAD:refs/heads/t"], &[])
            .await
            .unwrap();
        assert!(scan_commit(&mirror, &base, &head).await.unwrap().is_some());
    }
    #[tokio::test]
    async fn only_the_recorded_base_can_allow_a_scan_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        std::fs::create_dir(ws.join(".dvb")).unwrap();
        std::fs::write(
            ws.join(".dvb/scan-ignore.toml"),
            "[[ignore]]\nfile = 'a.txt'\nrule = 'github-token'\n",
        )
        .unwrap();
        std::fs::write(ws.join("a.txt"), format!("one\n{}\n", token())).unwrap();
        Git::new(&ws).run(&["add", "."], &[]).await.unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-m", "worker tries an ignore"], &WHO)
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["push", "--quiet", "origin", "HEAD:refs/heads/t"], &[])
            .await
            .unwrap();
        assert_eq!(
            scan_commit(&mirror, &base, "t")
                .await
                .unwrap()
                .unwrap()
                .rule,
            "github-token"
        );
        // The same exact allow-list can apply when it is already on the trusted base.
        let trusted = Git::new(&mirror)
            .run(&["rev-parse", "t"], &[])
            .await
            .unwrap();
        std::fs::write(
            ws.join("a.txt"),
            format!("one\n{}\n", token().replace('a', "b")),
        )
        .unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-am", "allowed fixture"], &WHO)
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["push", "--quiet", "origin", "HEAD:refs/heads/t"], &[])
            .await
            .unwrap();
        assert_eq!(scan_commit(&mirror, &trusted, "t").await.unwrap(), None);
        // Allowing a token does not allow another rule on the same line.
        std::fs::write(
            ws.join("a.txt"),
            format!("one\n{} AKIA{}\n", token(), "A".repeat(16)),
        )
        .unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-am", "second rule"], &WHO)
            .await
            .unwrap();
        Git::new(&ws)
            .run(&["push", "--quiet", "origin", "HEAD:refs/heads/t"], &[])
            .await
            .unwrap();
        assert_eq!(
            scan_commit(&mirror, &trusted, "t")
                .await
                .unwrap()
                .unwrap()
                .rule,
            "aws-access-key"
        );
    }

    #[tokio::test]
    async fn a_collected_tree_equal_to_its_base_is_not_published() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        let tree = Git::new(&ws)
            .run(&["rev-parse", &format!("{base}^{{tree}}")], &[])
            .await
            .unwrap();
        let collected = Git::new(&ws)
            .run(&["commit-tree", &tree, "-p", &base, "-m", "c"], &WHO)
            .await
            .unwrap();
        bundle(&ws, &collected).await;
        let settings = PublicationSettings {
            name: "project-bot".into(),
            email: "project-bot@example.invalid".into(),
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
            as_owner: false,
        };
        let err = collected_publication(
            &mirror,
            run_dir(&ws),
            "r",
            &collected,
            (&base, None),
            "Task",
            &settings,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CollectionError::NoChanges));
    }

    #[tokio::test]
    async fn the_owner_rule_covers_the_identity_not_the_files_and_the_identity_is_configurable() {
        let tmp = tempfile::tempdir().unwrap();
        let (mirror, ws, base) = mirror_and_ws(tmp.path()).await;
        std::fs::write(ws.join("a.txt"), "one\nExample Owner\n").unwrap();
        Git::new(&ws)
            .run(&["commit", "--quiet", "-am", "identity"], &WHO)
            .await
            .unwrap();
        let collected = Git::new(&ws)
            .run(&["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        bundle(&ws, &collected).await;
        let settings = PublicationSettings {
            name: "project-bot".into(),
            email: "project-bot@example.invalid".into(),
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
            as_owner: false,
        };
        let commit = collected_publication(
            &mirror,
            run_dir(&ws),
            "r",
            &collected,
            (&base, None),
            "Task",
            &settings,
        )
        .await
        .unwrap();
        assert_eq!(
            Git::new(&mirror)
                .run(&["log", "-1", "--format=%an <%ae> %cn <%ce>", &commit], &[])
                .await
                .unwrap(),
            "project-bot <project-bot@example.invalid> project-bot <project-bot@example.invalid>"
        );
        // The owner's name in a file is the project's content, not dvb's writing.
        assert_eq!(scan_commit(&mirror, &base, &commit).await.unwrap(), None);
        let identifying = PublicationSettings {
            name: "Example Owner".into(),
            ..settings.clone()
        };
        assert!(identifying.validate().is_err());
        // Publishing as the owner names them as author.
        let as_owner = PublicationSettings {
            as_owner: true,
            ..identifying
        };
        as_owner.validate().unwrap();
        let commit = collected_publication(
            &mirror,
            run_dir(&ws),
            "r2",
            &collected,
            (&base, None),
            "Task",
            &as_owner,
        )
        .await
        .unwrap();
        assert_eq!(
            Git::new(&mirror)
                .run(&["log", "-1", "--format=%an <%ae>", &commit], &[])
                .await
                .unwrap(),
            "Example Owner <project-bot@example.invalid>"
        );
        assert_eq!(scan_commit(&mirror, &base, &commit).await.unwrap(), None);
    }

    #[test]
    fn each_identity_rule_names_the_field_and_the_rule() {
        let token = format!("ghp_{}", "a".repeat(36));
        let (owner, mail) = ("Example Owner", "owner@example.invalid");
        for (name, email, as_owner, refused) in [
            ("dvb", "dvb@dvb.invalid", false, None),
            ("", "a@example.invalid", false, Some("name is empty")),
            ("dvb", "", false, Some("email is empty")),
            (owner, "", true, Some("email is empty")),
            (
                "a\nb",
                "a@example.invalid",
                false,
                Some("name can't contain a line break"),
            ),
            (
                "dvb",
                "a@example.invalid\r",
                false,
                Some("email can't contain a line break"),
            ),
            (
                "a <b>",
                "a@example.invalid",
                false,
                Some("name can't contain '<' or '>'"),
            ),
            (
                owner,
                "<owner@example.invalid>",
                true,
                Some("email can't contain '<' or '>'"),
            ),
            (
                "example owner",
                "a@example.invalid",
                false,
                Some("name can't contain your name (Example Owner) unless you publish as yourself"),
            ),
            (
                "dvb",
                "Owner@Example.invalid",
                false,
                Some(
                    "email can't contain your email (owner@example.invalid) unless you publish as yourself",
                ),
            ),
            (owner, mail, true, None),
            (
                token.as_str(),
                mail,
                true,
                Some("name looks like a secret (scan rule github-token)"),
            ),
        ] {
            let settings = PublicationSettings {
                name: name.into(),
                email: email.into(),
                owner_name: owner.into(),
                owner_email: mail.into(),
                as_owner,
            };
            assert_eq!(
                settings.validate().map_err(|e| e.to_string()),
                refused.map_or(Ok(()), |rule| Err(format!("The commit author {rule}."))),
                "{name:?} {email:?} {as_owner}"
            );
        }
    }
}
