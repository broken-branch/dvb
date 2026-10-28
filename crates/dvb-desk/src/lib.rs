//! The desk: serves the app, proxies the API, notifications.
//!
//! Serves embedded `ui/dist` on `127.0.0.1`, proxies `/api/*` and `/events`
//! to the engine's socket, and provides desktop theme and notifications; an
//! installed desk keeps the owner's copy of the desktop entry and app icon current.
//! The browser uses a session cookie set by a one-time launch URL, with `Host`
//! and `Origin` checks; the engine never sees the browser. A launch URL minted
//! to open the app expires after `LAUNCH_LIFETIME_MS`, and any launch URL
//! opens only for a loopback client socket owned by the desk's own uid.

mod desktop;
mod github_redirect;
mod notifications;
mod omarchy;
mod theme;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use dvb_cli::client::AbortOnDrop;
use dvb_cli::github::token;
use dvb_core::{Clock, Timestamp};
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Ui;

const COOKIE: &str = "dvb_session";

/// A minted launch URL can reach process arguments that other local users read.
/// This covers the helper's five-second startup watch and a cold browser start.
const LAUNCH_LIFETIME_MS: i64 = 20_000;

struct LaunchToken {
    token: String,
    /// None only for the link printed at start, which goes to stdout.
    expires: Option<Timestamp>,
}

pub struct Desk {
    engine_sock: PathBuf,
    port: u16,
    /// The one-time launch token; taken on first use.
    launch: Mutex<Option<LaunchToken>>,
    session: String,
    theme_path: Option<PathBuf>,
    navigation: tokio::sync::broadcast::Sender<String>,
    clock: Arc<dyn Clock>,
    /// The uid owning the client socket `(peer, desk)`; None when it cannot be found.
    peer_uid: fn(SocketAddr, SocketAddr) -> Option<u32>,
}

type DeskState = Arc<Desk>;

impl Desk {
    pub fn new(engine_sock: PathBuf, port: u16) -> Self {
        Self {
            engine_sock,
            port,
            launch: Mutex::new(Some(LaunchToken {
                token: token(),
                expires: None,
            })),
            session: token(),
            theme_path: theme::colors_path(),
            navigation: tokio::sync::broadcast::channel(16).0,
            clock: Arc::new(dvb_core::SystemClock),
            peer_uid: procfs_peer_uid,
        }
    }

    /// The URL that opens the app once and sets the session cookie.
    pub fn launch_url(&self) -> Option<String> {
        let launch = self.launch.lock().unwrap_or_else(dvb_core::sync::recover);
        launch
            .as_ref()
            .map(|l| format!("http://127.0.0.1:{}/launch?token={}", self.port, l.token))
    }

    /// Rotate and return the matching link in one critical section.
    fn rotate_launch(&self) -> String {
        let expires = self.clock.now().plus_millis(LAUNCH_LIFETIME_MS);
        let mut launch = self.launch.lock().unwrap_or_else(dvb_core::sync::recover);
        let fresh = token();
        let url = format!("http://127.0.0.1:{}/launch?token={fresh}", self.port);
        *launch = Some(LaunchToken {
            token: fresh,
            expires: Some(expires),
        });
        url
    }

    /// Withdraws a link that only focused an open cockpit, unless a newer one replaced it.
    fn withdraw_launch(&self, token: &str) {
        let mut launch = self.launch.lock().unwrap_or_else(dvb_core::sync::recover);
        if launch.as_ref().is_some_and(|l| l.token == token) {
            *launch = None;
        }
    }

    fn host_ok(&self, headers: &HeaderMap) -> bool {
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    fn origin_ok(&self, method: &Method, headers: &HeaderMap) -> bool {
        match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
            Some(o) => {
                o == format!("http://127.0.0.1:{}", self.port)
                    || o == format!("http://localhost:{}", self.port)
            }
            // Browsers send Origin on every write; a write without one is refused.
            None => matches!(*method, Method::GET | Method::HEAD),
        }
    }

