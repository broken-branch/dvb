//! The Claude provider: `claude -p --output-format stream-json`, one
//! JSON object per line. The parser is versioned against the CLI recorded in
//! `RECORDED_VERSION`; a line that is not JSON is a coded parse failure,
//! not a silent loss.
//!
//! Events used: `assistant` (text, `tool_use`, `message.model`, `message.usage`),
//! `rate_limit_event` (the vendor window) and `result` (totals, errors). Others
//! (`system`, `user`, hook and stream events) are ignored.

use crate::stream::{Pipe, pin_matches};
use dvb_core::Effort;
use dvb_core::Timestamp;
use dvb_core::event::ServedModel;
use dvb_core::provider::{ExitStatus, Provider, ProviderError, RunItem, Session};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The CLI version the parser and these fixtures were recorded against.
pub const RECORDED_VERSION: &str = "2.1.286";

/// The run-level settings file `prepare` writes (sandbox off, pinned model and
/// memory off).
pub const SETTINGS_FILE: &str = "claude-settings.json";

pub fn settings_json() -> Value {
    serde_json::json!({
        "sandbox": { "enabled": false },
        "autoMemoryEnabled": false,
        "switchModelsOnFlag": false
    })
}

#[derive(Debug, Clone)]
pub struct Claude {
    /// The `claude` executable.
    pub bin: PathBuf,
    /// The run directory: holds `claude-settings.json` and `bin/dvb`.
    pub run_dir: PathBuf,
    /// Whether the session's cargo and rustc may install a missing toolchain:
    /// not while another run of the project may still be installing it into
    /// their shared RUSTUP_HOME, which rustup does not guard.
    pub toolchain_installs: bool,
}

impl Provider for Claude {
    fn start(
        &self,
        ws: &Path,
        home: &Path,
        prompt: &str,
        model: &str,
        effort: Effort,
    ) -> Result<Box<dyn Session>, ProviderError> {
        let effort = effort.as_str();
        let mut cmd = Command::new(&self.bin);
        // No `--fallback-model`, and the settings turn `switchModelsOnFlag` off:
        // the pinned model is the only one asked for.
        cmd.args(["-p", "--output-format", "stream-json", "--verbose"])
            .args(["--model", model, "--effort", effort])
            .args(["--permission-mode", "bypassPermissions"])
            .arg("--settings")
            .arg(self.run_dir.join(SETTINGS_FILE))
            .arg("--no-session-persistence")
            .current_dir(ws)
            .env("HOME", home)
            .env("PATH", crate::stream::path_with(&self.run_dir.join("bin")));
        if !self.toolchain_installs {
            cmd.env("RUSTUP_AUTO_INSTALL", "0");
        }
        let pipe = Pipe::spawn(cmd, prompt.as_bytes().to_vec())
            .map_err(|e| ProviderError::Other(format!("{}: {e}", self.bin.display())))?;
        Ok(Box::new(ClaudeSession {
            pipe,
            parser: Parser::new(model),
            queue: VecDeque::new(),
            ended: false,
        }))
    }
}

struct ClaudeSession {
    pipe: Pipe,
    parser: Parser,
    queue: VecDeque<RunItem>,
    ended: bool,
}

impl Session for ClaudeSession {
    fn next(&mut self) -> Option<RunItem> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                if matches!(item, RunItem::Ended(_) | RunItem::Error(_)) {
                    self.ended = true;
                }
                return Some(item);
            }
            if self.ended {
                return None;
            }
            match self.pipe.line() {
                Ok(Some(line)) => {
                    let mut items = Vec::new();
                    let parsed = self.parser.feed_into(&line, &mut items);
                    self.queue.extend(items);
                    if let Err(e) = parsed {
                        self.pipe.kill();
                        self.queue
                            .push_back(RunItem::Error(ProviderError::Parse(e)));
                    }
                }
                Ok(None) => match self.pipe.wait() {
                    Ok(code) => self.queue.extend(self.parser.finish(code)),
                    Err(error) => {
                        self.queue
                            .push_back(RunItem::Error(ProviderError::Other(format!(
                                "claude wait: {error}"
                            ))))
                    }
                },
                Err(e) => {
                    self.pipe.kill();
                    let what = format!("claude output: {e}");
                    // Output that is not UTF-8 is malformed, like a non-JSON line.
                    self.queue.push_back(RunItem::Error(
                        if e.kind() == std::io::ErrorKind::InvalidData {
                            ProviderError::Parse(what)
                        } else {
                            ProviderError::Other(what)
                        },
                    ));
                }
            }
        }
    }

    fn stop(&mut self) {
        self.pipe.kill();
        self.queue.clear();
        self.ended = true;
    }
}

