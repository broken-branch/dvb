//! Repository setup is an engine publication, before the backstop, on a new repo
//! only; an existing repo gets the backstop alone. CI names are recorded, never
//! fabricated.
use super::{EMPTY_TREE, Hit, neutral_commit};
use crate::{AppState, Fail};
use dvb_core::api::{CommitIdentity, PublicationMismatch};
use dvb_core::event::CiMode;
use dvb_core::forge::{Forge, Repo};
use dvb_core::git::Git;
use dvb_core::{Actor, EventId, EventKind, Refusal, RefusalCode};
use dvb_store::Select;
use std::path::Path;

pub fn scan_refusal(hit: Hit) -> Fail {
    Refusal::new(
        RefusalCode::ScanHit {
            file: hit.file,
            line: hit.line,
            rule: hit.rule,
        },
        "outbound text blocked",
    )
    .into()
}

/// What `project add --create` on a personal account is told: GitHub lets an App
/// installation create repositories only in an organization.
pub const PERSONAL_CREATE: &str = "dvb cannot create a repository in a personal GitHub account; create it on GitHub, give the dvb App access to it, then run dvb project add without --create";

pub fn expected_ci(required: &[String], choice: &CiMode) -> Result<CiMode, Fail> {
    let mut names = required.to_vec();
    match choice {
        CiMode::Expected(chosen) => names.extend(chosen.iter().cloned()),
        CiMode::NoCi if names.is_empty() => return Ok(CiMode::NoCi),
        CiMode::NoCi => {}
    }
    names.sort();
    names.dedup();
    if names.is_empty() || names.iter().any(|name| name.trim().is_empty()) {
        return Err(Refusal::new(
            RefusalCode::CiEvidenceMissing,
            "choose expected checks or explicitly choose no CI",
        )
        .into());
    }
    Ok(CiMode::Expected(names))
}

/// Find the immutable repository identity recorded for this project request.
pub fn creation_receipt(e: &AppState, name: &str, repo: &Repo) -> Result<Option<u64>, Fail> {
    let created = e
        .store
        .select(EventId(0), Select::Kinds(&["repository.created"]))?;
    Ok(created.iter().find_map(|ev| {
        if let EventKind::RepositoryCreated {
            name: created_name,
            repo: created_repo,
            repository_id,
        } = &ev.kind
            && created_name == name
            && created_repo == &repo.0
        {
            Some(*repository_id)
        } else {
            None
        }
    }))
}

/// Resume only a successful creation belonging to this project request. Existing
/// repositories with no receipt still go through create_repo and are refused.
pub async fn create_or_resume(
    e: &AppState,
    forge: &dyn Forge,
    name: &str,
    repo: &Repo,
    org: &str,
    repo_name: &str,
) -> Result<(), Fail> {
    let receipt = creation_receipt(e, name, repo)?;
    if let Some(id) = receipt {
        forge.resume_setup(repo, id).await.map_err(Fail::internal)?;
    } else {
        let created = forge
            .create_repo(org, repo_name)
            .await
            .map_err(Fail::internal)?;
        if &created != repo {
            return Err(Fail::internal("created repository differs from request"));
        }
        let repository_id = forge
            .created_repo_id(repo)
            .ok_or_else(|| Fail::internal("creation response has no repository identity"))?;
        e.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::RepositoryCreated {
                name: name.into(),
                repo: repo.0.clone(),
                repository_id,
            }])
        })?;
    }
    Ok(())
}

/// What `prepare` found: the default branch, the checks to record and whether
/// GitHub allows dvb's ruleset (not on a private repository on GitHub Free).
#[derive(Debug)]
pub struct Prepared {
    pub default: String,
    pub ci: CiMode,
    pub rulesets: bool,
}

