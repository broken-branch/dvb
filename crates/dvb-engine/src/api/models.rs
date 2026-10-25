//! Available models and efforts.
use crate::AppState;
use axum::{Json, extract::State};

#[utoipa::path(get, path = "/models", operation_id = "get_models", tag = "models",
    responses(crate::api::ErrorResponses, (status = 200, description = "Available models and efforts", body = dvb_core::ModelPool)))]
pub async fn get_models(State(e): State<AppState>) -> Json<dvb_core::ModelPool> {
    Json(e.pool.clone())
}
