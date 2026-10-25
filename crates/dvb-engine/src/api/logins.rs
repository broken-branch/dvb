//! Vendor logins inside the unit: `POST /logins/{vendor}` starts a
//! login run (the vendor's own login command in the vendor's unit, as the worker
//! user) and returns the URL and what to do with it; a second call with the run
//! and the code finishes it and reports how it ended. The run and the engine meet
//! in files in the run directory (`dvb_run::login`); no token or code is logged
//! or put in an event.

use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use dvb_core::api::{LoginCode, LoginStarted, LoginStatus, Logins};
use dvb_core::io::write_handoff;
use dvb_core::{RunId, Vendor};
use std::path::Path as FsPath;
use std::time::Duration;

/// How long the first call waits for the login command's URL.
const URL_WAIT: Duration = Duration::from_secs(60);
/// How long the second call waits for the login to end.
const RESULT_WAIT: Duration = Duration::from_secs(120);
/// The prompt while the login is still going; a client asks again with an empty
/// code (`dvb login` does).
pub const PENDING: &str = "Still waiting for the login to finish.";

/// `GET /logins`: which vendors' login files are present, and which vendors a
/// new project's default hires need.
#[utoipa::path(
    get,
    path = "/logins",
    operation_id = "get_logins",
    tag = "logins",
    responses(crate::api::ErrorResponses, (status = 200, description = "Which vendors are logged in and which a new project needs", body = dvb_core::api::Logins))
)]
pub async fn get_logins(State(e): State<AppState>) -> Json<Logins> {
    let [(_, claude), (_, codex)] = logged_in(&e);
    let needed = e
        .default_hire_model()
        .map(|m| m.vendor)
        .filter(|v| match v {
            Vendor::Claude | Vendor::Codex => true,
            Vendor::Fake => false,
        })
        .into_iter()
        .collect();
    Json(Logins {
        claude,
        codex,
        needed,
    })
}

/// Whether each vendor's login file is there, in the folder the vendor's unit
/// mounts as its home (`logins/<vendor>` beside `work`, as in the unit files).
/// Only the file's presence is read, never its content.
pub(super) fn logged_in(e: &AppState) -> [(&'static str, bool); 2] {
    let root = e.cfg.paths.work.parent().unwrap_or(FsPath::new("."));
    // The vendor folders and files are the worker's: a link at either level
    // would have the engine report on a path outside them.
    let has = |vendor: &str, file: &str| {
        let dir = root.join("logins").join(vendor);
        std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir())
            && std::fs::symlink_metadata(dir.join(file)).is_ok_and(|meta| meta.is_file())
    };
    [
        ("claude", has("claude", ".credentials.json")),
        ("codex", has("codex", "auth.json")),
    ]
}

#[utoipa::path(
    post,
    path = "/logins/{vendor}",
    operation_id = "start_login",
    tag = "logins",
    params(("vendor" = dvb_core::Vendor, Path)),
    request_body(content = Option<dvb_core::api::LoginCode>, description = "Absent: start a login run. With the run and the code: finish it."),
    responses(crate::api::ErrorResponses, (status = 200, description = "Login run started", body = dvb_core::api::LoginStarted))
)]
pub async fn start_login(
    State(e): State<AppState>,
    Path(vendor): Path<Vendor>,
    body: Bytes,
) -> Result<Json<LoginStarted>, Fail> {
    let body = super::optional_body::<LoginCode>(&body)?;
    if vendor == Vendor::Fake {
        return Err(Fail::NotFound("the fake vendor has no login".into()));
    }
    match body {
        None => begin(&e, vendor).await.map(Json),
        Some(code) => finish(&e, vendor, code).await.map(Json),
    }
}

