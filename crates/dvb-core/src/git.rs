//! Async local Git, shared by workers, publication and forge implementations.

use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {args}: {}", String::from_utf8_lossy(stderr).trim())]
    Failed {
        args: String,
        stderr: Vec<u8>,
        status: ExitStatus,
    },
    #[error("git {args}: stopped after {}s, its time limit", limit.as_secs())]
    TimedOut { args: String, limit: Duration },
}

/// One command; inherits the caller's environment unless explicitly overridden.
pub struct Git(Command);

impl Git {
    pub fn new(dir: &Path) -> Self {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).kill_on_drop(true);
        Self(command)
    }

    /// Configure isolation for callers that need it (notably the GitHub gateway).
    pub fn command(&mut self) -> &mut Command {
        &mut self.0
    }

    /// Preserve raw stdout for binary diffs and NUL-separated paths.
    pub async fn output(
        mut self,
        args: &[&str],
        env: &[(&str, &str)],
        input: Option<&[u8]>,
    ) -> Result<Output, GitError> {
        self.0.args(args).envs(env.iter().copied());
        let (out, written) = if let Some(input) = input {
            let mut child = self
                .0
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("git: no stdin"))?;
            // Drain output while writing; neither pipe may block the other.
            let (written, out) = tokio::join!(
                async move {
                    let written = stdin.write_all(input).await;
                    drop(stdin);
                    written
                },
                child.wait_with_output(),
            );
            (out?, written)
        } else {
            (self.0.output().await?, Ok(()))
        };
        let out = succeeded(args, out)?;
        written?;
        Ok(out)
    }

    /// `run` reading `stdin`, stopped once `limit` passes. git's own children,
    /// such as the `index-pack` of a fetch, outlive a killed git, so the
    /// command runs in its own process group and the whole group is killed.
    pub async fn run_within(
        mut self,
        limit: Duration,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Stdio,
    ) -> Result<String, GitError> {
        let child = self
            .0
            .args(args)
            .envs(env.iter().copied())
            .process_group(0)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let group = child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(i32::try_from(id).ok()?))
            .ok_or_else(|| std::io::Error::other("git: no process id"))?;
        let out = child.wait_with_output();
        tokio::pin!(out);
        // Wall time on a thread, not tokio's timer: a paused test clock fires
        // that as soon as git runs. Dropping `running` ends the thread.
        let (running, finished) = std::sync::mpsc::channel::<()>();
        let timer = tokio::task::spawn_blocking(move || finished.recv_timeout(limit));
        tokio::select! {
            // Until the output is ready the group still has a member, so its
            // id names no other process.
            biased;
            out = &mut out => {
                drop(running);
                let out = succeeded(args, out?)?;
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
            }
            // While `running` lives, only the limit ends the wait.
            _ = timer => {
                match rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                    Err(errno) => return Err(std::io::Error::from(errno).into()),
                }
                Err(GitError::TimedOut {
                    args: args.join(" "),
                    limit,
                })
            }
        }
    }

    /// Return trimmed, lossy UTF-8 stdout.
    pub async fn run(self, args: &[&str], env: &[(&str, &str)]) -> Result<String, GitError> {
        let out = self.output(args, env, None).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

fn succeeded(args: &[&str], out: Output) -> Result<Output, GitError> {
    if out.status.success() {
        Ok(out)
    } else {
        Err(GitError::Failed {
            args: args.join(" "),
            stderr: out.stderr,
            status: out.status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn git_preserves_stdout_environment_stderr_and_status() {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path())
            .run(&["init", "--quiet"], &[])
            .await
            .unwrap();
        assert_eq!(
            Git::new(dir.path())
                .run(
                    &["var", "GIT_AUTHOR_IDENT"],
                    &[
                        ("GIT_AUTHOR_NAME", "worker fixture"),
                        ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
                        ("GIT_AUTHOR_DATE", "@1 +0000"),
                    ]
                )
                .await
                .unwrap(),
            "worker fixture <fixture@example.invalid> 1 +0000"
        );
        let args = [
            "-c",
            "alias.fixture=!printf '  \\377stdout\\n  '",
            "fixture",
        ];
        let raw = Git::new(dir.path()).output(&args, &[], None).await.unwrap();
        assert_eq!(raw.stdout, b"  \xffstdout\n  ");
        assert_eq!(
            Git::new(dir.path()).run(&args, &[]).await.unwrap(),
            "\u{fffd}stdout"
        );
        for (command, code, signal) in [
            (
                "alias.fixture=!printf 'fixture failure\\n' >&2; exit 23",
                Some(23),
                None,
            ),
            (
                "alias.fixture=!printf 'fixture failure\\n' >&2; kill -TERM $PPID",
                None,
                Some(15),
            ),
        ] {
            let err = Git::new(dir.path())
                .run(&["-c", command, "fixture"], &[])
                .await
                .unwrap_err();
            let GitError::Failed { stderr, status, .. } = err else {
                panic!("expected command failure")
            };
            assert_eq!(stderr, b"fixture failure\n");
            assert_eq!(status.code(), code);
            assert_eq!(status.signal(), signal);
        }
        let mut git = Git::new(dir.path());
        git.command().current_dir(dir.path().join("missing"));
        assert!(matches!(
            git.run(&["status"], &[]).await,
            Err(GitError::Io(_))
        ));
    }

    #[tokio::test]
    async fn a_failed_command_keeps_its_status_when_it_closes_stdin_early() {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path())
            .run(&["init", "--quiet"], &[])
            .await
            .unwrap();
        let input = vec![b'a'; 256 * 1024];
        let error = Git::new(dir.path())
            .output(
                &[
                    "-c",
                    "alias.fixture=!printf 'fixture failure\\n' >&2; exit 23",
                    "fixture",
                ],
                &[],
                Some(&input),
            )
            .await
            .unwrap_err();
        let GitError::Failed { stderr, status, .. } = error else {
            panic!("expected command stderr and status, got {error}")
        };
        assert_eq!(status.code(), Some(23));
        assert_eq!(stderr, b"fixture failure\n");
    }

    #[tokio::test]
    async fn a_command_over_its_time_limit_is_stopped_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path())
            .run(&["init", "--quiet"], &[])
            .await
            .unwrap();
        assert_eq!(
            Git::new(dir.path())
                .run_within(
                    Duration::from_secs(60),
                    &["rev-parse", "--is-inside-git-dir"],
                    &[],
                    Stdio::null(),
                )
                .await
                .unwrap(),
            "false"
        );
        let pid = dir.path().join("pid");
        // A child that outlives git, as a fetch's index-pack does.
        let alias = format!(
            "alias.fixture=!sh -c 'echo $$ > {}; exec sleep 600' & wait",
            pid.display()
        );
        let error = Git::new(dir.path())
            .run_within(
                Duration::from_secs(1),
                &["-c", &alias, "fixture"],
                &[],
                Stdio::null(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, GitError::TimedOut { limit, .. } if limit == Duration::from_secs(1)),
            "{error}"
        );
        let pid = std::fs::read_to_string(pid).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        // SIGKILL is delivered asynchronously: wait, bounded, for the child to end.
        let ended = (0..500).any(|_| {
            let ended = std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit(')').next().unwrap().trim_start().starts_with('Z')
            });
            if !ended {
                std::thread::sleep(Duration::from_millis(10));
            }
            ended
        });
        assert!(ended, "the command's child still runs");
    }

    #[tokio::test]
    async fn git_writes_input_and_drains_output_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        Git::new(dir.path())
            .run(&["init", "--quiet"], &[])
            .await
            .unwrap();
        let input = vec![b'a'; 256 * 1024];
        // Echo more than a pipe buffer while receiving more than a pipe buffer.
        let out = Git::new(dir.path())
            .output(&["-c", "alias.echo=!cat", "echo"], &[], Some(&input))
            .await
            .unwrap();
        assert_eq!(out.stdout, input);
    }
}
