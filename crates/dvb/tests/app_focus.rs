//! Offline recordings of Hyprland app classes and launch-only Omarchy commands.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn script(dir: &Path, name: &str, body: &str) {
    executable::write_executable(&dir.join(name), body);
}

/// A fake Hyprland with these clients and a helper that records its arguments
/// and those of its parent.
fn fixture(dir: &Path, clients: &serde_json::Value) {
    std::fs::write(dir.join("clients.json"), clients.to_string()).unwrap();
    script(
        dir,
        "hyprctl",
        r#"#!/bin/sh
case "$1" in
clients) [ "$2" = -j ] || exit 1; /bin/cat "$FIXTURE_DIR/clients.json" ;;
dispatch) shift; printf 'focus %s\n' "$*" >> "$FIXTURE_DIR/actions" ;;
*) exit 1 ;;
esac
"#,
    );
    script(
        dir,
        "omarchy-launch-webapp",
        r#"#!/bin/sh
printf 'launch %s\n' "$1" >> "$FIXTURE_DIR/actions"
printf '%s\n' "$@" > "$FIXTURE_DIR/launch-args"
/bin/cat "/proc/$PPID/cmdline" > "$FIXTURE_DIR/parent"
/bin/mv "$FIXTURE_DIR/parent" "$FIXTURE_DIR/supervisor-args"
"#,
    );
}

fn launch_for_clients(clients: &serde_json::Value, url: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), clients);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .args(["app-launch", "--helper"])
        .arg(dir.path().join("omarchy-launch-webapp"))
        .env("PATH", dir.path())
        .env("FIXTURE_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{url}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let actions = std::fs::read_to_string(dir.path().join("actions")).unwrap();
    if actions.starts_with("launch ") {
        let port = url
            .strip_prefix("http://127.0.0.1:")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("launch-args")).unwrap(),
            format!("{url}\n--profile-directory=dvb-127.0.0.1-{port}\n"),
        );
        assert_eq!(output.stdout, b"started\n");
        assert_no_token(&dir.path().join("supervisor-args"));
    } else {
        assert!(!dir.path().join("launch-args").exists());
        assert_eq!(output.stdout, b"focused\n");
    }
    actions
}

fn assert_no_token(args: &Path) {
    let args = String::from_utf8(std::fs::read(args).unwrap()).unwrap();
    assert!(args.contains("app-launch"), "{args:?}");
    assert!(!args.contains("token"), "{args:?}");
}

/// Runs `dvb app` against a fake desk control socket; the requests it made.
async fn app_with_desk(
    clients: serde_json::Value,
    url: &'static str,
) -> (Vec<String>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), &clients);
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = requests.clone();
    let listener = tokio::net::UnixListener::bind(dir.path().join("desk.sock")).unwrap();
    let desk = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/app",
                axum::routing::get(move || async move { axum::Json(url) }).delete(
                    move |uri: axum::http::Uri| async move {
                        seen.lock().unwrap().push(uri.to_string());
                        axum::http::StatusCode::NO_CONTENT
                    },
                ),
            ),
        )
        .await
        .unwrap();
    });
    let fixture_dir = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_dvb"))
            .arg("--sock")
            .arg(fixture_dir.join("engine.sock"))
            .arg("app")
            .env("PATH", &fixture_dir)
            .env("FIXTURE_DIR", &fixture_dir)
            .env("WAYLAND_DISPLAY", "wayland-test")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    desk.abort();
    assert!(output.status.success(), "{output:?}");
    let requests = requests.lock().unwrap().clone();
    (requests, dir)
}

#[tokio::test]
async fn dvb_app_passes_the_url_to_its_supervisor_off_the_argument_list() {
    let url = "http://127.0.0.1:7300/launch?token=secret";
    let (withdrawn, dir) = app_with_desk(serde_json::json!([]), url).await;
    assert!(withdrawn.is_empty(), "{withdrawn:?}");
    // `dvb app` stops watching the supervisor after its short startup check.
    let recorded = dir.path().join("supervisor-args");
    for _ in 0..500 {
        if recorded.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("actions")).unwrap(),
        format!("launch {url}\n"),
    );
    assert_no_token(&recorded);
}

