//! Read the current file on each request: Omarchy replaces theme symlinks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FALLBACK: &[(&str, &str)] = &[
    ("mode", "dark"),
    ("background", "#1C1E26"),
    ("foreground", "#CBCED0"),
    ("accent", "#26BBD9"),
    ("red", "#E95678"),
    ("green", "#29D398"),
    ("yellow", "#FAC29A"),
    ("blue", "#26BBD9"),
    ("magenta", "#EE64AC"),
    ("cyan", "#59E1E3"),
    ("lighter_background", "#232530"),
    ("dark_background", "#15171D"),
    ("darker_background", "#0E0F13"),
    ("selection", "#2E303E"),
];

pub fn colors_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state_path = state.join("omarchy/current/theme/colors.toml");
    if state_path.exists() || !config.join("omarchy/current/theme/colors.toml").exists() {
        Some(state_path)
    } else {
        Some(config.join("omarchy/current/theme/colors.toml"))
    }
}

fn hex_colour(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn render(text: &str) -> String {
    let mut colors: BTreeMap<String, String> = FALLBACK
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if let Ok(table) = text.parse::<toml::Table>() {
        for (key, value) in &table {
            let Some(value) = value.as_str() else {
                continue;
            };
            if colors.contains_key(key)
                && ((key == "mode" && ["dark", "light"].contains(&value))
                    || (key != "mode" && hex_colour(value)))
            {
                colors.insert(key.clone(), value.to_string());
            }
        }
        for (role, alias, weight) in [
            ("lighter_background", "lighter_bg", 0.045),
            ("dark_background", "dark_bg", 0.015),
            ("darker_background", "darker_bg", 0.0),
            ("selection", "selection", 0.12),
        ] {
            if !table.contains_key(role)
                && (table.contains_key(alias)
                    || table.contains_key("background")
                    || table.contains_key("foreground"))
            {
                let value = table
                    .get(alias)
                    .and_then(toml::Value::as_str)
                    .filter(|v| hex_colour(v))
                    .map(str::to_owned)
                    .unwrap_or_else(|| mix(&colors["background"], &colors["foreground"], weight));
                colors.insert(role.into(), value);
            }
        }
        // Standard Omarchy themes may carry only the ANSI status colours.
        for (role, ansi) in [
            ("red", "color1"),
            ("green", "color2"),
            ("yellow", "color3"),
            ("blue", "color4"),
            ("magenta", "color5"),
            ("cyan", "color6"),
        ] {
            if !table.contains_key(role)
                && let Some(value) = table.get(ansi).and_then(toml::Value::as_str)
                && hex_colour(value)
            {
                colors.insert(role.into(), value.into());
            }
        }
    }
    format!(
        ":root {{\n{}\n}}\n",
        colors
            .into_iter()
            .map(|(k, v)| format!("  --dvb-{k}: {v};"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Both colours are `#rrggbb`: the fallback's or ones `hex_colour` accepted.
fn mix(bg: &str, fg: &str, weight: f64) -> String {
    let channel = |colour: &str, i: usize| {
        f64::from(
            colour
                .get(i..i + 2)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .unwrap_or_default(),
        )
    };
    let mut out = String::from("#");
    for i in [1, 3, 5] {
        out.push_str(&format!(
            "{:02x}",
            (channel(bg, i) * (1.0 - weight) + channel(fg, i) * weight).round() as u8
        ));
    }
    out
}

pub async fn css(path: Option<&Path>) -> String {
    let text = match path {
        Some(path) => tokio::fs::read_to_string(path).await.unwrap_or_default(),
        None => String::new(),
    };
    render(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_bad_theme_uses_event_horizon_and_never_injects_css() {
        assert!(render("").contains("--dvb-accent: #26BBD9"));
        assert_eq!(render("accent = 'red; } body { display:none'"), render(""));
        assert_eq!(render("not toml"), render(""));
    }
    #[test]
    fn reads_light_theme_and_ansi_status_colors() {
        let css = render(
            "mode = 'light'\nbackground = '#ffffff'\nforeground = '#111111'\naccent = '#112233'\ncolor1 = '#ff0000'",
        );
        assert!(css.contains("--dvb-mode: light"));
        assert!(css.contains("--dvb-background: #ffffff"));
        assert!(css.contains("--dvb-red: #ff0000"));
    }
    #[tokio::test]
    async fn observes_theme_symlink_replacement_and_missing_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.toml");
        let two = dir.path().join("two.toml");
        let current = dir.path().join("colors.toml");
        std::fs::write(&one, "accent = '#112233'").unwrap();
        std::fs::write(&two, "accent = '#445566'").unwrap();
        symlink(&one, &current).unwrap();
        let theme = || css(Some(&current));
        assert!(theme().await.contains("--dvb-accent: #112233"));
        std::fs::remove_file(&current).unwrap();
        symlink(&two, &current).unwrap();
        assert!(theme().await.contains("--dvb-accent: #445566"));
        std::fs::remove_file(&current).unwrap();
        assert_eq!(theme().await, render(""));
        std::fs::write(&current, "not toml").unwrap();
        assert_eq!(theme().await, render(""));
        std::fs::write(&current, [0xff]).unwrap();
        assert_eq!(theme().await, render(""));
        std::fs::remove_file(&current).unwrap();
        std::fs::create_dir(&current).unwrap();
        assert_eq!(theme().await, render(""));
        assert_eq!(css(None).await, render(""));
    }
}
