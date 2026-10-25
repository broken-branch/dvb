//! The Fake provider: reads a JSON script and plays it
//! as a session. It makes no model call and no network call; it is the routine
//! test path.
//!
//! A script is a list of steps:
//! `{"write": {"path", "text"}}` writes a file in the workspace;
//! `{"run": ["<verb words>", {body}], "save": "<name>"?}` calls `dvb run <verb>`
//! like an agent CLI would, and may save the reply; later strings may use
//! `${<name>.<field>.<index>}` to read a saved reply;
//! `{"usage": {"uncached_in", "cached_in", "out"}}`; `{"sleep_ms": n}`;
//! `{"stderr": "text"}` writes text as the CLI's stderr; `{"exit": code}`.

use dvb_core::Effort;
use dvb_core::event::ServedModel;
use dvb_core::provider::{ExitStatus, Provider, ProviderError, RunItem, Session};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Write {
        path: String,
        text: String,
    },
    Run(String, Value),
    Usage {
        uncached_in: u64,
        cached_in: u64,
        out: u64,
    },
    SleepMs(u64),
    Stderr(String),
    Exit(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptStep {
    #[serde(flatten)]
    step: Step,
    #[serde(default)]
    save: Option<String>,
}

/// The Fake provider for one script file.
#[derive(Debug, Clone)]
pub struct Fake {
    pub script: PathBuf,
    /// The binary whose `run` verbs the script calls (`dvb`).
    pub dvb: PathBuf,
    /// Where the script's `stderr` steps go.
    pub stderr: crate::stream::StderrTail,
}

impl Provider for Fake {
    fn start(
        &self,
        ws: &Path,
        _home: &Path,
        _prompt: &str,
        model: &str,
        _effort: Effort,
    ) -> Result<Box<dyn Session>, ProviderError> {
        let text = std::fs::read_to_string(&self.script)
            .map_err(|e| ProviderError::Other(format!("{}: {e}", self.script.display())))?;
        let steps: Vec<ScriptStep> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Other(format!("{}: {e}", self.script.display())))?;
        let mut queue = VecDeque::new();
        queue.push_back(Pending::Item(RunItem::ModelServed(ServedModel::Confirmed(
            model.to_owned(),
        ))));
        queue.extend(steps.into_iter().map(Pending::Step));
        Ok(Box::new(FakeSession {
            ws: ws.to_owned(),
            dvb: self.dvb.clone(),
            stderr: self.stderr.clone(),
            queue,
            saved: BTreeMap::new(),
            ended: false,
        }))
    }
}

enum Pending {
    Item(RunItem),
    Step(ScriptStep),
}

struct FakeSession {
    ws: PathBuf,
    dvb: PathBuf,
    stderr: crate::stream::StderrTail,
    queue: VecDeque<Pending>,
    saved: BTreeMap<String, Value>,
    ended: bool,
}

impl FakeSession {
    fn play(&mut self, s: ScriptStep) -> RunItem {
        match s.step {
            Step::Write { path, text } => {
                let full = self.ws.join(&path);
                let written = full
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&full, text));
                match written {
                    Ok(()) => RunItem::ToolCall {
                        name: "write".into(),
                    },
                    Err(e) => RunItem::Error(ProviderError::Other(format!("write {path}: {e}"))),
                }
            }
            Step::Run(verb, body) => {
                let body = self.fill(body);
                let out = Command::new(&self.dvb)
                    .arg("run")
                    .args(verb.split_whitespace())
                    .arg(body.to_string())
                    .current_dir(&self.ws)
                    .output();
                match out {
                    Ok(out) => {
                        let reply = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                        if let (Some(name), Ok(v)) = (s.save, serde_json::from_str(&reply)) {
                            self.saved.insert(name, v);
                        }
                        RunItem::ToolCall {
                            name: format!("dvb run {verb}"),
                        }
                    }
                    Err(e) => RunItem::Error(ProviderError::Other(format!("dvb run {verb}: {e}"))),
                }
            }
            Step::Usage {
                uncached_in,
                cached_in,
                out,
            } => RunItem::Usage {
                uncached_in,
                cached_in,
                out,
            },
            Step::SleepMs(ms) => {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                RunItem::Text(format!("slept {ms} ms"))
            }
            Step::Stderr(text) => {
                eprintln!("{text}");
                self.stderr.push(&text);
                RunItem::Text("wrote to stderr".into())
            }
            Step::Exit(code) => RunItem::Ended(ExitStatus { code: Some(code) }),
        }
    }

    /// Replaces `${name.field.0}` in every string with the saved reply's value.
    fn fill(&self, v: Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.fill_str(&s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|v| self.fill(v)).collect()),
            Value::Object(o) => {
                Value::Object(o.into_iter().map(|(k, v)| (k, self.fill(v))).collect())
            }
            v @ (Value::Null | Value::Bool(_) | Value::Number(_)) => v,
        }
    }

    fn fill_str(&self, s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find("${") {
            let Some(len) = rest[start..].find('}') else {
                break;
            };
            out.push_str(&rest[..start]);
            let key = &rest[start + 2..start + len];
            let mut parts = key.split('.');
            let found = parts
                .next()
                .and_then(|name| self.saved.get(name))
                .and_then(|v| {
                    parts.try_fold(v, |v, p| match p.parse::<usize>() {
                        Ok(i) => v.get(i),
                        Err(_) => v.get(p),
                    })
                });
            match found {
                Some(Value::String(s)) => out.push_str(s),
                Some(v) => out.push_str(&v.to_string()),
                None => out.push_str(&rest[start..start + len + 1]),
            }
            rest = &rest[start + len + 1..];
        }
        out.push_str(rest);
        out
    }
}

