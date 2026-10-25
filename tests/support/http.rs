//! Offline HTTP fixtures using axum's Unix-socket transport.
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use tokio::{
    net::UnixListener,
    sync::{mpsc, oneshot},
};

pub type Requests = mpsc::Receiver<(Request, oneshot::Sender<Response>)>;

/// Let a test assert and answer requests in order; dropping the receiver stops serving.
pub fn requests(listener: UnixListener) -> Requests {
    let (tx, rx) = mpsc::channel(8);
    let closed = tx.clone();
    let router = Router::new().fallback(exchange).with_state(tx);
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { closed.closed().await })
            .await
            .unwrap();
    });
    rx
}

async fn exchange(
    State(tx): State<mpsc::Sender<(Request, oneshot::Sender<Response>)>>,
    request: Request,
) -> Response {
    let (reply, response) = oneshot::channel();
    tx.send((request, reply)).await.unwrap();
    response
        .await
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

pub fn json(status: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [(dvb_core::api::VERSION_HEADER, dvb_core::VERSION)],
        Json(body),
    )
        .into_response()
}

/// Assert a method/path, consume the complete body and send a JSON response.
pub async fn reply(requests: &mut Requests, method: Method, path: &str, status: u16, body: Value) {
    let (request, response) = requests.recv().await.unwrap();
    assert_eq!(request.method(), method);
    assert_eq!(request.uri(), path);
    to_bytes(request.into_body(), usize::MAX).await.unwrap();
    response.send(json(status, body)).unwrap();
}
