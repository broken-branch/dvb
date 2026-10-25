//! One persistent D-Bus sender for notification routes and owner actions.
use crate::DeskState;
use dvb_cli::client::{CliError, Client, WatchEvent};
use dvb_core::api::{EventPageQuery, RetryTask, Settings, SettingsKey};
use dvb_core::event::Answer;
use dvb_core::{Event, EventKind, QuestionId, TaskId};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Open(String),
    Answer { question: QuestionId, choice: u32 },
    Retry(TaskId),
}

#[derive(Debug, PartialEq, Eq)]
struct Notice {
    title: String,
    route: String,
    button: Option<(String, Action)>,
}

impl Notice {
    fn actions(&self) -> Vec<&str> {
        let mut actions = vec!["default", "Open"];
        if let Some((label, _)) = &self.button {
            actions.extend(["act", label.as_str()]);
        }
        actions
    }
}

fn failure(error: &CliError) -> String {
    format!("Action failed: {error}")
        .chars()
        .take(180)
        .collect::<String>()
        .lines()
        .next()
        .unwrap_or("Action failed")
        .to_owned()
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "only the events that notify have an arm; every other event, present or future, notifies nobody"
)]
fn notice(event: &Event, settings: &Settings) -> Option<Notice> {
    let notify_faults = settings
        .values
        .get(&SettingsKey::NotifyFaults)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let notify_completed = settings
        .values
        .get(&SettingsKey::NotifyCompleted)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    match &event.kind {
        EventKind::QuestionOpened {
            question,
            subject,
            options,
            recommended,
            ..
        } => Some(Notice {
            title: format!("Needs you: {subject}"),
            route: format!("/decision/{question}"),
            button: options
                .iter()
                .find(|option| option.id == *recommended && option.id > 0)
                .filter(|option| {
                    // Reply requirements are prose in the existing question contract.
                    let instructions = format!("{} {}", option.text, option.effect).to_lowercase();
                    ![
                        "write a reply",
                        "reply in words",
                        "reply with",
                        "answer in words",
                        "respond in words",
                        "provide free text",
                        "enter free text",
                    ]
                    .iter()
                    .any(|text| instructions.contains(text))
                })
                .map(|option| {
                    (
                        option.text.clone(),
                        Action::Answer {
                            question: question.clone(),
                            choice: option.id,
                        },
                    )
                }),
        }),
        EventKind::TaskFailed { task, .. } if notify_faults => Some(Notice {
            title: format!("Task failed: {task}"),
            route: format!("/task/{task}"),
            button: Some(("Retry".into(), Action::Retry(task.clone()))),
        }),
        EventKind::ChecksStuck { task, check, .. } if notify_faults => Some(Notice {
            title: format!("CI check stuck: {check}"),
            route: format!("/task/{task}"),
            button: None,
        }),
        EventKind::GoalDone { .. } if notify_completed => event.project.as_ref().map(|p| Notice {
            title: "Goal completed".into(),
            route: format!("/project/{p}"),
            button: None,
        }),
        EventKind::PrMerged { task, pr, .. } if notify_completed => Some(Notice {
            title: format!("PR #{pr} merged"),
            route: format!("/task/{task}"),
            button: None,
        }),
        _ => None,
    }
}