impl Session for FakeSession {
    fn next(&mut self) -> Option<RunItem> {
        if self.ended {
            return None;
        }
        let item = match self.queue.pop_front() {
            Some(Pending::Item(item)) => item,
            Some(Pending::Step(s)) => self.play(s),
            None => {
                let end = RunItem::Ended(ExitStatus { code: Some(0) });
                self.queue.push_back(Pending::Item(end));
                // The Fake never calls a model; scripts/run-e2e.sh checks this line.
                RunItem::Text("fake provider: 0 model calls".into())
            }
        };
        if matches!(item, RunItem::Ended(_)) {
            self.ended = true;
        }
        Some(item)
    }

    fn stop(&mut self) {
        self.ended = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(script: &str) -> (tempfile::TempDir, Box<dyn Session>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.json");
        std::fs::write(&path, script).unwrap();
        let fake = Fake {
            script: path,
            dvb: PathBuf::from("/bin/false"),
            stderr: crate::stream::StderrTail::default(),
        };
        let s = fake
            .start(dir.path(), dir.path(), "", "fake-1", Effort::Medium)
            .unwrap();
        (dir, s)
    }

    #[test]
    fn plays_a_script_and_counts_no_model_calls() {
        let (dir, mut s) = session(
            r#"[{"write": {"path": "docs/a.md", "text": "hi\n"}},
                {"usage": {"uncached_in": 1, "cached_in": 2, "out": 3}}]"#,
        );
        let mut items = Vec::new();
        while let Some(i) = s.next() {
            items.push(i);
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("docs/a.md")).unwrap(),
            "hi\n"
        );
        assert_eq!(
            items,
            vec![
                RunItem::ModelServed(ServedModel::Confirmed("fake-1".into())),
                RunItem::ToolCall {
                    name: "write".into(),
                },
                RunItem::Usage {
                    uncached_in: 1,
                    cached_in: 2,
                    out: 3
                },
                RunItem::Text("fake provider: 0 model calls".into()),
                RunItem::Ended(ExitStatus { code: Some(0) }),
            ]
        );
    }

    #[test]
    fn exit_ends_the_session() {
        let (_dir, mut s) = session(r#"[{"exit": 3}, {"sleep_ms": 1}]"#);
        assert!(matches!(s.next(), Some(RunItem::ModelServed(_))));
        assert_eq!(s.next(), Some(RunItem::Ended(ExitStatus { code: Some(3) })));
        assert_eq!(s.next(), None);
    }

    #[test]
    fn a_stderr_step_is_the_sessions_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.json");
        std::fs::write(&path, r#"[{"stderr": "disk full\nretrying"}, {"exit": 1}]"#).unwrap();
        let stderr = crate::stream::StderrTail::default();
        let fake = Fake {
            script: path,
            dvb: PathBuf::from("/bin/false"),
            stderr: stderr.clone(),
        };
        let mut s = fake
            .start(dir.path(), dir.path(), "", "fake-1", Effort::Medium)
            .unwrap();
        while s.next().is_some() {}
        assert_eq!(stderr.text(), "disk full\nretrying");
    }

    #[test]
    fn saved_replies_fill_later_bodies() {
        let mut f = FakeSession {
            ws: PathBuf::new(),
            dvb: PathBuf::new(),
            stderr: crate::stream::StderrTail::default(),
            queue: VecDeque::new(),
            saved: BTreeMap::new(),
            ended: false,
        };
        f.saved
            .insert("plan".into(), serde_json::json!({"tasks": ["T1"]}));
        assert_eq!(
            f.fill(serde_json::json!({"task": "${plan.tasks.0}", "x": "${nope}"})),
            serde_json::json!({"task": "T1", "x": "${nope}"})
        );
    }
}
