//! The Codex provider: the app-server over stdio, JSON-RPC
//! lines. `initialize`, `account/rateLimits/read`, `thread/start` with the pinned
//! model and no sandbox (dvb's unit is the boundary), `turn/start`; then the
//! notifications are mapped to run items. There is no `codex exec` fallback.
//!
//! The served model is "unconfirmed" unless a `model/rerouted` notification names
//! one. The shapes are the 0.159.2 schema's.

/// Feature policy shared by worker preparation and preflight, in CLI order.
pub(crate) const FEATURES_OFF: &[&str] = &["memories", "fast_mode"];

use crate::child::KillOnDrop;
use dvb_core::Effort;
use dvb_core::Timestamp;
use dvb_core::event::ServedModel;
use dvb_core::provider::{ExitStatus, Provider, ProviderError, RunItem, Session};
use dvb_core::text::clip;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The CLI version the mapping and these fixtures were recorded against.
pub const RECORDED_VERSION: &str = "0.159.2";

/// Bounds the whole preflight/startup exchange, including blocking stdio.
/// Notifications during an ordinary model turn have no such deadline.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Codex {
    /// The `codex` executable.
    pub bin: PathBuf,
    /// The run directory: holds `bin/dvb`.
    pub run_dir: PathBuf,
    /// Whether the session's cargo and rustc may install a missing toolchain:
    /// not while another run of the project may still be installing it into
    /// their shared RUSTUP_HOME, which rustup does not guard.
    pub toolchain_installs: bool,
    /// Where the session's app-server stderr goes.
    pub stderr: crate::stream::StderrTail,
}

/// JSON-RPC over lines. Notifications that arrive while a response is awaited
/// are kept in `pending` for the session to map.
pub struct Rpc<R: BufRead, W: Write> {
    r: R,
    w: W,
    next_id: u64,
    pending: VecDeque<Notification>,
}

impl<R: BufRead, W: Write> Rpc<R, W> {
    pub fn new(r: R, w: W) -> Self {
        Self {
            r,
            w,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    fn send(&mut self, v: &Value) -> Result<(), ProviderError> {
        writeln!(self.w, "{v}")
            .and_then(|()| self.w.flush())
            .map_err(|e| ProviderError::Other(format!("app-server: {e}")))
    }

    pub fn notify(&mut self, method: &str) -> Result<(), ProviderError> {
        self.send(&json!({ "method": method }))
    }

    /// The next message from the server, or `None` at the end of the output.
    /// Server requests (approvals) are declined and not returned.
    fn read(&mut self) -> Result<Option<RpcMessage>, ProviderError> {
        loop {
            let mut line = String::new();
            if self
                .r
                .read_line(&mut line)
                .map_err(|e| ProviderError::Other(format!("app-server: {e}")))?
                == 0
            {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: RpcMessage = serde_json::from_str(&line)
                .map_err(|e| bad(format!("not JSON ({e}): {}", clip(line.trim(), 120))))?;
            if let RpcMessage::Request { id, .. } = message {
                self.send(&json!({ "id": id, "result": { "decision": "decline" } }))?;
                continue;
            }
            return Ok(Some(message));
        }
    }

    /// Sends a request and decodes its typed result; `Err` for an error response.
    pub fn request<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Result<T, ProviderError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "id": id, "method": method, "params": params }))?;
        loop {
            let Some(message) = self.read()? else {
                return Err(ProviderError::Other(format!(
                    "app-server closed before answering {method}"
                )));
            };
            match message {
                RpcMessage::Response {
                    id: reply_id,
                    reply,
                } if reply_id == json!(id) => {
                    return match reply {
                        RpcReply::Error { error } => Err(ProviderError::Other(format!(
                            "{method}: {}",
                            clip(&error.to_string(), 300)
                        ))),
                        RpcReply::Result { result } => decode(result),
                    };
                }
                RpcMessage::Notification(notification) => self.pending.push_back(notification),
                RpcMessage::Request { .. } | RpcMessage::Response { .. } => {}
            }
        }
    }

    pub fn initialize(&mut self) -> Result<(), ProviderError> {
        self.request::<Value>(
            "initialize",
            &json!({ "clientInfo": { "name": "dvb", "version": dvb_core::VERSION } }),
        )?;
        self.notify("initialized")
    }
}

