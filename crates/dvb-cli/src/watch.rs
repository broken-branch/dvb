//! Filtered event following, text and JSON feed output, and project and
//! prior-value context for owner changes in the watch feed.
use crate::client::WatchEvent;
use crate::{CliError, Client, Event};
use dvb_core::EventKind;
use dvb_core::api::{ProjectView, RoleFields, RoleHistory};
use dvb_core::roles::RoleRecord;
use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;

impl Client {
    /// Read the engine's membership-filtered pages through a fixed tail, oldest first.
    pub(crate) async fn filtered_since(
        &self,
        after: u64,
        filter: &dvb_core::api::EventPageQuery,
    ) -> Result<(u64, Vec<WatchEvent>), CliError> {
        #[derive(serde::Deserialize)]
        struct Page {
            cursor: dvb_core::EventId,
            events: Vec<WatchEvent>,
            before: Option<dvb_core::EventId>,
        }
        let mut query = filter.clone();
        query.before = None;
        query.limit = Some(100);
        query.latest = Some(false);
        query.summary = Some(false);
        let mut cursor = None;
        let mut events = Vec::new();
        loop {
            let params = serde_urlencoded::to_string(&query).map_err(crate::client::message)?;
            let page: Page = self.get(&format!("/events/recent?{params}")).await?;
            let tail = *cursor.get_or_insert(page.cursor.0);
            let reached = page.events.iter().any(|event| event.id().0 <= after);
            events.extend(
                page.events
                    .into_iter()
                    .filter(|event| event.id().0 > after && event.id().0 <= tail),
            );
            if reached || page.before.is_none() {
                break;
            }
            query.before = page.before;
        }
        events.reverse();
        Ok((cursor.unwrap_or(after).max(after), events))
    }

    /// SSE supplies wakeups; only engine-filtered pages decide watch membership.
    /// Callback errors follow the same reconnect policy as request errors. Advance
    /// past each delivered event so a failed callback can retry without duplicates.
    /// `each` returns a future instead of being an `AsyncFnMut`: the compiler cannot
    /// yet show such a callback's future `Send`, which the desk's spawned feed needs.
    pub async fn watch_filtered_events<F>(
        &self,
        after: u64,
        once: bool,
        filter: &dvb_core::api::EventPageQuery,
        mut each: impl FnMut(WatchEvent) -> F,
    ) -> Result<(), CliError>
    where
        F: Future<Output = Result<bool, CliError>>,
    {
        let mut cursor = after;
        loop {
            let pass = async {
                let (tail, events) = self.filtered_since(cursor, filter).await?;
                for event in events {
                    let id = event.id().0;
                    if !each(event).await? {
                        return Ok(ControlFlow::Break(()));
                    }
                    cursor = id;
                }
                cursor = tail;
                Ok(if once {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(cursor)
                })
            }
            .await;
            if let Some(()) = self.reconnecting(once, "dvb watch", pass).await? {
                return Ok(());
            }
        }
    }