    fn cookie_ok(&self, headers: &HeaderMap) -> bool {
        let want = format!("{COOKIE}={}", self.session);
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .any(|c| c.trim() == want)
    }
    fn authenticated(&self, method: &Method, headers: &HeaderMap) -> bool {
        self.host_ok(headers) && self.origin_ok(method, headers) && self.cookie_ok(headers)
    }
}

pub fn router(desk: DeskState) -> Router {
    Router::new()
        .route("/launch", get(launch))
        .route("/github/start", axum::routing::post(github_redirect::start))
        .route("/github/callback", get(github_redirect::callback))
        .route("/github/prefill", get(github_redirect::prefill))
        .route(
            "/github/install",
            axum::routing::post(github_redirect::install),
        )
        .route("/api/{*rest}", axum::routing::any(proxy))
        .route("/events", get(proxy))
        .route("/desk/events", get(desk_events))
        .route("/theme.css", get(theme_css))
        .fallback(get(asset))
        .with_state(desk)
}

/// Serves the desk on loopback and a mode-0600 owner control socket.
/// Port zero serves an isolated desk without control socket or notifications.
pub async fn serve(engine_sock: PathBuf, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let control_path = dvb_cli::app::control_socket(&engine_sock);
    let actual_port = listener.local_addr()?.port();
    // Only an installed desk: the entry it writes opens /opt/dvb/current.
    if port != 0
        && engine_sock == std::path::Path::new(dvb_core::io::ENGINE_SOCKET)
        && let Some(data) = desktop::data_home()
        && let Err(e) = desktop::refresh(&data)
    {
        eprintln!(
            "dvb desk: cannot update the desktop entry and icon in {}: {e}",
            data.display()
        );
    }
    if port != 0
        && let Err(e) = omarchy::at_start()
    {
        eprintln!("dvb desk: cannot add the Omarchy menu: {e}");
    }
    let desk = Arc::new(Desk::new(engine_sock, actual_port));
    // The watch task ends when this server future is dropped or cancelled.
    let notifications =
        (port != 0).then(|| AbortOnDrop(tokio::spawn(notifications::watch(desk.clone()))));
    if let Some(url) = desk.launch_url() {
        use std::io::Write;
        writeln!(std::io::stdout().lock(), "{url}")?;
    }
    let browser_app = router(desk.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let result = if port == 0 {
        axum::serve(listener, browser_app).await
    } else {
        let control = bind_control(&control_path).await?;
        let control_app = control_router(desk.clone());
        let browser = axum::serve(listener, browser_app);
        let local = axum::serve(control, control_app);
        let result = tokio::select! { r = browser => r, r = local => r };
        let _ = std::fs::remove_file(control_path);
        result
    };
    drop(notifications);
    result
}

async fn bind_control(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    // Never unlink a live desk or an unrelated file.
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "the desk is already running",
            ));
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e),
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the control path is not a socket",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn control_router(desk: DeskState) -> Router {
    Router::new()
        .route("/app", get(open_app).delete(withdraw_app))
        .with_state(desk)
}

