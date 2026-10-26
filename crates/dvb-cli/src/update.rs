//! Owner update routing and bounded engine progress polling.
use super::{Target, write_stdout};
use crate::client::{CliError, Client};
use dvb_core::api::{StartUpdate, UpdateInfo};

fn refused(detail: String) -> CliError {
    CliError::Refused(dvb_core::Refusal::new(
        dvb_core::RefusalCode::UpdateRefused,
        detail,
    ))
}

/// `dvb update`. An engine box asks its engine (the CLI restarts the desk). A
/// cockpit-only box (known from install, not from the socket, which may be
/// forwarded from the engine's box) installs the version its engine runs into its
/// own `/opt/dvb` and restarts the desk: after updating the engine, or on its own
/// when the engine already updated. With no engine reachable it takes the newest
/// release itself.
pub(super) async fn update_target(
    target: &Target,
    client: &Client,
    check: bool,
    to: Option<String>,
) -> Result<(), CliError> {
    use dvb_engine::update::{handover::Layout, local, protocol::Source};
    let dev = target.dev.as_deref();
    let l = dev.map_or_else(Layout::install, |d| Layout {
        opt: d.join("opt"),
        state: d.join("state"),
    });
    let engine_box = !l.cockpit_only() && (l.has_role("engine") || target.sock().exists());
    if engine_box {
        return update(client, dev.is_some(), check, to).await;
    }
    let src = Source::for_mode(dev.is_some());
    let engine = match client.get::<dvb_core::api::VersionInfo>("/version").await {
        Ok(version) => Some(version),
        Err(error) if error.restart_connection() => None,
        Err(error) => return Err(error),
    };
    let result = match (check, engine) {
        (true, Some(_)) => {
            return update(client, dev.is_some(), check, to).await;
        }
        (false, Some(engine)) => {
            let ours = env!("CARGO_PKG_VERSION");
            let version = if engine.version != ours {
                if let Some(requested) = &to
                    && requested.trim_start_matches('v') != engine.version
                {
                    return Err(refused(format!(
                        "this cockpit must match engine version {}; --to {requested} would select another version",
                        engine.version
                    )));
                }
                // The engine already updated: line up with it.
                write_stdout(format_args!(
                    "The engine runs {}; this box runs {ours}.\n",
                    engine.version
                ))?;
                engine.version
            } else {
                engine_update(client, to).await?
            };
            local::install(&l, &src, Some(version))
                .await
                .map(|v| (v, true))
        }
        (true, None) => src.newest().await.map(|v| (v, false)),
        (false, None) => local::install(&l, &src, to).await.map(|v| (v, true)),
    };
    match result {
        Ok((None, true)) => {
            // Already at the engine's version: only the desk needs to follow.
            write_stdout(format_args!(
                "dvb {} is installed\n",
                env!("CARGO_PKG_VERSION")
            ))?;
            if dev.is_none() {
                restart_desk();
            }
        }
        Ok((None, false)) => write_stdout(format_args!(
            "dvb {} is the newest release\n",
            env!("CARGO_PKG_VERSION")
        ))?,
        Ok((Some(v), false)) => write_stdout(format_args!(
            "dvb {} is installed; {v} is available. Install it with: dvb update\n",
            env!("CARGO_PKG_VERSION")
        ))?,
        Ok((Some(v), true)) => {
            write_stdout(format_args!("dvb is now {v} on this box\n"))?;
            if dev.is_none() {
                restart_desk();
            }
        }
        Err(why) => return Err(refused(why.to_string())),
    }
    Ok(())
}

async fn update(
    client: &Client,
    dev: bool,
    check: bool,
    to: Option<String>,
) -> Result<(), CliError> {
    if check {
        let info: UpdateInfo = client.get("/update?check=true").await?;
        match &info.available {
            None => write_stdout(format_args!("dvb {} is the newest release\n", info.current))?,
            Some(v) => write_stdout(format_args!(
                "dvb {} is installed; {v} is available. Install it with: dvb update\n",
                info.current
            ))?,
        }
        return Ok(());
    }
    engine_update(client, to).await?;
    if !dev {
        restart_desk();
    }
    Ok(())
}

