use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn socket_malformed_run_verbs_are_refused_before_connecting() {
    for word in [
        "",
        "Task",
        "task/stop",
        "task?x",
        "task stop",
        "task\r\nX: injected",
        "task1",
        "tásk",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("run.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            if let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept()).await
            {
                let mut request = Vec::new();
                stream.read_to_end(&mut request).await.unwrap();
                stream
                    .write_all(b"HTTP/1.1 409 Fixture\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
                return true;
            }
            false
        });
        let args = word.to_owned();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_dvb"))
                .env("DVB_RUN_SOCK", sock)
                .args(["run", &args, "{}"])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(output.status.code(), Some(2), "{word:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
        assert!(!server.await.unwrap(), "malformed verb reached the socket");
    }
}
