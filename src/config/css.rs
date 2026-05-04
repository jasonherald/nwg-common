use gtk4::gdk;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};

/// Upper bound on how many `@import` targets `discover_watched_imports`
/// will follow in a single pass. Guards against pathologically deep (or
/// malformed-but-non-cyclical) chains. 32 is well above any reasonable
/// real-world theme tree; most setups have 1–5.
const MAX_IMPORT_GRAPH_SIZE: usize = 32;

/// CSS priority: embedded defaults (base layer).
const CSS_PRIORITY_EMBEDDED: u32 = gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION;
/// CSS priority: programmatic overrides like --opacity (middle layer).
const CSS_PRIORITY_OVERRIDE: u32 = CSS_PRIORITY_EMBEDDED + 1;
/// CSS priority: user CSS file (highest — always wins, including after hot-reload).
const CSS_PRIORITY_USER: u32 = CSS_PRIORITY_EMBEDDED + 2;

/// Debounce interval for CSS file change detection (milliseconds).
const CSS_RELOAD_DEBOUNCE_MS: u64 = 100;

/// Loads a CSS file and applies it at the highest priority (user overrides).
/// Always returns a CssProvider — if the file doesn't exist yet, an empty
/// provider is installed so `watch_css` can hot-load it when created.
///
/// Priority order: embedded defaults < programmatic overrides < user CSS file.
/// This ensures user CSS always wins, including after hot-reload.
pub fn load_css(css_path: &Path) -> gtk4::CssProvider {
    let provider = gtk4::CssProvider::new();

    if css_path.exists() {
        provider.load_from_path(css_path);
        log::info!("Loaded CSS from {}", css_path.display());
    } else {
        log::info!("{} not found — watching for creation", css_path.display());
    }

    apply_provider(&provider, CSS_PRIORITY_USER);
    provider
}

/// Loads CSS from a string as embedded defaults (lowest priority).
///
/// User CSS file and programmatic overrides both take precedence.
pub fn load_css_from_data(css: &str) -> gtk4::CssProvider {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);
    apply_provider(&provider, CSS_PRIORITY_EMBEDDED);
    provider
}

/// Loads CSS from a string as a programmatic override (middle priority).
///
/// Overrides embedded defaults, but user CSS file still wins.
pub fn load_css_override(css: &str) -> gtk4::CssProvider {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);
    apply_provider(&provider, CSS_PRIORITY_OVERRIDE);
    provider
}

/// Watches a CSS file for changes and reloads the provider automatically.
/// Uses inotify (Linux) via the `notify` crate — no polling.
/// The watcher lives on the GLib main loop for the lifetime of the
/// owning application.
///
/// Also watches files referenced via `@import` directives in the main
/// CSS, so theme managers like `tinty` that update imported files
/// (rather than the main CSS directly) trigger hot-reload too
/// (issue #73). On every main-CSS reload, the `@import` set is
/// re-scanned and the underlying `notify` watcher is rebuilt if the
/// set of watched files actually changed (issue #74). Adding or
/// removing an `@import` line while the dock is running now picks
/// up the new target on the next save, without a restart.
pub fn watch_css(css_path: &Path, provider: &gtk4::CssProvider) {
    let path = css_path.to_path_buf();
    let Some(parent) = path.parent() else {
        log::debug!(
            "CSS watch skipped: no parent directory for {}",
            path.display()
        );
        return;
    };
    // Canonicalize the parent directory so path comparisons against
    // notify events work consistently. notify reports canonical paths
    // (dot/dotdot segments resolved, symlinks followed) — if we stored
    // the lexical form (e.g. `/tmp/./dir`) events would arrive as
    // `/tmp/dir` and `HashSet<PathBuf>::contains` would silently
    // miss them, breaking hot-reload for any relative import path.
    // Parent is canonicalized rather than the full path so the watch
    // still works when the main CSS file doesn't exist yet (the
    // "watch for creation" flow in `load_css`).
    let main_dir = match parent.canonicalize() {
        Ok(d) => d,
        Err(e) => {
            log::debug!(
                "CSS watch skipped: can't canonicalize parent dir of {}: {}",
                path.display(),
                e
            );
            return;
        }
    };
    let Some(file_name) = path.file_name() else {
        log::debug!("CSS watch skipped: no filename for {}", path.display());
        return;
    };
    let canonical_path = main_dir.join(file_name);

    // Two root forms are threaded through the whole watcher flow:
    //
    // - `path` (as-referenced) is what we hand back to GTK via
    //   `load_from_path` at reload time AND what drives
    //   `discover_watched_imports`' first-hop resolution. GTK
    //   resolves relative `@import` paths against the directory of
    //   the path it was given; our discovery has to use the same
    //   base so the two stay in sync for symlinked stylesheet trees.
    // - `canonical_path` feeds the watched set and the notify match,
    //   because that's what inotify reports back in events.
    //
    // Mixing them (e.g. using canonical for discovery) would silently
    // watch a different set of files than GTK actually loads when the
    // config path is reached via a symlinked parent — the exact bug
    // CodeRabbit caught on #79.
    let imports = discover_watched_imports(&path);
    if !imports.is_empty() {
        log::info!(
            "Watching {} CSS @import target{} for hot-reload",
            imports.len(),
            if imports.len() == 1 { "" } else { "s" }
        );
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let initial = match build_watch_state(&canonical_path, &imports, tx.clone()) {
        Ok(state) => state,
        Err(_) => {
            // build_watch_state already logged the underlying notify
            // error at warn level; the one-shot `watch_css` API has
            // no Result to return, so we log-and-continue with no
            // hot-reload (matches the pre-Result behavior).
            return;
        }
    };
    install_reload_timer(path, canonical_path, provider.clone(), rx, tx, initial);
}

// ─── Rebindable watcher API (CR-2026-05-03-26) ────────────────────────────

/// Error returned by [`CssWatchHandle::rebind`] when the watcher cannot be
/// atomically moved to a new CSS file path.
///
/// On `Err`, the original watcher is preserved and hot-reload continues on
/// the old file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CssRebindError {
    /// The new CSS file path could not be accessed; the underlying
    /// `std::io::Error` contains the OS reason (not-found, permission
    /// denied, etc.).
    #[error("cannot read new CSS file '{path}': {source}")]
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A new `notify` watcher could not be set up on the new path. Hot-reload
    /// will not work on the new file. The original watcher is still active.
    #[error("failed to set up CSS file watcher for '{path}': {message}")]
    WatcherSetup {
        /// Path for which watcher setup failed.
        path: PathBuf,
        /// Description of the underlying watcher error.
        message: String,
    },
}

/// The mutable core of a [`CssWatchHandle`]: the current watcher and the
/// paths it watches. Wrapped in `Arc<Mutex<...>>` so the GLib timer
/// closure and the handle can share it safely across rebinds.
struct RebindableState {
    /// Active watcher — dropped here to stop the old notify thread.
    watch: WatchState,
    /// The path we hand to GTK for `load_from_path` and to
    /// `discover_watched_imports` for relative-import resolution.
    as_referenced: PathBuf,
    /// Canonical form of the main CSS path — used for event matching
    /// and `build_watch_state`.
    canonical: PathBuf,
}

/// Handle returned by [`watch_css_rebindable`]. Owns the inotify watcher's
/// lifetime and supports atomically rebinding to a new CSS file path at
/// runtime.
///
/// Drop to stop hot-reload entirely (the GLib timer is cancelled).
pub struct CssWatchHandle {
    /// Shared state between this handle and the GLib timer closure.
    /// The timer holds a `Weak` reference; when the handle is dropped,
    /// the `Arc` reference count reaches zero and the timer sees `None`
    /// on the next tick and stops.
    state: Arc<Mutex<RebindableState>>,
    /// Clone of the event-signal sender kept so `rebind` can wire up
    /// the new watcher without needing to re-derive one from the state.
    tx: std::sync::mpsc::Sender<()>,
    /// The GTK provider being kept up to date. Held here so `rebind`
    /// can reload the new CSS into it.
    provider: gtk4::CssProvider,
}

impl std::fmt::Debug for CssWatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `notify::RecommendedWatcher` (inside the shared state) and
        // `gtk4::CssProvider` aren't `Debug`-printable, so we report
        // structural facts a downstream consumer can actually use:
        // the strong-reference count tells you whether the timer
        // closure is still alive (drops to 1 after the timer's `Weak`
        // upgrades fail), and the path comes off the locked state.
        let strong = Arc::strong_count(&self.state);
        let path = self
            .state
            .lock()
            .ok()
            .map(|s| s.as_referenced.display().to_string())
            .unwrap_or_else(|| "<poisoned>".to_string());
        f.debug_struct("CssWatchHandle")
            .field("path", &path)
            .field("strong_refs", &strong)
            .finish()
    }
}

/// Same setup as [`watch_css`], but returns a [`CssWatchHandle`] that can
/// be used to rebind the watcher to a different file path later.
///
/// The provider reference remains stable across rebinds — only the watched
/// path changes.
pub fn watch_css_rebindable(css_path: &Path, provider: &gtk4::CssProvider) -> CssWatchHandle {
    let path = css_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<()>();

    // Compute canonical path for the initial file (same logic as `watch_css`).
    // A canonicalize failure here (parent missing, permission denied, etc.)
    // installs a dormant handle: the rebind timer still runs so a future
    // `rebind` to a working path can resume hot-reload.
    let (as_referenced, canonical) = match compute_canonical_pair(&path) {
        Ok(pair) => pair,
        Err(e) => {
            // Canonicalize failed — fall through to the dormant
            // construction below. We use the original path for both
            // `as_referenced` and `canonical` because there's nothing
            // better to point at; both fields are only consulted by
            // the timer's `drain_events` path, which is a no-op while
            // `WatchState::_watcher` is None.
            //
            // Surface the underlying io::Error so the user can see
            // whether this was missing-parent, permission-denied, EIO,
            // etc. rather than every dormant-startup looking the same
            // in logs.
            log::debug!(
                "watch_css_rebindable: cannot resolve path for {}; hot-reload inactive: {e}",
                path.display()
            );
            (path.clone(), path.clone())
        }
    };

    let imports = discover_watched_imports(&as_referenced);
    if !imports.is_empty() {
        log::info!(
            "Watching {} CSS @import target{} for hot-reload",
            imports.len(),
            if imports.len() == 1 { "" } else { "s" }
        );
    }

    // Try to build the initial watcher; if that fails, install a
    // dormant `WatchState` (no notify watcher, empty watched set).
    // Crucially we do NOT early-return here — the rebind timer still
    // gets installed below. Without that, `rx` would be dropped, and
    // a later successful `rebind()` would set up a new watcher whose
    // events have nowhere to go (channel disconnected). With the
    // timer in place, the dormant handle stays "live" — `rebind`
    // installs a real watcher into the locked state and the timer's
    // next tick drains the new events normally.
    let initial_watch =
        build_watch_state(&canonical, &imports, tx.clone()).unwrap_or_else(|e| {
            log::warn!(
                "watch_css_rebindable: failed to set up initial watcher for {}; hot-reload inactive until rebind(): {e}",
                canonical.display()
            );
            WatchState {
                _watcher: None,
                watched: HashSet::new(),
            }
        });

    let shared = Arc::new(Mutex::new(RebindableState {
        watch: initial_watch,
        as_referenced,
        canonical,
    }));

    install_rebindable_timer(shared.clone(), provider.clone(), rx, tx.clone());

    CssWatchHandle {
        state: shared,
        tx,
        provider: provider.clone(),
    }
}