/// Clone and validate the request without writing to the remote repository.
pub async fn prepare(
    forge: &dyn Forge,
    repo: &Repo,
    mirror: &Path,
    created: bool,
    requested_branch: Option<&str>,
    choice: &CiMode,
) -> Result<Prepared, Fail> {
    let rulesets = forge.rulesets_allowed(repo).await.map_err(Fail::internal)?;
    let required = if created || !rulesets {
        vec![]
    } else {
        forge
            .ruleset(repo)
            .await
            .map_err(Fail::internal)?
            .required_checks
    };
    let ci = expected_ci(&required, choice)?;
    let default = forge
        .clone_repo(repo, mirror)
        .await
        .map_err(Fail::internal)?;
    if requested_branch.is_some_and(|b| b != default) {
        return Err(Fail::internal(
            "requested branch differs from the repository default branch",
        ));
    }
    Ok(Prepared {
        default,
        ci,
        rulesets,
    })
}

/// The author of the newest dvb publication the default branch's recent
/// squash merges name (`Reviewed publication <sha>.`). GitHub names its App as
/// the merge's author, so read the publication itself, through the forge when
/// branch cleanup has made it unreachable from the clone.
pub async fn recent_dvb_author(
    forge: &dyn Forge,
    repo: &Repo,
    mirror: &Path,
    branch: &str,
) -> Result<Option<CommitIdentity>, crate::InternalError> {
    let branch = format!("refs/heads/{branch}");
    // An empty repository has no default-branch commit.
    if Git::new(mirror)
        .run(&["rev-parse", "--verify", "--quiet", &branch], &[])
        .await
        .is_err()
    {
        return Ok(None);
    }
    let bodies = Git::new(mirror)
        .run(
            &["log", "--first-parent", "-n", "200", "--format=%b", &branch],
            &[],
        )
        .await?;
    let mut named = bodies.lines().filter_map(|line| {
        line.strip_prefix("Reviewed publication ")?
            .strip_suffix('.')
            .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    let Some(sha) = named.next() else {
        return Ok(None);
    };
    let commit = format!("{sha}^{{commit}}");
    if Git::new(mirror)
        .run(&["rev-parse", "--verify", "--quiet", &commit], &[])
        .await
        .is_err()
    {
        return Ok(Some(forge.commit_author(repo, sha).await?));
    }
    let who = Git::new(mirror)
        .run(&["log", "-1", "--format=%an%n%ae", &commit], &[])
        .await?;
    if let Some((name, email)) = who.split_once('\n') {
        return Ok(Some(CommitIdentity {
            name: name.into(),
            email: email.into(),
        }));
    }
    Ok(None)
}

/// The publishing identity and `recent`, unless they are the same.
pub fn mismatch(
    recent: CommitIdentity,
    publication: &super::PublicationSettings,
) -> Option<PublicationMismatch> {
    let (name, email) = (&publication.name, &publication.email);
    (recent.name != *name || !recent.email.eq_ignore_ascii_case(email)).then(|| {
        let policy = publication.policy();
        let recent_is_owner = [&recent.name, &recent.email].into_iter().any(|value| {
            policy
                .hits("commit identity", value)
                .iter()
                .any(|hit| matches!(hit.rule.as_str(), "owner-name" | "owner-email"))
        });
        PublicationMismatch {
            publishes_as: CommitIdentity {
                name: name.clone(),
                email: email.clone(),
            },
            recent,
            recent_is_owner,
        }
    })
}

/// Publish setup only after the caller has checked any existing project mirror.
/// Where GitHub allows no ruleset the branch gets none: the engine is its only
/// pusher and merges only when the recorded checks pass.
pub async fn publish_setup(
    e: &AppState,
    forge: &dyn Forge,
    repo: &Repo,
    mirror: &Path,
    created: bool,
    prepared: &Prepared,
) -> Result<CiMode, Fail> {
    let (default, ci) = (prepared.default.as_str(), &prepared.ci);
    let names = match ci {
        CiMode::Expected(names) => names.clone(),
        CiMode::NoCi => vec![],
    };
    if created {
        let source = serde_json::to_string_pretty(&serde_json::json!({"version":1,"ci":ci}))
            .map_err(Fail::internal)?;
        publish_created(e, forge, repo, mirror, default, ci, &source).await?;
    }
    if !prepared.rulesets {
        return Ok(ci.clone());
    }
    forge
        .install_ruleset(repo, default, &names)
        .await
        .map_err(Fail::internal)?;
    // Read back the effective policy. A refused/inactive backstop must not be recorded as ready.
    let actual = forge.ruleset(repo).await.map_err(Fail::internal)?;
    if !actual.pr_required
        || !actual.no_force_push
        || names.iter().any(|n| !actual.required_checks.contains(n))
    {
        return Err(Fail::internal(
            "GitHub did not enforce the requested branch backstop",
        ));
    }
    expected_ci(&actual.required_checks, ci)
}

async fn publish_created(
    e: &AppState,
    forge: &dyn Forge,
    repo: &Repo,
    mirror: &Path,
    default: &str,
    ci: &CiMode,
    source: &str,
) -> Result<(), Fail> {
    let setups = e
        .store
        .select(EventId(0), Select::Kinds(&["repository.setup_prepared"]))?;
    let prepared = setups.into_iter().rev().find_map(|ev| {
        if let EventKind::RepositorySetupPrepared {
            repo: saved_repo,
            branch,
            head,
            ci,
        } = ev.kind
            && saved_repo == repo.0
        {
            Some((branch, head, ci))
        } else {
            None
        }
    });
    let head = if let Some((branch, head, saved_ci)) = prepared {
        if branch != default || &saved_ci != ci {
            return Err(Fail::internal("setup retry differs from recorded setup"));
        }
        head
    } else {
        let head = setup_commit(e, mirror, source).await?;
        // Pin the object before recording it, so recovery can push exactly it.
        let pin = setup_mirror(e, repo)?;
        Git::new(mirror)
            .run(&["update-ref", "refs/dvb/setup", &head], &[])
            .await
            .map_err(Fail::internal)?;
        if !pin.exists() {
            std::fs::create_dir_all(
                pin.parent()
                    .ok_or_else(|| Fail::internal("setup mirror has no parent"))?,
            )
            .map_err(Fail::internal)?;
            Git::new(&e.cfg.paths.mirrors)
                .run(&["init", "--quiet", "--bare", &pin.to_string_lossy()], &[])
                .await
                .map_err(Fail::internal)?;
        }
        Git::new(&pin)
            .run(
                &[
                    "fetch",
                    "--quiet",
                    &mirror.to_string_lossy(),
                    "+refs/dvb/setup:refs/dvb/setup",
                ],
                &[],
            )
            .await
            .map_err(Fail::internal)?;
        // Keep recovery independent of a temporary project-add staging directory.
        e.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::RepositorySetupPrepared {
                repo: repo.0.clone(),
                branch: default.into(),
                head: head.clone(),
                ci: ci.clone(),
            }])
        })?;
        head
    };
    let remote_head = Git::new(mirror)
        .run(
            &["rev-parse", "--verify", &format!("refs/heads/{default}")],
            &[],
        )
        .await
        .ok();
    if remote_head.as_deref().is_some_and(|h| h != head) {
        return Err(Fail::internal(
            "created repository was initialized by another writer",
        ));
    }
    if remote_head.is_none() {
        // Recover the prepared object from the engine-owned setup mirror.
        Git::new(mirror)
            .run(
                &[
                    "fetch",
                    "--quiet",
                    &setup_mirror(e, repo)?.to_string_lossy(),
                    "refs/dvb/setup",
                ],
                &[],
            )
            .await
            .map_err(Fail::internal)?;
        forge
            .push_setup(repo, mirror, default, &head)
            .await
            .map_err(Fail::internal)?;
    }
    Git::new(mirror)
        .run(
            &["update-ref", &format!("refs/heads/{default}"), &head],
            &[],
        )
        .await
        .map_err(Fail::internal)?;
    Ok(())
}