    /// Ends a follower's pass: a break ends the follow; a pass that continues
    /// waits for any event after its cursor. When the engine went away (it
    /// restarts on update) the next pass starts a second later, unless `once`.
    pub(crate) async fn reconnecting<T>(
        &self,
        once: bool,
        label: &str,
        pass: Result<ControlFlow<T, u64>, CliError>,
    ) -> Result<Option<T>, CliError> {
        let mut woke = false;
        let result = match pass {
            Ok(ControlFlow::Break(value)) => return Ok(Some(value)),
            Ok(ControlFlow::Continue(cursor)) => {
                self.read_events::<serde::de::IgnoredAny>(cursor, false, |_| {
                    woke = true;
                    false
                })
                .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {}
            Err(
                error @ (CliError::Message(_) | CliError::Connection(_) | CliError::Transport(_)),
            ) if !once => eprintln!("{label}: {error}; reconnecting"),
            Err(error) => return Err(error),
        }
        if !woke {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Ok(None)
    }
}

/// Prints each event as a feed line, or with `json` as [`json_line`].
pub async fn follow(
    client: &Client,
    after: u64,
    once: bool,
    json: bool,
    filter: &dvb_core::api::EventPageQuery,
    output: impl FnMut(String) -> Result<bool, CliError>,
) -> Result<(), CliError> {
    // Each event's future is awaited before the next is made, so only one borrows these.
    let output = RefCell::new(output);
    let output_error = Cell::new(None);
    client
        .watch_filtered_events(after, once, filter, |ev| {
            let (output, output_error) = (&output, &output_error);
            async move {
                let rendered = match ev {
                    WatchEvent::Known(event) => {
                        if matches!(event.kind, EventKind::ProjectSeen {}) {
                            return Ok(true);
                        }
                        if json {
                            serde_json::to_value(&*event)
                                .map_err(crate::client::message)
                                .map(|raw| json_line(raw, crate::feed_word(&event.kind)))
                        } else {
                            line(client, &event).await
                        }
                    }
                    WatchEvent::Unknown { raw, .. } if json => Ok(json_line(raw, None)),
                    WatchEvent::Unknown { id, kind, raw } => {
                        Ok(format!("{id:>5}  {kind:<18} {}", raw["payload"]))
                    }
                };
                let result = match rendered {
                    Ok(line) => output.borrow_mut()(line),
                    Err(error) if error.restart_connection() => return Err(error),
                    Err(error) => Err(error),
                };
                match result {
                    Ok(keep) => Ok(keep),
                    Err(error) => {
                        output_error.set(Some(error));
                        Ok(false)
                    }
                }
            }
        })
        .await?;
    output_error.take().map_or(Ok(()), Err)
}

/// One `dvb watch --json` line: the event as the engine serves it (`id`, `at`,
/// `actor`, `project`, `kind`, `payload`, `evidence`), with `time` as RFC 3339
/// UTC and `text`, the feed's wording, when the kind has one.
pub fn json_line(mut event: serde_json::Value, text: Option<String>) -> String {
    let time = event["at"]
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    event["time"] = time.into();
    if let Some(text) = text {
        event["text"] = text.into();
    }
    event.to_string()
}

pub async fn line(client: &Client, event: &Event) -> Result<String, CliError> {
    let Some(project) = event.project.as_ref().filter(|_| {
        matches!(
            event.kind,
            EventKind::ProjectBriefSet { .. }
                | EventKind::RoleAdded(_)
                | EventKind::RoleChanged { .. }
                | EventKind::RoleRemoved(_)
                | EventKind::HireAdded(_)
                | EventKind::HireChanged { .. }
                | EventKind::HireRetired(_)
                | EventKind::SettingsChanged { .. }
        )
    }) else {
        return Ok(crate::line(event));
    };
    let project: ProjectView = client.get(&format!("/projects/{project}")).await?;
    let mut text = format!(
        "{} ({})",
        crate::feed_word(&event.kind).unwrap_or_default(),
        project.name
    );
    if let EventKind::RoleChanged { role, .. } = &event.kind {
        let history: RoleHistory = client.get(&format!("/roles/{}/history", role.id)).await?;
        let previous = history
            .versions
            .iter()
            .filter(|version| version.event < event.id)
            .max_by_key(|version| version.event)
            .ok_or_else(|| {
                CliError::Message(format!(
                    "role {} has no version before event {}",
                    role.id, event.id
                ))
            })?;
        let fields = changed_fields(role, &previous.value);
        if !fields.is_empty() {
            text.push_str(": ");
            text.push_str(&fields.join(", "));
        }
    }
    Ok(crate::event_line(event, Some(text)))
}

fn changed_fields(role: &RoleRecord, previous: &RoleFields) -> Vec<&'static str> {
    [
        ("name", role.name != previous.name),
        ("job", role.job != previous.job),
        ("lane", role.lane != previous.lane),
        ("lead", role.lead != previous.lead),
        ("reports_to", role.reports_to != previous.reports_to),
        ("reviewed_by", role.reviewed_by != previous.reviewed_by),
        (
            "review_instructions",
            role.review_instructions != previous.review_instructions,
        ),
        ("group", role.group != previous.group),
        ("order", role.order != previous.order),
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http;
    use dvb_core::api::RoleHistory;
    use dvb_core::roles::{HireRecord, HireStatus, RoleRecord, default_records};
    use dvb_core::{Actor, Effort, EventId, EventKind, HireId, ProjectId, RoleId, Timestamp};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn unknown_kind_prints_one_line_and_watch_continues_with_the_next_known_event() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let unknown = json!({
            "id": 11, "at": 0, "actor": "engine", "project": null,
            "kind": "future.event", "payload": {"text": format!("Café\r\n{}", "x".repeat(220))}
        });
        let mut known = event(EventKind::EngineStarted {
            version: "new".into(),
        });
        known.id = EventId(12);
        let known_line = crate::line(&known);
        let known = serde_json::to_value(known).unwrap();
        let expected_unknown = format!("11  future.event       {}", unknown["payload"]);
        let server = tokio::spawn(async move {
            for step in 0..5 {
                let (request, response) = requests.recv().await.unwrap();
                let body = match step {
                    1 | 3 => {
                        let (after, event) = if step == 1 {
                            (10, &unknown)
                        } else {
                            (11, &known)
                        };
                        assert_eq!(request.uri().to_string(), format!("/events?after={after}"));
                        axum::response::Response::new(axum::body::Body::from(format!(
                            "data: {event}\n\n"
                        )))
                    }
                    0 | 2 | 4 => {
                        assert_eq!(request.uri().path(), "/events/recent");
                        let (cursor, events) = match step {
                            0 => (10, json!([])),
                            2 => (11, json!([unknown])),
                            4 => (12, json!([known, unknown])),
                            _ => unreachable!(),
                        };
                        test_http::json(
                            200,
                            json!({"cursor": cursor, "events": events, "before": null, "activity": []}),
                        )
                    }
                    _ => unreachable!(),
                };
                response.send(body).unwrap();
            }
        });
        let mut lines = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            follow(
                &Client::new(sock),
                0,
                false,
                false,
                &Default::default(),
                |line| {
                    lines.push(line);
                    Ok(lines.len() < 2)
                },
            ),
        )
        .await
        .expect("watch must continue past an unknown kind")
        .unwrap();
        server.await.unwrap();
        assert_eq!(lines, [expected_unknown, known_line]);
        assert!(lines.iter().all(|line| line.lines().count() == 1));
    }