impl CssWatchHandle {
    /// Atomically tear down the current watcher, load the CSS at `new_path`
    /// into the existing provider, and start watching `new_path` instead.
    ///
    /// # Atomicity guarantee
    ///
    /// The old watcher is preserved until both setup steps succeed:
    ///
    /// 1. A new `notify` watcher is created for `new_path` (most likely
    ///    failure point — returns [`CssRebindError::WatcherSetup`] on
    ///    failure without touching the old watcher).
    /// 2. `new_path` is loaded into the provider via GTK's `load_from_path`.
    /// 3. Only after both succeed: the old watcher is replaced in the
    ///    shared state and `new_path`'s CSS is now active.
    ///
    /// If `new_path` doesn't exist yet, step 1 may succeed (we watch the
    /// parent directory, as `watch_css` does) and the provider will receive
    /// an empty stylesheet until the file is created.
    ///
    /// # Errors
    ///
    /// Returns [`CssRebindError::Io`] if `new_path` cannot be accessed,
    /// [`CssRebindError::WatcherSetup`] if the notify watcher cannot be
    /// initialised.
    pub fn rebind(&mut self, new_path: impl AsRef<Path>) -> Result<(), CssRebindError> {
        let new_path_buf = new_path.as_ref().to_path_buf();

        // Step 1: resolve canonical forms for the new path. The
        // underlying canonicalize() error (PermissionDenied, NotFound,
        // EIO, etc.) flows through verbatim so the caller sees the real
        // OS reason rather than a synthesized NotFound.
        let (new_as_referenced, new_canonical) =
            compute_canonical_pair(&new_path_buf).map_err(|source| CssRebindError::Io {
                path: new_path_buf.clone(),
                source,
            })?;

        // Step 2: build the new watcher BEFORE touching the old one
        // (atomicity — if this fails, old watcher is still live).
        // The underlying notify error (CreateFailed or WatchFailed) is
        // surfaced through `BuildWatchError`'s Display so the caller
        // sees something actionable like "failed to watch directory
        // '/etc/nwg-dock-hyprland': Operation not permitted (os error 1)"
        // rather than the previous generic "failed to initialise
        // notify watcher".
        let new_imports = discover_watched_imports(&new_as_referenced);
        let new_watch =
            build_watch_state(&new_canonical, &new_imports, self.tx.clone()).map_err(|e| {
                CssRebindError::WatcherSetup {
                    path: new_path_buf.clone(),
                    message: e.to_string(),
                }
            })?;

        // Step 3: load the new CSS into the provider.  This is a GTK
        // call and is always safe to retry — if it fails GTK logs
        // internally; the old watcher is still untouched.
        if new_path_buf.exists() {
            self.provider.load_from_path(&new_path_buf);
            log::info!("CSS rebound: loaded {}", new_path_buf.display());
        } else {
            self.provider.load_from_data("");
            log::info!(
                "CSS rebound: {} not found yet — watching for creation",
                new_path_buf.display()
            );
        }

        // Step 4: swap in the new state.  Old `_watcher` is dropped here,
        // stopping the old notify worker thread.
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.watch = new_watch;
            state.as_referenced = new_as_referenced;
            state.canonical = new_canonical;
        }

        Ok(())
    }
}

/// Installs a GLib timer that drives the rebindable watcher.
///
/// The timer holds a `Weak` reference to the shared state.  When the
/// `CssWatchHandle` is dropped, the `Arc` count reaches zero and the
/// `Weak::upgrade` returns `None`, causing the timer to stop cleanly.
// NOTE: structurally mirrors `install_reload_timer` below — same
// `drain_events` → `reload_provider` → `maybe_rebuild_watcher` shape.
// The functions can't share a body because their `WatchState` ownership
// models differ: `install_reload_timer` captures `WatchState` by value
// into the timer closure (so it can mutate freely), while this function
// holds it behind `Arc<Mutex<RebindableState>>` (so `rebind` can swap
// the watcher from a different code path). Sharing the path/canonical
// lookup + watched-set computation is feasible but requires the helper
// to drop and reacquire the mutex around the GTK call, which is what
// drives the inline duplication here. A future refactor that unifies
// both timers under the `Arc<Mutex<>>` model could collapse this.
fn install_rebindable_timer(
    shared: Arc<Mutex<RebindableState>>,
    provider: gtk4::CssProvider,
    rx: std::sync::mpsc::Receiver<()>,
    tx: std::sync::mpsc::Sender<()>,
) {
    let weak = Arc::downgrade(&shared);
    gtk4::glib::timeout_add_local(
        std::time::Duration::from_millis(CSS_RELOAD_DEBOUNCE_MS),
        move || {
            let Some(arc) = weak.upgrade() else {
                // Handle dropped — stop the timer.
                return gtk4::glib::ControlFlow::Break;
            };
            match drain_events(&rx) {
                DrainResult::Changed => {
                    // Read paths under lock, then call GTK outside the lock
                    // to avoid holding the mutex across a potentially-blocking
                    // GTK call.
                    let (as_referenced, canonical, tx_clone) = {
                        let state = arc.lock().unwrap_or_else(|e| e.into_inner());
                        (
                            state.as_referenced.clone(),
                            state.canonical.clone(),
                            tx.clone(),
                        )
                    };
                    reload_provider(&provider, &as_referenced);
                    // Rebuild watcher if @import set changed, then write
                    // the new WatchState back under the lock.
                    let new_imports = discover_watched_imports(&as_referenced);
                    let new_watched = compute_watched_set(&canonical, &new_imports);
                    let needs_rebuild = {
                        let state = arc.lock().unwrap_or_else(|e| e.into_inner());
                        new_watched != state.watch.watched
                    };
                    if needs_rebuild {
                        log::info!(
                            "CSS @import set changed; rebuilding watcher for {}",
                            as_referenced.display()
                        );
                        match build_watch_state(&canonical, &new_imports, tx_clone) {
                            Ok(new_watch) => {
                                let mut state = arc.lock().unwrap_or_else(|e| e.into_inner());
                                state.watch = new_watch;
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to rebuild CSS watcher for {}; keeping previous watch set: {e}",
                                    as_referenced.display()
                                );
                            }
                        }
                    }
                    gtk4::glib::ControlFlow::Continue
                }
                DrainResult::Empty => gtk4::glib::ControlFlow::Continue,
                DrainResult::Disconnected => {
                    log::warn!("CSS rebindable watcher disconnected; stopping hot-reload");
                    gtk4::glib::ControlFlow::Break
                }
            }
        },
    );
}

/// Computes the `(as_referenced, canonical)` path pair used throughout the
/// watcher. Returns `Err` when the path has no canonicalisable parent
/// directory or `canonicalize()` fails for any other OS reason
/// (PermissionDenied, EIO, etc.); the underlying `std::io::Error` is
/// preserved so callers can surface the real failure mode in their error
/// types instead of synthesising a fake `NotFound`.
///
/// The same two-form invariant as `watch_css` applies: `as_referenced` is
/// what GTK gets (so relative `@import` resolution agrees with ours), and
/// `canonical` is what inotify events carry (so event paths match the
/// watched set).
fn compute_canonical_pair(path: &Path) -> Result<(PathBuf, PathBuf), std::io::Error> {
    // Bare filename with no parent component: there's no directory to
    // canonicalize against. Synthesize a NotFound here — there's no
    // OS-side error to preserve since we never made the syscall.
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "path '{}' has no parent directory to canonicalize",
                path.display()
            ),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("path '{}' has no file-name component", path.display()),
        )
    })?;
    // Pass the real canonicalize() error through. PermissionDenied,
    // NotFound, EIO, etc. all surface accurately to the caller.
    let main_dir = parent.canonicalize()?;
    let canonical = main_dir.join(file_name);
    Ok((path.to_path_buf(), canonical))
}

/// Everything required to keep the `notify` watcher alive and to know
/// which files are currently tracked, so we can diff against a
/// re-scanned set on each reload.
struct WatchState {
    /// Owns the notify worker thread — dropping `Some(_)` stops the
    /// worker. The leading underscore tells both the compiler and
    /// future readers that this field is intentionally never read:
    /// its entire purpose is RAII lifetime management.
    ///
    /// `None` is the dormant state used when the rebindable handle
    /// can't set up a real watcher at construction time (parent
    /// missing, `notify::recommended_watcher` failed, etc.). The
    /// handle is still alive and `rebind` can succeed later; until
    /// then no events fire and `watched` stays empty. Modeling
    /// dormancy this way avoids the prior `make_null_watcher` shape
    /// that retried the same constructor that just failed and could
    /// panic on the exact resource-exhaustion fallback it was meant
    /// to survive.
    _watcher: Option<notify::RecommendedWatcher>,
    /// Absolute paths we signal reloads for. Compared structurally
    /// to detect `@import` set changes across reloads.
    watched: HashSet<PathBuf>,
}