fn command(bin: &Path, home: &Path, run_dir: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["app-server", "--disable", "apps"])
        .current_dir(cwd)
        .env("HOME", home)
        .env("PATH", crate::stream::path_with(&run_dir.join("bin")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd
}

type Conn = Rpc<BufReader<std::process::ChildStdout>, std::process::ChildStdin>;

fn connect(mut cmd: Command) -> Result<(KillOnDrop, Conn), ProviderError> {
    let mut child = KillOnDrop(
        cmd.spawn()
            .map_err(|e| ProviderError::Other(format!("codex app-server: {e}")))?,
    );
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(ProviderError::Other("codex app-server: no stdio".into()));
    };
    Ok((child, Rpc::new(BufReader::new(stdout), stdin)))
}

/// The caller retains the child so a blocked RPC read or write cannot prevent
/// killing and reaping it. A successful exchange returns its live transport.
fn startup<T: Send + 'static>(
    child: KillOnDrop,
    timeout: Duration,
    exchange: impl FnOnce() -> Result<T, ProviderError> + Send + 'static,
) -> Result<(KillOnDrop, T), ProviderError> {
    let (send, recv) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(exchange());
    });
    let result = match recv.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(ProviderError::Other(format!(
            "codex app-server startup timed out after {timeout:?}"
        ))),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProviderError::Other(
            "codex app-server startup stopped before answering".into(),
        )),
    };
    match result {
        Ok(value) => Ok((child, value)),
        Err(err) => Err(err),
    }
}

/// The model ids `model/list` returns, at no cost (the preflight's pin check).
pub fn list_models(bin: &Path, home: &Path, cwd: &Path) -> Result<Vec<String>, String> {
    list_models_with_timeout(bin, home, cwd, STARTUP_TIMEOUT).map_err(|e| e.to_string())
}