/// Reads `path` once it exists, looking every 100 ms for at most `limit`.
async fn poll(path: &FsPath, limit: Duration) -> Option<String> {
    for _ in 0..=(limit.as_millis() / 100) {
        if let Ok(s) = crate::launch::read_run_file(path) {
            return Some(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

async fn begin(e: &AppState, vendor: Vendor) -> Result<LoginStarted, Fail> {
    let run = RunId::new(e.new_id());
    let dir = e.cfg.paths.work.join(run.as_str());
    let io = |what: &str| {
        let what = what.to_owned();
        move |err: std::io::Error| Fail::io(what, err)
    };
    crate::launch::create_shared_dir(&dir).map_err(io("run directory"))?;
    crate::units::mount_target(&dir, vendor).map_err(io("login mount target"))?;
    let env = [
        ("DVB_RUN_ID", run.to_string()),
        ("DVB_LOGIN", "1".to_owned()),
        ("DVB_VENDOR", vendor.as_str().to_owned()),
        ("DVB_MODEL", "login".to_owned()),
        ("DVB_EFFORT", "medium".to_owned()),
        ("DVB_BASE", String::new()),
        ("DVB_MIRROR", String::new()),
        (
            "DVB_RUN_SOCK",
            e.cfg.paths.run_sock.to_string_lossy().into_owned(),
        ),
    ];
    let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    crate::launch::write_run_file(&dir.join("env"), text).map_err(io("env"))?;
    e.start_unit(vendor, &run)
        .await
        .map_err(|err| Fail::context("starting the login run", err))?;

    let Some(started) = poll(&dir.join("login.json"), URL_WAIT).await else {
        // The run ended without a URL, or is still silent.
        let why = poll(&dir.join("login-result"), Duration::from_millis(500))
            .await
            .unwrap_or_else(|| "it printed nothing in time".into());
        return Err(Fail::internal(format!("the login run gave no URL: {why}")));
    };
    let v: serde_json::Value =
        serde_json::from_str(&started).map_err(|err| Fail::context("login.json", err))?;
    Ok(LoginStarted {
        status: LoginStatus::Pending,
        run,
        url: v["url"].as_str().unwrap_or_default().to_owned(),
        prompt: v["prompt"].as_str().unwrap_or_default().to_owned(),
    })
}

async fn finish(e: &AppState, vendor: Vendor, c: LoginCode) -> Result<LoginStarted, Fail> {
    // The id names a directory in `work/`, and nothing beyond it.
    if !c.run.is_ulid() {
        return Err(Fail::NotFound(format!("login run {}", c.run)));
    }
    let dir = e.cfg.paths.work.join(c.run.as_str());
    let started = crate::launch::read_run_file(&dir.join("login.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|_| {
            // Only a login run of this vendor, not any run's directory.
            crate::launch::read_run_file(&dir.join("env")).is_ok_and(|env| {
                env.contains("DVB_LOGIN=1")
                    && env.contains(&format!("DVB_VENDOR={}\n", vendor.as_str()))
            })
        })
        .ok_or_else(|| Fail::NotFound(format!("login run {}", c.run)))?;
    let url = started["url"].as_str().unwrap_or_default().to_owned();
    let ended = |r: &str| {
        let said = r.trim().trim_start_matches("failed: ").to_owned();
        if r.trim() == "ok" {
            "Logged in.".to_owned()
        } else {
            format!("Login failed: {said}")
        }
    };
    let result_file = dir.join("login-result");
    // A run that has ended has no one left to read a code: reject it.
    if let Ok(r) = crate::launch::read_run_file(&result_file) {
        return Ok(LoginStarted {
            status: result_status(&r),
            run: c.run,
            url,
            prompt: format!(
                "This login run has already ended ({}); start a new one.",
                ended(&r)
            ),
        });
    }
    let code = c.code.trim();
    let code_file = dir.join("code");
    if !code.is_empty() {
        write_handoff(&code_file, code).map_err(|err| Fail::io("code", err))?;
        // The run may have ended between the check and the write. It removes the
        // code after writing its result, and so do we if the result is there now:
        // one of the two sees the other, so no code is left behind.
        if std::fs::symlink_metadata(&result_file).is_ok() {
            let _ = std::fs::remove_file(&code_file);
        }
    }
    let (status, prompt) = match poll(&result_file, RESULT_WAIT).await {
        Some(r) => {
            let _ = std::fs::remove_file(&code_file);
            (result_status(&r), ended(&r))
        }
        None => (LoginStatus::Pending, PENDING.to_owned()),
    };
    Ok(LoginStarted {
        status,
        run: c.run,
        url,
        prompt,
    })
}

fn result_status(result: &str) -> LoginStatus {
    if result.trim() == "ok" {
        LoginStatus::LoggedIn
    } else {
        LoginStatus::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};

    #[tokio::test]
    async fn needed_follows_the_first_selected_pool_models_vendor() {
        let model = |id: &str, vendor| dvb_core::PoolModel {
            id: id.into(),
            vendor,
            efforts: vec![],
        };
        let claude = model("claude-sonnet-5-5", Vendor::Claude);
        let codex = model("gpt-6.1-sol", Vendor::Codex);
        let fake = model("fake-1", Vendor::Fake);
        for (models, needed) in [
            (vec![claude.clone(), codex.clone()], vec![Vendor::Claude]),
            (vec![codex.clone(), claude.clone()], vec![Vendor::Codex]),
            (vec![fake, claude], vec![]),
            (vec![], vec![]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let e = Engine::open(
                Config::dev(dir.path(), "unused-dvb".into()),
                crate::EngineOptions {
                    pool: Some(dvb_core::ModelPool { models }),
                    ..Default::default()
                },
            )
            .unwrap();
            let logins = get_logins(State(e.clone())).await.0;
            assert_eq!(logins.needed, needed);
        }
        // An older engine's reply still reads.
        let old: Logins = serde_json::from_str(r#"{"claude":true,"codex":false}"#).unwrap();
        assert!(old.needed.is_empty());
    }

    #[test]
    fn a_linked_login_folder_or_file_is_no_login() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "unused-dvb".into()),
            Default::default(),
        )
        .unwrap();
        let logins = e.cfg.paths.work.parent().unwrap().join("logins");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(".credentials.json"), "{}").unwrap();
        std::fs::write(elsewhere.join("auth.json"), "{}").unwrap();
        std::fs::create_dir_all(logins.join("codex")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, logins.join("claude")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("auth.json"), logins.join("codex/auth.json"))
            .unwrap();
        assert_eq!(logged_in(&e), [("claude", false), ("codex", false)]);
        std::fs::remove_file(logins.join("claude")).unwrap();
        std::fs::remove_file(logins.join("codex/auth.json")).unwrap();
        std::fs::create_dir(logins.join("claude")).unwrap();
        std::fs::rename(
            elsewhere.join(".credentials.json"),
            logins.join("claude/.credentials.json"),
        )
        .unwrap();
        std::fs::rename(elsewhere.join("auth.json"), logins.join("codex/auth.json")).unwrap();
        assert_eq!(logged_in(&e), [("claude", true), ("codex", true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_login_finish_reports_pending_and_keeps_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "unused-dvb".into()),
            Default::default(),
        )
        .unwrap();
        let run = RunId::new("01M5A00000000000000000G1N0");
        let work = e.cfg.paths.work.join(run.as_str());
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("env"), "DVB_LOGIN=1\nDVB_VENDOR=codex\n").unwrap();
        std::fs::write(
            work.join("login.json"),
            r#"{"url":"https://example.invalid/auth"}"#,
        )
        .unwrap();
        let reply = finish(
            &e,
            Vendor::Codex,
            LoginCode {
                run: run.clone(),
                code: String::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.run, run);
        assert_eq!(reply.status, LoginStatus::Pending);
        assert_eq!(reply.prompt, PENDING);
        assert_eq!(serde_json::to_value(reply).unwrap()["status"], "pending");
    }

    #[tokio::test]
    async fn a_login_run_id_names_no_path_beyond_work() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "unused-dvb".into()),
            Default::default(),
        )
        .unwrap();
        // A directory that looks like a login run, beside `work/`.
        let beside = e.cfg.paths.work.parent().unwrap().join("beside");
        std::fs::create_dir_all(&beside).unwrap();
        std::fs::write(beside.join("env"), "DVB_LOGIN=1\nDVB_VENDOR=codex\n").unwrap();
        std::fs::write(beside.join("login.json"), r#"{"url":"u"}"#).unwrap();
        std::fs::write(beside.join("login-result"), "ok").unwrap();
        let reply = finish(
            &e,
            Vendor::Codex,
            LoginCode {
                run: RunId::new("../beside"),
                code: "pasted".into(),
            },
        )
        .await;
        assert!(matches!(reply, Err(Fail::NotFound(_))), "{reply:?}");
        assert!(!beside.join("code").exists());
    }
}