/// Internal error returned by [`build_watch_state`]. Captures whether
/// the failure came from creating the `notify::RecommendedWatcher` or
/// from subscribing it to a directory; either reason flows through to
/// callers so they can surface the real cause (rather than a generic
/// "failed to initialise notify watcher" message).
#[derive(Debug)]
enum BuildWatchError {
    /// `notify::recommended_watcher` itself failed (e.g. inotify
    /// resource exhaustion at the kernel level).
    CreateFailed(notify::Error),
    /// `watcher.watch(dir, ...)` failed for at least one directory.
    /// Stores the FIRST failing directory's error — additional
    /// failures are logged inside `build_watch_state` for diagnosis
    /// but only one is surfaced through the return.
    WatchFailed { dir: PathBuf, source: notify::Error },
}

impl std::fmt::Display for BuildWatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildWatchError::CreateFailed(e) => {
                write!(f, "failed to create notify watcher: {e}")
            }
            BuildWatchError::WatchFailed { dir, source } => {
                write!(f, "failed to watch directory '{}': {source}", dir.display())
            }
        }
    }
}

/// Builds a fresh `WatchState` for the given main CSS path plus the
/// current set of imported files. Subscribes the watcher to the
/// parent directory of the main CSS AND the parent directory of each
/// import (the same dir if they share a parent). Returns `Err` with
/// the underlying notify error if the watcher itself can't be created
/// or any `watch(...)` call fails — callers can then either log-and-
/// continue (the fall-through-to-dormant pattern in
/// `watch_css_rebindable`) or surface the error to their own caller
/// (the `rebind` path that wraps it in `CssRebindError::WatcherSetup`).
fn build_watch_state(
    main_css: &Path,
    imports: &[PathBuf],
    tx: std::sync::mpsc::Sender<()>,
) -> Result<WatchState, BuildWatchError> {
    use notify::{RecursiveMode, Watcher};

    let watched = compute_watched_set(main_css, imports);
    let dirs = compute_watched_dirs(main_css, imports);

    let mut watcher = match notify::recommended_watcher(make_css_handler(watched.clone(), tx)) {
        Ok(w) => w,
        Err(e) => {
            // Log with the full message AND propagate so the error
            // type carries the same string for callers that want it.
            log::warn!("Failed to create CSS watcher: {e}");
            return Err(BuildWatchError::CreateFailed(e));
        }
    };
    // If any `watch(...)` call fails, the returned `WatchState` would
    // claim files in `watched` whose parent dir we're not actually
    // subscribed to. `maybe_rebuild_watcher` compares the old and new
    // watched sets to decide whether to rebuild — if the claim is
    // inaccurate, edits to an un-subscribed file won't fire events,
    // which won't trigger a reload, which won't re-attempt the watch.
    // The mis-subscription persists until the user changes their
    // `@import` set or restarts. Failing fast here lets the outer
    // reload-loop (or the startup path) surface the issue instead of
    // silently degrading. CodeRabbit catch on #76.
    let mut first_failure: Option<(PathBuf, notify::Error)> = None;
    for dir in &dirs {
        if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
            log::warn!("Failed to watch CSS directory '{}': {e}", dir.display());
            if first_failure.is_none() {
                first_failure = Some((dir.clone(), e));
            }
        }
    }
    if let Some((dir, source)) = first_failure {
        return Err(BuildWatchError::WatchFailed { dir, source });
    }
    Ok(WatchState {
        _watcher: Some(watcher),
        watched,
    })
}

/// Computes the full set of absolute paths we want to fire reloads for:
/// the main CSS and every currently-discovered `@import` target.
/// Pure; testable without notify or the filesystem.
fn compute_watched_set(main_css: &Path, imports: &[PathBuf]) -> HashSet<PathBuf> {
    let mut out = HashSet::with_capacity(imports.len() + 1); // +1 for main_css
    out.insert(main_css.to_path_buf());
    for imp in imports {
        out.insert(imp.clone());
    }
    out
}