fn list_models_with_timeout(
    bin: &Path,
    home: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<String>, ProviderError> {
    let (child, mut rpc) = connect(command(bin, home, cwd, cwd))?;
    let (child, ids) = startup(child, timeout, move || {
        rpc.initialize()?;
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let r: ModelPage = rpc.request(
                "model/list",
                &json!({ "cursor": cursor, "includeHidden": true }),
            )?;
            ids.extend(r.ids());
            cursor = r.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(ids)
    })?;
    drop(child);
    Ok(ids)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPage {
    #[serde(default)]
    data: Vec<Model>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Model {
    id: Option<String>,
    model: Option<String>,
}

impl ModelPage {
    fn ids(&self) -> Vec<String> {
        self.data
            .iter()
            .flat_map(|m| [m.id.clone(), m.model.clone()])
            .flatten()
            .collect()
    }
}

impl Provider for Codex {
    fn start(
        &self,
        ws: &Path,
        home: &Path,
        prompt: &str,
        model: &str,
        effort: Effort,
    ) -> Result<Box<dyn Session>, ProviderError> {
        self.start_with_timeout(ws, home, prompt, model, effort, STARTUP_TIMEOUT)
            .map(|s| Box::new(s) as Box<dyn Session>)
    }
}

impl Codex {
    fn start_with_timeout(
        &self,
        ws: &Path,
        home: &Path,
        prompt: &str,
        model: &str,
        effort: Effort,
        timeout: Duration,
    ) -> Result<
        CodexSession<BufReader<std::process::ChildStdout>, std::process::ChildStdin>,
        ProviderError,
    > {
        let mut cmd = command(&self.bin, home, &self.run_dir, ws);
        if !self.toolchain_installs {
            cmd.env("RUSTUP_AUTO_INSTALL", "0");
        }
        cmd.stderr(Stdio::piped());
        let (mut child, rpc) = connect(cmd)?;
        if let Some(err) = child.stderr.take() {
            self.stderr.tee(err);
        }
        let (ws, prompt, model) = (ws.to_owned(), prompt.to_owned(), model.to_owned());
        let (child, mut session) = startup(child, timeout, move || {
            let mut session = CodexSession::over(rpc, &model);
            session.begin(&ws, &prompt, &model, effort)?;
            Ok(session)
        })?;
        session.child = Some(child);
        Ok(session)
    }
}

pub struct CodexSession<R: BufRead, W: Write> {
    child: Option<KillOnDrop>,
    rpc: Rpc<R, W>,
    mapper: Mapper,
    queue: VecDeque<RunItem>,
    ended: bool,
}

impl<R: BufRead, W: Write> CodexSession<R, W> {
    /// A session over any transport (the tests replay a recorded transcript).
    pub fn over(rpc: Rpc<R, W>, pin: &str) -> Self {
        Self {
            child: None,
            rpc,
            mapper: Mapper::new(pin),
            queue: VecDeque::new(),
            ended: false,
        }
    }

    /// initialize, window read, thread/start, turn/start.
    pub fn begin(
        &mut self,
        cwd: &Path,
        prompt: &str,
        model: &str,
        effort: Effort,
    ) -> Result<(), ProviderError> {
        self.rpc.initialize()?;
        // The window is free to read; a failure here (no login yet) is not fatal.
        match self
            .rpc
            .request::<RateLimits>("account/rateLimits/read", &Value::Null)
        {
            Ok(r) => self.queue.extend(rate_limit_item(r.rate_limits)),
            Err(e @ ProviderError::Parse(_)) => return Err(e),
            Err(ProviderError::Other(_)) => {}
        }
        let t: ThreadStarted = self.rpc.request(
            "thread/start",
            &json!({
                "model": model,
                "cwd": cwd,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access",
            }),
        )?;
        let thread = t.thread.id;
        self.rpc.request::<Value>(
            "turn/start",
            &json!({
                "threadId": thread,
                "effort": effort,
                "input": [{ "type": "text", "text": prompt }],
            }),
        )?;
        // thread/start echoes any model, so nothing is confirmed yet.
        self.queue
            .push_back(RunItem::ModelServed(ServedModel::Unconfirmed));
        Ok(())
    }
}

impl<R: BufRead, W: Write> Session for CodexSession<R, W> {
    fn next(&mut self) -> Option<RunItem> {
        loop {
            while let Some(m) = self.rpc.pending.pop_front() {
                match self.mapper.notification(m) {
                    Ok(items) => self.queue.extend(items),
                    Err(e) => {
                        // Usage seen so far is already in the queue ahead of this.
                        self.queue.push_back(RunItem::Error(e));
                        self.rpc.pending.clear();
                    }
                }
            }
            if let Some(item) = self.queue.pop_front() {
                if matches!(item, RunItem::Ended(_) | RunItem::Error(_)) {
                    self.ended = true;
                    self.stop_child();
                }
                return Some(item);
            }
            if self.ended {
                return None;
            }
            match self.rpc.read() {
                Ok(Some(RpcMessage::Notification(m))) => self.rpc.pending.push_back(m),
                Ok(Some(RpcMessage::Response { .. } | RpcMessage::Request { .. })) => {}
                Ok(None) => self.queue.push_back(RunItem::Error(ProviderError::Other(
                    "codex app-server closed before the turn ended".into(),
                ))),
                Err(e) => self.queue.push_back(RunItem::Error(e)),
            }
        }
    }

    fn stop(&mut self) {
        self.queue.clear();
        self.rpc.pending.clear();
        self.ended = true;
        self.stop_child();
    }
}

impl<R: BufRead, W: Write> CodexSession<R, W> {
    fn stop_child(&mut self) {
        self.child.take();
    }
}

/// A malformed recognized message from the vendor.
fn bad(what: impl Into<String>) -> ProviderError {
    ProviderError::Parse(what.into())
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|e| bad(e.to_string()))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RpcMessage {
    Request {
        #[serde(deserialize_with = "Value::deserialize")]
        id: Value,
        #[serde(rename = "method")]
        _method: String,
    },
    Response {
        #[serde(deserialize_with = "Value::deserialize")]
        id: Value,
        #[serde(flatten)]
        reply: RpcReply,
    },
    Notification(Notification),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RpcReply {
    Error {
        #[serde(deserialize_with = "Value::deserialize")]
        error: Value,
    },
    Result {
        #[serde(deserialize_with = "Value::deserialize")]
        result: Value,
    },
}

#[derive(Deserialize)]
struct ThreadStarted {
    thread: Thread,
}
#[derive(Deserialize)]
struct Thread {
    id: String,
}

// Keep notification payloads undecoded until the session drains them, so paid
// usage/output queued during startup survives a later malformed notification.
#[derive(Deserialize)]
#[serde(tag = "method")]
enum Notification {
    #[serde(rename = "item/completed")]
    Item {
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "thread/tokenUsage/updated")]
    Usage {
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "account/rateLimits/updated")]
    RateLimits {
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "model/rerouted")]
    Rerouted {
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "turn/completed")]
    Completed {
        #[serde(default)]
        params: Value,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct ItemCompleted {
    item: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageUpdated {
    token_usage: TokenUsage,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rerouted {
    to_model: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorNotification {
    #[serde(default)]
    will_retry: bool,
    error: Value,
}
#[derive(Deserialize)]
struct TurnCompleted {
    turn: Turn,
}

#[derive(Deserialize)]
struct TokenUsage {
    total: TokenTotal,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenTotal {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}
#[derive(Deserialize)]
#[serde(tag = "type")]
enum Item {
    #[serde(rename = "agentMessage")]
    Message { text: String },
    #[serde(rename = "commandExecution")]
    Command {
        #[serde(rename = "command")]
        _command: String,
    },
    #[serde(rename = "fileChange")]
    FileChange,
    #[serde(rename = "mcpToolCall")]
    Mcp {
        #[serde(default, deserialize_with = "crate::stream::optional")]
        tool: Option<String>,
    },
    #[serde(rename = "dynamicToolCall")]
    Dynamic {
        #[serde(default, deserialize_with = "crate::stream::optional")]
        tool: Option<String>,
    },
    #[serde(
        rename = "userMessage",
        alias = "hookPrompt",
        alias = "reasoning",
        alias = "plan"
    )]
    Ignored,
    #[serde(other)]
    Other,
}
#[derive(Deserialize)]
struct ItemName {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexError {
    message: String,
    codex_error_info: Option<Value>,
}
#[derive(Deserialize)]
struct Turn {
    status: String,
    #[serde(default, deserialize_with = "crate::stream::optional")]
    error: Option<TurnError>,
}
#[derive(Deserialize)]
struct TurnError {
    #[serde(default, deserialize_with = "crate::stream::optional")]
    message: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimits {
    rate_limits: Option<RateLimitSnapshot>,
}
#[derive(Deserialize)]
struct RateLimitSnapshot {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitWindow {
    used_percent: f64,
    #[serde(default, deserialize_with = "crate::stream::optional")]
    window_duration_mins: Option<i64>,
    #[serde(default, deserialize_with = "crate::stream::optional")]
    resets_at: Option<i64>,
}

/// The highest-used window of a `RateLimitSnapshot`.
fn rate_limit_item(snapshot: Option<RateLimitSnapshot>) -> Option<RunItem> {
    let snap = snapshot?;
    let mut best: Option<(f64, RunItem)> = None;
    for w in [snap.primary, snap.secondary].into_iter().flatten() {
        let used = w.used_percent;
        let item = RunItem::RateLimit {
            used_pct: used,
            window: window_name(w.window_duration_mins),
            resets_at: w.resets_at.map(|s| Timestamp(s.saturating_mul(1000))),
        };
        if best.as_ref().is_none_or(|(u, _)| used > *u) {
            best = Some((used, item));
        }
    }
    best.map(|(_, item)| item)
}

fn window_name(mins: Option<i64>) -> String {
    match mins {
        Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => "?".into(),
    }
}

/// Maps notifications to run items. Recognised notifications must have their
/// schema shape (a wrong one is a parse failure); unknown methods are ignored.
#[derive(Debug)]
pub struct Mapper {
    pin: String,
    /// The last cumulative `total` of `thread/tokenUsage/updated`: (input, cached, output).
    total: crate::usage::Tokens,
}

impl Mapper {
    pub fn new(pin: &str) -> Self {
        Self {
            pin: pin.to_owned(),
            total: crate::usage::Tokens::default(),
        }
    }

    #[cfg(test)]
    fn map(&mut self, message: &Value) -> Result<Vec<RunItem>, ProviderError> {
        self.notification(decode(message.clone())?)
    }

    fn notification(&mut self, notification: Notification) -> Result<Vec<RunItem>, ProviderError> {
        match notification {
            Notification::Item { params } => Self::item(decode::<ItemCompleted>(params)?.item),
            Notification::Usage { params } => {
                let t = decode::<UsageUpdated>(params)?.token_usage.total;
                // Codex includes cached tokens in input, unlike Claude.
                let now = crate::usage::Tokens {
                    uncached_in: t.input_tokens.saturating_sub(t.cached_input_tokens),
                    cached_in: t.cached_input_tokens,
                    out: t.output_tokens,
                };
                let before = std::mem::replace(&mut self.total, now);
                Ok(now.delta(before).item().into_iter().collect())
            }
            Notification::RateLimits { params } => {
                Ok(rate_limit_item(decode::<RateLimits>(params)?.rate_limits)
                    .into_iter()
                    .collect())
            }
            Notification::Rerouted { params } => {
                let to_model = decode::<Rerouted>(params)?.to_model;
                Ok(vec![RunItem::ModelServed(
                    if crate::stream::pin_matches(&self.pin, &to_model) {
                        ServedModel::Confirmed(to_model)
                    } else {
                        ServedModel::Mismatch(to_model)
                    },
                )])
            }
            Notification::Error { params } => {
                let params: ErrorNotification = decode(params)?;
                if params.will_retry {
                    return Ok(Vec::new());
                }
                let error: CodexError = decode(params.error)?;
                let mut items = Vec::new();
                if error.codex_error_info == Some(json!("usageLimitExceeded")) {
                    items.push(RunItem::RateLimit {
                        used_pct: 100.0,
                        window: "?".into(),
                        resets_at: None,
                    });
                }
                items.push(RunItem::Error(ProviderError::Other(format!(
                    "codex: {}",
                    clip(&error.message, 300)
                ))));
                Ok(items)
            }
            Notification::Completed { params } => {
                let turn = decode::<TurnCompleted>(params)?.turn;
                Ok(vec![if turn.status == "completed" {
                    RunItem::Ended(ExitStatus { code: Some(0) })
                } else {
                    RunItem::Error(ProviderError::Other(format!(
                        "codex: turn {}: {}",
                        turn.status,
                        clip(
                            turn.error.and_then(|e| e.message).as_deref().unwrap_or(""),
                            300
                        )
                    )))
                }])
            }
            Notification::Ignored => Ok(Vec::new()),
        }
    }

    fn item(value: Value) -> Result<Vec<RunItem>, ProviderError> {
        let name = match decode::<Item>(value.clone())? {
            Item::Message { text } => {
                return Ok(if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![RunItem::Text(text)]
                });
            }
            Item::Command { .. } => "command".into(),
            Item::FileChange => "file_change".into(),
            Item::Mcp { tool } => tool.unwrap_or_else(|| "mcpToolCall".into()),
            Item::Dynamic { tool } => tool.unwrap_or_else(|| "dynamicToolCall".into()),
            Item::Ignored => return Ok(Vec::new()),
            Item::Other => decode::<ItemName>(value)?.kind,
        };
        Ok(vec![RunItem::ToolCall { name }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const OK: &str = include_str!("../tests/fixtures/codex-ok.jsonl");
    const REROUTED: &str = include_str!("../tests/fixtures/codex-rerouted.jsonl");
    const LIMITED: &str = include_str!("../tests/fixtures/codex-limited.jsonl");

    #[test]
    fn app_server_disables_account_connectors() {
        let cmd = command(
            Path::new("codex"),
            Path::new("/home"),
            Path::new("/run"),
            Path::new("/ws"),
        );
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["app-server", "--disable", "apps"]
        );
    }

    /// Plays a recorded server transcript through a session; also returns what
    /// the client sent.
    fn play(transcript: &str, pin: &str) -> (Vec<RunItem>, Vec<Value>) {
        let mut sent = Vec::new();
        let items = {
            let rpc = Rpc::new(Cursor::new(transcript.to_owned()), &mut sent);
            let mut s = CodexSession::over(rpc, pin);
            s.begin(Path::new("/ws"), "do it", pin, Effort::High)
                .unwrap();
            let mut items = Vec::new();
            while let Some(i) = s.next() {
                items.push(i);
            }
            items
        };
        let sent = String::from_utf8(sent)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (items, sent)
    }

    #[test]
    fn a_recorded_turn_gives_window_text_tools_usage_and_unconfirmed_model() {
        let (items, sent) = play(OK, "gpt-6.1-sol");
        assert_eq!(
            items,
            vec![
                RunItem::RateLimit {
                    used_pct: 67.0,
                    window: "7d".into(),
                    resets_at: Some(Timestamp(1_791_046_706_000)),
                },
                RunItem::ModelServed(ServedModel::Unconfirmed),
                RunItem::ToolCall {
                    name: "command".into(),
                },
                RunItem::Text("42".into()),
                RunItem::Usage {
                    uncached_in: 700,
                    cached_in: 300,
                    out: 20
                },
                RunItem::RateLimit {
                    used_pct: 67.0,
                    window: "7d".into(),
                    resets_at: Some(Timestamp(1_791_046_706_000)),
                },
                RunItem::Ended(ExitStatus { code: Some(0) }),
            ]
        );
        let methods: Vec<&str> = sent.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "account/rateLimits/read",
                "thread/start",
                "turn/start"
            ]
        );
    }

    #[test]
    fn the_thread_is_pinned_and_unsandboxed() {
        let (_, sent) = play(OK, "gpt-6.1-sol");
        let t = sent.iter().find(|m| m["method"] == "thread/start").unwrap();
        assert_eq!(t["params"]["model"], "gpt-6.1-sol");
        assert_eq!(t["params"]["sandbox"], "danger-full-access");
        assert_eq!(t["params"]["approvalPolicy"], "never");
        let turn = sent.iter().find(|m| m["method"] == "turn/start").unwrap();
        assert_eq!(turn["params"]["threadId"], "th_1");
        assert_eq!(turn["params"]["effort"], "high");
        assert_eq!(turn["params"]["input"][0]["text"], "do it");
    }

    #[test]
    fn a_reroute_to_another_model_is_a_mismatch() {
        let (items, _) = play(REROUTED, "gpt-6.1-sol");
        assert!(items.contains(&RunItem::ModelServed(ServedModel::Mismatch(
            "gpt-6-luna".into()
        ))));
    }

    #[test]
    fn a_dated_reroute_to_the_pin_confirms_it() {
        let mut mapper = Mapper::new("gpt-5");
        let items = mapper
            .map(&serde_json::json!({
                "method":"model/rerouted",
                "params":{
                    "toModel":"gpt-5-20261001"
                }
            }))
            .unwrap();
        assert!(
            matches!(&items[0], RunItem::ModelServed(ServedModel::Confirmed(id)) if id == "gpt-5-20261001")
        );
    }

    #[test]
    fn a_reroute_to_the_pin_confirms_it() {
        let (items, _) = play(REROUTED, "gpt-6-luna");
        assert!(items.contains(&RunItem::ModelServed(ServedModel::Confirmed(
            "gpt-6-luna".into()
        ))));
    }

    #[test]
    fn a_usage_limit_error_is_a_full_window_and_an_error() {
        let (items, _) = play(LIMITED, "gpt-6.1-sol");
        assert!(
            items
                .iter()
                .any(|i| matches!(i, RunItem::RateLimit { used_pct, .. } if *used_pct >= 100.0))
        );
        assert!(
            matches!(items.last(), Some(RunItem::Error(ProviderError::Other(m))) if m.contains("usage limit"))
        );
    }

    #[test]
    fn usage_is_reported_as_it_arrives_so_a_fatal_error_keeps_it() {
        let (items, _) = play(
            include_str!("../tests/fixtures/codex-usage-then-error.jsonl"),
            "gpt-6.1-sol",
        );
        let used: Vec<&RunItem> = items
            .iter()
            .filter(|i| matches!(i, RunItem::Usage { .. }))
            .collect();
        // 1000 total with 300 cached, then 1500 with 500: deltas, not totals.
        assert_eq!(
            used,
            vec![
                &RunItem::Usage {
                    uncached_in: 700,
                    cached_in: 300,
                    out: 20
                },
                &RunItem::Usage {
                    uncached_in: 300,
                    cached_in: 200,
                    out: 30
                },
            ]
        );
        assert!(
            matches!(items.last(), Some(RunItem::Error(ProviderError::Other(m))) if m.contains("boom"))
        );
    }

    #[test]
    fn malformed_known_notifications_are_parse_failures() {
        for bad in [
            r#"{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":"x"}}}}"#,
            r#"{"method":"thread/tokenUsage/updated","params":{}}"#,
            r#"{"method":"turn/completed","params":{"turn":{}}}"#,
            r#"{"method":"turn/completed","params":{}}"#,
            r#"{"method":"item/completed","params":{"item":{"type":"agentMessage"}}}"#,
            r#"{"method":"item/completed","params":{}}"#,
            r#"{"method":"model/rerouted","params":{}}"#,
            r#"{"method":"error","params":{"error":{}}}"#,
            "{\"method\":\"account/rateLimits/updated\",\"params\":{\"rateLimits\":{\"primary\":{\
                \"usedPercent\":\"high\"}}}}",
        ] {
            let v: Value = serde_json::from_str(bad).unwrap();
            let e = Mapper::new("x").map(&v).unwrap_err();
            assert!(matches!(e, ProviderError::Parse(_)), "{bad}: {e}");
        }
        // Unknown methods and extra fields stay tolerated.
        let v: Value =
            serde_json::from_str(r#"{"method":"future/thing","params":{"x":1}}"#).unwrap();
        assert!(Mapper::new("x").map(&v).unwrap().is_empty());
    }

    #[test]
    fn optional_metadata_keeps_its_fallbacks() {
        let mut mapper = Mapper::new("pin");
        let items = mapper
            .map(&json!({
                "method": "account/rateLimits/updated",
                "params": { "rateLimits": { "primary": {
                    "usedPercent": 25, "windowDurationMins": "unknown", "resetsAt": "unknown"
                } } }
            }))
            .unwrap();
        assert_eq!(
            items,
            vec![RunItem::RateLimit {
                used_pct: 25.0,
                window: "?".into(),
                resets_at: None
            }]
        );
        assert_eq!(mapper.map(&json!({
            "method": "item/completed", "params": {"item": {"type": "mcpToolCall", "tool": 3}}
        })).unwrap(), vec![RunItem::ToolCall { name: "mcpToolCall".into() }]);
        assert_eq!(mapper.map(&json!({
            "method": "turn/completed", "params": {"turn": {"status": "failed", "error": false}}
        })).unwrap(), vec![RunItem::Error(ProviderError::Other("codex: turn failed: ".into()))]);
    }

    #[test]
    fn a_retrying_error_is_not_an_end() {
        let mut m = Mapper::new("x");
        let v: Value = serde_json::from_str(
            r#"{"method":"error","params":{"willRetry":true,"error":{"message":"reconnecting"}}}"#,
        )
        .unwrap();
        assert!(m.map(&v).unwrap().is_empty());
    }

    #[test]
    fn malformed_startup_responses_stay_parse_failures() {
        for (id, reply) in [
            (1, "not json"),
            (1, r#"{"id":1}"#),
            (2, "not json"),
            (
                2,
                r#"{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":"high"}}}}"#,
            ),
            (3, r#"{"id":3,"result":{"thread":{}}}"#),
            (4, "not json"),
        ] {
            let mut lines: Vec<_> = OK.lines().collect();
            let index = lines
                .iter()
                .position(|line| serde_json::from_str::<Value>(line).unwrap()["id"] == id)
                .unwrap();
            lines[index] = reply;
            let rpc = Rpc::new(Cursor::new(lines.join("\n")), Vec::new());
            let mut s = CodexSession::over(rpc, "pin");
            assert!(
                matches!(
                    s.begin(Path::new("/ws"), "x", "pin", Effort::Low),
                    Err(ProviderError::Parse(_))
                ),
                "{reply}"
            );
        }
        // An ordinary optional lookup failure still allows the run to start.
        let transcript = OK
            .lines()
            .map(|line| {
                if serde_json::from_str::<Value>(line).unwrap()["id"] == 2 {
                    r#"{"id":2,"error":{"message":"no login"}}"#
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (items, _) = play(&transcript, "pin");
        assert!(matches!(items.last(), Some(RunItem::Ended(_))));
    }

    #[test]
    fn usage_precedes_a_terminal_parse_failure() {
        for malformed in [
            "not json",
            r#"{"method":"turn/completed","params":{"turn":{}}}"#,
        ] {
            let mut lines: Vec<_> = OK.lines().collect();
            *lines.last_mut().unwrap() = malformed;
            let (items, _) = play(&lines.join("\n"), "gpt-6.1-sol");
            assert!(matches!(
                items.last(),
                Some(RunItem::Error(ProviderError::Parse(_)))
            ));
            assert!(items[..items.len() - 1].contains(&RunItem::Usage {
                uncached_in: 700,
                cached_in: 300,
                out: 20,
            }));
        }
    }

    #[test]
    fn queued_items_precede_missing_notification_params_during_startup() {
        for method in [
            "item/completed",
            "thread/tokenUsage/updated",
            "account/rateLimits/updated",
            "model/rerouted",
            "error",
            "turn/completed",
        ] {
            let mut lines: Vec<_> = OK.lines().map(str::to_owned).collect();
            let turn_start = lines
                .iter()
                .position(|line| serde_json::from_str::<Value>(line).unwrap()["id"] == 4)
                .unwrap();
            let text = lines
                .iter()
                .find(|line| line.contains("agentMessage"))
                .unwrap()
                .clone();
            let usage = lines
                .iter()
                .find(|line| line.contains("thread/tokenUsage/updated"))
                .unwrap()
                .clone();
            lines.splice(
                turn_start..turn_start,
                [
                    json!({ "method": "model/rerouted", "params": { "toModel": "pin" } })
                        .to_string(),
                    text,
                    usage,
                    json!({ "method": method }).to_string(),
                ],
            );
            let (items, _) = play(&lines.join("\n"), "pin");
            assert_eq!(
                &items[..items.len() - 1],
                &[
                    RunItem::RateLimit {
                        used_pct: 67.0,
                        window: "7d".into(),
                        resets_at: Some(Timestamp(1_791_046_706_000)),
                    },
                    RunItem::ModelServed(ServedModel::Unconfirmed),
                    RunItem::ModelServed(ServedModel::Confirmed("pin".into())),
                    RunItem::Text("42".into()),
                    RunItem::Usage {
                        uncached_in: 700,
                        cached_in: 300,
                        out: 20,
                    },
                ],
                "{method}"
            );
            assert!(
                matches!(items.last(), Some(RunItem::Error(ProviderError::Parse(_)))),
                "{method}: {items:?}"
            );
        }
    }

    #[test]
    fn an_approval_request_is_declined() {
        let line = r#"{"id":77,"method":"item/commandExecution/requestApproval","params":{}}"#;
        let mut sent = Vec::new();
        let mut rpc = Rpc::new(Cursor::new(format!("{line}\n")), &mut sent);
        assert!(rpc.read().unwrap().is_none());
        let reply: Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(reply["id"], 77);
        assert_eq!(reply["result"]["decision"], "decline");
    }

    /// Reply to the startup calls before `stall`, then leave a partial line
    /// open forever. No real provider or socket is used.
    fn startup_stub(dir: &Path, replies: &[Value], stall: bool) -> PathBuf {
        let stub = dir.join("app-server");
        let mut script = String::from("#!/bin/sh\necho $$ > pid\n");
        for (i, reply) in replies.iter().enumerate() {
            // initialize's response is followed by an initialized notification.
            if i == 1 {
                script.push_str("read line\n");
            }
            script.push_str(&format!("read line\nprintf '%s\\n' '{reply}'\n"));
        }
        if stall {
            script.push_str("read line\nprintf '{'\nexec sleep 30\n");
        } else {
            // The ordinary turn takes longer than the startup allowance.
            script.push_str("sleep 1\n\
                echo '{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"status\":\"completed\"}}}'\n");
        }
        crate::testutil::write_executable(&stub, &script);
        stub
    }

    fn assert_child_reaped(dir: &Path) {
        let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "child {pid} was not reaped"
        );
    }

    #[test]
    fn dropping_a_live_session_terminates_and_reaps_its_child() {
        let mut child = Command::new("sh")
            .args(["-c", "printf 'ready\\n'; read line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let mut session = CodexSession::over(Rpc::new(Cursor::new(Vec::new()), Vec::new()), "pin");
        session.child = Some(KillOnDrop(child));
        drop(session);
        let alive = Path::new("/proc").join(pid.to_string()).exists();
        if alive {
            Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status()
                .unwrap();
        }
        assert!(
            !alive,
            "the dropped session left its child running or unreaped"
        );
    }

    #[test]
    fn every_session_startup_rpc_has_a_deadline_and_reaps_the_child() {
        let replies = [
            json!({ "id": 1, "result": {} }),
            json!({ "id": 2, "result": {} }),
            json!({ "id": 3, "result": { "thread": { "id": "t" } } }),
        ];
        for answered in 0..=replies.len() {
            let dir = tempfile::tempdir().unwrap();
            let provider = Codex {
                bin: startup_stub(dir.path(), &replies[..answered], true),
                run_dir: dir.path().to_owned(),
                toolchain_installs: true,
                stderr: crate::stream::StderrTail::default(),
            };
            let result = provider.start_with_timeout(
                dir.path(),
                dir.path(),
                "x",
                "pin",
                Effort::Low,
                Duration::from_millis(500),
            );
            let err = result.err().expect("the stalled startup must fail");
            assert!(
                matches!(&err, ProviderError::Other(m) if m.contains("startup timed out")),
                "{err}"
            );
            assert_child_reaped(dir.path());
        }
    }

    #[test]
    fn model_discovery_has_a_deadline_and_reaps_the_child() {
        for replies in [vec![], vec![json!({ "id": 1, "result": {} })]] {
            let dir = tempfile::tempdir().unwrap();
            let bin = startup_stub(dir.path(), &replies, true);
            let err =
                list_models_with_timeout(&bin, dir.path(), dir.path(), Duration::from_millis(500))
                    .unwrap_err();
            assert!(
                matches!(&err, ProviderError::Other(m) if m.contains("startup timed out")),
                "{err}"
            );
            assert_child_reaped(dir.path());
        }
    }

    #[test]
    fn a_model_turn_can_outlive_the_startup_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let replies = [
            json!({ "id": 1, "result": {} }),
            json!({ "id": 2, "result": {} }),
            json!({ "id": 3, "result": { "thread": { "id": "t" } } }),
            json!({ "id": 4, "result": {} }),
        ];
        let provider = Codex {
            bin: startup_stub(dir.path(), &replies, false),
            run_dir: dir.path().to_owned(),
            toolchain_installs: true,
            stderr: crate::stream::StderrTail::default(),
        };
        let mut session = provider
            .start_with_timeout(
                dir.path(),
                dir.path(),
                "x",
                "pin",
                Effort::Low,
                Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(
            session.next(),
            Some(RunItem::ModelServed(ServedModel::Unconfirmed))
        );
        assert!(matches!(
            session.next(),
            Some(RunItem::Ended(ExitStatus { code: Some(0) }))
        ));
        assert_eq!(session.next(), None);
        assert_child_reaped(dir.path());
    }
}
