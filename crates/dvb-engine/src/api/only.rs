//! Owner-only control of the timed install-wide model switch.
use crate::{AppState, Fail};
use axum::{Json, extract::State};
use dvb_core::api::SetOnly;
use dvb_core::model::OnlyModels;
use dvb_core::{Refusal, RefusalCode};

#[utoipa::path(
    get, path = "/only", operation_id = "get_only", tag = "only",
    responses(crate::api::ErrorResponses, (status = 200, description = "Active install-wide model switch, or null", body = Option<dvb_core::model::OnlyModels>))
)]
pub async fn get(State(e): State<AppState>) -> Result<Json<Option<OnlyModels>>, Fail> {
    e.only_models().map(Json)
}

#[utoipa::path(
    put, path = "/only", operation_id = "set_only", tag = "only",
    request_body = dvb_core::api::SetOnly,
    responses(crate::api::ErrorResponses, (status = 200, description = "Replaced install-wide model switch", body = dvb_core::model::OnlyModels),
              (status = 409, description = "Invalid models or hours", body = dvb_core::Refusal))
)]
pub async fn set(
    State(e): State<AppState>,
    body: Result<Json<SetOnly>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<OnlyModels>, Fail> {
    let Json(body) = body.map_err(|_| {
        Refusal::new(
            RefusalCode::InvalidValue,
            "give exact model ids and hours as a whole number from 1 to 168",
        )
    })?;
    e.set_only(body).map(Json)
}

#[utoipa::path(
    post, path = "/only/clear", operation_id = "clear_only", tag = "only",
    responses(crate::api::ErrorResponses, (status = 200, description = "Switch ended (idempotent)", body = Option<dvb_core::model::OnlyModels>))
)]
pub async fn clear(State(e): State<AppState>) -> Result<Json<Option<OnlyModels>>, Fail> {
    e.clear_only()?;
    Ok(Json(None))
}