#[tokio::test]
async fn dvb_app_withdraws_the_link_when_it_only_focuses_the_open_cockpit() {
    let clients = serde_json::json!([
        {"class": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7300", "title": "Cockpit", "address": "0x222"},
    ]);
    let url = "http://127.0.0.1:7300/launch?token=secret&route=/setup";
    let (withdrawn, dir) = app_with_desk(clients, url).await;
    assert_eq!(withdrawn, ["/app?token=secret"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("actions")).unwrap(),
        "focus focuswindow address:0x222\n",
    );
}

#[test]
fn dvb_notes_title_launches_the_app() {
    assert_title_launches("DVB notes");
}

#[test]
fn dvb_ghostty_title_launches_the_app() {
    assert_title_launches("dvb - Ghostty");
}

#[test]
fn loopback_title_launches_the_app() {
    assert_title_launches("dvb http://127.0.0.1:7300/launch");
    assert_title_launches("127.0.0.1:7300 connection notes");
}

#[test]
fn an_app_class_in_an_editor_title_launches_the_app() {
    let url = "http://127.0.0.1:7300/launch?token=abc&route=/setup";
    assert_eq!(
        launch_for_clients(
            &serde_json::json!([
                {
                    "class": "code",
                    "title": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7300",
                    "address": "0x111"
                }
            ]),
            url,
        ),
        format!("launch {url}\n"),
    );
}

#[test]
fn the_existing_app_class_is_focused_without_a_second_launch() {
    let clients = serde_json::json!([
        {"class": "com.mitchellh.ghostty", "title": "DVB notes", "address": "0x111"},
        {"class": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7300", "title": "Cockpit", "address": "0x222"},
    ]);
    for url in [
        "http://127.0.0.1:7300/launch?token=old",
        "http://127.0.0.1:7300/launch?token=new&route=/task/one",
    ] {
        assert_eq!(
            launch_for_clients(&clients, url),
            "focus focuswindow address:0x222\n",
        );
    }
}

#[test]
fn another_desk_port_launches_its_own_app() {
    let url = "http://127.0.0.1:7400/launch?token=abc&route=/setup";
    assert_eq!(
        launch_for_clients(
            &serde_json::json!([
                {
                    "class": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7300",
                    "title": "dvb",
                    "address": "0x222"
                }
            ]),
            url,
        ),
        format!("launch {url}\n"),
    );
    assert_eq!(
        launch_for_clients(
            &serde_json::json!([
                {"class": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7300", "title": "dvb", "address": "0x222"},
                {"class": "chrome-127.0.0.1__launch-dvb-127.0.0.1-7400", "title": "Cockpit", "address": "0x333"},
            ]),
            url,
        ),
        "focus focuswindow address:0x333\n",
    );
    // The recorded default app class has no port. It cannot safely identify
    // this desk; neither an ambiguous class nor unescaped regex lookalikes fit.
    for class in [
        "chrome-127.0.0.1__launch-Default",
        "chrome-127x0x0x1__launch-dvb-127x0x0x1-7400",
        "code-127.0.0.1__launch-dvb-127.0.0.1-7400",
    ] {
        assert_eq!(
            launch_for_clients(
                &serde_json::json!([{"class": class, "title": "dvb", "address": "0x222"}]),
                url,
            ),
            format!("launch {url}\n"),
        );
    }
}

fn assert_title_launches(title: &str) {
    let url = "http://127.0.0.1:7300/launch?token=abc&route=/setup";
    assert_eq!(
        launch_for_clients(
            &serde_json::json!([{"class": "com.mitchellh.ghostty", "title": title, "address": "0x111"}]),
            url,
        ),
        format!("launch {url}\n"),
    );
}

#[path = "../../../tests/support/executable.rs"]
mod executable;
