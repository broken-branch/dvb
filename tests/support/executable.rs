use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A child writes and closes the executable before returning, so parallel forks
/// cannot inherit a writable descriptor and cause ETXTBSY on execution.
pub fn write_executable(path: &Path, text: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "write"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}
