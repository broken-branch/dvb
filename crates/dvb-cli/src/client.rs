//! Shared engine-socket HTTP/1.1 client for owner and worker calls. The
//! desk also uses `connect` to proxy browser calls.

use bytes::Bytes;
use dvb_core::api::VERSION_HEADER;
use dvb_core::{Event, EventId, EventKind, Refusal};
use http_body_util::{BodyExt, Full};
pub use hyper::Method;
use hyper::body::{Body, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

#[derive(Debug)]
pub enum CliError {
    /// A coded refusal, such as `NOT_FOUND` (404) or a conflict (409).
    Refused(Refusal),
    Http(u16, String),
    Message(String),
    Connection(std::io::Error),
    Transport(hyper::Error),
    /// An event message that cannot be decoded or parsed.
    Decode(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Refused(r) => write!(f, "refused: {r}"),
            CliError::Http(status, body) => write!(f, "HTTP {status}: {body}"),
            CliError::Message(e) | CliError::Decode(e) => write!(f, "{e}"),
            CliError::Connection(e) => write!(f, "{e}"),
            CliError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CliError {}

pub(crate) fn message(e: impl std::fmt::Display) -> CliError {
    CliError::Message(e.to_string())
}

pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(150);

pub async fn bounded<T>(future: impl std::future::Future<Output = T>) -> Result<T, CliError> {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .map_err(|_| CliError::Message("the engine request timed out".into()))
}

fn restart_io(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

impl CliError {
    /// Only failures caused by a disappearing or closed restart connection.
    pub(crate) fn restart_connection(&self) -> bool {
        match self {
            Self::Connection(error) => restart_io(error.kind()),
            Self::Transport(error) => {
                if error.is_closed() || error.is_incomplete_message() || error.is_canceled() {
                    return true;
                }
                let mut source = std::error::Error::source(error);
                while let Some(cause) = source {
                    if let Some(error) = cause.downcast_ref::<std::io::Error>() {
                        return restart_io(error.kind());
                    }
                    source = cause.source();
                }
                false
            }
            Self::Message(_) | Self::Refused(_) | Self::Http(_, _) | Self::Decode(_) => false,
        }
    }
}

/// One HTTP/1.1 connection to the socket; the connection task runs until the
/// response (and its body) is done.
pub async fn connect<B>(sock: &Path) -> Result<SendRequest<B>, CliError>
where
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let stream = tokio::time::timeout(std::time::Duration::from_secs(5), UnixStream::connect(sock))
        .await
        .map_err(|_| CliError::Message("engine connection timed out".into()))?
        .map_err(|error| {
            CliError::Connection(std::io::Error::new(
                error.kind(),
                format!("{}: {error}", sock.display()),
            ))
        })?;
    let (send, conn) = bounded(hyper::client::conn::http1::handshake(TokioIo::new(stream)))
        .await?
        .map_err(CliError::Transport)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(send)
}

/// Task start/retry can perform several bounded Git and GitHub operations.
pub const RUN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

pub struct Client {
    pub sock: PathBuf,
}

impl Client {
    pub fn new(sock: PathBuf) -> Self {
        Self { sock }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response<Incoming>, CliError> {
        bounded(self.request(method, path, body)).await?
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response<Incoming>, CliError> {
        let mut send = connect::<Full<Bytes>>(&self.sock).await?;
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "dvb");
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(message)?;
        send.send_request(req).await.map_err(CliError::Transport)
    }

    /// One request without the owner version probe, its status and body kept as
    /// they came: worker verbs show refusals verbatim, and the desk forwards them.
    pub async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        timeout: std::time::Duration,
    ) -> Result<Reply, CliError> {
        tokio::time::timeout(timeout, async {
            let res = self.request(method, path, Some(body)).await?;
            let status = res.status().as_u16();
            let bytes = res
                .into_body()
                .collect()
                .await
                .map_err(CliError::Transport)?
                .to_bytes();
            Ok(Reply {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        })
        .await
        .map_err(|_| message("the engine request timed out"))?
    }

    async fn json<T: DeserializeOwned>(res: Response<Incoming>) -> Result<T, CliError> {
        let status = res.status();
        let bytes = bounded(res.into_body().collect())
            .await?
            .map_err(CliError::Transport)?
            .to_bytes();
        if status == StatusCode::NO_CONTENT {
            return serde_json::from_value(serde_json::Value::Null).map_err(message);
        }
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(message);
        }
        match serde_json::from_slice::<Refusal>(&bytes) {
            Ok(r) => Err(CliError::Refused(r)),
            Err(_) => Err(CliError::Http(
                status.as_u16(),
                String::from_utf8_lossy(&bytes).into_owned(),
            )),
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CliError> {
        Self::json(self.send(Method::GET, path, None).await?).await
    }

    /// A write. Refused when the engine is another version.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, CliError> {
        self.write(Method::POST, path, body).await
    }

    pub async fn put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, CliError> {
        self.write(Method::PUT, path, body).await
    }

    pub async fn patch<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, CliError> {
        self.write(Method::PATCH, path, body).await
    }

    async fn write<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, CliError> {
        let res = self.send(Method::GET, "/version", None).await?;
        if !res.status().is_success() {
            return Self::json(res).await;
        }
        let engine = res
            .headers()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if engine != dvb_core::VERSION {
            return Err(CliError::Refused(Refusal::new(
                dvb_core::RefusalCode::VersionMismatch {
                    client: dvb_core::VERSION.into(),
                    engine,
                },
                "this dvb and the engine are different versions",
            )));
        }
        let body = serde_json::to_vec(body).map_err(message)?;
        Self::json(self.send(method, path, Some(body)).await?).await
    }

    /// Reads `GET /events?after=<after>`, calling `each` for every event; stops at
    /// the end of the backlog when `once`, otherwise when `each` returns false.
    pub async fn read_events<T: DeserializeOwned>(
        &self,
        after: u64,
        once: bool,
        mut each: impl FnMut(T) -> bool,
    ) -> Result<(), CliError> {
        let res = self
            .send(Method::GET, &format!("/events?after={after}"), None)
            .await?;
        if !res.status().is_success() {
            return Self::json::<serde_json::Value>(res).await.map(|_| ());
        }
        let mut body = res.into_body();
        let mut buf = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(CliError::Transport)?;
            let Some(data) = frame.data_ref() else {
                continue;
            };
            buf.extend_from_slice(data);
            while let Some(end) = buf.windows(2).position(|bytes| bytes == b"\n\n") {
                let msg = String::from_utf8(buf.drain(..end + 2).collect())
                    .map_err(|e| CliError::Decode(e.to_string()))?;
                for line in msg.lines() {
                    if line == ": live" && once {
                        return Ok(());
                    }
                    if let Some(data) = line.strip_prefix("data: ").or(line.strip_prefix("data:")) {
                        let ev: T = serde_json::from_str(data)
                            .map_err(|e| CliError::Decode(e.to_string()))?;
                        if !each(ev) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// An event of the feed; a kind this binary does not know keeps its raw form.
#[derive(Deserialize)]
#[serde(try_from = "serde_json::Value")]
pub enum WatchEvent {
    Known(Box<Event>),
    Unknown {
        id: EventId,
        kind: String,
        raw: serde_json::Value,
    },
}

impl TryFrom<serde_json::Value> for WatchEvent {
    type Error = serde_json::Error;

    fn try_from(raw: serde_json::Value) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        struct Header {
            id: EventId,
            kind: String,
        }
        let Header { id, kind } = Header::deserialize(&raw)?;
        if EventKind::ALL_KINDS.contains(&kind.as_str()) {
            serde_json::from_value(raw).map(|event| Self::Known(Box::new(event)))
        } else {
            Ok(Self::Unknown { id, kind, raw })
        }
    }
}

impl WatchEvent {
    pub fn id(&self) -> EventId {
        match self {
            Self::Known(event) => event.id,
            Self::Unknown { id, .. } => *id,
        }
    }
}

/// A background task that stops when its handle is dropped.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http;
    use axum::{
        body::Body,
        response::{IntoResponse, Response},
    };
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn page_response(cursor: u64, events: &[Event], before: Option<u64>) -> Response {
        test_http::json(
            200,
            serde_json::json!({
                "cursor":cursor, "events":events, "before":before, "activity":[]
            }),
        )
    }

    #[tokio::test]
    async fn filtered_watch_encodes_reserved_unicode_empty_and_absent_query_values() {
        for kinds in [None, Some(""), Some("decision.recorded,é +&=/#?%")] {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("engine.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let mut requests = test_http::requests(listener);
            let filter = dvb_core::api::EventPageQuery {
                project: Some(dvb_core::ProjectId::new("p &name=é/#?+%")),
                role: Some(dvb_core::RoleId::new("role+&=")),
                task: Some(dvb_core::TaskId::new("")),
                kinds: kinds.map(str::to_owned),
                ..Default::default()
            };
            let mut expected = filter.clone();
            expected.limit = Some(100);
            expected.latest = Some(false);
            expected.summary = Some(false);
            let server = tokio::spawn(async move {
                let (request, response) = requests.recv().await.unwrap();
                assert_eq!(request.method(), Method::GET);
                assert_eq!(request.uri().path(), "/events/recent");
                let params = request.uri().query().unwrap();
                assert!(!params.contains("null"));
                assert!(!params.contains("before="));
                let actual: dvb_core::api::EventPageQuery =
                    serde_urlencoded::from_str(params).unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
                response.send(page_response(0, &[], None)).unwrap();
            });
            Client::new(sock)
                .watch_filtered_events(0, true, &filter, async |_| panic!("empty page"))
                .await
                .unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn filtered_watch_pages_combined_scopes_oldest_first_after_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let server = tokio::spawn(async move {
            for (before, ids) in [(None, vec![9, 7]), (Some(7), vec![6, 4, 2])] {
                let (request, response) = requests.recv().await.unwrap();
                assert_eq!(request.method(), Method::GET);
                assert_eq!(request.uri().path(), "/events/recent");
                let request = request.uri().to_string();
                for filter in [
                    "project=p",
                    "role=backend",
                    "task=t",
                    "kinds=decision.recorded%2Cplan.proposed",
                    "latest=false",
                    "summary=false",
                ] {
                    assert!(request.contains(filter), "{request}");
                }
                assert_eq!(request.contains("before=7"), before.is_some());
                // The engine may associate a decision or an entire proposed plan;
                // neither needs direct task/role fields in its payload.
                let events: Vec<Event> = ids
                    .into_iter()
                    .map(|id| {
                        event(
                            id,
                            dvb_core::EventKind::PlanProposed {
                                goal: dvb_core::GoalId::new("g"),
                                tasks: vec![],
                            },
                        )
                    })
                    .collect();
                response
                    .send(page_response(
                        10,
                        &events,
                        if before.is_none() { Some(7) } else { None },
                    ))
                    .unwrap();
            }
        });
        let filter = dvb_core::api::EventPageQuery {
            project: Some(dvb_core::ProjectId::new("p")),
            role: Some(dvb_core::RoleId::new("backend")),
            task: Some(dvb_core::TaskId::new("t")),
            kinds: Some("decision.recorded,plan.proposed".into()),
            ..Default::default()
        };
        let mut ids = Vec::new();
        Client::new(sock)
            .watch_filtered_events(3, true, &filter, |e| {
                ids.push(e.id().0);
                std::future::ready(Ok(true))
            })
            .await
            .unwrap();
        assert_eq!(ids, [4, 6, 7, 9]);
        server.await.unwrap();
    }

    // Real time: a paused clock jumps to the guard below whenever the fixture's
    // disconnect reaches the client late, as when a child another test forks
    // still holds the socket until it execs.
    #[tokio::test]
    async fn filtered_watch_follows_global_tail_and_reconnects_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let server = tokio::spawn(async move {
            for step in 0..5 {
                let (request, response) = requests.recv().await.unwrap();
                assert_eq!(request.method(), Method::GET);
                let request = request.uri().to_string();
                match step {
                    0 => {
                        assert!(request.starts_with("/events/recent?"));
                        response
                            .send(page_response(10, &[goal(7, "first")], None))
                            .unwrap();
                    }
                    1 => {
                        assert!(request == "/events?after=10", "{request}");
                        response
                            .send(Response::new(Body::from(message(&goal(
                                11,
                                "unrelated wakeup",
                            )))))
                            .unwrap();
                    }
                    2 => {
                        assert!(request.starts_with("/events/recent?"));
                        response
                            .send(page_response(11, &[goal(7, "first")], None))
                            .unwrap();
                    }
                    3 => {
                        assert!(request == "/events?after=11", "{request}");
                        // A failed stream must leave the next page starting after 11.
                        let failed = futures_util::stream::iter([Err::<bytes::Bytes, _>(
                            std::io::Error::other("fixture disconnect"),
                        )]);
                        response
                            .send(Response::new(Body::from_stream(failed)))
                            .unwrap();
                    }
                    4 => {
                        assert!(request.starts_with("/events/recent?"));
                        response
                            .send(page_response(
                                14,
                                &[goal(13, "after restart"), goal(7, "first")],
                                None,
                            ))
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
            }
        });
        let mut ids = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            Client::new(sock).watch_filtered_events(5, false, &Default::default(), |e| {
                ids.push(e.id().0);
                std::future::ready(Ok(e.id().0 != 13))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            ids,
            [7, 13],
            "SSE wakeups have no authority over membership"
        );
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn socket_stalled_headers_and_bodies_are_bounded() {
        for headers in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("engine.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                if headers {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{")
                        .await
                        .unwrap();
                }
                std::future::pending::<()>().await;
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(160),
                Client::new(sock).get::<serde_json::Value>("/version"),
            )
            .await
            .expect("owner request must have its own deadline");
            assert!(result.is_err());
            server.abort();
        }
    }

    #[tokio::test]
    async fn socket_run_posts_preserve_framing_and_refusals_without_an_owner_probe() {
        for (status, framing, body) in [
            (200, "Content-Length: 2", "{}"),
            (
                409,
                "Transfer-Encoding: chunked",
                "1b\r\n{\"code\":\"ROLE_NOT_ALLOWED\"}\r\n0\r\n\r\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("run.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.starts_with("POST /run/decide HTTP/1.1\r\n"));
                assert!(request.to_lowercase().contains("host: dvb\r\n"));
                let mut payload = [0; 2];
                stream.read_exact(&mut payload).await.unwrap();
                assert_eq!(&payload, b"{}");
                stream
                    .write_all(
                        format!("HTTP/1.1 {status} Reply\r\n{framing}\r\n\r\n{body}").as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let reply = Client::new(sock)
                .raw(
                    Method::POST,
                    "/run/decide",
                    b"{}".to_vec(),
                    RUN_REQUEST_TIMEOUT,
                )
                .await
                .unwrap();
            assert_eq!(reply.status, status);
            assert_eq!(reply.ok(), status == 200);
            assert_eq!(
                reply.body,
                match status {
                    200 => "{}",
                    409 => r#"{"code":"ROLE_NOT_ALLOWED"}"#,
                    _ => unreachable!(),
                }
            );
            server.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn socket_run_start_and_retry_outlive_owner_deadlines_but_remain_bounded() {
        for verb in ["start", "retry"] {
            for headers in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let sock = dir.path().join("run.sock");
                let listener = tokio::net::UnixListener::bind(&sock).unwrap();
                let (ready, request_read) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    assert!(
                        read_request(&mut stream)
                            .await
                            .starts_with(&format!("POST /run/task/{verb} "))
                    );
                    if headers {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{")
                            .await
                            .unwrap();
                    }
                    ready.send(()).unwrap();
                    std::future::pending::<()>().await;
                });
                let request = tokio::spawn(async move {
                    Client::new(sock)
                        .raw(
                            Method::POST,
                            &format!("/run/task/{verb}"),
                            b"{}".to_vec(),
                            RUN_REQUEST_TIMEOUT,
                        )
                        .await
                });
                request_read.await.unwrap();
                tokio::time::advance(REQUEST_TIMEOUT + std::time::Duration::from_secs(1)).await;
                assert!(
                    !request.is_finished(),
                    "worker operation must outlive the owner deadline"
                );
                let result = tokio::time::timeout(RUN_REQUEST_TIMEOUT, request)
                    .await
                    .expect("worker operation must have its own deadline")
                    .unwrap();
                assert!(
                    matches!(result, Err(CliError::Message(ref message)) if message.contains("timed out"))
                );
                server.abort();
            }
        }
    }

    #[test]
    fn update_retries_only_restart_connection_errors() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(CliError::Connection(std::io::Error::from(kind)).restart_connection());
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::InvalidData,
            ErrorKind::Other,
        ] {
            assert!(!CliError::Connection(std::io::Error::from(kind)).restart_connection());
        }
        assert!(!CliError::Http(403, "forbidden".into()).restart_connection());
        assert!(
            !CliError::Refused(Refusal::new(
                dvb_core::RefusalCode::UpdateRefused,
                "fixture"
            ))
            .restart_connection()
        );
        assert!(!CliError::Message("timed out".into()).restart_connection());
    }

    #[tokio::test(start_paused = true)]
    async fn socket_idle_event_stream_outlives_the_request_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let server = tokio::spawn(async move {
            let (request, response) = requests.recv().await.unwrap();
            assert_eq!(request.uri(), "/events?after=0");
            let live =
                futures_util::stream::iter([Ok::<_, std::io::Error>(": live\n\n".to_owned())]);
            let later = futures_util::stream::once(async {
                tokio::time::sleep(REQUEST_TIMEOUT + std::time::Duration::from_secs(1)).await;
                Ok::<_, std::io::Error>(message(&goal(1, "After idle")))
            });
            response
                .send(Response::new(Body::from_stream(live.chain(later))))
                .unwrap();
        });
        let mut received = Vec::new();
        Client::new(sock)
            .read_events(0, false, |event: Event| {
                received.push(event);
                true
            })
            .await
            .unwrap();
        assert_eq!(received, vec![goal(1, "After idle")]);
        server.await.unwrap();
    }

    async fn read_request(stream: &mut UnixStream) -> String {
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(request).unwrap()
    }

    fn event(id: u64, kind: dvb_core::EventKind) -> Event {
        Event {
            id: dvb_core::EventId(id),
            at: dvb_core::Timestamp(0),
            actor: dvb_core::Actor::Engine,
            project: None,
            kind,
            evidence: None,
        }
    }

    fn goal(id: u64, text: &str) -> Event {
        event(
            id,
            dvb_core::EventKind::GoalStated {
                goal: dvb_core::GoalId::new("g"),
                text: text.into(),
                from_issue: None,
                issues: Vec::new(),
                after: None,
            },
        )
    }

    pub(crate) fn message(event: &Event) -> String {
        format!(
            "id: {}\ndata: {}\n\n",
            event.id.0,
            serde_json::to_string(event).unwrap()
        )
    }

    async fn chunk(stream: &mut UnixStream, data: &[u8]) {
        stream
            .write_all(format!("{:x}\r\n", data.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(data).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
    }

    const SSE_HEADER: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: \
                 chunked\r\nConnection: close\r\n\r\n";

    #[tokio::test]
    async fn socket_events_preserve_unicode_split_across_http_frames() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let expected = goal(1, "Café");
        let msg = message(&expected);
        let split = msg.find('é').unwrap() + 1;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_request(&mut stream)
                    .await
                    .starts_with("GET /events?after=0 ")
            );
            stream.write_all(SSE_HEADER).await.unwrap();
            chunk(&mut stream, &msg.as_bytes()[..split]).await;
            chunk(&mut stream, &msg.as_bytes()[split..]).await;
            chunk(&mut stream, b": live\n\n").await;
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let mut events = Vec::new();
        Client::new(sock)
            .read_events(0, true, |event: Event| {
                events.push(event);
                true
            })
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(events, vec![expected]);
    }

    #[tokio::test]
    async fn socket_failed_version_probe_preserves_http_error_and_never_writes() {
        for method in [Method::POST, Method::PUT] {
            for status in [403, 500] {
                let dir = tempfile::tempdir().unwrap();
                let sock = dir.path().join("engine.sock");
                let listener = tokio::net::UnixListener::bind(&sock).unwrap();
                let mut requests = test_http::requests(listener);
                let server = tokio::spawn(async move {
                    let (request, response) = requests.recv().await.unwrap();
                    assert_eq!(request.method(), Method::GET);
                    assert_eq!(request.uri(), "/version");
                    response
                        .send(
                            (
                                axum::http::StatusCode::from_u16(status).unwrap(),
                                [(VERSION_HEADER, dvb_core::VERSION)],
                                "probe refused",
                            )
                                .into_response(),
                        )
                        .unwrap();
                    assert!(
                        tokio::time::timeout(
                            std::time::Duration::from_millis(100),
                            requests.recv()
                        )
                        .await
                        .is_err(),
                        "must not send a write after a failed probe"
                    );
                });
                let error = Client::new(sock)
                    .write::<_, serde_json::Value>(method.clone(), "/settings", &())
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, CliError::Http(s, ref body) if s == status && body == "probe refused"),
                    "{error:?}"
                );
                server.await.unwrap();
            }
        }
    }
}
