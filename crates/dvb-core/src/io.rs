//! Shared Linux identity and same-directory atomic file replacement.

use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The installed owner API socket; dev sockets remain under their dev root.
pub const ENGINE_SOCKET: &str = "/run/dvb/engine.sock";

/// This process's effective uid, the owner procfs reports for `/proc/self`.
pub fn own_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Publish whole bytes through a fresh temp file beside `path` (created with
/// `mode` less the umask) and an atomic rename that replaces, never follows, a
/// symlink. `sync` requests durability before rename. The temp file is removed
/// on every failure. Directory syncing belongs to the caller, which decides
/// whether replacement also needs directory durability.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32, sync: bool) -> io::Result<()> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".write.")
        .permissions(std::fs::Permissions::from_mode(mode))
        .tempfile_in(path.parent().unwrap_or(Path::new(".")))?;
    tmp.write_all(bytes)?;
    if sync {
        tmp.as_file().sync_all()?;
    }
    tmp.persist(path).map(|_| ()).map_err(|e| e.error)
}

/// Login files cross users. Match `fs::write`'s 0666 subject to the process umask.
pub fn write_handoff(path: &Path, text: &str) -> io::Result<()> {
    write_atomic(path, text.as_bytes(), 0o666, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn overlapping_atomic_writes_publish_only_whole_payloads_with_the_umask_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code");
        let payloads = ["good-code", "wrongcode-that-is-longer"];
        std::thread::scope(|s| {
            for p in payloads {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..500 {
                        write_handoff(path, p).unwrap();
                    }
                });
            }
            for _ in 0..2000 {
                if let Ok(read) = std::fs::read_to_string(&path) {
                    assert!(payloads.contains(&read.as_str()), "torn read: {read:?}");
                }
            }
        });
        // Same mode as `fs::write` (0666 less the umask): the reader is another user.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), mode(&plain));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".write."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
    }

    #[test]
    fn uid_and_socket_keep_the_installed_linux_values() {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(own_uid(), std::fs::metadata("/proc/self").unwrap().uid());
        assert_eq!(ENGINE_SOCKET, "/run/dvb/engine.sock");
    }

    #[test]
    fn replacement_preserves_the_temp_mode_and_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, "old").unwrap();
        for sync in [false, true] {
            let path = dir.path().join("target");
            symlink(&source, &path).unwrap();
            write_atomic(&path, b"whole payload", 0o600, sync).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"whole payload");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "old");
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn staging_and_install_failures_leave_no_temp_file_or_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing/file");
        assert!(write_handoff(&missing, "data").is_err());
        let path = dir.path().join("directory");
        std::fs::create_dir(&path).unwrap();
        for sync in [false, true] {
            assert!(write_atomic(&path, b"data", 0o600, sync).is_err());
            assert!(path.is_dir());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
}
