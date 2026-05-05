use crate::desktop::dirs::search_desktop_dirs;
use crate::desktop::entry::parse_desktop_file;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Resolves the icon name for an application.
///
/// Searches .desktop files in the given app directories to find the Icon field.
pub fn get_icon(app_name: &str, app_dirs: &[PathBuf]) -> Option<String> {
    // Special case for GIMP
    if app_name.to_uppercase().starts_with("GIMP") {
        return Some("gimp".to_string());
    }

    let desktop_path = find_desktop_file(app_name, app_dirs)?;
    let entry = parse_desktop_file(app_name, &desktop_path).ok()?;
    if entry.icon.is_empty() {
        None
    } else {
        Some(entry.icon)
    }
}

/// Resolves the Exec command for an application.
pub fn get_exec(app_name: &str, app_dirs: &[PathBuf]) -> Option<String> {
    let cmd = if app_name.to_uppercase().starts_with("GIMP") {
        "gimp"
    } else {
        app_name
    };

    let desktop_path = find_desktop_file(app_name, app_dirs)?;
    let entry = parse_desktop_file(app_name, &desktop_path).ok()?;

    if entry.exec.is_empty() {
        return Some(cmd.to_string());
    }

    Some(super::entry::strip_field_codes(&entry.exec))
}

/// Resolves the display name for an application.
///
/// Returns the locale-aware name (e.g. `Name[pl]` for Polish) if available,
/// falling back to the base `Name` field, then the raw app class name.
pub fn get_name(app_name: &str, app_dirs: &[PathBuf]) -> String {
    if let Some(desktop_path) = find_desktop_file(app_name, app_dirs)
        && let Ok(entry) = parse_desktop_file(app_name, &desktop_path)
    {
        if !entry.name_loc.is_empty() {
            return entry.name_loc;
        }
        if !entry.name.is_empty() {
            return entry.name;
        }
    }
    app_name.to_string()
}

/// Finds a .desktop file for the given app name.
fn find_desktop_file(app_name: &str, app_dirs: &[PathBuf]) -> Option<PathBuf> {
    // Try exact match, lowercase, and hyphen↔space variant
    let variant = if app_name.contains('-') {
        app_name.replace('-', " ")
    } else {
        app_name.replace(' ', "-")
    };
    for dir in app_dirs {
        for name in [
            app_name,
            &app_name.to_lowercase(),
            &variant,
            &variant.to_lowercase(),
        ] {
            let path = dir.join(format!("{}.desktop", name));
            if path.exists() {
                return Some(path);
            }
        }
    }

    // Fall back to fuzzy search
    if !app_name.starts_with('/') {
        search_desktop_dirs(app_name, app_dirs)
    } else {
        None
    }
}

// Pixbuf cache rationale.
//
// `gdk_pixbuf_new_from_file_at_scale` delegates to glycin in modern
// gdk-pixbuf builds, and glycin leaks a few KiB of decoder state per
// call (jasonherald/nwg-dock#83). On a long-running dock that rebuilds
// icons on every focus event, that compounded to a 15.9 GiB peak over
// 2.5 days uptime — heaptrack confirmed the leak is per-decode.
//
// Caching by `(icon, size)` / `(path, w, h)` means we only invoke the
// glycin path once per unique input. `gtk4::gdk_pixbuf::Pixbuf` is a
// GObject; `clone()` is a refcount bump (g_object_ref) — cheap. The
// cache holds its own ref forever, callers receive a refcount-bumped
// clone.
//
// Bounded by `unique_inputs` — typical dock has ~10 icons × ~5 sizes
// (icon-size scaling kicks in as item count grows) plus ~6 indicator
// SVGs × handful of sizes. Cap is well under 1 MiB of pixbuf data per
// process, so we don't bother with eviction.
//
// `thread_local!` rather than `static`: `gtk4::gdk_pixbuf::Pixbuf` is
// `!Send` (gtk4-rs marks GObjects main-thread-only), so a `Mutex<...>`
// in a `static` can't satisfy `Sync`. All callers run on the GTK main
// thread today (icon loads happen inside button-rebuild code paths
// driven by GLib timers / signal handlers), so per-thread storage is
// the natural fit.

