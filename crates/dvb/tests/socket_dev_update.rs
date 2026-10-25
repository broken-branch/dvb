#[path = "../../../tests/support/executable.rs"]
pub mod executable;
use std::process::Command;
#[path = "../../../tests/support/http.rs"]
pub mod http;
use axum::http::Method;

#[tokio::test]
async fn socket_dev_updates_never_restart_the_installed_desk() {
    for cockpit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("opt")).unwrap();
        std::fs::write(
            dir.path().join("opt").join(if cockpit {
                "role-cockpit"
            } else {
                "role-engine"
            }),
            "",
        )
        .unwrap();
        let stub_dir = dir.path().join("bin");
        std::fs::create_dir(&stub_dir).unwrap();
        let stub = stub_dir.join("systemctl");
        executable::write_executable(
            &stub,
            "#!/bin/sh\nprintf restarted > \"$DVB_RESTART_LOG\"\n",
        );
        let restart_log = dir.path().join("restart");
        let server = if cockpit {
            None
        } else {
            let listener = tokio::net::UnixListener::bind(dir.path().join("engine.sock")).unwrap();
            let mut requests = http::requests(listener);
            Some(tokio::spawn(async move {
                for (method, body) in [
                    (
                        (Method::GET, "/version"),
                        serde_json::json!({"version":dvb_core::VERSION}),
                    ),
                    (
                        (Method::POST, "/update"),
                        serde_json::json!({
                            "current":dvb_core::VERSION,
                            "available":null,
                            "updating":{
                                "to":dvb_core::VERSION,
                                "runs_active":0
                            }
                        }),
                    ),
                    (
                        (Method::GET, "/update"),
                        serde_json::json!({"current":dvb_core::VERSION,"available":null,"updating":null}),
                    ),
                ] {
                    http::reply(&mut requests, method.0, method.1, 200, body).await;
                }
            }))
        };
        let path = std::env::join_paths(std::iter::once(stub_dir).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let root = dir.path().to_owned();
        let log = restart_log.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_dvb"))
                .env("PATH", path)
                .env("DVB_RESTART_LOG", log)
                .arg("--dev")
                .arg(root)
                .args(["update", "--to", dvb_core::VERSION])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        if let Some(server) = server {
            server.await.unwrap();
        }
        assert!(
            !restart_log.exists(),
            "a dev update restarted the installed desk"
        );
    }
}
