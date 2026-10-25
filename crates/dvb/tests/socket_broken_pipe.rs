#[path = "../../../tests/support/http.rs"]
pub mod http;
pub mod support;

use axum::http::Method;
use std::process::{Command, Stdio};

#[tokio::test(flavor = "multi_thread")]
async fn socket_stdout_exits_quietly_for_closed_pipes_and_fails_on_other_write_errors() {
    let dir = tempfile::tempdir().unwrap();
    let e = dvb_engine::Engine::open(
        dvb_engine::Config::dev(dir.path(), env!("CARGO_BIN_EXE_dvb").into()),
        Default::default(),
    )
    .unwrap();
    let serving = tokio::spawn(dvb_engine::serve::serve(e));
    support::engine_up(&dvb_cli::client::Client::new(
        dir.path().join("engine.sock"),
    ))
    .await;

    for worker in [false, true] {
        for closed in [false, true] {
            let sock = dir.path().join("worker.sock");
            let server = if worker {
                let listener = tokio::net::UnixListener::bind(&sock).unwrap();
                let mut requests = http::requests(listener);
                Some(tokio::spawn(async move {
                    http::reply(
                        &mut requests,
                        Method::POST,
                        "/run/decide",
                        200,
                        serde_json::json!({}),
                    )
                    .await;
                }))
            } else {
                None
            };
            let root = dir.path().to_owned();
            let output = tokio::task::spawn_blocking(move || {
                let mut command = Command::new(env!("CARGO_BIN_EXE_dvb"));
                if worker {
                    command
                        .env("DVB_RUN_SOCK", sock)
                        .args(["run", "decide", "{}"]);
                } else {
                    command.arg("--dev").arg(root).arg("status");
                }
                command.stderr(Stdio::piped());
                if closed {
                    command.stdout(Stdio::piped());
                } else {
                    command.stdout(
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open("/dev/full")
                            .unwrap(),
                    );
                }
                let mut child = command.spawn().unwrap();
                drop(child.stdout.take());
                child.wait_with_output().unwrap()
            })
            .await
            .unwrap();
            if let Some(server) = server {
                server.await.unwrap();
                std::fs::remove_file(dir.path().join("worker.sock")).unwrap();
            }
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(
                output.status.code(),
                // An owner verb's error exits 1; a worker verb's keeps its 2.
                Some(match (closed, worker) {
                    (true, _) => 0,
                    (false, false) => 1,
                    (false, true) => 2,
                }),
                "{stderr}"
            );
            if closed {
                assert!(stderr.is_empty(), "{stderr}");
            } else {
                assert!(stderr.contains("stdout:"), "{stderr}");
                assert!(!stderr.contains("panicked"), "{stderr}");
            }
        }
    }
    serving.abort();
}