thread_local! {
    static NAME_PIXBUF_CACHE: RefCell<HashMap<(String, i32), gtk4::gdk_pixbuf::Pixbuf>> =
        RefCell::new(HashMap::new());
    static FILE_PIXBUF_CACHE: RefCell<HashMap<(PathBuf, i32, i32), gtk4::gdk_pixbuf::Pixbuf>> =
        RefCell::new(HashMap::new());
    /// Set to `true` after the first successful install of the
    /// `GtkIconTheme::changed` listener that clears `NAME_PIXBUF_CACHE`.
    /// Stays `false` until a `gdk::Display` is available (the listener
    /// install retries on every cache miss before that).
    static THEME_LISTENER_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Installs (once) a `GtkIconTheme::changed` listener that clears
/// `NAME_PIXBUF_CACHE`. Re-calling is a no-op via the install-flag.
///
/// Called from `create_pixbuf` so that the listener is wired up the
/// first time the cache is consulted with a Display available — earlier
/// calls (before any window has presented) silently no-op and retry.
fn ensure_theme_listener() {
    if THEME_LISTENER_INSTALLED.with(Cell::get) {
        return;
    }
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    theme.connect_changed(|_| {
        NAME_PIXBUF_CACHE.with(|c| c.borrow_mut().clear());
        log::debug!("Icon theme changed; cleared name pixbuf cache");
    });
    THEME_LISTENER_INSTALLED.with(|c| c.set(true));
}

/// Creates a GTK4 pixbuf from an icon name or path.
///
/// If `icon` is an absolute path, loads from file. Otherwise, tries the
/// icon theme, then falls back to `/usr/share/pixmaps`.
///
/// Decoded pixbufs are cached by `(icon, size)` per thread — see the
/// cache rationale in this module's source. The cache is cleared
/// automatically when the user's icon theme changes (the
/// `GtkIconTheme::changed` listener is installed lazily on first call
/// with a `gdk::Display` available).
pub fn create_pixbuf(icon: &str, size: i32) -> Option<gtk4::gdk_pixbuf::Pixbuf> {
    ensure_theme_listener();
    let key = (icon.to_string(), size);
    if let Some(pb) = NAME_PIXBUF_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Some(pb);
    }
    let pb = create_pixbuf_uncached(icon, size)?;
    NAME_PIXBUF_CACHE.with(|c| c.borrow_mut().insert(key, pb.clone()));
    Some(pb)
}

/// Cache-bypassing decode used by [`create_pixbuf`] on miss. Kept private
/// so consumers always go through the cache.
fn create_pixbuf_uncached(icon: &str, size: i32) -> Option<gtk4::gdk_pixbuf::Pixbuf> {
    // Absolute path
    if icon.contains('/') {
        return gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(icon, size, size).ok();
    }

    // Strip file extensions (matches Go behavior for entries like "Icon=firefox.svg")
    let icon_name = icon
        .strip_suffix(".svg")
        .or_else(|| icon.strip_suffix(".png"))
        .or_else(|| icon.strip_suffix(".xpm"))
        .unwrap_or(icon);

    // Try icon theme
    let display = gtk4::gdk::Display::default()?;
    let theme = gtk4::IconTheme::for_display(&display);

    if theme.has_icon(icon_name) {
        let icon_paintable = theme.lookup_icon(
            icon_name,
            &[],
            size,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::FORCE_REGULAR,
        );
        let file = icon_paintable.file()?;
        let path = file.path()?;
        return gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(path, size, size).ok();
    }

    // Fallback: try original name (with extension) in case it's a custom theme icon
    if icon_name != icon && theme.has_icon(icon) {
        let icon_paintable = theme.lookup_icon(
            icon,
            &[],
            size,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::FORCE_REGULAR,
        );
        let file = icon_paintable.file()?;
        let path = file.path()?;
        return gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(path, size, size).ok();
    }

    // Fallback: try /usr/share/pixmaps (many apps install icons there)
    for ext in &["svg", "png", "xpm"] {
        let path =
            std::path::Path::new("/usr/share/pixmaps").join(format!("{}.{}", icon_name, ext));
        if path.exists() {
            return gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(path, size, size).ok();
        }
    }

    None
}

/// Creates a GTK4 Image widget from an app ID.
pub fn create_image(app_id: &str, size: i32, app_dirs: &[PathBuf]) -> Option<gtk4::Image> {
    let icon_name = get_icon(app_id, app_dirs).unwrap_or_else(|| app_id.to_string());
    let pixbuf = create_pixbuf(&icon_name, size)?;
    Some(gtk4::Image::from_pixbuf(Some(&pixbuf)))
}

/// Loads a pixbuf from a file path at the given dimensions.
///
/// Decoded pixbufs are cached by `(path, width, height)` per thread for
/// the lifetime of the thread — see the cache rationale in this
/// module's source. The cache does **not** track file mtime: callers
/// must use this only for paths whose contents are stable across the
/// process lifetime (e.g. assets shipped with the binary). Mutable
/// paths must go through `gtk4::gdk_pixbuf::Pixbuf::from_file_at_size`
/// directly to bypass the cache.
pub fn pixbuf_from_file(path: &Path, width: i32, height: i32) -> Option<gtk4::gdk_pixbuf::Pixbuf> {
    let key = (path.to_path_buf(), width, height);
    if let Some(pb) = FILE_PIXBUF_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Some(pb);
    }
    let pb = gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(path, width, height).ok()?;
    FILE_PIXBUF_CACHE.with(|c| c.borrow_mut().insert(key, pb.clone()));
    Some(pb)
}
