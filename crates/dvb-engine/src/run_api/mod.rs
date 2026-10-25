//! The run socket router: `dvb run <verb>` is
//! `POST /run/<verb>` on `/run/dvb/run.sock`. Listed here once, like the owner
//! API. Handlers enforce the calling run’s role.

mod driver;
pub(crate) mod orchestrator;
mod shared;
mod verdict;

use crate::AppState;
use crate::api::with_version;
use axum::Router;
use axum::middleware::map_response;
use axum::routing::post;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/run/plan", post(orchestrator::plan))
        .route("/run/ask", post(orchestrator::ask))
        .route("/run/task/start", post(orchestrator::task_start))
        .route("/run/task/stop", post(orchestrator::task_stop))
        .route("/run/task/retry", post(orchestrator::task_retry))
        .route("/run/hire", post(orchestrator::hire))
        .route("/run/retire", post(orchestrator::retire))
        .route("/run/decide", post(shared::decide))
        .route("/run/note", post(shared::note))
        .route("/run/issue", post(shared::issue))
        .route("/run/issues", post(shared::issues))
        .route("/run/verdict", post(verdict::verdict))
        .route("/run/preflight", post(driver::preflight))
        .route("/run/model", post(driver::model))
        .route("/run/output", post(driver::output))
        .route("/run/usage", post(driver::usage))
        .route("/run/ratelimit", post(driver::ratelimit))
        .route("/run/collected", post(driver::collected))
        .layer(map_response(with_version))
        .with_state(state)
}