fn setup_mirror(e: &AppState, repo: &Repo) -> Result<std::path::PathBuf, Fail> {
    let id = e
        .store
        .select(EventId(0), Select::Kinds(&["repository.created"]))?
        .iter()
        .find_map(|ev| {
            if let EventKind::RepositoryCreated {
                repo: saved,
                repository_id,
                ..
            } = &ev.kind
                && saved == &repo.0
            {
                Some(*repository_id)
            } else {
                None
            }
        })
        .ok_or_else(|| Fail::internal("no repository creation receipt"))?;
    Ok(e.cfg.paths.mirrors.join(".setup").join(format!("{id}.git")))
}

async fn setup_commit(e: &AppState, mirror: &Path, source: &str) -> Result<String, Fail> {
    let temp = tempfile::tempdir().map_err(Fail::internal)?;
    let index = temp.path().join("index");
    let file = temp.path().join("project.json");
    std::fs::write(&file, source).map_err(Fail::internal)?;
    let index_name = index.to_string_lossy();
    let env = [("GIT_INDEX_FILE", index_name.as_ref())];
    Git::new(mirror)
        .run(&["read-tree", EMPTY_TREE], &env)
        .await
        .map_err(Fail::internal)?;
    let blob = Git::new(mirror)
        .run(&["hash-object", "-w", &file.to_string_lossy()], &[])
        .await
        .map_err(Fail::internal)?;
    Git::new(mirror)
        .run(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},.dvb/project.json"),
            ],
            &env,
        )
        .await
        .map_err(Fail::internal)?;
    let tree = Git::new(mirror)
        .run(&["write-tree"], &env)
        .await
        .map_err(Fail::internal)?;
    let message = "Set up dvb";
    neutral_commit(mirror, &tree, &[], message, &e.publication())
        .await
        .map_err(Fail::internal)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn setup_errors_retain_forge_classification_and_rate_limit_sources() {
        use crate::InternalError;
        use dvb_core::forge::{ForgeError, RateLimit};
        use std::error::Error;
        for rate_limit in [
            None,
            Some(RateLimit {
                reset: Some(dvb_core::Timestamp(i64::MAX - 1_000)),
            }),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let forge = std::sync::Arc::new(dvb_forge::FakeForge::local_git());
            let e = crate::Engine::open(
                crate::Config::dev(dir.path(), "unused".into()),
                crate::EngineOptions {
                    forge: Some(forge.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            let original = ForgeError {
                rate_limit,
                retryable: true,
                detail: "setup budget".into(),
            };
            forge.fail_next("ruleset", original.clone());
            let error = prepare(
                forge.as_ref(),
                &Repo("org/repo".into()),
                &e.mirror("p"),
                false,
                None,
                &CiMode::NoCi,
            )
            .await
            .unwrap_err();
            let internal = error
                .source()
                .unwrap()
                .downcast_ref::<InternalError>()
                .unwrap();
            assert_eq!(
                internal.source().unwrap().downcast_ref::<ForgeError>(),
                Some(&original)
            );
            assert!(matches!(internal, InternalError::Forge(source) if source == &original));
        }
    }

    use super::*;
    #[test]
    fn a_mismatch_says_whether_the_recent_author_is_the_owner() {
        let publication = crate::publish::PublicationSettings {
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
            ..Default::default()
        };
        let recent_is_owner = |name: &str, email: &str| {
            let recent = CommitIdentity {
                name: name.into(),
                email: email.into(),
            };
            mismatch(recent, &publication).map(|m| m.recent_is_owner)
        };
        assert_eq!(recent_is_owner("dvb", "dvb@dvb.invalid"), None);
        assert_eq!(recent_is_owner("bot", "bot@example.invalid"), Some(false));
        assert_eq!(recent_is_owner("bot", "Owner@example.invalid"), Some(true));
        assert_eq!(
            recent_is_owner("Example Owner", "x@example.invalid"),
            Some(true)
        );
    }
    #[test]
    fn required_checks_survive_owner_choices_and_silence_is_refused() {
        let required = vec!["rust".into()];
        assert_eq!(
            expected_ci(&required, &CiMode::NoCi).unwrap(),
            CiMode::Expected(required.clone())
        );
        assert_eq!(
            expected_ci(&required, &CiMode::Expected(vec!["ui".into()])).unwrap(),
            CiMode::Expected(vec!["rust".into(), "ui".into()])
        );
        assert!(matches!(
            expected_ci(&[], &CiMode::Expected(vec![])),
            Err(Fail::Refused(Refusal {
                code: RefusalCode::CiEvidenceMissing,
                ..
            }))
        ));
        assert_eq!(expected_ci(&[], &CiMode::NoCi).unwrap(), CiMode::NoCi);
    }
    #[tokio::test]
    async fn existing_repo_add_opens_no_pr_and_pushes_no_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let forge = std::sync::Arc::new(dvb_forge::FakeForge::local_git());
        let e = crate::Engine::open(
            crate::Config::dev(&tmp.path().join("engine"), "dvb".into()),
            crate::EngineOptions {
                forge: Some(forge.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let seed = tmp.path().join("seed");
        let origin = tmp.path().join("origin.git");
        Git::new(tmp.path())
            .run(
                &["init", "--quiet", "-b", "trunk", seed.to_str().unwrap()],
                &[],
            )
            .await
            .unwrap();
        std::fs::write(seed.join("existing.txt"), "keep this\n").unwrap();
        Git::new(&seed).run(&["add", "."], &[]).await.unwrap();
        let identity = [
            ("GIT_AUTHOR_NAME", "seed"),
            ("GIT_AUTHOR_EMAIL", "seed@example.invalid"),
            ("GIT_COMMITTER_NAME", "seed"),
            ("GIT_COMMITTER_EMAIL", "seed@example.invalid"),
        ];
        Git::new(&seed)
            .run(&["commit", "--quiet", "-m", "seed"], &identity)
            .await
            .unwrap();
        Git::new(tmp.path())
            .run(
                &[
                    "clone",
                    "--quiet",
                    "--bare",
                    seed.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
                &[],
            )
            .await
            .unwrap();
        let refs = Git::new(&origin)
            .run(&["for-each-ref", "--format=%(objectname) %(refname)"], &[])
            .await
            .unwrap();
        let repo = Repo(origin.to_str().unwrap().into());
        forge.set_ruleset(
            &repo,
            dvb_core::forge::ExpectedChecks {
                pr_required: true,
                required_checks: vec!["existing-ci".into()],
                no_force_push: true,
            },
        );
        let mirror = e.mirror("project");
        let prepared = prepare(
            forge.as_ref(),
            &repo,
            &mirror,
            false,
            None,
            &CiMode::Expected(vec!["new-ci".into()]),
        )
        .await
        .unwrap();
        let ci = publish_setup(&e, forge.as_ref(), &repo, &mirror, false, &prepared)
            .await
            .unwrap();
        assert_eq!(prepared.default, "trunk");
        assert_eq!(
            ci,
            CiMode::Expected(vec!["existing-ci".into(), "new-ci".into()])
        );
        assert_eq!(
            Git::new(&origin)
                .run(&["for-each-ref", "--format=%(objectname) %(refname)"], &[])
                .await
                .unwrap(),
            refs
        );
        let calls = forge.calls();
        for write in ["push", "push_setup", "open_pr"] {
            assert!(!calls.iter().any(|call| call == write), "{calls:?}");
        }
        assert!(
            calls.iter().any(|call| call == "install_ruleset"),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn new_repo_setup_is_one_root_commit_without_worker_history() {
        let tmp = tempfile::tempdir().unwrap();
        let e = crate::Engine::open(
            crate::Config::dev(&tmp.path().join("engine"), "dvb".into()),
            Default::default(),
        )
        .unwrap();
        let mirror = e.mirror("project");
        Git::new(tmp.path())
            .run(
                &["init", "--quiet", "--bare", mirror.to_str().unwrap()],
                &[],
            )
            .await
            .unwrap();
        let head = setup_commit(&e, &mirror, "{}\n").await.unwrap();
        assert_eq!(
            Git::new(&mirror)
                .run(&["rev-list", "--count", &head], &[])
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(
            Git::new(&mirror)
                .run(&["show", &format!("{head}:.dvb/project.json")], &[])
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(
            Git::new(&mirror)
                .run(&["log", "-1", "--format=%an <%ae>", &head], &[])
                .await
                .unwrap(),
            "dvb <dvb@dvb.invalid>"
        );
    }
}