    #[tokio::test(start_paused = true)]
    async fn context_disconnect_retries_the_unrendered_event() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let page = json!({
            "cursor": 10,
            "events": [event(EventKind::ProjectBriefSet { brief: "brief".into() })],
            "before": null,
            "activity": []
        });
        let server = tokio::spawn(async move {
            let mut disconnected = false;
            for _ in 0..4 {
                let (request, response) = requests.recv().await.unwrap();
                let body = match request.uri().path() {
                    "/events/recent" => test_http::json(200, page.clone()),
                    "/projects/project-id" if !disconnected => {
                        disconnected = true;
                        axum::response::Response::new(axum::body::Body::from_stream(
                            futures_util::stream::iter([Err::<bytes::Bytes, _>(
                                std::io::Error::from(std::io::ErrorKind::ConnectionReset),
                            )]),
                        ))
                    }
                    "/projects/project-id" => test_http::json(
                        200,
                        json!({
                            "id":"project-id", "name":"example", "repo":"local",
                            "default_branch":"main", "ci":"no_ci", "paused":false
                        }),
                    ),
                    path => panic!("unexpected path: {path}"),
                };
                response.send(body).unwrap();
            }
        });
        let mut lines = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            follow(
                &Client::new(sock),
                0,
                false,
                false,
                &Default::default(),
                |line| {
                    lines.push(line);
                    Ok(false)
                },
            ),
        )
        .await
        .expect("watch must reconnect after a context lookup disconnects")
        .unwrap();
        server.await.unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("Brief updated (example)"));
    }

    fn role() -> RoleRecord {
        default_records(|r| RoleId::new(r.slug)).remove(4)
    }

    fn event(kind: EventKind) -> Event {
        Event {
            id: EventId(10),
            at: Timestamp(0),
            actor: Actor::Owner,
            project: Some(ProjectId::new("project-id")),
            kind,
            evidence: None,
        }
    }

    async fn rendered(event: &Event, history: Option<Value>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let mut requests = test_http::requests(listener);
        let project = json!({
            "id":"project-id", "name":"example", "repo":"local",
            "default_branch":"main", "ci":"no_ci", "paused":false
        });
        let server = tokio::spawn(async move {
            let count = 1 + usize::from(history.is_some());
            for _ in 0..count {
                let (request, response) = requests.recv().await.unwrap();
                let body = match request.uri().path() {
                    "/projects/project-id" => project.clone(),
                    "/roles/code-reviewer/history" => history.clone().unwrap(),
                    path => panic!("unexpected path: {path}"),
                };
                response.send(test_http::json(200, body)).unwrap();
            }
        });
        let text = line(&Client::new(sock), event).await.unwrap();
        assert!(text.contains("(example)"), "{text}");
        server.await.unwrap();
        text
    }

    #[tokio::test]
    async fn owner_change_lines_name_the_project() {
        let hire = HireRecord {
            id: HireId::new("hire"),
            role: RoleId::new("code-reviewer"),
            model: "fake-1".into(),
            effort: Effort::Medium,
            status: HireStatus::Active,
            routine: false,
        };
        for kind in [
            EventKind::ProjectBriefSet {
                brief: "private brief".into(),
            },
            EventKind::RoleAdded(role()),
            EventKind::RoleRemoved(role()),
            EventKind::HireAdded(hire.clone()),
            EventKind::HireChanged {
                hire: hire.clone(),
                restored_from: None,
            },
            EventKind::HireChanged {
                hire: hire.clone(),
                restored_from: Some(EventId(1)),
            },
            EventKind::HireRetired(HireRecord {
                status: HireStatus::Retired,
                ..hire
            }),
            EventKind::SettingsChanged {
                key: "project_checks".into(),
                old: json!("no_ci"),
                new: json!({"expected":["rust"]}),
                restored_from: None,
            },
            EventKind::SettingsChanged {
                key: "project_checks".into(),
                old: json!({"expected":["rust"]}),
                new: json!("no_ci"),
                restored_from: Some(EventId(1)),
            },
            EventKind::SettingsChanged {
                key: "project_setting".into(),
                old: Value::Null,
                new: json!(true),
                restored_from: None,
            },
        ] {
            let ev = event(kind);
            let text = rendered(&ev, None).await;
            assert!(
                text.starts_with(&format!("{}  {:<18} ", ev.id, ev.kind.kind())),
                "{text}"
            );
            assert!(!text.contains("private brief"));
        }
    }

    fn history(previous: Value, current: &RoleRecord) -> Value {
        // Newer changes must not become the baseline for an older watched event.
        let mut future = serde_json::to_value(current).unwrap();
        future["job"] = json!("a future edit");
        let versions = [
            (20, future),
            (10, serde_json::to_value(current).unwrap()),
            (8, previous),
        ]
        .map(|(id, value)| json!({"event":id,"at":0,"actor":"owner","value":value}));
        let history = json!({"role":"code-reviewer", "versions":versions});
        serde_json::from_value::<RoleHistory>(history.clone()).unwrap();
        history
    }

    #[tokio::test]
    async fn role_change_names_each_changed_field_from_its_previous_version() {
        let previous = serde_json::to_value(role()).unwrap();
        for (field, value) in [
            ("name", json!("Reviewer")),
            ("job", json!("New job")),
            ("lane", json!("New lane")),
            ("lead", json!(true)),
            ("reports_to", Value::Null),
            ("reviewed_by", json!("architect")),
            ("review_instructions", json!("Read the tests")),
            ("group", json!("Review")),
            ("order", json!(5)),
        ] {
            let mut current = previous.clone();
            current[field] = value;
            let current: RoleRecord = serde_json::from_value(current).unwrap();
            let h = history(previous.clone(), &current);
            let text = rendered(
                &event(EventKind::RoleChanged {
                    role: current,
                    restored_from: None,
                }),
                Some(h),
            )
            .await;
            assert!(text.ends_with(&format!(": {field}")), "{text}");
        }
    }
}
