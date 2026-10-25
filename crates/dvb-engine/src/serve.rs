//! Serving the two sockets: the owner API on `engine.sock`, allowed to
//! the install's owner and the engine's own user by `SO_PEERCRED`; the run API on
//! `run.sock`, where every call is attributed to the run whose unit (or, in dev
//! mode, whose child process) the caller is in.
//!
//! In an install both sockets come from systemd (`dvb.socket` and
//! `dvb-run.socket`, named `engine` and `run`); otherwise the engine binds them.

use crate::engine::{AppState, Fail};
use crate::proj::Run;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::serve::IncomingStream;
use dvb_core::{Refusal, RefusalCode, RunId};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::UnixListener;

/// The peer of a Unix socket connection.
#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: Option<u32>,
    pub pid: Option<i32>,
}

impl Connected<IncomingStream<'_, UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        let cred = stream.io().peer_cred().ok();
        Peer {
            uid: cred.map(|c| c.uid()),
            pid: cred.and_then(|c| c.pid()),
        }
    }
}

/// The owner API admits the recorded owner and the engine's own user.
async fn owner_only(State(e): State<AppState>, req: Request, next: Next) -> Response {
    let uid = req
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .and_then(|c| c.0.uid);
    let me = crate::engine::own_uid();
    match uid {
        Some(uid) if uid == e.cfg.owner_uid || uid == me => next.run(req).await,
        Some(_) | None => {
            crate::api::with_version(
                (
                    StatusCode::FORBIDDEN,
                    axum::Json(Refusal::new(
                        RefusalCode::RoleNotAllowed,
                        "not the owner of this install",
                    )),
                )
                    .into_response(),
            )
            .await
        }
    }
}

/// The calling run, resolved from the peer's process (never from the body).
pub struct Caller {
    pub run: Run,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, e: &AppState) -> Result<Self, Fail> {
        let not_a_run = |detail: &str| Fail::Refused(Refusal::new(RefusalCode::NotARun, detail));
        let pid = parts
            .extensions
            .get::<ConnectInfo<Peer>>()
            .and_then(|c| c.0.pid)
            .ok_or_else(|| not_a_run("no peer credentials"))?;
        let run: RunId = e
            .units
            .run_of_pid(pid)
            .ok_or_else(|| not_a_run("the caller is not in a dvb run"))?;
        let r = e
            .proj()
            .runs
            .get(&run)
            .cloned()
            .ok_or_else(|| not_a_run("unknown run"))?;
        if r.ended.is_some() {
            return Err(not_a_run("the run has ended"));
        }
        Ok(Caller { run: r })
    }
}

/// The two sockets from systemd, and `LISTEN_FDNAMES` as it was. The listeners
/// are left inheritable: an update `exec`s the new binary, which must find them
/// at the same descriptors (`update::protocol::exec_target` sets the variables
/// again, because `listenfd` removes them).
fn from_systemd() -> Option<(UnixListener, UnixListener, String)> {
    let names = std::env::var("LISTEN_FDNAMES").ok()?;
    let list: Vec<&str> = names.split(':').collect();
    let mut fds = listenfd::ListenFd::from_env();
    let mut take = |name: &str| -> Option<UnixListener> {
        let i = list.iter().position(|n| *n == name)?;
        let l = fds.take_unix_listener(i).ok()??;
        l.set_nonblocking(true).ok()?;
        rustix::io::fcntl_setfd(&l, rustix::io::FdFlags::empty()).ok()?;
        UnixListener::from_std(l).ok()
    };
    let (owner, run) = (take("engine")?, take("run")?);
    Some((owner, run, names))
}

fn bind(path: &Path, mode: u32) -> std::io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(l)
}

/// Serves both sockets until either fails.
pub async fn serve(e: AppState) -> std::io::Result<()> {
    crate::vendors::restore(&e).map_err(|f| std::io::Error::other(f.to_string()))?;
    let mirror = tokio::spawn(crate::publish::mirror::watch(e.clone()));
    struct AbortMirror(tokio::task::JoinHandle<()>);
    impl Drop for AbortMirror {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _mirror = AbortMirror(mirror);
    let (owner, run) = match from_systemd() {
        Some((owner, run, names)) => {
            e.update.set_listen(names);
            (owner, run)
        }
        None => (
            bind(&e.cfg.paths.engine_sock, 0o600)?,
            bind(&e.cfg.paths.run_sock, 0o660)?,
        ),
    };
    let owner_app = crate::api::router(e.clone())
        .layer(axum::middleware::from_fn_with_state(e.clone(), owner_only));
    let run_app = crate::run_api::router(e.clone());
    let a = axum::serve(
        owner,
        owner_app.into_make_service_with_connect_info::<Peer>(),
    );
    let b = axum::serve(run, run_app.into_make_service_with_connect_info::<Peer>());
    tokio::try_join!(async { a.await }, async { b.await })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn owner_peer_authorization_keeps_its_code_and_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let e = crate::Engine::open(
            crate::Config::dev(dir.path(), "unused".into()),
            Default::default(),
        )
        .unwrap();
        for uid in [
            None,
            Some(crate::own_uid() ^ u32::MAX),
            Some(e.cfg.owner_uid),
        ] {
            let app = crate::api::router(e.clone())
                .layer(axum::middleware::from_fn_with_state(e.clone(), owner_only));
            let response = app
                .oneshot(
                    Request::get("/version")
                        .extension(ConnectInfo(Peer { uid, pid: None }))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.headers()["dvb-version"], dvb_core::VERSION);
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap();
            if uid == Some(e.cfg.owner_uid) {
                assert_eq!(status, StatusCode::OK);
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN);
                let refusal: Refusal = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(refusal.code, RefusalCode::RoleNotAllowed);
            }
        }
    }
}
