//! Self-update: `GET /update` reads the release list; `POST /update` starts one.

use crate::AppState;
use crate::update::{self, protocol::Source};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use dvb_core::api::{StartUpdate, UpdateInfo, UpdateQuery};

/// The running version and the newest release, if it is newer (`?check=true`
/// reads GitHub now; otherwise the last check's answer). Reads GitHub
/// (`DVB_RELEASES_URL` overrides the public list; `DVB_GITHUB_TOKEN` is needed
/// only for a private fork's releases). A dev-mode engine has no release source
/// unless one is given. When the list cannot be read the answer is 502 with the reason.
#[utoipa::path(
    get,
    path = "/update",
    operation_id = "get_update",
    tag = "update",
    params(dvb_core::api::UpdateQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "The running and available versions", body = dvb_core::api::UpdateInfo), (status = 502, description = "Release check failed", body = dvb_core::Refusal))
)]
pub async fn get_update(State(e): State<AppState>, Query(q): Query<UpdateQuery>) -> Response {
    // While an update is under way, answer from it: no network, and the
    // progress stays readable if the release list is not.
    let updating = update::protocol::status(&e);
    let completed = update::protocol::completion(&e);
    if updating.is_some() || q.check != Some(true) {
        return Json(UpdateInfo {
            current: dvb_core::VERSION.into(),
            available: updating
                .as_ref()
                .map(|u| u.to.clone())
                .or_else(|| e.update.available()),
            updating,
            completed,
        })
        .into_response();
    }
    match Source::from_env(&e).newest().await {
        Ok(available) => {
            e.update.set_available(available.clone());
            Json(UpdateInfo {
                current: dvb_core::VERSION.into(),
                available,
                updating: None,
                completed,
            })
            .into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(dvb_core::Refusal::new(
                dvb_core::RefusalCode::UpdateRefused,
                error.to_string(),
            )),
        )
            .into_response(),
    }
}

/// Fetches and verifies the release, then drains and hands over in the
/// background (202). A refusal comes back at once as `UPDATE_REFUSED`.
#[utoipa::path(
    post,
    path = "/update",
    operation_id = "start_update",
    tag = "update",
    request_body = Option<dvb_core::api::StartUpdate>,
    responses(crate::api::ErrorResponses, (status = 202, description = "Update started", body = dvb_core::api::UpdateInfo))
)]
pub async fn start_update(State(e): State<AppState>, body: axum::body::Bytes) -> Response {
    let body = match super::optional_body::<StartUpdate>(&body) {
        Ok(body) => body.unwrap_or_default(),
        Err(error) => return error.into_response(),
    };
    match update::protocol::start(e, body).await {
        Ok(info) => (StatusCode::ACCEPTED, Json(info)).into_response(),
        Err(f) => f.into_response(),
    }
}