pub async fn watch(desk: DeskState) {
    loop {
        if let Err(e) = session(desk.clone()).await {
            eprintln!("dvb desk notifications: {e}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn session(desk: DeskState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    // Subscribe before Notify: the default action can arrive immediately.
    let mut signals = proxy.receive_all_signals().await?;
    let mut items = HashMap::new();
    let client = Client::new(desk.engine_sock.clone());
    let feed_client = Client::new(desk.engine_sock.clone());
    let mut calls = FuturesUnordered::new();
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    let reader = crate::AbortOnDrop(tokio::spawn(async move {
        loop {
            if let Err(error) = read_events(&feed_client, &send).await {
                eprintln!("dvb desk notification feed: {error}");
            }
            if send.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }));
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some((event, settings)) = event else { break; };
                if let Some(n) = notice(&event, &settings) {
                    let id = notify(&proxy, &n.title, n.actions()).await?;
                    items.insert(id, n);
                }
            }
            signal = signals.next() => {
                let Some(signal) = signal else { break; };
                if let Some(action) = action(&mut items, &signal)? {
                    calls.push(invoke(&desk, &client, action));
                }
            }
            result = calls.next(), if !calls.is_empty() => {
                if let Some(Err(error)) = result {
                    notify(&proxy, &failure(&error), vec![]).await?;
                }
            }
        }
    }
    drop(reader);
    Ok(())
}

/// The kinds `notice` matches, and the settings that switch some of them off.
const KINDS: &str = "question.opened,task.failed,checks.stuck,goal.done,pr.merged,settings.changed";

async fn read_events(
    client: &Client,
    send: &tokio::sync::mpsc::UnboundedSender<(Event, Settings)>,
) -> Result<(), CliError> {
    let page: dvb_core::api::EventPage = client.get("/events/recent?limit=1").await?;
    let mut settings: Settings = client.get("/settings").await?;
    let filter = EventPageQuery {
        kinds: Some(KINDS.into()),
        ..Default::default()
    };
    client
        .watch_filtered_events(page.cursor.0, false, &filter, |event| {
            std::future::ready(Ok(match event {
                WatchEvent::Known(event) => {
                    if let EventKind::SettingsChanged { key, new, .. } = &event.kind {
                        if let Ok(key) = key.parse::<SettingsKey>() {
                            settings.values.insert(key, new.clone());
                        }
                        !send.is_closed()
                    } else {
                        send.send((*event, settings.clone())).is_ok()
                    }
                }
                WatchEvent::Unknown { .. } => !send.is_closed(),
            }))
        })
        .await
}

async fn notify(
    proxy: &zbus::Proxy<'_>,
    title: &str,
    actions: Vec<&str>,
) -> Result<u32, zbus::Error> {
    let hints: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
    proxy
        .call(
            "Notify",
            &("dvb", 0_u32, "dvb", title, "", actions, hints, -1_i32),
        )
        .await
}

fn action(
    items: &mut HashMap<u32, Notice>,
    signal: &zbus::Message,
) -> Result<Option<Action>, zbus::Error> {
    match signal
        .header()
        .member()
        .map(zbus::names::MemberName::as_str)
    {
        Some("ActionInvoked") => {
            let (id, key): (u32, String) = signal.body().deserialize()?;
            return Ok(match key.as_str() {
                "default" => items.remove(&id).map(|n| Action::Open(n.route)),
                "act" if items.get(&id).is_some_and(|n| n.button.is_some()) => items
                    .remove(&id)
                    .and_then(|n| n.button.map(|(_, action)| action)),
                _ => None,
            });
        }
        Some("NotificationClosed") => {
            let (id, _reason): (u32, u32) = signal.body().deserialize()?;
            items.remove(&id);
        }
        Some(_) | None => {}
    }
    Ok(None)
}

async fn invoke(desk: &DeskState, client: &Client, action: Action) -> Result<(), CliError> {
    match action {
        Action::Open(route) => {
            let desk = desk.clone();
            match tokio::task::spawn_blocking(move || desk.launch_item(&route)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("dvb desk notification action: {error}"),
                Err(error) => eprintln!("dvb desk notification action: {error}"),
            }
        }
        Action::Answer { question, choice } => {
            let _: dvb_core::api::QuestionView = client
                .post(
                    &format!("/questions/{question}/answer"),
                    &Answer::Choice(choice),
                )
                .await?;
        }
        Action::Retry(task) => {
            let _: dvb_core::api::TaskView = client
                .post(&format!("/tasks/{task}/retry"), &RetryTask { text: None })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvb_core::{Actor, EventId, ProjectId, Timestamp};

    #[test]
    fn notification_signals_decode_actions_and_remove_closed_items() {
        let mut items = HashMap::from([(
            7,
            Notice {
                title: "Task failed: t".into(),
                route: "/task/t".into(),
                button: None,
            },
        )]);
        let signal = |member: &'static str| {
            zbus::Message::signal(
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                member,
            )
        };
        let other_action = signal("ActionInvoked").build(&(7_u32, "other")).unwrap();
        action(&mut items, &other_action).unwrap();
        assert_eq!(items.get(&7).map(|n| n.route.as_str()), Some("/task/t"));
        let malformed = signal("NotificationClosed").build(&(7_u32,)).unwrap();
        assert!(action(&mut items, &malformed).is_err());
        assert!(items.contains_key(&7));
        let closed = signal("NotificationClosed").build(&(7_u32, 2_u32)).unwrap();
        action(&mut items, &closed).unwrap();
        assert!(items.is_empty());
    }

    fn event(kind: EventKind) -> Event {
        Event {
            id: EventId(1),
            at: Timestamp(1),
            actor: Actor::Engine,
            project: Some(ProjectId::new("p")),
            kind,
            evidence: None,
        }
    }
    fn question(recommended: u32, text: &str, effect: &str) -> Event {
        event(EventKind::QuestionOpened {
            kind: None,
            task: None,
            question: QuestionId::new("q"),
            subject: "Approve?".into(),
            format: dvb_core::event::QuestionFormat::Simple,
            options: vec![
                dvb_core::event::QuestionOption::owner(1, "Wait", "Wait for later"),
                dvb_core::event::QuestionOption::owner(2, text, effect),
            ],
            recommended,
            lapses_on: dvb_core::event::LapsesOn::GoalVersion(EventId(1)),
            main_ci: None,
        })
    }

    fn invoked(id: u32, key: &str) -> zbus::Message {
        zbus::Message::signal(
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "ActionInvoked",
        )
        .build(&(id, key))
        .unwrap()
    }

    fn notification_events() -> Vec<(Event, &'static str, Vec<&'static str>)> {
        vec![
            (
                question(2, "Approve", "Start the plan"),
                "/decision/q",
                vec!["default", "Open", "act", "Approve"],
            ),
            (
                event(EventKind::TaskFailed {
                    task: TaskId::new("t"),
                    reason: None,
                }),
                "/task/t",
                vec!["default", "Open", "act", "Retry"],
            ),
            (
                event(EventKind::ChecksStuck {
                    task: TaskId::new("t"),
                    head: "head".into(),
                    check: "rust".into(),
                    url: None,
                    limit_minutes: 60,
                }),
                "/task/t",
                vec!["default", "Open"],
            ),
            (
                event(EventKind::GoalDone {
                    goal: dvb_core::GoalId::new("g"),
                }),
                "/project/p",
                vec!["default", "Open"],
            ),
            (
                event(EventKind::PrMerged {
                    task: TaskId::new("t"),
                    pr: 328,
                    sha: "head".into(),
                }),
                "/task/t",
                vec!["default", "Open"],
            ),
        ]
    }

    #[test]
    fn buttons_match_events_and_body_clicks_keep_their_routes() {
        let mut settings = Settings::default();
        settings
            .values
            .insert(SettingsKey::NotifyCompleted, serde_json::json!(true));
        for (event, route, expected) in notification_events() {
            let n = notice(&event, &settings).unwrap();
            assert_eq!(n.actions(), expected);
            let mut items = HashMap::from([(7, n)]);
            assert_eq!(
                action(&mut items, &invoked(7, "default")).unwrap(),
                Some(Action::Open(route.into()))
            );
            assert!(items.is_empty());
            assert_eq!(action(&mut items, &invoked(7, "default")).unwrap(), None);
        }
    }

    #[test]
    fn plain_choices_mentioning_replies_keep_the_recommended_button() {
        for (text, effect) in [
            ("Work on #12: Fix reply parsing", "Start this work"),
            ("Work on #13: Fix noreply addresses", "Start this work"),
            ("Work on #14: Free text search", "Start this work"),
            ("Work on #15: Numbers in words", "Start this work"),
            ("Approve", "Start work on reply parsing"),
            ("Approve", "Fix noreply addresses"),
        ] {
            let n = notice(&question(2, text, effect), &Settings::default()).unwrap();
            assert_eq!(n.actions(), ["default", "Open", "act", text]);
            let mut items = HashMap::from([(7, n)]);
            assert_eq!(
                action(&mut items, &invoked(7, "act")).unwrap(),
                Some(Action::Answer {
                    question: QuestionId::new("q"),
                    choice: 2,
                }),
                "{text}: {effect}",
            );
        }
    }

    #[test]
    fn missing_recommendations_and_written_replies_have_no_button() {
        for (recommended, text, effect) in [
            (0, "Approve", "Start the plan"),
            (99, "Approve", "Start the plan"),
            (2, "Write a reply", "Give the team direction"),
            (2, "Specify", "Reply with the issue number"),
            (2, "A different issue", "Reply in words naming its number."),
            (2, "Describe it", "Answer IN WORDS"),
            (2, "Describe it", "Respond in words"),
            (2, "Specify", "Provide free text"),
            (2, "Specify", "Enter free text"),
        ] {
            let n = notice(&question(recommended, text, effect), &Settings::default()).unwrap();
            assert_eq!(n.actions(), ["default", "Open"], "{text}: {effect}");
            assert!(n.button.is_none());
        }
        let mut empty = question(2, "Approve", "Start the plan");
        if let EventKind::QuestionOpened { options, .. } = &mut empty.kind {
            options.clear();
        }
        assert!(
            notice(&empty, &Settings::default())
                .unwrap()
                .button
                .is_none()
        );
    }

    fn fake_client(
        refused: bool,
    ) -> (
        tempfile::TempDir,
        Client,
        tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)>,
        crate::AbortOnDrop,
    ) {
        use axum::{
            Json, Router,
            extract::OriginalUri,
            routing::{get, post},
        };
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route("/version", get(|| async {
                ([(dvb_core::api::VERSION_HEADER, dvb_core::VERSION)], Json(serde_json::json!({})))
            }))
            .route("/{*path}", post(move |OriginalUri(uri): OriginalUri, Json(body): Json<serde_json::Value>| {
                let path = uri.path().to_owned();
                send.send((path.clone(), body)).unwrap();
                async move {
                    if refused {
                        return (axum::http::StatusCode::CONFLICT, Json(serde_json::to_value(
                            dvb_core::Refusal::new(dvb_core::RefusalCode::QuestionLapsed,
                                "This question was already answered"),
                        ).unwrap()));
                    }
                    let response = if path == "/questions/q/answer" {
                        serde_json::json!({
                            "id": "q", "project": "p", "subject": "Approve?", "format": "simple",
                            "options": [], "recommended": 2, "open": false, "answer": {"choice": 2}
                        })
                    } else {
                        serde_json::json!({
                            "id": "t", "project": "p", "goal": "g", "title": "Build",
                            "role": "backend", "review": "code", "state": {"state": "planned"},
                            "pr": null, "head": null
                        })
                    };
                    (axum::http::StatusCode::OK, Json(response))
                }
            }));
        let server = crate::AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        (dir, Client::new(sock), recv, server)
    }

    #[tokio::test]
    async fn invoked_buttons_call_the_matching_owner_api_once() {
        let (_dir, client, mut requests, _server) = fake_client(false);
        let desk = std::sync::Arc::new(crate::Desk::new(client.sock.clone(), 7300));
        for (event, expected_path, expected_body) in [
            (
                question(2, "Approve", "Start the plan"),
                "/questions/q/answer",
                serde_json::json!({"choice": 2}),
            ),
            (
                question(2, "Work on #12: Fix reply parsing", "Start this work"),
                "/questions/q/answer",
                serde_json::json!({"choice": 2}),
            ),
            (
                event(EventKind::TaskFailed {
                    task: TaskId::new("t"),
                    reason: None,
                }),
                "/tasks/t/retry",
                serde_json::json!({}),
            ),
        ] {
            let mut items = HashMap::from([(7, notice(&event, &Settings::default()).unwrap())]);
            let a = action(&mut items, &invoked(7, "act")).unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), invoke(&desk, &client, a))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                requests.try_recv().unwrap(),
                (expected_path.into(), expected_body)
            );
            assert!(requests.try_recv().is_err());
            assert_eq!(action(&mut items, &invoked(7, "act")).unwrap(), None);
        }
    }

    #[tokio::test]
    async fn failed_button_calls_produce_short_inert_error_notices() {
        let (_dir, client, mut requests, _server) = fake_client(true);
        let desk = std::sync::Arc::new(crate::Desk::new(client.sock.clone(), 7300));
        for (event, _, _) in notification_events().into_iter().take(2) {
            let mut items = HashMap::from([(7, notice(&event, &Settings::default()).unwrap())]);
            let a = action(&mut items, &invoked(7, "act")).unwrap().unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), invoke(&desk, &client, a))
                .await
                .unwrap()
                .unwrap_err();
            let failure = failure(&error);
            assert!(failure.contains("already answered"));
            assert!(requests.try_recv().is_ok());
            assert!(requests.try_recv().is_err());
            assert_eq!(action(&mut items, &invoked(7, "act")).unwrap(), None);
        }
        let failure = failure(&CliError::Message(format!("{}\nignored", "é".repeat(500))));
        assert_eq!(failure.chars().count(), 180);
        assert!(!failure.contains('\n'));
    }

    #[tokio::test]
    async fn socket_notification_connections_start_at_the_newest_cursor_and_refresh_settings() {
        use axum::{
            Json, Router,
            extract::Query,
            response::sse::{Event as SseEvent, Sse},
            routing::get,
        };
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        // The newest event when the feed connects, then the filtered pages it reads.
        let newest = Arc::new(AtomicU64::new(0));
        let pages = Arc::new(Mutex::new(Vec::<Vec<Event>>::new()));
        let (recent_newest, settings_newest) = (newest.clone(), newest.clone());
        let (recent_pages, wake_pages) = (pages.clone(), pages.clone());
        let app = Router::new()
            .route(
                "/events/recent",
                get(move |Query(query): Query<HashMap<String, String>>| {
                    let page = if query.get("limit").map(String::as_str) == Some("1") {
                        dvb_core::api::EventPage {
                            cursor: EventId(recent_newest.load(Ordering::SeqCst)),
                            events: vec![],
                            before: None,
                            activity: vec![],
                        }
                    } else {
                        // The engine keeps only the kinds asked for.
                        let kinds = query["kinds"].split(',').collect::<Vec<_>>();
                        let mut events = recent_pages.lock().unwrap().remove(0);
                        let cursor = events[0].id;
                        events.retain(|event| kinds.contains(&event.kind.kind()));
                        dvb_core::api::EventPage {
                            cursor,
                            events,
                            before: None,
                            activity: vec![],
                        }
                    };
                    async move { Json(page) }
                }),
            )
            .route(
                "/settings",
                get(move || {
                    let mut settings = Settings::default();
                    settings.values.insert(
                        SettingsKey::NotifyFaults,
                        serde_json::json!(settings_newest.load(Ordering::SeqCst) == 100),
                    );
                    async move { Json(settings) }
                }),
            )
            // A wake while a page is left; then the stream stays open.
            .route(
                "/events",
                get(move || {
                    let wake = !wake_pages.lock().unwrap().is_empty();
                    let wakes = wake
                        .then(|| Ok::<_, std::convert::Infallible>(SseEvent::default().data("{}")));
                    let stream =
                        futures_util::stream::iter(wakes).chain(futures_util::stream::pending());
                    async move { Sse::new(stream) }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let at = |id: u64, kind: EventKind| Event {
            id: EventId(id),
            ..event(kind)
        };
        let stuck = |id: u64| {
            at(
                id,
                EventKind::ChecksStuck {
                    task: TaskId::new("t"),
                    head: "h".into(),
                    check: "rust".into(),
                    url: Some("https://ci.example/rust/1".into()),
                    limit_minutes: 60,
                },
            )
        };
        for id in [100, 200] {
            newest.store(id, Ordering::SeqCst);
            let enable = at(
                id + 2,
                EventKind::SettingsChanged {
                    key: SettingsKey::NotifyFaults.as_str().into(),
                    old: serde_json::json!(false),
                    new: serde_json::json!(true),
                    restored_from: None,
                },
            );
            let merged = at(
                id + 5,
                EventKind::PrMerged {
                    task: TaskId::new("t"),
                    pr: 328,
                    sha: "head".into(),
                },
            );
            // Newest first; each page repeats an event the feed already has.
            *pages.lock().unwrap() = vec![
                vec![stuck(id + 3), enable, stuck(id + 1), stuck(id)],
                vec![merged, stuck(id + 4), stuck(id + 3)],
            ];
            let client = Client::new(sock.clone());
            let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
            let reader = tokio::spawn(async move { read_events(&client, &send).await });
            let mut next = async || {
                tokio::time::timeout(Duration::from_secs(5), recv.recv())
                    .await
                    .expect("event forwarded to notification consumer")
                    .unwrap()
            };
            let (first, settings) = next().await;
            assert_eq!(first.id, EventId(id + 1));
            assert_eq!(notice(&first, &settings).is_some(), id == 100);
            let (enabled, settings) = next().await;
            assert_eq!(enabled.id, EventId(id + 3));
            assert_eq!(
                notice(&enabled, &settings),
                Some(Notice {
                    title: "CI check stuck: rust".into(),
                    route: "/task/t".into(),
                    button: None,
                })
            );
            assert!(
                matches!(&enabled.kind, EventKind::ChecksStuck { url: Some(url), .. }
                if url == "https://ci.example/rust/1")
            );
            let (woken, settings) = next().await;
            assert_eq!(woken.id, EventId(id + 4));
            assert!(notice(&woken, &settings).is_some());
            let (merged, mut settings) = next().await;
            assert_eq!(merged.id, EventId(id + 5));
            assert!(notice(&merged, &settings).is_none());
            settings
                .values
                .insert(SettingsKey::NotifyCompleted, serde_json::json!(true));
            assert_eq!(notice(&merged, &settings).unwrap().title, "PR #328 merged");
            assert!(recv.try_recv().is_err());
            reader.abort();
        }
        server.abort();
    }

    #[test]
    fn questions_always_notify_and_preferences_control_faults_and_completions() {
        let mut settings = Settings::default();
        let question: EventKind = serde_json::from_value(serde_json::json!({
            "kind": "question.opened",
            "payload": {
                "question": "q",
                "subject": "Approve?",
                "format": "simple",
                "options": [],
                "recommended": 1,
                "lapses_on": { "goal_version": 1 }
            }
        }))
        .unwrap();
        assert_eq!(
            notice(&event(question.clone()), &settings).unwrap().route,
            "/decision/q"
        );
        let failed = event(EventKind::TaskFailed {
            task: dvb_core::TaskId::new("t"),
            reason: None,
        });
        assert_eq!(notice(&failed, &settings).unwrap().route, "/task/t");
        let stuck = event(EventKind::ChecksStuck {
            task: dvb_core::TaskId::new("t"),
            head: "h".into(),
            check: "rust".into(),
            url: None,
            limit_minutes: 60,
        });
        assert_eq!(
            notice(&stuck, &settings),
            Some(Notice {
                title: "CI check stuck: rust".into(),
                route: "/task/t".into(),
                button: None,
            })
        );
        let done = event(EventKind::GoalDone {
            goal: dvb_core::GoalId::new("g"),
        });
        assert!(notice(&done, &settings).is_none());
        settings.values.insert(
            dvb_core::api::SettingsKey::NotifyFaults,
            serde_json::json!(false),
        );
        settings.values.insert(
            dvb_core::api::SettingsKey::NotifyCompleted,
            serde_json::json!(true),
        );
        assert!(notice(&failed, &settings).is_none());
        assert!(notice(&stuck, &settings).is_none());
        assert_eq!(notice(&done, &settings).unwrap().route, "/project/p");
        assert!(notice(&event(question), &settings).is_some());
    }
}