use crate::usage::Tokens;

/// Turns the stream's lines into run items. Recognised events must have the
/// shape this parser was recorded against (a wrong shape is a parse failure);
/// unknown event kinds and optional fields are tolerated.
#[derive(Debug)]
pub struct Parser {
    pin: String,
    confirmed: bool,
    /// Tokens already reported per message id (a message repeats its usage per block).
    by_message: BTreeMap<String, Tokens>,
    /// Everything reported so far.
    reported: Tokens,
    result: Option<ResultSummary>,
}

#[derive(Debug)]
struct ResultSummary {
    is_error: bool,
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "assistant")]
    Assistant {
        message: Message,
        #[serde(default)]
        parent_tool_use_id: Option<Value>,
    },
    #[serde(rename = "rate_limit_event")]
    RateLimit { rate_limit_info: RateLimit },
    #[serde(rename = "result")]
    Result {
        is_error: bool,
        result: Option<String>,
        #[serde(default, deserialize_with = "present")]
        usage: Option<Usage>,
    },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    model: String,
    #[serde(default, deserialize_with = "present")]
    usage: Option<Usage>,
    // Decode content after accounting for the paid message and model evidence.
    #[serde(default)]
    content: Value,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { name: String },
    #[serde(other)]
    Ignored,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl Usage {
    fn tokens(self) -> Tokens {
        Tokens {
            uncached_in: self.input_tokens + self.cache_creation_input_tokens,
            cached_in: self.cache_read_input_tokens,
            out: self.output_tokens,
        }
    }
}

// An absent usage is optional; an explicit null is malformed vendor data.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct RateLimit {
    status: String,
    utilization: Option<f64>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(
        default,
        rename = "rateLimitType",
        deserialize_with = "crate::stream::optional"
    )]
    kind: Option<String>,
}

fn window_name(kind: &str) -> String {
    match kind {
        "five_hour" => "5h".into(),
        "seven_day" | "seven_day_opus" | "seven_day_sonnet" => "7d".into(),
        other => other.into(),
    }
}

impl Parser {
    pub fn new(pin: &str) -> Self {
        Self {
            pin: pin.to_owned(),
            confirmed: false,
            by_message: BTreeMap::new(),
            reported: Tokens::default(),
            result: None,
        }
    }

    /// Keep items parsed before an error, especially paid usage on a malformed
    /// assistant message. The session queues them before the terminal error.
    fn feed_into(&mut self, line: &str, items: &mut Vec<RunItem>) -> Result<(), String> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let event: Event = serde_json::from_str(line)
            .map_err(|e| format!("claude event ({e}): {}", dvb_core::text::clip(line, 120)))?;
        match event {
            Event::Assistant {
                message,
                parent_tool_use_id,
            } => {
                self.assistant(message, parent_tool_use_id.is_none(), items)?;
            }
            Event::RateLimit { rate_limit_info } => Self::rate_limit(&rate_limit_info, items),
            Event::Result {
                is_error,
                result,
                usage,
            } => {
                if let Some(usage) = usage {
                    let total = usage.tokens();
                    items.extend(total.delta(self.reported).item());
                    self.reported = Tokens {
                        uncached_in: self.reported.uncached_in.max(total.uncached_in),
                        cached_in: self.reported.cached_in.max(total.cached_in),
                        out: self.reported.out.max(total.out),
                    };
                }
                self.result = Some(ResultSummary {
                    is_error,
                    text: result.unwrap_or_default(),
                });
            }
            Event::Ignored => {}
        }
        Ok(())
    }

    fn assistant(
        &mut self,
        msg: Message,
        top: bool,
        items: &mut Vec<RunItem>,
    ) -> Result<(), String> {
        if let Some(usage) = msg.usage {
            let now = usage.tokens();
            let before = self.by_message.insert(msg.id, now).unwrap_or_default();
            let d = now.delta(before);
            self.reported.uncached_in += d.uncached_in;
            self.reported.cached_in += d.cached_in;
            self.reported.out += d.out;
            items.extend(d.item());
        }
        if top && msg.model != "<synthetic>" {
            if !pin_matches(&self.pin, &msg.model) {
                items.push(RunItem::ModelServed(ServedModel::Mismatch(msg.model)));
            } else if !self.confirmed {
                self.confirmed = true;
                items.push(RunItem::ModelServed(ServedModel::Confirmed(
                    self.pin.clone(),
                )));
            }
        }
        let blocks: Vec<Value> = serde_json::from_value(msg.content).map_err(|e| e.to_string())?;
        for block in blocks {
            match serde_json::from_value::<Block>(block).map_err(|e| e.to_string())? {
                Block::Text { text } => {
                    if !text.trim().is_empty() {
                        items.push(RunItem::Text(text));
                    }
                }
                Block::ToolUse { name } => items.push(RunItem::ToolCall { name }),
                Block::Ignored => {}
            }
        }
        Ok(())
    }

    fn rate_limit(info: &RateLimit, items: &mut Vec<RunItem>) {
        let rejected = info.status.starts_with("rejected");
        let used = info
            .utilization
            .map(|f| if f <= 1.0 { f * 100.0 } else { f });
        let Some(used_pct) = used.or(rejected.then_some(100.0)) else {
            return;
        };
        items.push(RunItem::RateLimit {
            used_pct,
            window: window_name(info.kind.as_deref().unwrap_or("5h")),
            resets_at: info.resets_at.map(|s| Timestamp(s.saturating_mul(1000))),
        });
    }

    /// The stream ended: how the run ended.
    pub fn finish(&mut self, exit: Option<i32>) -> Vec<RunItem> {
        vec![match &self.result {
            Some(r) if r.is_error => RunItem::Error(ProviderError::Other(format!(
                "claude: {}",
                dvb_core::text::clip(&r.text, 300)
            ))),
            // A signal leaves no code, which is not a success.
            Some(_) => RunItem::Ended(ExitStatus { code: exit }),
            None => RunItem::Error(ProviderError::Parse(format!(
                "the stream ended without a result (exit {exit:?})"
            ))),
        }]
    }
}

