//! The event stream, SSE with `Last-Event-ID`.
//!
//! Every event after `after` (or the `Last-Event-ID` header, which wins) is sent
//! from the store, then the comment `: live` marks the end of the backlog, then
//! new events follow as they are appended. Each SSE message has the event's id
//! and its JSON as data. `dvb watch --once` stops at `: live`. A client that
//! falls behind is caught up from the store (`crate::feed`).

use crate::AppState;
use crate::feed::FeedItem;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as Sse, KeepAlive};
use axum::response::{IntoResponse, Response};
use dvb_core::EventId;
use dvb_core::api::AfterQuery;
use futures_util::StreamExt;
use std::convert::Infallible;

#[utoipa::path(
    get,
    path = "/events",
    operation_id = "get_events",
    tag = "events",
    params(dvb_core::api::AfterQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Every event after `after` (or `Last-Event-ID`), then new ones as they happen", content_type = "text/event-stream", body = dvb_core::Event))
)]
pub async fn get_events(
    State(e): State<AppState>,
    Query(q): Query<AfterQuery>,
    headers: HeaderMap,
) -> Response {
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .map(EventId)
        .or(q.after)
        .unwrap_or(EventId(0));
    match crate::feed::after(&e, after) {
        Ok(items) => axum::response::Sse::new(items.map(|item| {
            Ok::<_, Infallible>(match item {
                FeedItem::Event(ev) => Sse::default()
                    .id(ev.id.to_string())
                    .data(serde_json::to_string(&ev).unwrap_or_default()),
                FeedItem::Live => Sse::default().comment("live"),
            })
        }))
        .keep_alive(KeepAlive::default())
        .into_response(),
        Err(fail) => fail.into_response(),
    }
}

/// The store captures a consistent cursor; goal order is read under a short guard.
#[utoipa::path(
    get, path = "/events/recent", operation_id = "event_page", tag = "events",
    params(dvb_core::api::EventPageQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Bounded recent events and an exclusive older-page cursor", body = dvb_core::api::EventPage))
)]
pub async fn event_page(
    State(e): State<AppState>,
    Query(q): Query<dvb_core::api::EventPageQuery>,
) -> Result<axum::Json<dvb_core::api::EventPage>, crate::Fail> {
    let mut page = e.store.event_page(&q)?;
    let projection = e.proj();
    for activity in &mut page.activity {
        activity.goals = projection
            .goals_in_order(&activity.project)
            .iter()
            .filter(|g| !g.proposal)
            .map(|g| g.id.clone())
            .collect();
    }
    drop(projection);
    Ok(axum::Json(page))
}
