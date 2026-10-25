//! Owner routes and OpenAPI share the annotated handlers below.

mod events;
mod github;
mod goals;
pub(crate) mod hires;
mod history;
mod logins;
mod models;
mod only;
mod projects;
mod projects_add;
pub(crate) mod questions;
mod report;
mod roles;
mod runs;
mod settings;
mod status;
mod tasks;
mod update;
mod usage;

use crate::AppState;
use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use dvb_core::Refusal;
use dvb_core::api::VERSION_HEADER;
use utoipa_axum::{router::OpenApiRouter, routes};

/// Shared failures for every owner route; successes and special statuses stay on the handler.
#[derive(utoipa::IntoResponses)]
#[expect(
    dead_code,
    reason = "Variants describe OpenAPI responses, not runtime values"
)]
enum ErrorResponses {
    #[response(status = 409, description = "Refused, with a code")]
    Refused(dvb_core::Refusal),
    #[response(status = 404, description = "Refused, with a code")]
    NotFound(dvb_core::Refusal),
    #[response(status = 500, description = "Refused, with a code")]
    Internal(dvb_core::Refusal),
}

fn invalid(detail: impl Into<String>) -> crate::Fail {
    Refusal::new(dvb_core::RefusalCode::InvalidValue, detail).into()
}

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "dvb engine API",
        description = "The owner API over /run/dvb/engine.sock. Generated; do not edit."
    ),
    components(schemas(
        dvb_core::api::SettingsKey,
        dvb_core::api::ResponseStyle,
        dvb_core::api::ModelMode
    ))
)]
struct ApiDoc;

fn routes() -> OpenApiRouter<AppState> {
    use utoipa::OpenApi;
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(github::connection))
        .routes(routes!(github::prefill))
        .routes(routes!(github::manifest))
        .routes(routes!(github::callback))
        .routes(routes!(github::install))
        .routes(routes!(github::repositories))
        .routes(routes!(github::checks))
        .routes(routes!(status::get_version))
        .routes(routes!(status::get_status))
        .routes(routes!(only::get))
        .routes(routes!(only::set))
        .routes(routes!(only::clear))
        .routes(routes!(goals::list_goals))
        .routes(routes!(models::get_models))
        .routes(routes!(projects::move_chart))
        .routes(routes!(events::get_events))
        .routes(routes!(events::event_page))
        .routes(routes!(projects::list_projects))
        .routes(routes!(projects_add::add_project))
        .routes(routes!(projects::get_project))
        .routes(routes!(projects::set_project_checks))
        .routes(routes!(projects::project_checks_history))
        .routes(routes!(projects::pause_project))
        .routes(routes!(projects::resume_project))
        .routes(routes!(projects::get_brief))
        .routes(routes!(projects::set_brief))
        .routes(routes!(projects::mark_project_seen))
        .routes(routes!(goals::state_goal))
        .routes(routes!(goals::list_project_issues))
        .routes(routes!(goals::count_project_issues))
        .routes(routes!(goals::propose_goal))
        .routes(routes!(goals::redirect_goal))
        .routes(routes!(goals::drop_goal))
        .routes(routes!(goals::set_goal_after))
        .routes(routes!(goals::resume_goal))
        .routes(routes!(questions::list_questions))
        .routes(routes!(questions::answer_question))
        .routes(routes!(tasks::list_tasks))
        .routes(routes!(tasks::get_task))
        .routes(routes!(tasks::stop_task))
        .routes(routes!(tasks::resume_task))
        .routes(routes!(tasks::retry_task))
        .routes(routes!(tasks::get_task_diff))
        .routes(routes!(tasks::get_task_verdict))
        .routes(routes!(runs::get_run))
        .routes(routes!(runs::get_run_output))
        .routes(routes!(roles::list_roles))
        .routes(routes!(roles::add_role))
        .routes(routes!(roles::change_role))
        .routes(routes!(hires::list_hires))
        .routes(routes!(hires::add_hire))
        .routes(routes!(hires::retire_hire))
        .routes(routes!(roles::role_history))
        .routes(routes!(roles::restore_role))
        .routes(routes!(hires::hire_history))
        .routes(routes!(hires::restore_hire))
        .routes(routes!(settings::settings_history))
        .routes(routes!(settings::restore_settings))
        .routes(routes!(settings::get_settings))
        .routes(routes!(settings::put_settings))
        .routes(routes!(settings::get_publication))
        .routes(routes!(settings::set_publication))
        .routes(routes!(usage::get_usage))
        .routes(routes!(update::get_update))
        .routes(routes!(update::start_update))
        .routes(routes!(report::report))
        .routes(routes!(logins::get_logins))
        .routes(routes!(logins::start_login))
}

pub fn router(state: AppState) -> Router {
    let (router, _) = routes().split_for_parts();
    router.layer(map_response(owner_response)).with_state(state)
}

pub fn openapi() -> utoipa::openapi::OpenApi {
    routes().split_for_parts().1
}

/// The committed owner contract, generated from the router.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    let mut json = openapi().to_pretty_json()?;
    json.push('\n');
    Ok(json)
}

pub(crate) async fn with_version(mut res: Response) -> Response {
    res.headers_mut()
        .insert(VERSION_HEADER, HeaderValue::from_static(dvb_core::VERSION));
    res
}

/// A refusal as the API sends it, with its code and detail.
pub(crate) fn refusal(r: Refusal) -> Response {
    let status =
        StatusCode::from_u16(r.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(r)).into_response()
}

// Axum's extractor rejections and router fallbacks share the owner error contract.
async fn owner_response(res: Response) -> Response {
    if !res.status().is_client_error() && !res.status().is_server_error() {
        return with_version(res).await;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return with_version(crate::Fail::internal(error.to_string()).into_response()).await;
        }
    };
    let response = if serde_json::from_slice::<Refusal>(&bytes).is_ok() {
        Response::from_parts(parts, axum::body::Body::from(bytes))
    } else {
        let code = match parts.status {
            StatusCode::NOT_FOUND => dvb_core::RefusalCode::NotFound,
            StatusCode::BAD_REQUEST
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::UNSUPPORTED_MEDIA_TYPE
            | StatusCode::PAYLOAD_TOO_LARGE => {
                parts.status = StatusCode::CONFLICT;
                dvb_core::RefusalCode::InvalidValue
            }
            StatusCode::METHOD_NOT_ALLOWED => dvb_core::RefusalCode::InvalidValue,
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                dvb_core::RefusalCode::RoleNotAllowed
            }
            _ => dvb_core::RefusalCode::InternalError,
        };
        let detail = if bytes.is_empty() {
            parts.status.to_string()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let mut response = (parts.status, Json(Refusal::new(code, detail))).into_response();
        // Preserve method negotiation and any other response metadata.
        parts.headers.remove(axum::http::header::CONTENT_TYPE);
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        response.headers_mut().extend(parts.headers);
        response
    };
    with_version(response).await
}

/// Empty bytes and JSON null mean absence; supplied values must decode completely.
fn optional_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<Option<T>, crate::Fail> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if !matches!(
        bytes.iter().find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{' | b'n')
    ) {
        return Err(Refusal::new(
            dvb_core::RefusalCode::InvalidValue,
            "give a JSON object or null",
        )
        .into());
    }
    serde_json::from_slice(bytes).map_err(|error| {
        Refusal::new(dvb_core::RefusalCode::InvalidValue, error.to_string()).into()
    })
}