#[cfg(test)]
mod tests {
    impl Parser {
        /// One line of the stream.
        fn feed(&mut self, line: &str) -> Result<Vec<RunItem>, String> {
            let mut items = Vec::new();
            self.feed_into(line, &mut items)?;
            Ok(items)
        }
    }

    use super::*;

    const OK: &str = include_str!("../tests/fixtures/claude-ok.jsonl");
    const LIMITED: &str = include_str!("../tests/fixtures/claude-limited.jsonl");
    const SUBSTITUTED: &str = include_str!("../tests/fixtures/claude-substituted.jsonl");

    fn play(stream: &str, pin: &str) -> Vec<RunItem> {
        let mut p = Parser::new(pin);
        let mut items = Vec::new();
        for l in stream.lines() {
            items.extend(p.feed(l).unwrap());
        }
        items.extend(p.finish(Some(0)));
        items
    }

    fn usage_sum(items: &[RunItem]) -> (u64, u64, u64) {
        items.iter().fold((0, 0, 0), |a, i| match i {
            RunItem::Usage {
                uncached_in,
                cached_in,
                out,
            } => (a.0 + uncached_in, a.1 + cached_in, a.2 + out),
            RunItem::ModelServed(_)
            | RunItem::Text(_)
            | RunItem::ToolCall { .. }
            | RunItem::RateLimit { .. }
            | RunItem::Ended(_)
            | RunItem::Error(_) => a,
        })
    }

    #[test]
    fn a_recorded_run_gives_text_tools_model_usage_and_window() {
        let items = play(OK, "claude-haiku-4-5");
        let rest: Vec<&RunItem> = items
            .iter()
            .filter(|i| !matches!(i, RunItem::Usage { .. }))
            .collect();
        assert_eq!(
            rest,
            vec![
                &RunItem::RateLimit {
                    used_pct: 41.0,
                    window: "5h".into(),
                    resets_at: Some(Timestamp(1_791_046_706_000)),
                },
                &RunItem::ModelServed(ServedModel::Confirmed("claude-haiku-4-5".into())),
                &RunItem::Text("I will list the files.".into()),
                &RunItem::ToolCall {
                    name: "Bash".into()
                },
                &RunItem::Text("Done: 42".into()),
                &RunItem::Ended(ExitStatus { code: Some(0) }),
            ]
        );
        // Reported as it arrived, once: the messages' totals equal the result's.
        assert_eq!(usage_sum(&items), (130, 4000, 25));
    }

