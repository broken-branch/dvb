//! Synchronous provider children must be terminated and reaped on every exit.

use std::ops::{Deref, DerefMut};
use std::process::Child;

pub(crate) struct KillOnDrop(pub(crate) Child);

impl Deref for KillOnDrop {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The caller keeps kill/reap control while pipe readers drain both outputs.
pub(crate) fn output(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut child = KillOnDrop(
        cmd.process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let group = rustix::process::Pid::from_child(&child);
    std::thread::scope(|scope| {
        let mut result = (|| {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("no stdout"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| std::io::Error::other("no stderr"))?;
            let (send, recv) = std::sync::mpsc::channel();
            for (is_stdout, mut pipe) in [
                (true, Box::new(stdout) as Box<dyn Read + Send>),
                (false, Box::new(stderr) as Box<dyn Read + Send>),
            ] {
                let send = send.clone();
                std::thread::Builder::new().spawn_scoped(scope, move || {
                    let mut bytes = Vec::new();
                    let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
                    let _ = send.send((is_stdout, result));
                })?;
            }
            drop(send);
            let (mut stdout, mut stderr) = (None, None);
            let interval = std::time::Duration::from_millis(10);
            let mut remaining = timeout;
            loop {
                while let Ok((is_stdout, bytes)) = recv.try_recv() {
                    if is_stdout {
                        stdout = Some(bytes?);
                    } else {
                        stderr = Some(bytes?);
                    }
                }
                if let Some(status) = child.try_wait()?
                    && stdout.is_some()
                    && stderr.is_some()
                {
                    return Ok(std::process::Output {
                        status,
                        stdout: stdout.unwrap_or_default(),
                        stderr: stderr.unwrap_or_default(),
                    });
                }
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "provider command timed out",
                    ));
                }
                let wait = interval.min(remaining);
                std::thread::sleep(wait);
                remaining = remaining.saturating_sub(wait);
            }
        })();
        if result.is_err()
            && let Err(error) =
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
            && error != rustix::io::Errno::SRCH
        {
            result = result.map_err(|original| {
                std::io::Error::new(
                    original.kind(),
                    format!("{original}; probe group cleanup: {error}"),
                )
            });
        }
        // Reap the launcher before the scope joins readers released by group termination.
        drop(child);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::Clock;
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[test]
    fn probe_timeout_terminates_launcher_worker_and_finishes_both_readers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(dir.path().join("hold"))
                .status()
                .unwrap()
                .success()
        );
        let error = output(
            Command::new("sh").current_dir(dir.path()).args([
                "-c",
                "echo $$ > launcher; sh -c 'echo $$ > worker; readlink /proc/$$/fd/1 > stdout_pipe; \
                readlink /proc/$$/fd/2 > stderr_pipe; read line < hold' & wait",
            ]),
            std::time::Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let pid = std::fs::read_to_string(dir.path().join("worker")).unwrap();
        let worker = Path::new("/proc").join(pid.trim());
        // The launcher is our child; its worker is reaped asynchronously by init.
        let deadline = dvb_core::SystemClock.now().plus_millis(5_000);
        while worker.exists() && dvb_core::SystemClock.now() < deadline {
            std::thread::yield_now();
        }
        let alive = worker.exists();
        let pipes: Vec<_> = ["stdout_pipe", "stderr_pipe"]
            .map(|name| std::fs::read_to_string(dir.path().join(name)).unwrap())
            .into_iter()
            .collect();
        let blocked_readers = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|fd| match std::fs::read_link(fd.unwrap().path()) {
                Ok(target) => Some(target),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("reading probe pipe descriptor: {error}"),
            })
            .any(|target| pipes.iter().any(|pipe| target == Path::new(pipe.trim())));
        // Clean up the deliberately leaked worker.
        if alive {
            assert!(
                Command::new("kill")
                    .args(["-KILL", pid.trim()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let launcher = std::fs::read_to_string(dir.path().join("launcher")).unwrap();
        reaped(launcher.trim().parse().unwrap());
        assert!(!alive, "probe timeout left FIFO worker {pid} behind");
        assert!(
            !blocked_readers,
            "probe timeout left output readers blocked"
        );
    }

    #[test]
    fn short_probe_times_out_and_reaps_a_stuck_child() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("hold");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let pidfile = dir.path().join("pid");
        let error = output(
            Command::new("sh")
                .args(["-c", "echo $$ > \"$1\"; read line < \"$2\"", "probe"])
                .arg(&pidfile)
                .arg(fifo),
            std::time::Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let pid = std::fs::read_to_string(pidfile).unwrap();
        reaped(pid.trim().parse().unwrap());
    }

    fn live_child() -> KillOnDrop {
        let mut child = KillOnDrop(
            Command::new("sh")
                .args(["-c", "printf 'ready\\n'; read line"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        child
    }

    fn reaped(pid: u32) {
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "child {pid} was not reaped"
        );
    }

    #[test]
    fn cleanup_handles_error_unwind_and_already_completed_children() {
        let child = live_child();
        let pid = child.id();
        let fail = |_: KillOnDrop| -> Result<(), ()> { Err(()) };
        assert_eq!(fail(child), Err(()));
        reaped(pid);
        let child = live_child();
        let pid = child.id();
        assert!(
            std::panic::catch_unwind(move || {
                let _guard = child;
                panic!("fixture");
            })
            .is_err()
        );
        reaped(pid);
        for code in [0, 7] {
            let mut child = KillOnDrop(
                Command::new("sh")
                    .args(["-c", &format!("exit {code}")])
                    .spawn()
                    .unwrap(),
            );
            let pid = child.id();
            assert_eq!(child.wait().unwrap().code(), Some(code));
            drop(child);
            reaped(pid);
        }
    }

    #[tokio::test]
    async fn cancelling_a_task_terminates_and_reaps_its_guarded_child() {
        let (send, recv) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let child = live_child();
            send.send(child.id()).unwrap();
            std::future::pending::<()>().await;
            drop(child);
        });
        let pid = recv.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        reaped(pid);
    }
}