async fn open_app(
    State(d): State<DeskState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<axum::Json<String>, StatusCode> {
    if let Some(route) = q.get("route") {
        let url = d
            .prepare_launch(route)
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        return Ok(axum::Json(url));
    }
    Ok(axum::Json(d.rotate_launch()))
}

async fn withdraw_app(
    State(d): State<DeskState>,
    Query(q): Query<HashMap<String, String>>,
) -> StatusCode {
    match q.get("token") {
        Some(token) => {
            d.withdraw_launch(token);
            StatusCode::NO_CONTENT
        }
        None => StatusCode::BAD_REQUEST,
    }
}
/// Authenticated desk-to-browser navigation; a subscription tracks an open cockpit.
async fn desk_events(State(d): State<DeskState>, req: Request) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    if !d.authenticated(req.method(), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let stream =
        futures_util::stream::unfold(d.navigation.subscribe(), |mut receiver| async move {
            loop {
                match receiver.recv().await {
                    Ok(route) => {
                        return Some((
                            Ok::<_, std::convert::Infallible>(
                                Event::default().event("navigate").data(route),
                            ),
                            receiver,
                        ));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn theme_css(State(d): State<DeskState>, req: Request) -> Response {
    if !d.authenticated(req.method(), req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        theme::css(d.theme_path.as_deref()).await,
    )
        .into_response()
}

async fn launch(
    State(d): State<DeskState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    // The launch URL has a token but must still resist DNS rebinding.
    if !d.host_ok(&headers) || !d.origin_ok(&Method::GET, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Other local accounts can read the URL from the browser's arguments.
    // Their refusal leaves the token for the owner's browser.
    let desk = SocketAddr::from(([127, 0, 0, 1], d.port));
    if (d.peer_uid)(peer, desk) != Some(dvb_core::io::own_uid()) {
        return (
            StatusCode::FORBIDDEN,
            "Only the account running dvb can open this launch link.",
        )
            .into_response();
    }
    let now = d.clock.now();
    let mut launch = d.launch.lock().unwrap_or_else(dvb_core::sync::recover);
    let refused = match launch.as_ref() {
        None => Some("This launch link was already used."),
        Some(l) if q.get("token") != Some(&l.token) => {
            Some("A newer launch link replaced this one.")
        }
        Some(l) if l.expires.is_some_and(|expires| now >= expires) => {
            Some("This launch link expired.")
        }
        Some(_) => None,
    };
    if let Some(refused) = refused {
        return (StatusCode::FORBIDDEN, refused).into_response();
    }
    *launch = None;
    drop(launch);
    let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", d.session);
    let target = q
        .get("route")
        .filter(|r| dvb_cli::app::parse_route(r).is_ok())
        .map(|r| r.strip_prefix('/').unwrap_or(r))
        .map(|r| format!("/#{r}"))
        .unwrap_or_else(|| "/".into());
    let mut res = Redirect::to(&target).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    res
}

/// The uid of this host's TCP socket from `peer` to `desk`, from procfs.
fn procfs_peer_uid(peer: SocketAddr, desk: SocketAddr) -> Option<u32> {
    // A dual-stack client socket is listed in tcp6 with v4-mapped addresses.
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .into_iter()
        .find_map(|path| match std::fs::read_to_string(path) {
            Ok(table) => socket_uid(&table, peer, desk),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("dvb desk: cannot read {path}: {e}");
                }
                None
            }
        })
}

/// The uid of the entry in a `/proc/net/tcp` or `tcp6` table whose local
/// address is `local` and remote address is `remote`.
fn socket_uid(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
    let (local, remote) = (canonical(local), canonical(remote));
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(l), Some(r), Some(uid)) = (fields.get(1), fields.get(2), fields.get(7)) else {
            return None;
        };
        (proc_addr(l)? == local && proc_addr(r)? == remote)
            .then(|| uid.parse().ok())
            .flatten()
    })
}

/// Parses `ADDR:PORT`, where the address is 32-bit words in host byte order.
fn proc_addr(field: &str) -> Option<SocketAddr> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let mut bytes = Vec::with_capacity(16);
    for word in 0..addr.len() / 8 {
        let hex = addr.get(word * 8..word * 8 + 8)?;
        bytes.extend(u32::from_str_radix(hex, 16).ok()?.to_ne_bytes());
    }
    let ip = match (addr.len(), bytes.as_slice()) {
        (8, &[a, b, c, d]) => IpAddr::from([a, b, c, d]),
        (32, _) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => return None,
    };
    Some(canonical(SocketAddr::new(ip, port)))
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// `/api/<path>` -> the engine's `/<path>`; `/events` -> `/events`.
async fn proxy(State(d): State<DeskState>, req: Request) -> Response {
    let headers = req.headers();
    if !d.authenticated(req.method(), headers) {
        return (StatusCode::FORBIDDEN, "Open dvb from its launch link.").into_response();
    }
    let path = req.uri().path();
    let path = path.strip_prefix("/api").unwrap_or(path).to_owned();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let (parts, body) = req.into_parts();
    let mut out = hyper::Request::builder()
        .method(parts.method)
        .uri(format!("{path}{query}"))
        .header(header::HOST, "dvb");
    let last_event_id = HeaderName::from_static("last-event-id");
    for name in [header::CONTENT_TYPE, header::ACCEPT, last_event_id] {
        if let Some(v) = parts.headers.get(&name) {
            out = out.header(name, v);
        }
    }
    let Ok(out) = out.body(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let sent = match dvb_cli::client::connect::<Body>(&d.engine_sock).await {
        Ok(mut send) => match dvb_cli::client::bounded(send.send_request(out)).await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        },
        Err(e) => Err(e.to_string()),
    };
    match sent {
        Ok(res) => res.map(Body::new).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("The engine is not reachable: {e}"),
        )
            .into_response(),
    }
}

/// The app. `index.html` gets `<meta name="dvb-version">` with this binary's
/// version, which the UI sends as its own.
async fn asset(State(d): State<DeskState>, req: Request) -> Response {
    if !d.host_ok(req.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some((served, file)) = Ui::get(path)
        .map(|file| (path, file))
        .or_else(|| Ui::get("index.html").map(|file| ("index.html", file)))
    else {
        return (StatusCode::NOT_FOUND, "The app is not built into this dvb.").into_response();
    };
    let mime = mime_of(served);
    let mut bytes = file.data.into_owned();
    if mime.starts_with("text/html") {
        let html = with_version(&String::from_utf8_lossy(&bytes));
        bytes = html.into_bytes();
    }
    ([(header::CONTENT_TYPE, mime)], bytes).into_response()
}

/// Adds the version marker after `<head>`.
fn with_version(html: &str) -> String {
    let marker = format!(
        "<meta name=\"dvb-version\" content=\"{}\">",
        dvb_core::VERSION
    );
    html.replacen("<head>", &format!("<head>{marker}"), 1)
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

impl Desk {
    fn launch_item(&self, route: &str) -> Result<(), dvb_cli::client::CliError> {
        let url = self.prepare_launch(route)?;
        if dvb_cli::app::launch(&url)? == dvb_cli::app::Opened::Focused
            && let Some(token) = dvb_cli::app::launch_token(&url)
        {
            // The navigation channel carries the route to the open cockpit.
            self.withdraw_launch(token);
        }
        Ok(())
    }

    fn prepare_launch(&self, route: &str) -> Result<String, dvb_cli::client::CliError> {
        let route = dvb_cli::app::parse_route(route).map_err(dvb_cli::client::CliError::Message)?;
        let url = format!("{}&route={route}", self.rotate_launch());
        // The helper may only focus an existing window and ignore its URL.
        // Deliver the route independently; with no subscriber the launch redirect handles it.
        let _ = self
            .navigation
            .send(route.trim_start_matches('/').to_owned());
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    const OWNER: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40_000);
    const STRANGER: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40_001);

    fn test_desk() -> Desk {
        Desk {
            peer_uid: |peer, desk| {
                assert_eq!(desk, SocketAddr::from(([127, 0, 0, 1], 7300)));
                let own = dvb_core::io::own_uid();
                Some(if peer == STRANGER { own ^ 1 } else { own })
            },
            ..Desk::new("/nonexistent/engine.sock".into(), 7300)
        }
    }

    fn desk() -> DeskState {
        Arc::new(test_desk())
    }

    async fn call(d: &DeskState, req: axum::http::Request<Body>) -> Response {
        call_from(d, OWNER, req).await
    }

    async fn call_from(
        d: &DeskState,
        peer: SocketAddr,
        mut req: axum::http::Request<Body>,
    ) -> Response {
        req.extensions_mut().insert(ConnectInfo(peer));
        router(d.clone()).oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn prepared_launch_navigates_an_open_cockpit_without_a_browser_launch() {
        let d = desk();
        use futures_util::StreamExt;
        let response = call(
            &d,
            Request::get("/desk/events")
                .header("host", "127.0.0.1:7300")
                .header("cookie", format!("{COOKIE}={}", d.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let mut browser = response.into_body().into_data_stream();
        let url = d.prepare_launch("/task/t").unwrap();
        assert!(url.ends_with("&route=/task/t"));
        let frame = browser.next().await.unwrap().unwrap();
        let event = String::from_utf8(frame.to_vec()).unwrap();
        assert!(event.contains("event: navigate\n"), "{event}");
        assert!(event.contains("data: task/t\n"), "{event}");
        let response = control_router(d.clone())
            .oneshot(
                Request::get("/app?route=/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let url: String = serde_json::from_slice(&bytes).unwrap();
        assert!(url.ends_with("&route=/setup"));
        let frame = browser.next().await.unwrap().unwrap();
        assert!(
            String::from_utf8(frame.to_vec())
                .unwrap()
                .contains("data: setup\n")
        );
        let token_before = d.launch_url();
        assert!(d.prepare_launch("https://evil.test").is_err());
        assert_eq!(d.launch_url(), token_before);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), browser.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn notification_launch_redirect_handles_a_closed_cockpit_and_channel_needs_a_session() {
        let d = desk();
        let old = d.launch_url().unwrap();
        let url = d.prepare_launch("/decision/q").unwrap();
        assert_ne!(d.launch_url().unwrap(), old);
        let replaced = call(
            &d,
            Request::get(old.trim_start_matches("http://127.0.0.1:7300"))
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(replaced.status(), StatusCode::FORBIDDEN);
        let message = axum::body::to_bytes(replaced.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(message.as_ref(), b"A newer launch link replaced this one.");
        let res = call(
            &d,
            Request::get(url.trim_start_matches("http://127.0.0.1:7300"))
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.headers()[header::LOCATION], "/#decision/q");
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(res.headers().contains_key(header::SET_COOKIE));
        assert!(d.launch_url().is_none());
        assert_eq!(
            call(
                &d,
                Request::get(url.trim_start_matches("http://127.0.0.1:7300"))
                    .header("host", "127.0.0.1:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        let res = call(
            &d,
            Request::get("/desk/events")
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = call(
            &d,
            Request::get("/desk/events")
                .header("host", "127.0.0.1:7300")
                .header("cookie", format!("{COOKIE}={}", d.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(d.navigation.receiver_count(), 1);
        drop(res);
        assert_eq!(d.navigation.receiver_count(), 0);
    }
    #[tokio::test]
    async fn notification_links_redirect_to_the_exact_item_and_reject_bad_routes() {
        let d = desk();
        let path = format!(
            "{}&route=/decision/q",
            d.launch_url()
                .unwrap()
                .trim_start_matches("http://127.0.0.1:7300")
        );
        let res = call(
            &d,
            Request::get(path)
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(res.headers()[header::LOCATION], "/#decision/q");
        for (route, location) in [("/setup", "/#setup"), ("/", "/#")] {
            let d = desk();
            let path = format!(
                "{}&route={route}",
                d.launch_url()
                    .unwrap()
                    .trim_start_matches("http://127.0.0.1:7300")
            );
            let res = call(
                &d,
                Request::get(path)
                    .header("host", "127.0.0.1:7300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(res.headers()[header::LOCATION], location);
        }
        for route in [
            "https://evil.invalid",
            "decision/q?token=bad",
            "home",
            "task/<script>",
        ] {
            assert!(dvb_cli::app::parse_route(route).is_err());
        }
    }

    #[test]
    fn the_app_page_carries_this_binarys_version() {
        let marker = format!(
            "<meta name=\"dvb-version\" content=\"{}\">",
            dvb_core::VERSION
        );
        let page = "<html><head><title>x</title></head></html>";
        let served = with_version(page);
        assert!(served.contains(&marker), "{page}");
        assert!(!served.contains("dvb-mode"), "{page}");
    }

    #[tokio::test]
    async fn socket_serve_fixed_port_exposes_the_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let engine = dir.path().join("engine.sock");
        let control = dvb_cli::client::Client::new(dvb_cli::app::control_socket(&engine));
        let serving = tokio::spawn(serve(engine, port));
        let mut opened = None;
        for _ in 0..100 {
            if let Ok(url) = control.get::<String>("/app").await {
                opened = Some(url);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        serving.abort();
        let _ = serving.await;
        assert!(
            opened
                .unwrap()
                .starts_with(&format!("http://127.0.0.1:{port}/launch?token="))
        );
    }

    #[tokio::test]
    async fn socket_control_keeps_unrelated_paths_and_replaces_owned_stale_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("unrelated");
        std::fs::write(&file, "keep").unwrap();
        assert!(bind_control(&file).await.is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        let stale = dir.path().join("stale.sock");
        let old = std::os::unix::net::UnixListener::bind(&stale).unwrap();
        drop(old);
        let listener = bind_control(&stale).await.unwrap();
        drop(listener);
        let path = dir.path().join("missing-parent/socket");
        assert!(bind_control(&path).await.is_err());
    }

    #[tokio::test]
    async fn socket_serve_refuses_a_port_already_in_use() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(
            serve(
                "/nonexistent/engine.sock".into(),
                occupied.local_addr().unwrap().port()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn the_launch_link_works_once_and_sets_the_cookie() {
        let d = desk();
        let url = d.launch_url().unwrap();
        let path = url.trim_start_matches("http://127.0.0.1:7300").to_owned();
        let req = || {
            axum::http::Request::get(path.as_str())
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap()
        };
        let res = call(&d, req()).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie = res.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.starts_with(&format!("{COOKIE}={}", d.session)));
        assert_eq!(call(&d, req()).await.status(), StatusCode::FORBIDDEN);
    }

    async fn redeem(d: &DeskState, url: &str) -> Response {
        call(
            d,
            Request::get(url.trim_start_matches("http://127.0.0.1:7300"))
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn refusal(res: Response) -> String {
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_minted_launch_link_works_once_and_expires_unredeemed() {
        let clock = Arc::new(dvb_core::FakeClock::at(Timestamp(1_000_000)));
        let d = Arc::new(Desk {
            clock: clock.clone(),
            ..test_desk()
        });
        let url = d.rotate_launch();
        clock.advance_millis(LAUNCH_LIFETIME_MS - 1);
        assert_eq!(redeem(&d, &url).await.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            refusal(redeem(&d, &url).await).await,
            "This launch link was already used."
        );
        for mint in [Desk::rotate_launch, |d: &Desk| {
            d.prepare_launch("/setup").unwrap()
        }] {
            let url = mint(&d);
            clock.advance_millis(LAUNCH_LIFETIME_MS);
            assert_eq!(
                refusal(redeem(&d, &url).await).await,
                "This launch link expired."
            );
        }
        // The link printed at start goes to stdout, not process arguments.
        let d = Arc::new(Desk {
            clock: clock.clone(),
            ..test_desk()
        });
        let started = d.launch_url().unwrap();
        clock.advance_millis(100 * LAUNCH_LIFETIME_MS);
        assert_eq!(redeem(&d, &started).await.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn a_launch_link_opens_only_for_the_desks_own_uid_and_survives_a_stranger() {
        let d = desk();
        let url = d.rotate_launch();
        let req = || {
            Request::get(url.trim_start_matches("http://127.0.0.1:7300"))
                .header("host", "127.0.0.1:7300")
                .body(Body::empty())
                .unwrap()
        };
        let res = call_from(&d, STRANGER, req()).await;
        assert!(!res.headers().contains_key(header::SET_COOKIE));
        assert_eq!(
            refusal(res).await,
            "Only the account running dvb can open this launch link."
        );
        assert_eq!(d.launch_url().as_deref(), Some(url.as_str()));
        let res = call_from(&d, OWNER, req()).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(res.headers().contains_key(header::SET_COOKIE));
        // A socket the lookup cannot find is refused too.
        let d = Arc::new(Desk {
            peer_uid: |_, _| None,
            ..test_desk()
        });
        let url = d.launch_url().unwrap();
        assert_eq!(redeem(&d, &url).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(d.launch_url().as_deref(), Some(url.as_str()));
    }

    #[tokio::test]
    async fn socket_launch_redeems_for_this_process_through_procfs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let d = Arc::new(Desk::new("/nonexistent/engine.sock".into(), port));
        let url = d.launch_url().unwrap();
        let app = router(d.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let serving = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let path = url.trim_start_matches(&format!("http://127.0.0.1:{port}"));
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        serving.abort();
        assert!(response.starts_with("HTTP/1.1 303"), "{response}");
        assert!(d.launch_url().is_none());
    }

    // Fixture rows are as a little-endian kernel prints them; the desk runs as 2000.
    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1C94 00000000:0000 0A 00000000:00000000 00:00000000 00000000  2000        0 4001 1 0 100 0 0 10 0
   1: 0100007F:1C94 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  2000        0 4002 1 0 20 4 30 10 -1
   2: 0100007F:9C40 0100007F:1C94 01 00000000:00000000 00:00000000 00000000  1000        0 4003 1 0 20 4 30 10 -1
   3: 0100007F:9C41 0100007F:1C94 01 00000000:00000000 00:00000000 00000000  1001        0 4004 1 0 20 4 30 10 -1
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:9C40 00000000000000000000000001000000:1C94 01 00000000:00000000 00:00000000 00000000  1000        0 5001 1 0 20 4 30 10 -1
   1: 0000000000000000FFFF00000100007F:9C42 0000000000000000FFFF00000100007F:1C94 01 00000000:00000000 00:00000000 00000000  1002        0 5002 1 0 20 4 30 10 -1
";

    #[test]
    fn the_client_socket_uid_comes_from_the_matching_proc_net_tcp_entry() {
        let v4 = |port| SocketAddr::from(([127, 0, 0, 1], port));
        let v6 = |port| SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
        let desk = v4(7316);
        assert_eq!(socket_uid(TCP, v4(40_000), desk), Some(1000));
        assert_eq!(socket_uid(TCP, v4(40_001), desk), Some(1001));
        // The desk's accepted socket runs the other way.
        assert_eq!(socket_uid(TCP, desk, v4(40_000)), Some(2000));
        assert_eq!(socket_uid(TCP, v4(40_002), desk), None);
        assert_eq!(socket_uid(TCP, v4(40_000), v4(7317)), None);
        assert_eq!(socket_uid(TCP6, v6(40_000), v6(7316)), Some(1000));
        assert_eq!(socket_uid(TCP6, v4(40_000), desk), None);
        // A dual-stack client reaches the IPv4 desk through v4-mapped addresses.
        assert_eq!(socket_uid(TCP6, v4(40_002), desk), Some(1002));
        assert_eq!(socket_uid("", v4(40_000), desk), None);
        let broken = "header\n 0: 0100007F:9C40 0100007F:1C94 01 x x x nope\n 1: 7F:9C40 0100007F:1C94 01 x x x 1000";
        assert_eq!(socket_uid(broken, v4(40_000), desk), None);
    }

    #[tokio::test]
    async fn withdrawing_a_focus_only_link_leaves_no_valid_token() {
        let d = desk();
        let control = |method: Method, path: &str| {
            control_router(d.clone()).oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let res = control(Method::GET, "/app?route=/setup").await.unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let focused: String = serde_json::from_slice(&bytes).unwrap();
        let newer = d.rotate_launch();
        // A late withdrawal of the replaced link keeps the newer one.
        let stale = dvb_cli::app::launch_token(&focused).unwrap();
        let res = control(Method::DELETE, &format!("/app?token={stale}")).await;
        assert_eq!(res.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(d.launch_url().as_deref(), Some(newer.as_str()));
        let token = dvb_cli::app::launch_token(&newer).unwrap();
        let res = control(Method::DELETE, &format!("/app?token={token}")).await;
        assert_eq!(res.unwrap().status(), StatusCode::NO_CONTENT);
        assert!(d.launch_url().is_none());
        assert_eq!(
            refusal(redeem(&d, &newer).await).await,
            "This launch link was already used."
        );
        let res = control(Method::DELETE, "/app").await;
        assert_eq!(res.unwrap().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_api_needs_the_cookie_host_and_origin() {
        let d = desk();
        let cookie = format!("{COOKIE}={}", d.session);
        let req = |host: &str, cookie: &str, origin: Option<&str>| {
            let mut r = axum::http::Request::post("/api/version")
                .header("host", host)
                .header("cookie", cookie);
            if let Some(o) = origin {
                r = r.header("origin", o);
            }
            r.body(Body::empty()).unwrap()
        };
        let ok_origin = Some("http://127.0.0.1:7300");
        for bad in [
            req("127.0.0.1:7300", "dvb_session=nope", ok_origin),
            req("evil.example:7300", &cookie, ok_origin),
            req("127.0.0.1:7300", &cookie, Some("http://evil.example")),
            req("127.0.0.1:7300", &cookie, None),
        ] {
            assert_eq!(call(&d, bad).await.status(), StatusCode::FORBIDDEN);
        }
        // Let through, to an engine that is not there.
        let res = call(&d, req("127.0.0.1:7300", &cookie, ok_origin)).await;
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    }
    #[tokio::test]
    async fn launch_and_theme_require_the_expected_host_and_theme_needs_a_session() {
        let d = desk();
        let path = d
            .launch_url()
            .unwrap()
            .trim_start_matches("http://127.0.0.1:7300")
            .to_owned();
        let wrong = axum::http::Request::get(path)
            .header("host", "evil.test:7300")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(&d, wrong).await.status(), StatusCode::FORBIDDEN);
        assert!(d.launch_url().is_some());
        let req = |cookie: &str| {
            axum::http::Request::get("/theme.css")
                .header("host", "127.0.0.1:7300")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(call(&d, req("")).await.status(), StatusCode::FORBIDDEN);
        let res = call(&d, req(&format!("{COOKIE}={}", d.session))).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn socket_proxy_preserves_json_version_and_the_event_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let upstream = Router::new()
            .route(
                "/echo",
                axum::routing::post(|headers: HeaderMap, body: axum::body::Bytes| async move {
                    assert!(!headers.contains_key(header::COOKIE));
                    assert!(!headers.contains_key(header::ORIGIN));
                    (
                        [
                            ("dvb-version", "0.1.0"),
                            ("content-type", "application/json"),
                        ],
                        body,
                    )
                }),
            )
            .route(
                "/events",
                get(|req: Request| async move {
                    assert_eq!(req.uri().query(), Some("after=7"));
                    assert_eq!(req.headers()["last-event-id"], "7");
                    (
                        [
                            ("dvb-version", "0.1.0"),
                            ("content-type", "text/event-stream"),
                        ],
                        "id: 8\ndata: {\"id\":8}\n\n: live\n\n",
                    )
                }),
            );
        let serving = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let d = Arc::new(Desk::new(path, 7300));
        let req = axum::http::Request::post("/api/echo")
            .header("host", "127.0.0.1:7300")
            .header("origin", "http://127.0.0.1:7300")
            .header("cookie", format!("{COOKIE}={}", d.session))
            .header("content-type", "application/json")
            .body(Body::from("{\"choice\":1}"))
            .unwrap();
        let res = call(&d, req).await;
        assert_eq!(res.headers()["dvb-version"], "0.1.0");
        assert_eq!(
            axum::body::to_bytes(res.into_body(), 4096).await.unwrap(),
            "{\"choice\":1}"
        );
        let req = axum::http::Request::get("/events?after=7")
            .header("host", "127.0.0.1:7300")
            .header("cookie", format!("{COOKIE}={}", d.session))
            .header("last-event-id", "7")
            .body(Body::empty())
            .unwrap();
        let res = call(&d, req).await;
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(
            axum::body::to_bytes(res.into_body(), 4096).await.unwrap(),
            "id: 8\ndata: {\"id\":8}\n\n: live\n\n"
        );
        serving.abort();
    }

    #[tokio::test]
    async fn socket_control_is_private_and_reuses_the_running_desk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desk.sock");
        let listener = bind_control(&path).await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind_control(&path).await.is_err());
        let d = desk();
        let serving = tokio::spawn(async move {
            axum::serve(listener, control_router(d)).await.unwrap();
        });
        let client = dvb_cli::client::Client::new(path);
        let one: String = client.get("/app").await.unwrap();
        let two: String = client.get("/app").await.unwrap();
        assert_ne!(one, two);
        serving.abort();
    }
}
