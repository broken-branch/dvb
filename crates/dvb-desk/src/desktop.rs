//! The owner's own copy of the desktop entry and app icon. `dvb update` replaces
//! `/opt/dvb/current` but never reruns install.sh, so the system copies stay as
//! the first install left them; the copies in the owner's data directory take
//! precedence and follow the running binary.

use std::path::{Path, PathBuf};

const ENTRY: &[u8] = include_bytes!("../../../install/dvb.desktop");
const ICONS: &[(&str, &[u8])] = &[
    (
        "16x16/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-16.png"),
    ),
    (
        "32x32/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-32.png"),
    ),
    (
        "48x48/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-48.png"),
    ),
    (
        "64x64/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-64.png"),
    ),
    (
        "128x128/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-128.png"),
    ),
    (
        "256x256/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-256.png"),
    ),
    (
        "512x512/apps/dvb.png",
        include_bytes!("../../../ui/public/icons/dvb-512.png"),
    ),
    (
        "scalable/apps/dvb.svg",
        include_bytes!("../../../ui/public/icons/dvb.svg"),
    ),
];

/// `$XDG_DATA_HOME`, or `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share")))
}

/// Writes only what differs. The icons go first, so the entry naming them never
/// appears without them.
pub fn refresh(data: &Path) -> std::io::Result<()> {
    let theme = data.join("icons/hicolor");
    if ICONS.iter().any(|(path, bytes)| {
        !std::fs::read(theme.join(path)).is_ok_and(|current| current == *bytes)
    }) {
        // Existing size/apps directories leave the theme's mtime unchanged, so
        // GTK can trust a cache that omits the new icons. Invalidate it before
        // writing icons so a failure leaves the next refresh able to retry.
        let cache = theme.join("icon-theme.cache");
        match std::fs::remove_file(&cache) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("cannot invalidate {}: {e}", cache.display()),
                ));
            }
        }
    }
    let icons = ICONS.iter().map(|(path, bytes)| (theme.join(path), *bytes));
    for (path, bytes) in icons.chain([(data.join("applications/dvb.desktop"), ENTRY)]) {
        if std::fs::read(&path).is_ok_and(|current| current == bytes) {
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Launchers watch these directories: they see a whole file or none.
        let partial = path.with_extension("partial");
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_installs_the_entry_and_every_icon_and_replaces_an_old_entry() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("applications/dvb.desktop");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, "[Desktop Entry]\nName=dvb\n").unwrap();
        refresh(dir.path()).unwrap();
        let text = std::fs::read_to_string(&entry).unwrap();
        assert!(text.lines().any(|line| line == "Icon=dvb"));
        assert!(
            text.lines()
                .any(|line| line == "StartupWMClass=chromium-127.0.0.1__launch-dvb-127.0.0.1-7300")
        );
        let hicolor = dir.path().join("icons/hicolor");
        for size in [16, 32, 48, 64, 128, 256, 512] {
            let png = std::fs::read(hicolor.join(format!("{size}x{size}/apps/dvb.png"))).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            // The IHDR width, big-endian at byte 16: each file is its folder's size.
            assert_eq!(png[16..20], u32::to_be_bytes(size));
        }
        let svg = std::fs::read_to_string(hicolor.join("scalable/apps/dvb.svg")).unwrap();
        assert!(svg.contains("<svg"));
        let names = walk(dir.path());
        assert!(
            names.iter().all(|name| !name.ends_with(".partial")),
            "{names:?}"
        );
        assert_eq!(names.len(), ICONS.len() + 1);
        // Current files are left as they are: a rewrite would be a new inode.
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(&entry).unwrap().ino();
        refresh(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(&entry).unwrap().ino(), before);
    }

    #[test]
    fn refresh_invalidates_an_existing_theme_cache_when_icons_change() {
        let dir = tempfile::tempdir().unwrap();
        let theme = dir.path().join("icons/hicolor");
        // Cached themes already have these directories: adding icons need not
        // change the theme directory's mtime and make GTK distrust its cache.
        for (path, _) in ICONS {
            std::fs::create_dir_all(theme.join(path).parent().unwrap()).unwrap();
        }
        let cache = theme.join("icon-theme.cache");
        std::fs::write(&cache, b"cached theme without dvb").unwrap();

        refresh(dir.path()).unwrap();
        assert!(!cache.exists());

        // A rebuilt cache stays valid when only the desktop entry changes.
        std::fs::write(&cache, b"rebuilt theme with dvb").unwrap();
        std::fs::write(dir.path().join("applications/dvb.desktop"), b"old entry").unwrap();
        refresh(dir.path()).unwrap();
        assert_eq!(std::fs::read(&cache).unwrap(), b"rebuilt theme with dvb");

        // Replacing a previous icon also invalidates the cache.
        std::fs::write(theme.join(ICONS[0].0), b"old icon").unwrap();
        refresh(dir.path()).unwrap();
        assert!(!cache.exists());
        assert_eq!(std::fs::read(theme.join(ICONS[0].0)).unwrap(), ICONS[0].1);
    }

    #[test]
    fn refresh_reports_cache_invalidation_errors_before_writing_icons() {
        let dir = tempfile::tempdir().unwrap();
        let theme = dir.path().join("icons/hicolor");
        let cache = theme.join("icon-theme.cache");
        // A directory cannot be unlinked as a cache file, even as root.
        std::fs::create_dir_all(&cache).unwrap();
        assert!(refresh(dir.path()).is_err());
        assert!(!theme.join(ICONS[0].0).exists());
        assert!(!dir.path().join("applications/dvb.desktop").exists());

        std::fs::remove_dir(&cache).unwrap();
        refresh(dir.path()).unwrap();
        assert_eq!(std::fs::read(theme.join(ICONS[0].0)).unwrap(), ICONS[0].1);
    }

    fn walk(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flat_map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path)
                } else {
                    vec![path.display().to_string()]
                }
            })
            .collect()
    }
}
