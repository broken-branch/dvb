//! Claude's child pipe, plus pin matching and PATH helpers for providers.

use crate::child::KillOnDrop;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};

/// A child whose stdout is read line by line.
pub struct Pipe {
    child: Option<KillOnDrop>,
    out: BufReader<ChildStdout>,
}

impl Pipe {
    /// Starts `cmd` with `stdin` written to it from a thread (a long prompt must
    /// not block reading the output) and then closed.
    pub fn spawn(mut cmd: Command, stdin: Vec<u8>) -> std::io::Result<Self> {
        let mut child = KillOnDrop(
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        if let Some(mut w) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = w.write_all(&stdin);
            });
        }
        let out = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no stdout"))?,
        );
        Ok(Self {
            child: Some(child),
            out,
        })
    }

    /// The next line, or `None` at the end of the output. A line that is not
    /// UTF-8 is an `InvalidData` error.
    pub fn line(&mut self) -> std::io::Result<Option<String>> {
        let mut s = String::new();
        Ok((self.out.read_line(&mut s)? > 0).then_some(s))
    }

    /// Waits for the child; its exit code (`None` when a signal ended it).
    pub fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.child
            .as_mut()
            .ok_or_else(|| std::io::Error::other("child already stopped"))?
            .wait()
            .map(|status| status.code())
    }

    pub fn kill(&mut self) {
        drop(self.child.take());
    }
}

/// Optional vendor metadata falls back when its type is
/// unfamiliar. Required evidence and usage fields still deserialize strictly.
pub fn optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Whether a served model id is the pinned one: equal, or the pin plus a date
/// suffix (`claude-haiku-4-5` and `claude-haiku-4-5-20251001`).
pub fn pin_matches(pin: &str, served: &str) -> bool {
    served == pin
        || served
            .strip_prefix(pin)
            .and_then(|r| r.strip_prefix('-'))
            .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// `PATH` with `dir` first.
pub fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_a_live_pipe_terminates_and_reaps_its_child() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("hold");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo $$; read line < \"$1\"", "pipe"])
            .arg(fifo);
        let mut pipe = Pipe::spawn(command, Vec::new()).unwrap();
        let pid = pipe.line().unwrap().unwrap();
        assert!(std::path::Path::new("/proc").join(pid.trim()).exists());
        drop(pipe);
        assert!(!std::path::Path::new("/proc").join(pid.trim()).exists());
    }

    #[test]
    fn a_pin_matches_itself_and_its_dated_form_only() {
        assert!(pin_matches("claude-haiku-4-5", "claude-haiku-4-5"));
        assert!(pin_matches("claude-haiku-4-5", "claude-haiku-4-5-20251001"));
        assert!(!pin_matches("claude-haiku-4-5", "claude-haiku-4-5-turbo"));
        assert!(!pin_matches(
            "claude-sonnet-5-5",
            "claude-haiku-4-5-20251001"
        ));
        assert!(!pin_matches("claude-haiku-4", "claude-haiku-4-5"));
    }
}
