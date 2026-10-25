//! Bug and feature reports: the issue as it would be filed, and the GitHub
//! new-issue link that opens it prefilled. No token, no App permission; nothing is
//! filed or recorded.

use super::logins::logged_in;
use crate::AppState;
use axum::Json;
use axum::extract::State;
use dvb_core::api::{Report, ReportKind, ReportPreview};

#[utoipa::path(
    post,
    path = "/report",
    operation_id = "report",
    tag = "report",
    request_body = dvb_core::api::Report,
    responses(crate::api::ErrorResponses, (status = 200, description = "The issue as it would be filed, and its new-issue URL", body = dvb_core::api::ReportPreview))
)]
pub async fn report(State(e): State<AppState>, Json(r): Json<Report>) -> Json<ReportPreview> {
    let vendors: Vec<&str> = logged_in(&e)
        .into_iter()
        .filter_map(|(name, yes)| yes.then_some(name))
        .collect();
    let kind = match r.kind {
        ReportKind::Bug => "bug",
        ReportKind::Feature => "feature",
    };
    let shape = if e.cfg.dev {
        "engine on this machine (dev mode)"
    } else {
        "engine on this machine"
    };
    let footer = format!(
        "---\n- Kind: {kind}\n- dvb: {}\n- Install: {shape}; cockpit-only: no\n- OS: {}\n- Logged in: {}\n",
        dvb_core::VERSION,
        os_release(),
        if vendors.is_empty() {
            "none".to_owned()
        } else {
            vendors.join(", ")
        },
    );
    let body = format!("{}\n\n{footer}", r.text.trim_end());
    let url = format!(
        "https://github.com/{}/issues/new?title={}&body={}",
        crate::update::REPO,
        encode(&r.title),
        encode(&body)
    );
    Json(ReportPreview {
        title: r.title,
        body,
        url,
    })
}

/// `ID VERSION_ID` from `/etc/os-release`, as the machine says it.
fn os_release() -> String {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_owned())
    };
    match (field("ID"), field("VERSION_ID")) {
        (Some(id), Some(v)) => format!("{id} {v}"),
        (Some(id), None) => id,
        _ => "unknown".into(),
    }
}

/// Percent-encodes everything but unreserved characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