/// Computes the set of parent directories that notify needs to subscribe
/// to in order to observe every watched file. One notify watch per
/// directory suffices — events are then matched against the absolute
/// path set built by `compute_watched_set`.
fn compute_watched_dirs(main_css: &Path, imports: &[PathBuf]) -> HashSet<PathBuf> {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    if let Some(parent) = main_css.parent() {
        dirs.insert(parent.to_path_buf());
    }
    for imp in imports {
        if let Some(parent) = imp.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    dirs
}

/// Installs a debounced GLib timer that reloads the provider on file
/// change and rebuilds the underlying watcher if the `@import` set
/// has shifted since the last reload. The timer closure owns the
/// active `WatchState` so the watcher's worker thread stays alive for
/// the lifetime of the GLib main loop.
///
/// Rebuilding the watcher is opt-in: we construct the new state first
/// and only then drop the old one, which creates a brief overlap
/// where both watchers may fire for the same event. The debounce in
/// `drain_events` folds duplicates, so the extra event is harmless.
fn install_reload_timer(
    as_referenced: std::path::PathBuf,
    canonical: std::path::PathBuf,
    provider: gtk4::CssProvider,
    rx: std::sync::mpsc::Receiver<()>,
    tx: std::sync::mpsc::Sender<()>,
    initial: WatchState,
) {
    let mut state = initial;
    gtk4::glib::timeout_add_local(
        std::time::Duration::from_millis(CSS_RELOAD_DEBOUNCE_MS),
        move || match drain_events(&rx) {
            DrainResult::Changed => {
                reload_provider(&provider, &as_referenced);
                maybe_rebuild_watcher(&as_referenced, &canonical, &tx, &mut state);
                gtk4::glib::ControlFlow::Continue
            }
            DrainResult::Empty => gtk4::glib::ControlFlow::Continue,
            DrainResult::Disconnected => {
                log::warn!("CSS watcher disconnected; stopping hot-reload");
                gtk4::glib::ControlFlow::Break
            }
        },
    );
}

/// Re-discovers the `@import` set from the main CSS and, if it differs
/// from what the current watcher is tracking, replaces the watcher.
/// No-op (and fast) in the common case where the user changed a file
/// we already watch without touching any `@import` lines.
///
/// The two-path invariant matters here too: we walk the graph from
/// the *as-referenced* root (so relative imports resolve the same way
/// GTK's `load_from_path` will), but the `watched` set and every
/// `build_watch_state` call keys on the *canonical* root so notify
/// event paths match the stored keys.
fn maybe_rebuild_watcher(
    as_referenced: &Path,
    canonical: &Path,
    tx: &std::sync::mpsc::Sender<()>,
    state: &mut WatchState,
) {
    let new_imports = discover_watched_imports(as_referenced);
    let new_watched = compute_watched_set(canonical, &new_imports);
    if new_watched == state.watched {
        return;
    }
    log::info!(
        "CSS @import set changed ({} → {} tracked file{}); rebuilding watcher",
        state.watched.len(),
        new_watched.len(),
        if new_watched.len() == 1 { "" } else { "s" }
    );
    // Build the new state BEFORE dropping the old one so we don't have
    // a window where nothing is watching. The old `state.watcher` is
    // dropped at the assignment below, which stops its worker thread.
    match build_watch_state(canonical, &new_imports, tx.clone()) {
        Ok(new_state) => *state = new_state,
        Err(e) => log::warn!("Failed to rebuild CSS watcher; keeping previous watch set: {e}"),
    }
}

enum DrainResult {
    Changed,
    Empty,
    Disconnected,
}

/// Drains all pending events from the watcher channel.
fn drain_events(rx: &std::sync::mpsc::Receiver<()>) -> DrainResult {
    let mut changed = false;
    loop {
        match rx.try_recv() {
            Ok(()) => changed = true,
            Err(TryRecvError::Empty) => {
                return if changed {
                    DrainResult::Changed
                } else {
                    DrainResult::Empty
                };
            }
            Err(TryRecvError::Disconnected) => return DrainResult::Disconnected,
        }
    }
}

/// Reloads the CSS provider from the file, or clears it if the file is gone.
fn reload_provider(provider: &gtk4::CssProvider, path: &Path) {
    log::info!("CSS file changed, reloading: {}", path.display());
    if path.exists() {
        provider.load_from_path(path);
    } else {
        // File deleted — clear user styles so defaults show through
        provider.load_from_data("");
    }
}

/// Creates a notify event handler that sends on the channel when any of
/// the `watched` absolute paths is affected (by any save strategy,
/// including deletion). Each path should be the absolute path of a
/// CSS file we care about — the main stylesheet or an `@import`
/// target.
fn make_css_handler(
    watched: HashSet<PathBuf>,
    tx: std::sync::mpsc::Sender<()>,
) -> impl FnMut(Result<notify::Event, notify::Error>) {
    move |event| {
        let ev = match event {
            Ok(ev) => ev,
            Err(e) => {
                log::warn!("CSS watcher error: {}", e);
                return;
            }
        };
        if !is_content_change(&ev.kind) {
            return;
        }
        let matches = ev.paths.iter().any(|p| watched.contains(p));
        if matches && let Err(e) = tx.send(()) {
            log::warn!("CSS watcher channel closed: {}", e);
        }
    }
}

/// Filters notify event kinds to just the ones that indicate the file's
/// *content* changed (or the file was created/removed). Access events
/// (`Open`, `Close(Read)`, etc.) fire when *any* reader opens the
/// file — including our own `load_from_path` / `read_to_string` calls
/// in the reload path. Treating those as change signals creates an
/// infinite feedback loop: reload opens file → Access event fires →
/// reload opens file → … The `notify` crate's default inotify filter
/// includes Access events on some backends, so this kind-based guard
/// is required even though the path-set filter normally constrains
/// which files we react to.
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

fn apply_provider(provider: &gtk4::CssProvider, priority: u32) {
    let Some(display) = gdk::Display::default() else {
        log::error!("Cannot apply CSS: GTK display not available — is GTK initialized?");
        return;
    };
    gtk4::style_context_add_provider_for_display(&display, provider, priority);
}

// ─── @import discovery (issue #73) ────────────────────────────────────────
//
// The CSS watcher fires on a stat change of the main stylesheet, but
// `@import`-referenced files live wherever the user wants them — theme
// managers like tinty keep color-scheme CSS under `~/.local/share/...`
// and reference it from `~/.config/.../style.css`. Without this, the
// user changes their color scheme, the imported file changes on disk,
// and the dock looks stale until the user manually touches their main
// CSS file. Parsing `@import` directives lets us watch the target files
// too, at the cost of a tiny CSS mini-parser below.
//
// The parser is lenient: anything it can't recognize is silently
// skipped, which means we might miss an exotic `@import` form (we don't
// hot-reload the target) but we never crash or corrupt user CSS. Real
// CSS evaluation is still done by GTK via `load_from_path`; we only
// peek at the file to find out what else to watch.

/// Walks the `@import` graph rooted at the main CSS and returns the
/// canonical paths of every reachable file that currently exists on
/// disk. Safe against read failure at any node (skip-and-continue),
/// and terminates cleanly on cycles (each canonical path is visited
/// at most once) and on pathologically deep chains
/// (capped at `MAX_IMPORT_GRAPH_SIZE` nodes with a warning).
///
/// The main CSS itself is not in the returned vec — the caller
/// (`watch_css`) already tracks it separately as the root.
///
/// Canonical paths are used for dedup (`visited`) and for the returned
/// set (so the notify event match — which reports canonical paths —
/// works), but the *as-referenced* form of each file is what drives
/// the next hop's relative-import resolution. GTK4 resolves relative
/// `@import` paths against the directory of the path it was *given*,
/// not the symlink-resolved target, so our discovery must do the same
/// to stay in sync. Without this a symlinked stylesheet tree could
/// make us watch different files than GTK actually loads (CodeRabbit
/// catch on #79).
fn discover_watched_imports(main_css: &Path) -> Vec<PathBuf> {
    let main_canonical = match main_css.canonicalize() {
        Ok(c) => c,
        Err(e) => {
            log::debug!(
                "CSS @import discovery: can't canonicalize {} ({}); continuing without imports",
                main_css.display(),
                e
            );
            return Vec::new();
        }
    };

    // `visited` tracks every canonical path we've seen (including the
    // main file) so we don't re-process a node reached via two paths
    // (diamond graph) or loop on a cycle. `queue` holds the
    // *as-referenced* paths to process — each file's own
    // `@import` resolution uses that path's parent as `base_dir`,
    // matching GTK's behavior. `out` collects the discovered imports
    // in BFS order, excluding the main file.
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(main_canonical);
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(main_css.to_path_buf());
    let mut out: Vec<PathBuf> = Vec::new();

    while let Some(current) = queue.pop_front() {
        if let Some(imports) = read_direct_imports(&current) {
            for (import_ref, import_canonical) in imports {
                if out.len() >= MAX_IMPORT_GRAPH_SIZE {
                    log::warn!(
                        "CSS @import graph reached the {}-file cap; not discovering further targets",
                        MAX_IMPORT_GRAPH_SIZE
                    );
                    return out;
                }
                if visited.insert(import_canonical.clone()) {
                    out.push(import_canonical);
                    // Queue the AS-REFERENCED form so its own
                    // relative @imports resolve against the same
                    // base_dir GTK will use at load time.
                    queue.push_back(import_ref);
                }
            }
        }
    }

    out
}

/// Reads a single CSS file and returns its directly-referenced
/// `@import` targets as `(as_referenced, canonical)` pairs.
/// `as_referenced` is the resolved path using the file's parent as
/// base_dir — used for the next hop's relative-import resolution.
/// `canonical` is `as_referenced.canonicalize()` — used for dedup and
/// the final watched set. Unresolvable entries (missing files,
/// unsupported URL schemes, unparseable directives) are skipped.
/// Returns `None` if the file itself can't be read — callers treat
/// that as "no imports" and continue.
fn read_direct_imports(css_file: &Path) -> Option<Vec<(PathBuf, PathBuf)>> {
    let base_dir = css_file.parent()?;
    let content = match std::fs::read_to_string(css_file) {
        Ok(c) => c,
        Err(e) => {
            log::debug!(
                "CSS @import discovery: can't read {} ({}); skipping",
                css_file.display(),
                e
            );
            return None;
        }
    };
    let mut out = Vec::new();
    for raw in parse_css_imports(&content) {
        let Some(resolved) = resolve_import_path(&raw, base_dir) else {
            continue;
        };
        match resolved.canonicalize() {
            Ok(canonical) => out.push((resolved, canonical)),
            Err(e) => {
                log::debug!(
                    "CSS @import target not accessible ({}): {}",
                    e,
                    resolved.display()
                );
            }
        }
    }
    Some(out)
}

/// Extracts the raw path string from every `@import` directive in the
/// supplied CSS source. Strips `/* ... */` comments first so commented-
/// out imports don't count. Malformed directives are skipped silently
/// (see module-level rationale).
fn parse_css_imports(css: &str) -> Vec<String> {
    let stripped = strip_css_comments(css);
    let mut imports = Vec::new();
    let mut rest = stripped.as_str();

    while let Some(pos) = rest.find("@import") {
        // Advance past this @import whether or not we can parse its
        // argument — otherwise a single malformed directive would
        // loop us forever.
        let after_kw = &rest[pos + "@import".len()..];
        rest = after_kw.trim_start();
        if let Some((path, after)) = take_import_path(rest) {
            if !path.trim().is_empty() {
                imports.push(path);
            }
            rest = after;
        }
    }

    imports
}

/// Parses the path portion of an `@import` directive, returning the
/// extracted path and the text that follows it. Recognized forms:
///
///   `"path"` · `'path'` · `url("path")` · `url('path')` · `url(path)`
///
/// Returns `None` if the input doesn't start with a recognized form.
fn take_import_path(s: &str) -> Option<(String, &str)> {
    // Helper: reject captured paths that contain a raw newline. CSS
    // string literals don't legally span lines unescaped, so a "quoted"
    // path with a newline in it almost always means the user forgot
    // the closing quote — and our lenient scan would otherwise
    // happily swallow text clear up to the NEXT unrelated `"` or `'`,
    // potentially eating the next `@import` directive wholesale. The
    // check keeps the parser from pathologically consuming good
    // imports because of a typo in an earlier one.
    fn finalize_quoted<'a>(path: &'a str, after: &'a str) -> Option<(String, &'a str)> {
        if path.contains('\n') {
            return None;
        }
        Some((path.to_string(), after))
    }

    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        return finalize_quoted(&rest[..end], &rest[end + 1..]);
    }
    if let Some(rest) = s.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return finalize_quoted(&rest[..end], &rest[end + 1..]);
    }
    if let Some(rest) = s.strip_prefix("url") {
        let rest = rest.trim_start().strip_prefix('(')?.trim_start();
        if let Some(inner) = rest.strip_prefix('"') {
            let end = inner.find('"')?;
            let after = inner[end + 1..].trim_start();
            let after = after.strip_prefix(')').unwrap_or(after);
            return finalize_quoted(&inner[..end], after);
        }
        if let Some(inner) = rest.strip_prefix('\'') {
            let end = inner.find('\'')?;
            let after = inner[end + 1..].trim_start();
            let after = after.strip_prefix(')').unwrap_or(after);
            return finalize_quoted(&inner[..end], after);
        }
        let end = rest.find(')')?;
        return Some((rest[..end].trim().to_string(), &rest[end + 1..]));
    }
    None
}

/// Removes `/* ... */` blocks from CSS source. Unterminated comments
/// consume the rest of the text (matches browser behavior). Leaves
/// everything else — including strings that happen to contain `/*` —
/// untouched. String-quoting awareness isn't required here because
/// the downstream `@import` parser only matches the directive
/// *outside* strings, and comment stripping only removes genuine
/// comment blocks.
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find("*/") {
            Some(end) => rest = &rest[end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Converts a raw `@import` path string into an absolute filesystem
/// path, relative paths resolved against `base_dir`. Returns `None`
/// for URLs we can't watch on the local filesystem (`http://`,
/// `https://`, `data:`, `file://` — the last is a valid file URL but
/// we'd need to strip the scheme and this hasn't surfaced as a
/// real-world need yet). Empty or whitespace-only input also returns
/// `None`.
fn resolve_import_path(raw: &str, base_dir: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_unwatchable_url(trimmed) {
        return None;
    }
    let p = Path::new(trimmed);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    })
}