    #[test]
    fn a_later_message_on_another_model_is_a_mismatch() {
        let mut p = Parser::new("claude-sonnet-5-5");
        let mut items = Vec::new();
        let mut stopped_at = None;
        for (n, l) in include_str!("../tests/fixtures/claude-substituted-later.jsonl")
            .lines()
            .enumerate()
        {
            let got = p.feed(l).unwrap();
            if got
                .iter()
                .any(|i| matches!(i, RunItem::ModelServed(ServedModel::Mismatch(_))))
            {
                stopped_at = Some(n);
            }
            items.extend(got);
        }
        assert_eq!(stopped_at, Some(2), "reported on the second message");
        assert!(items.contains(&RunItem::ModelServed(ServedModel::Confirmed(
            "claude-sonnet-5-5".into()
        ))));
    }

    #[test]
    fn a_bare_result_or_a_wrongly_shaped_known_event_is_a_parse_failure() {
        for bad in [
            r#"{"type":"result"}"#,
            r#"{"type":"result","is_error":"no"}"#,
            r#"{"type":"result","is_error":false,"usage":{"input_tokens":"x","output_tokens":1}}"#,
            r#"{"type":"assistant"}"#,
            r#"{"type":"assistant","message":{"id":"m","model":"x","content":"text"}}"#,
            r#"{"type":"assistant","message":{"id":"m","model":"x","content":[{"type":"text"}]}}"#,
            r#"{"type":"assistant","message":{"id":"m","model":"x","content":[],"usage":{"input_tokens":1}}}"#,
            r#"{"type":"rate_limit_event"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","utilization":"high"}}"#,
        ] {
            assert!(Parser::new("x").feed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn another_served_model_is_a_mismatch() {
        let items = play(SUBSTITUTED, "claude-sonnet-5-5");
        assert!(items.contains(&RunItem::ModelServed(ServedModel::Mismatch(
            "claude-haiku-4-5-20251001".into()
        ))));
    }

    #[test]
    fn a_rejected_window_is_a_full_window_and_the_error_is_reported() {
        let items = play(LIMITED, "claude-haiku-4-5");
        assert!(items.contains(&RunItem::RateLimit {
            used_pct: 100.0,
            window: "5h".into(),
            resets_at: Some(Timestamp(1_791_046_706_000)),
        }));
        assert!(
            matches!(items.last(), Some(RunItem::Error(ProviderError::Other(m))) if m.contains("usage limit"))
        );
    }

    #[test]
    fn a_stream_with_no_result_is_a_parse_failure() {
        let mut p = Parser::new("m");
        let items = p.finish(Some(0));
        assert!(matches!(
            &items[..],
            [RunItem::Error(ProviderError::Parse(_))]
        ));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let mut p = Parser::new("m");
        assert!(
            p.feed(r#"{"type":"future_thing","x":1}"#)
                .unwrap()
                .is_empty()
        );
        assert!(p.feed("").unwrap().is_empty());
    }

    #[test]
    fn the_process_driver_passes_the_pin_and_the_prompt_and_reads_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("stream.jsonl");
        std::fs::write(&fixture, OK).unwrap();
        let stub = dir.path().join("claude");
        crate::testutil::write_executable(&stub, include_str!("../tests/fixtures/claude-cli.sh"));
        let p = Claude {
            bin: stub,
            run_dir: dir.path().to_owned(),
            toolchain_installs: false,
        };
        let mut s = p
            .start(
                dir.path(),
                dir.path(),
                "write the thing",
                "claude-haiku-4-5",
                Effort::High,
            )
            .unwrap();
        let mut items = Vec::new();
        while let Some(i) = s.next() {
            items.push(i);
        }
        assert!(items.contains(&RunItem::Text("Done: 42".into())));
        assert_eq!(
            items.last(),
            Some(&RunItem::Ended(ExitStatus { code: Some(0) }))
        );
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(args.contains("--model claude-haiku-4-5"), "{args}");
        assert!(args.contains("--effort high"), "{args}");
        assert!(
            args.contains("--permission-mode bypassPermissions"),
            "{args}"
        );
        assert!(!args.contains("fallback"), "{args}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("rustup-auto-install")).unwrap(),
            "0"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("stdin")).unwrap(),
            "write the thing"
        );
    }

    #[test]
    fn a_process_that_prints_garbage_is_a_parse_failure_and_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude");
        crate::testutil::write_executable(
            &stub,
            "#!/bin/sh\necho $$ > pid\necho 'not json'\nexec sleep 30\n",
        );
        let p = Claude {
            bin: stub,
            run_dir: dir.path().to_owned(),
            toolchain_installs: true,
        };
        let (send, recv) = std::sync::mpsc::channel();
        let ws = dir.path().to_owned();
        let worker = std::thread::spawn(move || {
            let mut s = p
                .start(&ws, &ws, "x", "claude-haiku-4-5", Effort::Low)
                .unwrap();
            let first = s.next();
            let end = s.next();
            let pid = std::fs::read_to_string(ws.join("pid")).unwrap();
            // Check before explicit cleanup: next() must have reaped the child.
            let reaped = !Path::new("/proc").join(pid.trim()).exists();
            s.stop();
            let _ = send.send((first, end, reaped));
        });
        let result = recv.recv_timeout(std::time::Duration::from_secs(5));
        if result.is_err() {
            // Unblock the reader even if parsing regresses, then let it reap.
            if let Ok(pid) = std::fs::read_to_string(dir.path().join("pid")) {
                let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
            }
            let _ = recv.recv_timeout(std::time::Duration::from_secs(5));
        }
        let (first, end, reaped) = result.expect("garbage must fail within five seconds");
        worker.join().unwrap();
        assert!(matches!(
            first,
            Some(RunItem::Error(ProviderError::Parse(_)))
        ));
        assert_eq!(end, None);
        assert!(reaped, "the malformed-output child was not reaped");
    }

    #[test]
    fn malformed_assistant_content_reports_its_usage_before_the_terminal_error() {
        for content in [
            serde_json::Value::Null,
            serde_json::json!([{ "type": "text" }]),
            serde_json::json!([{ "type": "tool_use", "name": 42 }]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let stub = dir.path().join("claude");
            let message = serde_json::json!({
                "type": "assistant",
                "message": {
                    "id": "paid", "model": "pin", "content": content,
                    "usage": { "input_tokens": 11, "cache_read_input_tokens": 7, "output_tokens": 3 }
                }
            });
            crate::testutil::write_executable(
                &stub,
                &format!("#!/bin/sh\nprintf '%s\\n' '{message}'\nexec sleep 30\n"),
            );
            let command = Command::new(stub);
            let mut session = ClaudeSession {
                pipe: Pipe::spawn(command, Vec::new()).unwrap(),
                parser: Parser::new("pin"),
                queue: VecDeque::new(),
                ended: false,
            };
            assert_eq!(
                session.next(),
                Some(RunItem::Usage {
                    uncached_in: 11,
                    cached_in: 7,
                    out: 3
                })
            );
            assert!(matches!(
                session.next(),
                Some(RunItem::ModelServed(ServedModel::Confirmed(_)))
            ));
            assert!(matches!(
                session.next(),
                Some(RunItem::Error(ProviderError::Parse(_)))
            ));
            assert_eq!(session.next(), None);
        }
    }

    /// Every item a stub CLI's session gives, to its end.
    fn session_items(script: &str) -> Vec<RunItem> {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude");
        crate::testutil::write_executable(&stub, &format!("#!/bin/sh\n{script}"));
        let command = Command::new(stub);
        let mut session = ClaudeSession {
            pipe: Pipe::spawn(command, Vec::new()).unwrap(),
            parser: Parser::new("pin"),
            queue: VecDeque::new(),
            ended: false,
        };
        std::iter::from_fn(|| session.next()).collect()
    }

    const RESULT: &str = r#"{"type":"result","is_error":false,"result":"done"}"#;

    #[test]
    fn a_result_followed_by_a_signal_is_not_a_success() {
        let items = session_items(&format!("printf '%s\\n' '{RESULT}'\nkill -TERM $$\n"));
        assert_eq!(items, vec![RunItem::Ended(ExitStatus { code: None })]);
    }

    #[test]
    fn output_that_is_not_utf8_is_a_parse_failure() {
        let items = session_items(&format!(
            "printf '%s\\n' '{RESULT}'\nprintf '\\377\\n'\nexec sleep 30\n"
        ));
        assert!(
            matches!(&items[..], [RunItem::Error(ProviderError::Parse(_))]),
            "{items:?}"
        );
    }

    #[test]
    fn the_settings_turn_the_cli_sandbox_and_memory_off() {
        let s = settings_json();
        assert_eq!(s["sandbox"]["enabled"], false);
        assert_eq!(s["autoMemoryEnabled"], false);
        assert_eq!(s["switchModelsOnFlag"], false);
    }
}
