//! Owner-only desk control and detached browser launch.

use crate::Target;
use crate::client::{CliError, Client, message};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// One desk per engine socket, in the owner's runtime directory on an install.
pub fn control_socket(engine: &Path) -> PathBuf {
    if engine == Path::new(dvb_core::io::ENGINE_SOCKET) {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", dvb_core::io::own_uid())))
            .join("dvb-desk.sock")
    } else {
        engine.with_file_name("desk.sock")
    }
}

pub const DESK_PORT: u16 = 7300;

fn executable(name: &str) -> Option<PathBuf> {
    executable_in(name, &std::env::var_os("PATH").unwrap_or_default())
}
fn executable_in(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).map(|p| p.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The URL is created by the local desk and passed as one command argument.
pub fn launch(url: &str) -> Result<(), CliError> {
    if !has_display() {
        return Err(CliError::Message(format!(
            "no display (DISPLAY/WAYLAND_DISPLAY); cockpit: {url}"
        )));
    }
    let helper = executable("omarchy-launch-webapp");
    let browser = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ]
    .into_iter()
    .find_map(executable);
    let child = if let Some(helper) = helper {
        // A separate process survives the short-lived CLI. A thread would stop
        // monitoring as soon as `dvb app` exits.
        let mut command = Command::new(std::env::current_exe().map_err(message)?);
        command
            .args(["app-launch", url])
            .arg("--helper")
            .arg(helper);
        if let Some(browser) = browser {
            command.arg("--browser").arg(browser);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map(Some)
            .map_err(message)
    } else {
        launch_with(
            url,
            None,
            browser.as_deref(),
            executable("hyprctl").as_deref(),
        )
    }
    .map_err(|error| CliError::Message(format!("{error}; cockpit: {url}")))?;
    // The supervisor keeps monitoring a helper that outlives this check.
    check_startup(child, url)
}

fn check_startup(child: Option<std::process::Child>, url: &str) -> Result<(), CliError> {
    if let Some(mut child) = child {
        for _ in 0..20 {
            match child
                .try_wait()
                .map_err(|e| CliError::Message(format!("{e}; cockpit: {url}")))?
            {
                Some(status) if status.success() => return Ok(()),
                Some(_) => {
                    return Err(CliError::Message(format!(
                        "could not open a window; check the display; cockpit: {url}"
                    )));
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    Ok(())
}

fn has_display() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn validate_url(url: &str) -> Result<u16, CliError> {
    if !url.starts_with("http://127.0.0.1:")
        || url
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !":/?.=&-_".contains(c))
    {
        return Err(CliError::Message("invalid desk launch URL".into()));
    }
    url.parse::<hyper::Uri>()
        .ok()
        .and_then(|uri| uri.port_u16())
        .ok_or_else(|| CliError::Message("invalid desk launch URL".into()))
}

fn focus_app(profile: &str, hyprctl: &Path) -> bool {
    // Chromium's default app name omits the port. The URL-specific profile
    // supplies it in the native Wayland app id, independently of token/route.
    // Do not use Omarchy's class-OR-title matcher, even with an anchored regex.
    #[derive(serde::Deserialize)]
    struct Window {
        class: String,
        address: String,
    }
    let Ok(output) = Command::new(hyprctl)
        .args(["clients", "-j"])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(windows) = serde_json::from_slice::<Vec<Window>>(&output.stdout) else {
        return false;
    };
    let suffix = format!("-127.0.0.1__launch-{profile}");
    let Some(window) = windows.into_iter().find(|window| {
        window.class.strip_suffix(&suffix).is_some_and(|browser| {
            matches!(
                browser,
                "chrome" | "chromium" | "brave" | "msedge" | "opera" | "vivaldi" | "helium"
            )
        }) && window
            .address
            .strip_prefix("0x")
            .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    }) else {
        return false;
    };
    Command::new(hyprctl)
        .args([
            "dispatch",
            "focuswindow",
            &format!("address:{}", window.address),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Internal detached startup supervisor, invoked by the one dvb binary.
pub fn supervise_launch(
    url: &str,
    helper: Option<&Path>,
    browser: Option<&Path>,
) -> Result<(), CliError> {
    let child = launch_with(url, helper, browser, executable("hyprctl").as_deref())?;
    check_startup(child, url)
}

fn launch_with(
    url: &str,
    helper: Option<&Path>,
    browser: Option<&Path>,
    hyprctl: Option<&Path>,
) -> Result<Option<std::process::Child>, CliError> {
    let port = validate_url(url)?;
    let profile = format!("dvb-127.0.0.1-{port}");
    if hyprctl.is_some_and(|hyprctl| focus_app(&profile, hyprctl)) {
        return Ok(None);
    }
    let profile_arg = format!("--profile-directory={profile}");
    if let Some(helper) = helper {
        // Watch five seconds of startup in the detached supervisor, then leave
        // a still-running helper alone. It may stay attached for the browser's
        // lifetime. No process group of our own: Omarchy's `setsid` would fork
        // and hide startup failure.
        let mut child = Command::new(helper)
            .args([url, &profile_arg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(message)?;
        for _ in 0..500 {
            match child.try_wait().map_err(message)? {
                Some(status) if status.success() => return Ok(None),
                Some(_) => break,
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        match child.try_wait().map_err(message)? {
            Some(status) if status.success() => return Ok(None),
            Some(_) => {}
            None => return Ok(None),
        }
    }
    let browser =
        browser.ok_or_else(|| CliError::Message("install Chromium to open dvb".into()))?;
    let child = Command::new(browser)
        .arg(format!("--app={url}"))
        .arg(profile_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(message)?;
    Ok(Some(child))
}

/// A local app path, without a query, fragment or URL authority.
pub fn parse_route(route: &str) -> Result<String, String> {
    if route.starts_with('/')
        && !route.starts_with("//")
        && route.len() <= 200
        && route
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/-_".contains(c))
    {
        Ok(route.to_owned())
    } else {
        Err("route must be a plain path starting with / (for example /setup)".into())
    }
}

/// `route` is a `parse_route` path: clap checks it, and the desk again.
pub async fn open(target: &Target, route: Option<&str>) -> Result<(), CliError> {
    let path = route.map_or_else(|| "/app".to_owned(), |route| format!("/app?route={route}"));
    launch(&desk_url(target, &path).await?)
}

/// The port of this engine's desk, which serves GitHub's setup callback.
/// Starts the desk as `dvb app` does when it is not running.
pub async fn desk_port(target: &Target) -> Result<u16, CliError> {
    validate_url(&desk_url(target, "/app").await?)
}

/// A launch URL from the running desk, or from a desk started for it.
async fn desk_url(target: &Target, path: &str) -> Result<String, CliError> {
    let engine = target.sock();
    let control = Client::new(control_socket(&engine));
    if let Ok(url) = control.get::<String>(path).await {
        return Ok(url);
    }
    // The login service may not be running (notably in dev mode). Keep this
    // child alive when the window closes; the desk owns no engine work.
    let binary = std::env::current_exe().map_err(message)?;
    let mut child = Command::new(binary)
        .arg("--sock")
        .arg(engine)
        .arg("desk")
        .arg("--port")
        .arg(DESK_PORT.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| desk_unavailable())?;
    for _ in 0..100 {
        if let Ok(url) = control.get::<String>(path).await {
            return Ok(url);
        }
        if child.try_wait().map_err(message)?.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if child.try_wait().map_err(message)?.is_none() {
        child.kill().map_err(message)?;
        child.wait().map_err(message)?;
    }
    Err(desk_unavailable())
}

fn desk_unavailable() -> CliError {
    let display = if has_display() { "" } else { "no display; " };
    CliError::Message(format!(
        "{display}the desk is unavailable; in your desktop session run: systemctl --user start dvb-desk.service"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn app_without_a_display_prints_url_and_exits_nonzero() {
        const CASE: &str = "DVB_TEST_HEADLESS_APP";
        if let Some(sock) = std::env::var_os(CASE) {
            let code = crate::main(
                &Target {
                    sock: Some(sock.into()),
                    dev: None,
                },
                crate::Verb::App { route: None },
            )
            .await;
            std::process::exit(code);
        }
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(control_socket(&sock)).unwrap();
        let url = "http://127.0.0.1:7300/launch?token=offline";
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/app",
                    axum::routing::get(move || async move { axum::Json(url) }),
                ),
            )
            .await
            .unwrap();
        });
        let marker = dir.path().join("browser-ran");
        script(
            dir.path(),
            "chromium",
            &format!("#!/bin/sh\nprintf ran > '{}'\nexit 1\n", marker.display()),
        );
        for (display, wayland) in [
            (None, None),
            (Some(""), Some("")),
            (Some(":offline"), None),
            (None, Some("offline")),
        ] {
            let bin_dir = dir.path().to_owned();
            let sock = sock.clone();
            let output = tokio::task::spawn_blocking(move || {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "app::tests::app_without_a_display_prints_url_and_exits_nonzero",
                        "--nocapture",
                    ])
                    .env(CASE, sock)
                    .env("PATH", bin_dir)
                    .env_remove("DISPLAY")
                    .env_remove("WAYLAND_DISPLAY");
                if let Some(display) = display {
                    command.env("DISPLAY", display);
                }
                if let Some(wayland) = wayland {
                    command.env("WAYLAND_DISPLAY", wayland);
                }
                command.output().unwrap()
            })
            .await
            .unwrap();
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            let error = String::from_utf8(output.stderr).unwrap();
            let has_display =
                display.is_some_and(|v| !v.is_empty()) || wayland.is_some_and(|v| !v.is_empty());
            let reason = if has_display {
                "could not open a window"
            } else {
                "no display"
            };
            assert!(error.contains(reason), "{error}");
            assert!(error.contains(url), "{error}");
            assert_eq!(marker.exists(), has_display);
        }
        server.abort();
    }

    #[tokio::test]
    async fn app_setup_without_a_desk_prints_one_fix_and_exits_nonzero() {
        const CASE: &str = "DVB_TEST_MISSING_DESK";
        if let Some(sock) = std::env::var_os(CASE) {
            let code = crate::main(
                &Target {
                    sock: Some(sock.into()),
                    dev: None,
                },
                crate::Verb::App {
                    route: Some("/setup".into()),
                },
            )
            .await;
            std::process::exit(code);
        }
        let dir = tempfile::tempdir().unwrap();
        // No desk socket or session bus. The test binary also cannot serve a desk.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::tests::app_setup_without_a_desk_prints_one_fix_and_exits_nonzero",
                "--nocapture",
            ])
            .env(CASE, dir.path().join("missing/engine.sock"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/dvb-test-bus",
            )
            .env("XDG_RUNTIME_DIR", dir.path())
            .env("DISPLAY", ":offline")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            error,
            "dvb: the desk is unavailable; in your desktop session run: systemctl --user start dvb-desk.service\n"
        );
    }

    #[test]
    fn parse_route_passes_paths_and_rejects_non_paths() {
        for route in ["/", "/setup", "/task/my_task-1"] {
            assert_eq!(parse_route(route).unwrap(), route);
        }
        for route in [
            "",
            "setup",
            "https://evil.test",
            "//evil.test",
            "/setup?x=1",
            "/setup#x",
            "/setup&token=bad",
            "/a b",
            "/a\\b",
            "/../setup",
        ] {
            assert!(parse_route(route).is_err(), "{route}");
        }
    }

    fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        executable::write_executable(&path, text);
        path
    }

    #[test]
    fn supervisor_reports_fallback_browser_startup_failure() {
        let dir = tempfile::tempdir().unwrap();
        let helper = script(dir.path(), "helper", "#!/bin/sh\nexit 1\n");
        let marker = dir.path().join("browser-ran");
        let browser = script(
            dir.path(),
            "browser",
            &format!("#!/bin/sh\nprintf ran > '{}'\nexit 1\n", marker.display()),
        );
        let url = "http://127.0.0.1:7300/launch?token=offline";
        let result = supervise_launch(url, Some(&helper), Some(&browser));
        assert!(result.is_err(), "{result:?}");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("could not open a window"), "{error}");
        assert!(error.contains(url), "{error}");
        assert!(marker.exists(), "the fallback browser must run");
    }

    #[test]
    fn launches_with_omarchy_and_falls_back_to_a_chromium_app() {
        let dir = tempfile::tempdir().unwrap();
        let helper = script(
            dir.path(),
            "helper",
            "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = \
                 'http://127.0.0.1:7300/launch?token=abc&route=/setup' ] && [ \"$2\" = \
                 '--profile-directory=dvb-127.0.0.1-7300' ]\n",
        );
        let url = "http://127.0.0.1:7300/launch?token=abc&route=/setup";
        assert!(
            launch_with(url, Some(&helper), None, None)
                .unwrap()
                .is_none()
        );
        let fail = script(dir.path(), "fail", "#!/bin/sh\nexit 1\n");
        let browser = script(
            dir.path(),
            "browser",
            "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = \
                 '--app=http://127.0.0.1:7300/launch?token=abc&route=/setup' ] && [ \
                 \"$2\" = '--profile-directory=dvb-127.0.0.1-7300' ]\n",
        );
        let mut child = launch_with(url, Some(&fail), Some(&browser), None)
            .unwrap()
            .unwrap();
        assert!(child.wait().unwrap().success());
        let mut child = launch_with(url, None, Some(&browser), None)
            .unwrap()
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(launch_with(url, None, None, None).is_err());
        for url in [
            "http://evil.test/",
            "http://127.0.0.1:bad/launch",
            "http://127.0.0.1:7300/launch?token='bad'",
            "http://127.0.0.1:7300/launch?token=$(bad)",
            "http://127.0.0.1:7300/launch?token=bad;command",
        ] {
            assert!(launch_with(url, Some(&helper), Some(&browser), None).is_err());
        }
    }
    #[test]
    fn an_omarchy_style_setsid_launch_failure_falls_back_to_chromium() {
        let dir = tempfile::tempdir().unwrap();
        let helper = script(
            dir.path(),
            "helper",
            "#!/bin/sh\nexec setsid /nonexistent/launch\n",
        );
        let marker = dir.path().join("browser-ran");
        let browser = script(
            dir.path(),
            "browser",
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let url = "http://127.0.0.1:7300/launch?token=abc";
        let mut child = launch_with(url, Some(&helper), Some(&browser), None)
            .unwrap()
            .expect("falls back to the browser");
        assert!(child.wait().unwrap().success());
        assert!(marker.exists());
    }
    #[test]
    fn a_delayed_launcher_failure_falls_back_to_chromium() {
        let dir = tempfile::tempdir().unwrap();
        let helper = script(dir.path(), "helper", "#!/bin/sh\nsleep 0.6\nexit 1\n");
        let marker = dir.path().join("browser-ran");
        let browser = script(
            dir.path(),
            "browser",
            &format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", marker.display()),
        );
        let url = "http://127.0.0.1:7300/launch?token=abc";
        let mut child = launch_with(url, Some(&helper), Some(&browser), None)
            .unwrap()
            .expect("delayed failure must fall back");
        assert!(child.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            format!("--app={url}")
        );
    }

    #[test]
    fn supervision_stops_while_the_helper_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let helper = script(
            dir.path(),
            "helper",
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pidfile.display()
            ),
        );
        let url = "http://127.0.0.1:7300/launch?token=abc";
        assert!(
            launch_with(url, Some(&helper), None, None)
                .unwrap()
                .is_none()
        );
        // The helper writes its pid after launch returns.
        let pid = (0..500)
            .find_map(|_| {
                let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
                if pid.trim().is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    return None;
                }
                Some(pid.trim().to_owned())
            })
            .expect("the helper wrote its pid");
        let proc = PathBuf::from("/proc").join(&pid);
        assert!(proc.exists(), "helper is still running");
        let _ = Command::new("kill").arg(&pid).status();
    }
    #[test]
    fn control_paths_and_executable_permissions_are_checked() {
        assert_eq!(
            control_socket(Path::new("/tmp/engine.sock")),
            PathBuf::from("/tmp/desk.sock")
        );
        let expected = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", dvb_core::io::own_uid())))
            .join("dvb-desk.sock");
        assert_eq!(
            control_socket(Path::new(dvb_core::io::ENGINE_SOCKET)),
            expected
        );
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "test-bin", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            executable_in("test-bin", dir.path().as_os_str()),
            Some(binary.clone())
        );
        for mode in [0o000, 0o644, 0o600] {
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(executable_in("test-bin", dir.path().as_os_str()), None);
        }
        std::fs::remove_file(&binary).unwrap();
        std::fs::create_dir(&binary).unwrap();
        assert_eq!(executable_in("test-bin", dir.path().as_os_str()), None);
        assert_eq!(executable_in("missing", dir.path().as_os_str()), None);
    }
}

#[cfg(test)]
#[path = "../../../tests/support/executable.rs"]
mod executable;
