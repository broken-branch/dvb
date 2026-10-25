//! The launcher supervisor must survive the short-lived owner command.
#[path = "../../../tests/support/executable.rs"]
pub mod executable;
use std::process::{Command, Stdio};
use std::time::Duration;
#[path = "../../../tests/support/http.rs"]
pub mod http;
use axum::http::Method;

#[tokio::test]
async fn socket_app_falls_back_after_the_owner_command_has_exited() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("engine.sock");
    let desk = dir.path().join("desk.sock");
    let marker = dir.path().join("browser-ran");
    let helper = dir.path().join("omarchy-launch-webapp");
    let browser = dir.path().join("chromium");
    let release = dir.path().join("fail-helper");
    executable::write_executable(
        &helper,
        &format!(
            "#!/bin/sh\n/bin/sleep 0.6\nwhile [ ! -f '{}' ]; do /bin/sleep 0.01; done\nexit 1\n",
            release.display()
        ),
    );
    executable::write_executable(
        &browser,
        &format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", marker.display()),
    );
    let listener = tokio::net::UnixListener::bind(desk).unwrap();
    let url = "http://127.0.0.1:7300/launch?token=abc";
    let mut requests = http::requests(listener);
    let server = tokio::spawn(async move {
        http::reply(
            &mut requests,
            Method::GET,
            "/app",
            200,
            serde_json::json!(url),
        )
        .await;
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--sock")
        .arg(sock)
        .arg("app")
        .env("PATH", dir.path())
        .env("DISPLAY", ":offline")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The foreground CLI must exit before the delayed helper fails.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dvb app must return while startup is still pending");
    assert!(!marker.exists());
    std::fs::write(release, "").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the surviving supervisor must launch the fallback");
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        format!("--app={url}")
    );
    server.await.unwrap();
}
