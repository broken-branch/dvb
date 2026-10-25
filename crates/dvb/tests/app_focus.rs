//! Offline recordings of Hyprland app classes and launch-only Omarchy commands.
use std::path::Path;
use std::process::Command;

fn script(dir: &Path, name: &str, body: &str) {
    executable::write_executable(&dir.join(name), body);
}

fn launch_for_clients(clients: &serde_json::Value, url: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("clients.json"), clients.to_string()).unwrap();
    script(
        dir.path(),
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
        dir.path(),
        "omarchy-launch-webapp",
        r#"#!/bin/sh
printf 'launch %s\n' "$1" >> "$FIXTURE_DIR/actions"
printf '%s\n' "$@" > "$FIXTURE_DIR/launch-args"
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .args(["app-launch", url, "--helper"])
        .arg(dir.path().join("omarchy-launch-webapp"))
        .env("PATH", dir.path())
        .env("FIXTURE_DIR", dir.path())
        .output()
        .unwrap();
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
    } else {
        assert!(!dir.path().join("launch-args").exists());
    }
    actions
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
