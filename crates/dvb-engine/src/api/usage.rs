//! Usage: tokens, cost, vendor windows.

use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::extract::{Query, State};
use dvb_core::EventId;
use dvb_core::api::{Usage, UsageQuery};

#[utoipa::path(
    get,
    path = "/usage",
    operation_id = "get_usage",
    tag = "usage",
    params(dvb_core::api::UsageQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Tokens, estimated cost and vendor windows", body = dvb_core::api::Usage))
)]
pub async fn get_usage(
    State(e): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Usage>, Fail> {
    let mut sums = crate::usage::Sums::new(&q);
    e.store.each(
        EventId(0),
        dvb_store::Select::Kinds(&["run.started", "run.rerouted", "run.usage"]),
        |ev| sums.add(&ev),
    )?;
    Ok(Json(Usage {
        rows: sums.rows(),
        windows: e.vendors.windows(),
    }))
}