/// Asks the engine to update, shows its progress, and returns the version it is
/// now running. The desk is the caller's to restart.
async fn engine_update(client: &Client, to: Option<String>) -> Result<String, CliError> {
    engine_update_with(client, to, |line| write_stdout(format_args!("{line}"))).await
}

async fn engine_update_with(
    client: &Client,
    to: Option<String>,
    mut write: impl FnMut(&str) -> Result<(), CliError>,
) -> Result<String, CliError> {
    let started: UpdateInfo = client.post("/update", &StartUpdate { to }).await?;
    let Some(up) = started.updating else {
        return Err(CliError::Message(
            "the engine did not start an update".into(),
        ));
    };
    write(&format!("Updating dvb {} to {}\n", started.current, up.to))?;
    write(&drain_text(up.runs_active))?;
    let wait = async {
        let mut last = up.runs_active;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match client.get::<UpdateInfo>("/update").await {
                Ok(i) => {
                    let completed = i.completed.as_ref().filter(|c| c.to == up.to);
                    if i.current == up.to {
                        write(&format!("dvb is now {}\n", i.current))?;
                        write(&completion_text(completed))?;
                        return Ok(i.current);
                    }
                    match i.updating {
                        Some(u) if last != u.runs_active => {
                            last = u.runs_active;
                            write(&drain_text(u.runs_active))?;
                        }
                        Some(_) => {}
                        None => {
                            if completed.is_some() {
                                write(&completion_text(completed))?;
                            }
                            return Err(CliError::Message(
                                "the update did not take and was undone; `dvb watch` shows why"
                                    .into(),
                            ));
                        }
                    }
                }
                Err(error) if error.restart_connection() => {}
                Err(error) => return Err(error),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(3600), wait)
        .await
        .map_err(|_| CliError::Message("the engine did not come back in an hour".into()))?
}

fn drain_text(runs: u32) -> String {
    if runs == 0 {
        "Restarting dvb\n".into()
    } else {
        let seconds = dvb_engine::update::protocol::DRAIN_MILLIS / 1_000;
        format!(
            "Waiting up to {seconds} s for {runs} runs to finish; remaining task runs will be stopped and resumed after the restart\n"
        )
    }
}

fn completion_text(completed: Option<&dvb_core::api::UpdateCompletion>) -> String {
    match completed {
        Some(c) if c.stopped == 0 => "Every task run finished before the restart\n".into(),
        Some(c) if c.stopped == c.resumed.saturating_add(c.pending) => format!(
            "{} task runs were stopped and resume automatically; see `dvb status` for the tasks\n",
            c.stopped
        ),
        Some(c) => format!(
            "{} task runs were stopped; {} resumed after the restart; {} pending automatic resume; {} changed while stopped\n",
            c.stopped,
            c.resumed,
            c.pending,
            c.stopped
                .saturating_sub(c.resumed)
                .saturating_sub(c.pending)
        ),
        None => "The engine did not report which runs finished or resumed\n".into(),
    }
}

/// The desk runs the old binary until it restarts; best effort, and a box with no
/// desk unit has nothing to restart.
fn restart_desk() {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "try-restart", "dvb-desk.service"])
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{reply, requests};
    use axum::http::Method;
    use dvb_core::RefusalCode;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn update_output_reports_the_bound_and_the_actual_run_outcome() {
        for (rollback, stopped, resumed, pending) in [
            (false, 0, 0, 0),
            (false, 2, 2, 0),
            (false, 2, 0, 2),
            (false, 2, 1, 1),
            (false, 3, 1, 1),
            (true, 0, 0, 0),
            (true, 2, 2, 0),
            (true, 2, 0, 2),
            (true, 2, 1, 1),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("engine.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let mut requests = requests(listener);
            let server = tokio::spawn(async move {
                reply(
                    &mut requests,
                    Method::GET,
                    "/version",
                    200,
                    json!({"version":dvb_core::VERSION}),
                )
                .await;
                let progress = json!({"current":dvb_core::VERSION, "available":null,
                    "updating":{"to":"99.0.0", "runs_active":2}});
                reply(
                    &mut requests,
                    Method::POST,
                    "/update",
                    202,
                    progress.clone(),
                )
                .await;
                reply(&mut requests, Method::GET, "/update", 200, progress).await;
                // A pending resume must not delay reporting a completed restart or rollback.
                reply(
                    &mut requests,
                    Method::GET,
                    "/update",
                    200,
                    json!({"current":if rollback { dvb_core::VERSION } else { "99.0.0" },
                        "available":null, "updating":null,
                        "completed":{"to":"99.0.0", "stopped":stopped,
                            "resumed":resumed, "pending":pending}}),
                )
                .await;
            });
            let mut output = String::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                engine_update_with(&Client::new(sock), None, |line| {
                    output.push_str(line);
                    Ok(())
                }),
            )
            .await
            .expect("a reachable engine must return its receipt immediately");
            if rollback {
                assert!(matches!(result, Err(CliError::Message(ref m)) if m.contains("undone")));
            } else {
                assert_eq!(result.unwrap(), "99.0.0");
            }
            assert!(output.contains("Waiting up to 30 s for 2 runs to finish; remaining task runs will be stopped and resumed after the restart"), "{output}");
            assert!(!output.contains("stop them to go on"), "{output}");
            if stopped == 0 {
                assert!(
                    output.contains("Every task run finished before the restart"),
                    "{output}"
                );
            } else if stopped == resumed + pending {
                assert!(
                    output.contains("2 task runs were stopped and resume automatically"),
                    "{output}"
                );
                assert!(output.contains("dvb status"), "{output}");
            } else {
                assert!(output.contains(&format!("{stopped} task runs were stopped; {resumed} resumed after the restart; {pending} pending automatic resume")), "{output}");
                if stopped > resumed + pending {
                    assert!(output.contains("1 changed while stopped"), "{output}");
                }
            }
            server.await.unwrap();
        }
    }

    fn target(dir: &std::path::Path, cockpit: bool) -> Target {
        std::fs::create_dir(dir.join("opt")).unwrap();
        std::fs::write(
            dir.join("opt").join(if cockpit {
                "role-cockpit"
            } else {
                "role-engine"
            }),
            "",
        )
        .unwrap();
        Target {
            dev: Some(dir.into()),
            sock: Some(dir.join("engine.sock")),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn socket_update_surfaces_permissions_refusals_and_stalled_responses() {
        for status in [403, 409, 0] {
            let dir = tempfile::tempdir().unwrap();
            let target = target(dir.path(), false);
            let listener = tokio::net::UnixListener::bind(target.sock()).unwrap();
            let mut requests = requests(listener);
            let server = tokio::spawn(async move {
                reply(
                    &mut requests,
                    Method::GET,
                    "/version",
                    200,
                    json!({"version":dvb_core::VERSION}),
                )
                .await;
                reply(
                    &mut requests,
                    Method::POST,
                    "/update",
                    200,
                    json!({
                        "current":dvb_core::VERSION,
                        "available":null,
                        "updating":{
                            "to":"99.0.0",
                            "runs_active":1
                        }
                    }),
                )
                .await;

                if status == 0 {
                    let (request, _response) = requests.recv().await.unwrap();
                    assert_eq!(request.method(), Method::GET);
                    assert_eq!(request.uri(), "/update");
                    std::future::pending::<()>().await;
                } else {
                    reply(
                        &mut requests,
                        Method::GET,
                        "/update",
                        status,
                        json!(dvb_core::Refusal::new(
                            RefusalCode::UpdateRefused,
                            "fixture"
                        )),
                    )
                    .await;
                }
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(160),
                engine_update(&Client::new(target.sock()), None),
            )
            .await
            .expect("poll errors must surface immediately");
            match status {
                403 | 409 => assert!(
                    matches!(result, Err(CliError::Refused(ref r)) if r.code == RefusalCode::UpdateRefused)
                ),
                0 => assert!(
                    matches!(result, Err(CliError::Message(ref m)) if m.contains("timed out"))
                ),
                _ => unreachable!(),
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn socket_cockpit_refuses_a_conflicting_target_and_any_refusal_exits_1() {
        for status in [200, 403, 409] {
            let dir = tempfile::tempdir().unwrap();
            let target = target(dir.path(), true);
            let listener = tokio::net::UnixListener::bind(target.sock()).unwrap();
            let mut requests = requests(listener);
            let server = tokio::spawn(async move {
                let body = if status == 200 {
                    json!({"version":"99.0.0"})
                } else {
                    json!(dvb_core::Refusal::new(
                        RefusalCode::UpdateRefused,
                        "fixture"
                    ))
                };
                reply(&mut requests, Method::GET, "/version", status, body).await;
            });
            let verb = crate::Verb::Update {
                check: false,
                to: Some("98.0.0".into()),
            };
            assert_eq!(crate::main(&target, verb).await, 1);
            server.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn socket_update_refusals_exit_one_in_both_install_shapes() {
        for cockpit in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = target(dir.path(), cockpit);
            let listener = tokio::net::UnixListener::bind(target.sock()).unwrap();
            let mut requests = requests(listener);
            let server = tokio::spawn(async move {
                if cockpit {
                    reply(
                        &mut requests,
                        Method::GET,
                        "/version",
                        200,
                        json!({"version":dvb_core::VERSION}),
                    )
                    .await;
                }
                reply(
                    &mut requests,
                    Method::GET,
                    "/version",
                    200,
                    json!({"version":dvb_core::VERSION}),
                )
                .await;
                reply(
                    &mut requests,
                    Method::POST,
                    "/update",
                    409,
                    json!(dvb_core::Refusal::new(
                        RefusalCode::UpdateRefused,
                        "fixture"
                    )),
                )
                .await;
            });
            let verb = crate::Verb::Update {
                check: false,
                to: None,
            };
            assert_eq!(crate::main(&target, verb).await, 1);
            server.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn socket_update_retries_restart_connections_within_one_hour() {
        for returns in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = target(dir.path(), false);
            let sock = target.sock();
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let mut requests = requests(listener);
            let server = tokio::spawn(async move {
                reply(
                    &mut requests,
                    Method::GET,
                    "/version",
                    200,
                    json!({"version":dvb_core::VERSION}),
                )
                .await;
                reply(
                    &mut requests,
                    Method::POST,
                    "/update",
                    200,
                    json!({
                        "current":dvb_core::VERSION,
                        "available":null,
                        "updating":{
                            "to":"99.0.0",
                            "runs_active":0
                        }
                    }),
                )
                .await;

                drop(requests);
                std::fs::remove_file(&sock).unwrap();
                if returns {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
                    let mut requests = crate::test_http::requests(listener);
                    reply(
                        &mut requests,
                        Method::GET,
                        "/update",
                        200,
                        json!({"current":"99.0.0","available":null,"updating":null}),
                    )
                    .await;
                }
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3601),
                engine_update(&Client::new(target.sock()), None),
            )
            .await
            .expect("the overall polling deadline includes requests");
            if returns {
                assert_eq!(result.unwrap(), "99.0.0");
            } else {
                assert!(
                    matches!(result, Err(CliError::Message(ref m)) if m.contains("in an hour"))
                );
            }
            server.await.unwrap();
        }
    }
}