/// URL schemes we intentionally don't try to treat as filesystem paths.
fn is_unwatchable_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("data:")
        || s.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── strip_css_comments ──────────────────────────────────────────────

    #[test]
    fn strip_comments_none() {
        assert_eq!(
            strip_css_comments("window { color: red; }"),
            "window { color: red; }"
        );
    }

    #[test]
    fn strip_single_comment() {
        assert_eq!(strip_css_comments("a /* b */ c"), "a  c");
    }

    #[test]
    fn strip_multiple_comments() {
        assert_eq!(strip_css_comments("/* one */ middle /* two */"), " middle ");
    }

    #[test]
    fn strip_comment_containing_import_directive() {
        // The whole point: a commented-out @import must not be matched later.
        assert!(!strip_css_comments("/* @import \"fake.css\"; */ real").contains("@import"));
    }

    #[test]
    fn strip_unterminated_comment_consumes_rest() {
        assert_eq!(strip_css_comments("before /* oops"), "before ");
    }

    #[test]
    fn strip_empty_input() {
        assert_eq!(strip_css_comments(""), "");
    }

    #[test]
    fn strip_adjacent_comments() {
        assert_eq!(strip_css_comments("/*a*//*b*/c"), "c");
    }

    // ─── take_import_path ────────────────────────────────────────────────

    #[test]
    fn take_double_quoted_path() {
        let (p, rest) = take_import_path("\"theme.css\"; window { }").unwrap();
        assert_eq!(p, "theme.css");
        assert_eq!(rest, "; window { }");
    }

    #[test]
    fn take_single_quoted_path() {
        let (p, rest) = take_import_path("'theme.css'").unwrap();
        assert_eq!(p, "theme.css");
        assert_eq!(rest, "");
    }

    #[test]
    fn take_url_double_quoted() {
        let (p, rest) = take_import_path("url(\"theme.css\") screen;").unwrap();
        assert_eq!(p, "theme.css");
        // The trailing media query / semicolon is left to the caller.
        assert!(rest.contains("screen"));
    }

    #[test]
    fn take_url_single_quoted() {
        let (p, _) = take_import_path("url('theme.css')").unwrap();
        assert_eq!(p, "theme.css");
    }

    #[test]
    fn take_url_unquoted() {
        let (p, _) = take_import_path("url(theme.css)").unwrap();
        assert_eq!(p, "theme.css");
    }

    #[test]
    fn take_url_with_inner_whitespace() {
        let (p, _) = take_import_path("url(  theme.css  )").unwrap();
        assert_eq!(p, "theme.css");
    }

    #[test]
    fn take_unterminated_quote_returns_none() {
        assert!(take_import_path("\"unterminated").is_none());
    }

    #[test]
    fn take_non_import_returns_none() {
        assert!(take_import_path("window { color: red; }").is_none());
    }

    #[test]
    fn take_empty_returns_none() {
        assert!(take_import_path("").is_none());
    }

    // ─── parse_css_imports ───────────────────────────────────────────────

    #[test]
    fn parse_no_imports() {
        assert!(parse_css_imports("window { color: red; }").is_empty());
    }

    #[test]
    fn parse_single_double_quoted() {
        assert_eq!(
            parse_css_imports("@import \"theme.css\";\nwindow { }"),
            vec!["theme.css"]
        );
    }

    #[test]
    fn parse_single_single_quoted() {
        assert_eq!(parse_css_imports("@import 'theme.css';"), vec!["theme.css"]);
    }

    #[test]
    fn parse_url_forms() {
        assert_eq!(
            parse_css_imports("@import url(\"a.css\"); @import url('b.css'); @import url(c.css);"),
            vec!["a.css", "b.css", "c.css"]
        );
    }

    #[test]
    fn parse_multiple_mixed_imports() {
        let css = r#"
            @import "one.css";
            @import 'two.css';
            @import url("three.css");
            window { color: red; }
        "#;
        assert_eq!(
            parse_css_imports(css),
            vec!["one.css", "two.css", "three.css"]
        );
    }

    #[test]
    fn parse_ignores_commented_imports() {
        let css = r#"
            /* @import "fake.css"; */
            @import "real.css";
        "#;
        assert_eq!(parse_css_imports(css), vec!["real.css"]);
    }

    #[test]
    fn parse_import_with_media_query_suffix() {
        // CSS permits a media query after @import — we keep the path, drop
        // the media query. GTK doesn't honor media queries anyway.
        let css = r#"@import "print.css" print;"#;
        assert_eq!(parse_css_imports(css), vec!["print.css"]);
    }

    #[test]
    fn parse_malformed_continues_past() {
        // A broken @import shouldn't prevent finding later good ones.
        let css = r#"
            @import "unterminated
            @import "good.css";
        "#;
        let imports = parse_css_imports(css);
        assert!(imports.contains(&"good.css".to_string()));
    }

    #[test]
    fn parse_path_with_spaces_in_quotes() {
        // Users with spaces in their paths should still work via quoting.
        assert_eq!(
            parse_css_imports("@import \"my themes/base16.css\";"),
            vec!["my themes/base16.css"]
        );
    }

    #[test]
    fn parse_empty_string_paths_skipped() {
        assert!(parse_css_imports("@import \"\"; @import ' ';").is_empty());
    }

    #[test]
    fn parse_real_world_tinty_example() {
        // BlueInGreen68's reported stylesheet (issue #73).
        let css = r#"
            /* Color scheme */
            @import "/home/blueingreen68/.local/share/tinted-theming/tinty/base16-nwg-dock-themes-file.css";

            window {
              border-width: 3px;
              border-style: solid;
            }
        "#;
        assert_eq!(
            parse_css_imports(css),
            vec![
                "/home/blueingreen68/.local/share/tinted-theming/tinty/base16-nwg-dock-themes-file.css"
            ]
        );
    }

    // ─── resolve_import_path ─────────────────────────────────────────────

    #[test]
    fn resolve_absolute_path_unchanged() {
        let base = Path::new("/home/user/.config/nwg-dock-hyprland");
        assert_eq!(
            resolve_import_path("/abs/path/theme.css", base).unwrap(),
            PathBuf::from("/abs/path/theme.css")
        );
    }

    #[test]
    fn resolve_relative_path_against_base() {
        let base = Path::new("/home/user/.config/nwg-dock-hyprland");
        assert_eq!(
            resolve_import_path("theme.css", base).unwrap(),
            PathBuf::from("/home/user/.config/nwg-dock-hyprland/theme.css")
        );
    }

    #[test]
    fn resolve_nested_relative_path() {
        let base = Path::new("/home/user/.config/nwg-dock-hyprland");
        assert_eq!(
            resolve_import_path("themes/dark.css", base).unwrap(),
            PathBuf::from("/home/user/.config/nwg-dock-hyprland/themes/dark.css")
        );
    }

    #[test]
    fn resolve_http_is_none() {
        assert!(resolve_import_path("http://example.com/style.css", Path::new("/tmp")).is_none());
    }

    #[test]
    fn resolve_https_is_none() {
        assert!(resolve_import_path("https://example.com/style.css", Path::new("/tmp")).is_none());
    }

    #[test]
    fn resolve_data_url_is_none() {
        assert!(resolve_import_path("data:text/css,body{}", Path::new("/tmp")).is_none());
    }

    #[test]
    fn resolve_file_url_is_none() {
        // Could be supported later by stripping the scheme; today we skip.
        assert!(resolve_import_path("file:///etc/passwd", Path::new("/tmp")).is_none());
    }

    #[test]
    fn resolve_empty_is_none() {
        assert!(resolve_import_path("", Path::new("/tmp")).is_none());
    }

    #[test]
    fn resolve_whitespace_only_is_none() {
        assert!(resolve_import_path("   \t\n", Path::new("/tmp")).is_none());
    }

    // ─── discover_watched_imports (I/O; uses tempdir) ─────────────────────
    //
    // Each test carves a uniquely-named subdirectory under the OS temp
    // dir so parallel `cargo test` runs don't collide. `create_dir_all`
    // and `remove_dir_all` are wrapped with `.expect(...)` so filesystem
    // setup or cleanup errors fail loudly rather than quietly polluting
    // subsequent runs — per CodeRabbit review on #75 and the project
    // coding guideline against silent `let _ =` discards.

    /// Builds a fresh temp subdirectory for one of the I/O tests below.
    /// The directory name includes the test name and process id so a
    /// concurrent test can't trample it.
    fn make_test_dir(test_name: &str) -> std::path::PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("nwg-css-test-{}-{}", test_name, std::process::id()));
        // Start clean in case a prior test run crashed before cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)
            .unwrap_or_else(|e| panic!("create test dir {}: {}", tmp.display(), e));
        tmp
    }

    fn cleanup_test_dir(dir: &Path) {
        std::fs::remove_dir_all(dir)
            .unwrap_or_else(|e| panic!("remove test dir {}: {}", dir.display(), e));
    }

    #[test]
    fn discover_no_file_returns_empty() {
        let p = Path::new("/nonexistent/path/style.css");
        assert!(discover_watched_imports(p).is_empty());
    }

    #[test]
    fn discover_file_without_imports_returns_empty() {
        let tmp = make_test_dir("no-imports");
        let css = tmp.join("style.css");
        std::fs::write(&css, "window { color: red; }").expect("write style.css");
        assert!(discover_watched_imports(&css).is_empty());
        cleanup_test_dir(&tmp);
    }

    #[test]
    fn discover_relative_import_resolved_and_existing() {
        let tmp = make_test_dir("rel-import");
        let css = tmp.join("style.css");
        let import = tmp.join("theme.css");
        std::fs::write(&import, "").expect("write theme.css");
        std::fs::write(&css, "@import \"theme.css\";").expect("write style.css");
        let found = discover_watched_imports(&css);
        // `discover_watched_imports` canonicalizes — compare against the
        // canonical form of the import path so symlink-under-/tmp setups
        // (e.g. macOS /tmp → /private/tmp) still match.
        let expected = import.canonicalize().expect("canonicalize import");
        assert_eq!(found, vec![expected]);
        cleanup_test_dir(&tmp);
    }

    /// Regression for the CodeRabbit catch on #75: a relative import
    /// containing `.` segments used to be stored lexically (e.g.
    /// `/dir/./theme.css`) but notify events always use the canonical
    /// form (`/dir/theme.css`), so the `HashSet::contains` match
    /// silently failed and hot-reload never fired. Canonicalizing both
    /// the watched set entry and the (implicit) event path fixes it;
    /// this test pins the canonical form by construction.
    #[test]
    fn discover_dot_segment_import_canonicalized() {
        let tmp = make_test_dir("dot-segment");
        let css = tmp.join("style.css");
        let import = tmp.join("theme.css");
        std::fs::write(&import, "").expect("write theme.css");
        std::fs::write(&css, "@import \"./theme.css\";").expect("write style.css");
        let found = discover_watched_imports(&css);
        let expected = import.canonicalize().expect("canonicalize import");
        assert_eq!(found, vec![expected]);
        // Ensure no stray `.` segment survived into the stored path.
        assert!(
            !found[0].components().any(|c| matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )),
            "stored path should not contain `.` or `..` segments: {}",
            found[0].display()
        );
        cleanup_test_dir(&tmp);
    }

    #[test]
    fn discover_skips_nonexistent_imports() {
        let tmp = make_test_dir("missing-import");
        let css = tmp.join("style.css");
        std::fs::write(&css, "@import \"missing-theme.css\";").expect("write style.css");
        assert!(discover_watched_imports(&css).is_empty());
        cleanup_test_dir(&tmp);
    }

    #[test]
    fn discover_skips_http_imports() {
        let tmp = make_test_dir("http-import");
        let css = tmp.join("style.css");
        std::fs::write(&css, "@import \"https://example.com/theme.css\";")
            .expect("write style.css");
        assert!(discover_watched_imports(&css).is_empty());
        cleanup_test_dir(&tmp);
    }

    // ─── is_content_change (feedback-loop guard) ─────────────────────────
    //
    // Regression test for the loop that showed up during #74 smoke
    // testing: GTK's `load_from_path` and our own `read_to_string`
    // both fire `Access(Open)` inotify events on the CSS file they
    // read, which used to match the watched set and trigger a reload,
    // which opened the file again. `is_content_change` narrows the
    // handler to create/modify/remove kinds so self-reads don't
    // re-enter the reload path.

    #[test]
    fn is_content_change_accepts_create_modify_remove() {
        use notify::EventKind;
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
    }

    #[test]
    fn is_content_change_rejects_access_events() {
        use notify::EventKind;
        use notify::event::{AccessKind, AccessMode};
        // These are the kinds our own reload cycle generates when we
        // open the CSS file to reload it. They must NOT count as
        // content changes, otherwise we self-trigger a reload loop.
        assert!(!is_content_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
    }

    #[test]
    fn is_content_change_rejects_any_and_other() {
        use notify::EventKind;
        assert!(!is_content_change(&EventKind::Any));
        assert!(!is_content_change(&EventKind::Other));
    }

    // ─── make_css_handler (end-to-end event routing) ─────────────────────
    //
    // Exercises the full handler contract — content-change kind check
    // AND watched-path match AND channel send — by feeding synthetic
    // `notify::Event` values into the closure and reading from the
    // receiver. This is the layer where we missed the feedback-loop
    // bug during #74 smoke testing; the tests below pin down every
    // combination that should / shouldn't send.

    fn modify_event(path: &Path) -> Result<notify::Event, notify::Error> {
        use notify::event::{DataChange, ModifyKind};
        use notify::{Event, EventKind};
        Ok(
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(path.to_path_buf()),
        )
    }

    fn access_event(path: &Path) -> Result<notify::Event, notify::Error> {
        use notify::event::{AccessKind, AccessMode};
        use notify::{Event, EventKind};
        Ok(
            Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
                .add_path(path.to_path_buf()),
        )
    }

    fn create_event(path: &Path) -> Result<notify::Event, notify::Error> {
        use notify::event::CreateKind;
        use notify::{Event, EventKind};
        Ok(Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_path_buf()))
    }

    fn remove_event(path: &Path) -> Result<notify::Event, notify::Error> {
        use notify::event::RemoveKind;
        use notify::{Event, EventKind};
        Ok(Event::new(EventKind::Remove(RemoveKind::File)).add_path(path.to_path_buf()))
    }

    #[test]
    fn handler_sends_on_modify_to_watched_path() {
        let watched_path = PathBuf::from("/tmp/style.css");
        let mut watched = HashSet::new();
        watched.insert(watched_path.clone());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        handler(modify_event(&watched_path));
        assert!(rx.try_recv().is_ok(), "Modify on watched path must send");
    }

    #[test]
    fn handler_sends_on_create_and_remove_of_watched_path() {
        let watched_path = PathBuf::from("/tmp/style.css");
        let mut watched = HashSet::new();
        watched.insert(watched_path.clone());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        handler(create_event(&watched_path));
        handler(remove_event(&watched_path));
        // Two events → two sends (debounce happens downstream in
        // `drain_events`, not in the handler).
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    /// Regression for the #74 smoke-test bug: Access events on watched
    /// paths were firing reloads, which re-opened the file via
    /// `load_from_path`, which generated more Access events, which
    /// triggered more reloads. The handler must drop Access events
    /// on the floor even when the path matches.
    #[test]
    fn handler_ignores_access_events_on_watched_path() {
        let watched_path = PathBuf::from("/tmp/style.css");
        let mut watched = HashSet::new();
        watched.insert(watched_path.clone());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        // Fire a bunch of Access events — none should reach the channel.
        for _ in 0..5 {
            handler(access_event(&watched_path));
        }
        assert!(
            rx.try_recv().is_err(),
            "Access events must not send — they're our own reload's self-feedback"
        );
    }

    #[test]
    fn handler_ignores_modify_on_unwatched_path() {
        let watched_path = PathBuf::from("/tmp/style.css");
        let unrelated = PathBuf::from("/tmp/gdk-pixbuf-glycin-tmp.XYZ");
        let mut watched = HashSet::new();
        watched.insert(watched_path);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        // Glycin constantly churns temp files in /tmp; those must not
        // trigger reloads even though their parent dir is watched.
        handler(modify_event(&unrelated));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handler_sends_when_any_event_path_matches() {
        // Some notify events carry multiple paths (e.g. rename). If any
        // one matches the watched set, the event still counts.
        use notify::event::{DataChange, ModifyKind};
        use notify::{Event, EventKind};
        let watched_path = PathBuf::from("/tmp/style.css");
        let unrelated = PathBuf::from("/tmp/unrelated.tmp");
        let mut watched = HashSet::new();
        watched.insert(watched_path.clone());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        let ev = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
            .add_path(unrelated)
            .add_path(watched_path);
        handler(Ok(ev));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn handler_does_not_panic_on_error_event() {
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/tmp/style.css"));
        let (tx, _rx) = std::sync::mpsc::channel::<()>();
        let mut handler = make_css_handler(watched, tx);
        // `notify::Error` isn't easy to construct directly; use the
        // generic io-error path. This proves the handler's match arm
        // for `Err` is reachable and doesn't panic.
        let err = notify::Error::io(std::io::Error::other("synthetic test error"));
        handler(Err(err));
        // No assertion on channel — just prove the call returned cleanly.
    }

    // ─── compute_watched_set / compute_watched_dirs (issue #74) ────────────
    //
    // Pure helpers used by `maybe_rebuild_watcher` to diff old-vs-new
    // `@import` sets across reloads. Tested without notify or GTK so we
    // can assert the equality semantics that decide whether to rebuild.

    #[test]
    fn watched_set_contains_main_css_when_no_imports() {
        let main = PathBuf::from("/home/user/.config/dock/style.css");
        let set = compute_watched_set(&main, &[]);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&main));
    }

    #[test]
    fn watched_set_contains_main_and_all_imports() {
        let main = PathBuf::from("/home/user/.config/dock/style.css");
        let imports = vec![
            PathBuf::from("/home/user/.local/share/theme/base16.css"),
            PathBuf::from("/home/user/.config/dock/extras.css"),
        ];
        let set = compute_watched_set(&main, &imports);
        assert_eq!(set.len(), 3);
        assert!(set.contains(&main));
        for imp in &imports {
            assert!(set.contains(imp));
        }
    }

    /// Regression for the #74 rebuild decision: the equality check
    /// between old and new sets must treat "same imports" as "no
    /// rebuild needed", even if the order in which imports were
    /// passed to `compute_watched_set` differs.
    #[test]
    fn watched_set_equality_is_order_independent() {
        let main = PathBuf::from("/style.css");
        let a = PathBuf::from("/a.css");
        let b = PathBuf::from("/b.css");
        let set1 = compute_watched_set(&main, &[a.clone(), b.clone()]);
        let set2 = compute_watched_set(&main, &[b.clone(), a.clone()]);
        assert_eq!(set1, set2);
    }

    #[test]
    fn watched_set_differs_when_import_added_or_removed() {
        let main = PathBuf::from("/style.css");
        let a = PathBuf::from("/a.css");
        let b = PathBuf::from("/b.css");
        let before = compute_watched_set(&main, std::slice::from_ref(&a));
        let after_added = compute_watched_set(&main, &[a.clone(), b.clone()]);
        let after_removed = compute_watched_set(&main, &[]);
        assert_ne!(before, after_added);
        assert_ne!(before, after_removed);
        assert_ne!(after_added, after_removed);
    }

    #[test]
    fn watched_dirs_collapses_shared_parent() {
        // Two imports under the same directory should produce one
        // notify watch, not two — notify subscribes to a dir, not a
        // file, and double-watching the same dir wastes file handles.
        let main = PathBuf::from("/home/user/style.css");
        let imports = vec![
            PathBuf::from("/home/user/a.css"),
            PathBuf::from("/home/user/b.css"),
        ];
        let dirs = compute_watched_dirs(&main, &imports);
        assert_eq!(dirs.len(), 1);
        assert!(dirs.contains(Path::new("/home/user")));
    }

    #[test]
    fn watched_dirs_includes_all_distinct_parents() {
        let main = PathBuf::from("/home/user/.config/dock/style.css");
        let imports = vec![
            PathBuf::from("/home/user/.local/share/theme/base16.css"),
            PathBuf::from("/home/user/.cache/dock/colors.css"),
        ];
        let dirs = compute_watched_dirs(&main, &imports);
        assert_eq!(dirs.len(), 3);
        assert!(dirs.contains(Path::new("/home/user/.config/dock")));
        assert!(dirs.contains(Path::new("/home/user/.local/share/theme")));
        assert!(dirs.contains(Path::new("/home/user/.cache/dock")));
    }

    /// End-to-end regression for #74: the sequence of user actions
    /// (save main CSS with one set of imports, then save with a
    /// different set) must produce different watched sets so
    /// `maybe_rebuild_watcher` triggers a rebuild.
    #[test]
    fn discover_tracks_changing_import_set_across_rewrites() {
        let tmp = make_test_dir("dynamic-rescan");
        let css = tmp.join("style.css");
        let theme_a = tmp.join("theme-a.css");
        let theme_b = tmp.join("theme-b.css");
        std::fs::write(&theme_a, "").expect("write theme-a.css");
        std::fs::write(&theme_b, "").expect("write theme-b.css");

        // Initial state: imports theme-a.
        std::fs::write(&css, "@import \"theme-a.css\";").expect("write style.css");
        let set_a = compute_watched_set(&css, &discover_watched_imports(&css));

        // User edits main CSS to import theme-b instead.
        std::fs::write(&css, "@import \"theme-b.css\";").expect("rewrite style.css");
        let set_b = compute_watched_set(&css, &discover_watched_imports(&css));

        // User edits main CSS to import both.
        std::fs::write(&css, "@import \"theme-a.css\"; @import \"theme-b.css\";")
            .expect("rewrite style.css");
        let set_both = compute_watched_set(&css, &discover_watched_imports(&css));

        // User edits main CSS to drop all imports.
        std::fs::write(&css, "window { color: red; }").expect("rewrite style.css");
        let set_none = compute_watched_set(&css, &discover_watched_imports(&css));

        // Every transition must surface as a set change so the
        // rebuild guard fires.
        assert_ne!(set_a, set_b);
        assert_ne!(set_a, set_both);
        assert_ne!(set_a, set_none);
        assert_ne!(set_b, set_both);
        assert_ne!(set_b, set_none);
        assert_ne!(set_both, set_none);

        cleanup_test_dir(&tmp);
    }

    // ─── Cyclical / self-referential import safety ─────────────────────────
    //
    // These regressions pin the safety-by-construction properties that
    // protect against infinite loops or unbounded work when a user
    // accidentally (or deliberately) writes `@import` directives that
    // reference each other or themselves:
    //
    // 1. `discover_watched_imports` is non-recursive — it only parses the
    //    main CSS, never imports-of-imports. So `a.css ↔ b.css` produces
    //    a bounded watched set.
    // 2. `compute_watched_set` uses `HashSet<PathBuf>`, so identical
    //    canonical paths collapse — self-import (`a.css` importing
    //    itself) yields a one-element set, not an unbounded one.
    //
    // Actual CSS cycle-detection at *parse* time is GTK's responsibility;
    // we're only asserting that our watching logic doesn't blow up.

    #[test]
    fn self_import_dedupes_to_single_entry() {
        let tmp = make_test_dir("self-import");
        let css = tmp.join("style.css");
        // A file that `@import`s itself by absolute path.
        let content = format!("@import \"{}\";", css.display());
        std::fs::write(&css, &content).expect("write self-import style.css");

        let imports = discover_watched_imports(&css);
        let watched = compute_watched_set(&css, &imports);

        // The main CSS and the "import" point to the same file, so the
        // set contains exactly one entry after canonical dedup.
        assert_eq!(
            watched.len(),
            1,
            "self-import must dedupe via HashSet: {:?}",
            watched
        );
        let canonical_css = css.canonicalize().expect("canonicalize main css");
        assert!(watched.contains(&canonical_css));

        cleanup_test_dir(&tmp);
    }

    #[test]
    fn mutual_import_produces_bounded_set() {
        let tmp = make_test_dir("mutual-import");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        // Mutual cycle: a imports b, b imports a.
        std::fs::write(&a, format!("@import \"{}\";", b.display())).expect("write a.css");
        std::fs::write(&b, format!("@import \"{}\";", a.display())).expect("write b.css");

        let imports = discover_watched_imports(&a);
        let watched = compute_watched_set(&a, &imports);

        // We parse only the main CSS (a.css) and its direct imports
        // (b.css). We never recurse into b.css to discover its imports,
        // so the watched set is {a, b} — two entries, bounded.
        assert_eq!(
            watched.len(),
            2,
            "mutual import set must be bounded at direct-import depth: {:?}",
            watched
        );

        cleanup_test_dir(&tmp);
    }

    /// #77: a nested chain `main → a.css → b.css` now tracks every
    /// level. Changes to `b.css` fire a reload even though `main` only
    /// imports `a.css` directly.
    #[test]
    fn nested_imports_are_recursively_discovered() {
        let tmp = make_test_dir("nested-imports");
        let main = tmp.join("style.css");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        std::fs::write(&b, "").expect("write b.css");
        std::fs::write(&a, format!("@import \"{}\";", b.display())).expect("write a.css");
        std::fs::write(&main, format!("@import \"{}\";", a.display())).expect("write style.css");

        let imports = discover_watched_imports(&main);
        let watched = compute_watched_set(&main, &imports);

        let canonical_a = a.canonicalize().expect("canonicalize a.css");
        let canonical_b = b.canonicalize().expect("canonicalize b.css");
        assert_eq!(
            watched.len(),
            3,
            "expected {{main, a.css, b.css}} but got {:?}",
            watched
        );
        assert!(watched.contains(&canonical_a));
        assert!(watched.contains(&canonical_b));

        cleanup_test_dir(&tmp);
    }

    /// #77: deep chain `main → a → b → c → d` — the transitive closure.
    #[test]
    fn deep_import_chain_fully_discovered() {
        let tmp = make_test_dir("deep-chain");
        let main = tmp.join("style.css");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        let c = tmp.join("c.css");
        let d = tmp.join("d.css");
        std::fs::write(&d, "").expect("write d.css");
        std::fs::write(&c, format!("@import \"{}\";", d.display())).expect("write c.css");
        std::fs::write(&b, format!("@import \"{}\";", c.display())).expect("write b.css");
        std::fs::write(&a, format!("@import \"{}\";", b.display())).expect("write a.css");
        std::fs::write(&main, format!("@import \"{}\";", a.display())).expect("write style.css");

        let imports = discover_watched_imports(&main);
        let watched = compute_watched_set(&main, &imports);

        assert_eq!(
            watched.len(),
            5,
            "expected main + a + b + c + d, got {:?}",
            watched
        );
        for file in [&a, &b, &c, &d] {
            let canonical = file.canonicalize().expect("canonicalize");
            assert!(
                watched.contains(&canonical),
                "{} missing from watched set",
                file.display()
            );
        }

        cleanup_test_dir(&tmp);
    }

    /// #77: diamond graph `main → a, main → b, a → c, b → c` — `c` is
    /// reachable two ways but must only appear once in the output
    /// (no duplicate work, no duplicate watch).
    #[test]
    fn diamond_import_graph_visits_shared_node_once() {
        let tmp = make_test_dir("diamond-import");
        let main = tmp.join("style.css");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        let c = tmp.join("c.css");
        std::fs::write(&c, "").expect("write c.css");
        std::fs::write(&a, format!("@import \"{}\";", c.display())).expect("write a.css");
        std::fs::write(&b, format!("@import \"{}\";", c.display())).expect("write b.css");
        std::fs::write(
            &main,
            format!("@import \"{}\";\n@import \"{}\";", a.display(), b.display()),
        )
        .expect("write style.css");

        let imports = discover_watched_imports(&main);
        let watched = compute_watched_set(&main, &imports);

        // main + a + b + c = 4. c appears in imports at most once.
        assert_eq!(watched.len(), 4, "{:?}", watched);
        let canonical_c = c.canonicalize().expect("canonicalize c.css");
        assert_eq!(
            imports.iter().filter(|p| **p == canonical_c).count(),
            1,
            "c.css must appear exactly once in discovery output"
        );

        cleanup_test_dir(&tmp);
    }

    /// #77: cycles across the graph (not just self-import) terminate.
    /// Chain `main → a → b → a` — `a` is revisited via `b` but already
    /// in the visited set, so the walk terminates.
    #[test]
    fn multi_hop_cycle_terminates() {
        let tmp = make_test_dir("multihop-cycle");
        let main = tmp.join("style.css");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        // a imports b, b imports a (cycle starts at a, back via b).
        std::fs::write(&a, format!("@import \"{}\";", b.display())).expect("write a.css");
        std::fs::write(&b, format!("@import \"{}\";", a.display())).expect("write b.css");
        std::fs::write(&main, format!("@import \"{}\";", a.display())).expect("write style.css");

        let imports = discover_watched_imports(&main);
        let watched = compute_watched_set(&main, &imports);

        // main + a + b = 3. The back-edge b → a is detected as a cycle.
        assert_eq!(watched.len(), 3, "{:?}", watched);

        cleanup_test_dir(&tmp);
    }

    /// #77: depth cap — a longer-than-`MAX_IMPORT_GRAPH_SIZE` linear
    /// chain stops at the cap with a warning instead of following
    /// forever. We build `MAX_IMPORT_GRAPH_SIZE + 5` files so the cap
    /// actually bites.
    #[test]
    fn import_graph_size_is_capped() {
        let tmp = make_test_dir("depth-cap");
        let chain_len = MAX_IMPORT_GRAPH_SIZE + 5;
        let files: Vec<PathBuf> = (0..chain_len)
            .map(|i| tmp.join(format!("f{}.css", i)))
            .collect();
        // Build in reverse so each file's import target already exists.
        std::fs::write(files.last().unwrap(), "").expect("write tail");
        for pair in files.windows(2).rev() {
            let (from, to) = (&pair[0], &pair[1]);
            std::fs::write(from, format!("@import \"{}\";", to.display()))
                .expect("write chain link");
        }
        let main = tmp.join("style.css");
        std::fs::write(&main, format!("@import \"{}\";", files[0].display())).expect("write main");

        let imports = discover_watched_imports(&main);
        // Linear, duplicate-free chain → discovery should reach the
        // boundary exactly. A regression that stops the walk earlier
        // (e.g., off-by-one in the cap check) would be hidden by a
        // loose `<=` assertion; pin the exact value instead.
        assert_eq!(
            imports.len(),
            MAX_IMPORT_GRAPH_SIZE,
            "linear chain should discover exactly up to the cap (got {})",
            imports.len()
        );

        cleanup_test_dir(&tmp);
    }

    /// Non-UTF-8 content in the main CSS is treated the same as an
    /// unreadable file: `read_to_string` returns an `InvalidData` error,
    /// `read_direct_imports` logs at debug and returns `None`, and
    /// discovery produces an empty result without panicking. GTK will
    /// fail to load the file with its own warning at reload time — we
    /// just need to stay out of the way.
    #[test]
    fn discover_non_utf8_main_returns_empty_without_panic() {
        let tmp = make_test_dir("non-utf8-main");
        let main = tmp.join("style.css");
        // 0xFF 0xFE is a BOM-like byte sequence that is NOT valid UTF-8
        // when standalone. read_to_string rejects the whole file on
        // the first invalid byte.
        std::fs::write(&main, [0xFFu8, 0xFE, 0x80, 0x81, 0x82, 0xFF])
            .expect("write non-utf8 bytes");

        let imports = discover_watched_imports(&main);
        assert!(
            imports.is_empty(),
            "non-utf8 file should yield no imports; got {:?}",
            imports
        );
        cleanup_test_dir(&tmp);
    }

    /// The parser only cares about `@import` directives — anything else
    /// in the file is skipped. This test pins the "garbage surrounded"
    /// case: junk tokens, half-formed rules, and mismatched braces
    /// around a legitimate `@import` line, all in one file. Discovery
    /// must still extract the valid target without tripping over the
    /// surrounding mess.
    #[test]
    fn discover_extracts_valid_import_from_garbage_content() {
        let tmp = make_test_dir("garbage-plus-valid");
        let main = tmp.join("style.css");
        let theme = tmp.join("theme.css");
        std::fs::write(&theme, "").expect("write theme.css");
        // Mix of nonsense that GTK will reject plus one real @import.
        // The parser scans for the `@import` substring, extracts the
        // quoted path, and leaves the rest to GTK's own parse-warning
        // reporting.
        let content = format!(
            "{{ unclosed brace\n\
             nonsense garbage  ::: nope\n\
             @import \"{}\";\n\
             @;;; @@ garbage\n\
             window {{ not really css",
            theme.display()
        );
        std::fs::write(&main, content).expect("write garbage main");

        let imports = discover_watched_imports(&main);
        let canonical_theme = theme.canonicalize().expect("canonicalize theme.css");
        assert_eq!(
            imports,
            vec![canonical_theme],
            "valid @import inside garbage should still be discovered"
        );
        cleanup_test_dir(&tmp);
    }

    /// When a node in the `@import` graph is unreadable (permission
    /// denied), the walk should:
    ///   - keep the file itself in the discovered set (its parent
    ///     already canonicalized + queued it, and the watcher can
    ///     still react to future content changes or a chmod that
    ///     restores readability),
    ///   - skip its children silently — `read_direct_imports` logs
    ///     at debug level and returns `None`,
    ///   - NOT panic, NOT propagate the failure upward.
    ///
    /// The self-heal path: when perms are fixed, a subsequent
    /// chmod/save on the file fires a `Modify` event that passes our
    /// content-change filter, triggering `maybe_rebuild_watcher` →
    /// rescan → discovery completes the chain.
    #[cfg(unix)]
    #[test]
    fn unreadable_node_skips_children_without_panic() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = make_test_dir("unreadable-node");
        let main = tmp.join("style.css");
        let a = tmp.join("a.css");
        let b = tmp.join("b.css");
        std::fs::write(&b, "").expect("write b.css");
        std::fs::write(&a, format!("@import \"{}\";", b.display())).expect("write a.css");
        std::fs::write(&main, format!("@import \"{}\";", a.display())).expect("write style.css");

        // Strip read perms from a.css so its content (and therefore
        // its `@import b.css`) is invisible to discovery.
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000))
            .expect("chmod a.css to 000");

        let imports = discover_watched_imports(&main);
        let watched = compute_watched_set(&main, &imports);

        let canonical_a = a.canonicalize().expect("canonicalize a.css");
        let canonical_b = b.canonicalize().expect("canonicalize b.css");

        // a.css is still in the watched set — we canonicalize via
        // stat, which doesn't need read perms on the file — so
        // content changes fire events and the watcher will
        // self-heal once perms are fixed.
        assert!(
            watched.contains(&canonical_a),
            "unreadable a.css should still be watched (for self-heal on chmod); got {:?}",
            watched
        );
        // b.css is reachable through a.css but we couldn't read a
        // to find it, so it's not in the set.
        assert!(
            !watched.contains(&canonical_b),
            "b.css should not be discovered when a.css is unreadable; got {:?}",
            watched
        );

        // Restore perms so cleanup_test_dir's remove_dir_all doesn't
        // trip on the locked-down file.
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644))
            .expect("restore a.css perms");
        cleanup_test_dir(&tmp);
    }

    /// Regression for the CodeRabbit catch on #79: when the main CSS
    /// is reached via a symlinked directory, relative `@import` paths
    /// must resolve against the **as-referenced** parent (the symlink
    /// path the user handed to GTK), not the canonical target. GTK4
    /// uses the as-given base for its own `@import` resolution, so
    /// discovery must agree — otherwise we'd watch a different set of
    /// files than GTK actually loads, and edits to the real targets
    /// wouldn't hot-reload.
    ///
    /// Fixture:
    ///   /tmp/<test>/real/style.css   (contains `@import "theme.css"`)
    ///   /tmp/<test>/real/theme.css   — exists via the alias path
    ///   /tmp/<test>/alias            → symlink to `real`
    ///
    /// Discovery is invoked via the alias path. We verify the output
    /// contains the canonical form of `theme.css` so the notify match
    /// set lines up with event paths (which are canonical).
    #[cfg(unix)]
    #[test]
    fn discovery_uses_as_referenced_base_dir_for_symlinked_parent() {
        let tmp = make_test_dir("symlink-parent");
        let real = tmp.join("real");
        std::fs::create_dir_all(&real).expect("create real dir");
        let real_style = real.join("style.css");
        let real_theme = real.join("theme.css");
        std::fs::write(&real_theme, "").expect("write theme.css");
        std::fs::write(&real_style, "@import \"theme.css\";").expect("write style.css");

        let alias = tmp.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("create symlink alias→real");
        let alias_style = alias.join("style.css");

        let imports = discover_watched_imports(&alias_style);
        let canonical_theme = real_theme.canonicalize().expect("canonicalize theme.css");

        assert_eq!(imports.len(), 1, "expected one import, got {:?}", imports);
        assert_eq!(
            imports[0], canonical_theme,
            "discovery must canonicalize the import target so notify match works"
        );

        cleanup_test_dir(&tmp);
    }

    // ─── CssWatchHandle / watch_css_rebindable (CR-2026-05-03-26) ────────
    //
    // These tests exercise the rebindable watcher API without a GLib main
    // loop.  `watch_css_rebindable` and `rebind` are tested only for the
    // watcher-setup and path-resolution mechanics; the GLib timer closure
    // cannot be driven in a unit-test context (no GLib init in `cargo
    // test`), so the actual hot-reload callback path is covered by the
    // integration smoke test in nwg-dock.
    //
    // Tests that DO require GTK init are marked `#[ignore]` and document
    // why; they are exercised by `make test-integration`.

    /// `compute_canonical_pair` returns the correct pair for an existing path.
    #[test]
    fn canonical_pair_resolves_existing_path() {
        let tmp = make_test_dir("canonical-pair");
        let css = tmp.join("style.css");
        std::fs::write(&css, "").expect("write style.css");
        let (as_ref, canonical) =
            compute_canonical_pair(&css).expect("pair must resolve for existing path");
        // as_referenced is the path as given — not canonicalized.
        assert_eq!(as_ref, css);
        // canonical has no dot/dotdot segments (it's the canonicalized form).
        assert!(
            !canonical.components().any(|c| matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )),
            "canonical path must have no `.` or `..` segments: {}",
            canonical.display()
        );
        cleanup_test_dir(&tmp);
    }

    /// `compute_canonical_pair` returns `Err` preserving the OS error
    /// when the parent directory does not exist (or any other
    /// canonicalize() failure).
    #[test]
    fn canonical_pair_returns_err_for_missing_parent() {
        // Build a never-created subpath under a controlled tempdir
        // so the test can't accidentally pass (or fail) because of
        // some pre-existing absolute path on the test machine.
        let tmp = make_test_dir("canonical-pair-missing");
        let path = tmp.join("never-created").join("style.css");
        let err = compute_canonical_pair(&path).expect_err("missing parent dir must yield Err");
        // The OS reports this as NotFound on Linux; we don't pin the
        // exact ErrorKind across platforms but we confirm it surfaces
        // a real io::Error rather than being collapsed.
        assert!(
            err.kind() == std::io::ErrorKind::NotFound,
            "expected NotFound for missing parent, got: {:?}",
            err.kind()
        );
        cleanup_test_dir(&tmp);
    }

    /// `rebind` to a different file that exists returns `Ok`.
    ///
    /// GTK init required — the `rebind` path calls `provider.load_from_path`
    /// and `notify::recommended_watcher`, which both require a display
    /// context.  Without GTK init the CssProvider constructor and/or
    /// `apply_provider` will panic.
    #[ignore = "requires GTK display context; exercised by make test-integration"]
    #[test]
    fn rebind_to_existing_path_returns_ok() {
        let tmp = make_test_dir("rebind-ok");
        let original = tmp.join("original.css");
        let target = tmp.join("target.css");
        std::fs::write(&original, "window { color: red; }").expect("write original.css");
        std::fs::write(&target, "window { color: blue; }").expect("write target.css");

        let provider = gtk4::CssProvider::new();
        let mut handle = watch_css_rebindable(&original, &provider);
        let result = handle.rebind(&target);
        assert!(
            result.is_ok(),
            "rebind to existing file must succeed: {:?}",
            result
        );

        cleanup_test_dir(&tmp);
    }

    /// `rebind` to a path whose parent directory does not exist returns
    /// `CssRebindError::Io` and the handle is still usable afterwards.
    ///
    /// GTK init required for the same reasons as `rebind_to_existing_path_returns_ok`.
    #[ignore = "requires GTK display context; exercised by make test-integration"]
    #[test]
    fn rebind_to_missing_parent_returns_err_handle_survives() {
        let tmp = make_test_dir("rebind-missing-parent");
        let original = tmp.join("original.css");
        // Build the bad path under our tempdir so the parent's
        // non-existence is controlled by the fixture rather than the
        // test machine's filesystem layout.
        let bad_path = tmp.join("never-created").join("style.css");
        let good_target = tmp.join("target.css");
        std::fs::write(&original, "window { color: red; }").expect("write original.css");
        std::fs::write(&good_target, "window { color: blue; }").expect("write target.css");

        let provider = gtk4::CssProvider::new();
        let mut handle = watch_css_rebindable(&original, &provider);

        // First rebind to a nonexistent parent — must fail.
        let err = handle.rebind(&bad_path);
        assert!(
            matches!(err, Err(CssRebindError::Io { .. })),
            "expected Io error for missing parent dir, got {:?}",
            err
        );

        // Second rebind to a real file — handle must still be valid.
        let ok = handle.rebind(&good_target);
        assert!(
            ok.is_ok(),
            "handle must still be usable after failed rebind: {:?}",
            ok
        );

        cleanup_test_dir(&tmp);
    }

    /// The `CssRebindError` variants have `Display` impls that include
    /// the path — exercises the `#[error(...)]` template expansion at
    /// least once to catch formatting regressions.
    #[test]
    fn rebind_error_display_includes_path() {
        let io_err = CssRebindError::Io {
            path: PathBuf::from("/tmp/style.css"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let msg = format!("{io_err}");
        assert!(
            msg.contains("/tmp/style.css"),
            "Display must include the path: {msg}"
        );

        let setup_err = CssRebindError::WatcherSetup {
            path: PathBuf::from("/tmp/alt.css"),
            message: "inotify limit reached".to_string(),
        };
        let msg2 = format!("{setup_err}");
        assert!(
            msg2.contains("/tmp/alt.css"),
            "Display must include the path: {msg2}"
        );
    }
}
