# Changelog

All notable changes to `nwg-common` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Pre-split note:** Prior to v0.3.0, this crate lived inside the
> [`mac-doc-hyprland`](https://github.com/jasonherald/mac-doc-hyprland) monorepo
> as `nwg-dock-common` at version 0.2.0. v0.3.0 is the first release of the
> library under its own repo + crates.io name. The full pre-split history is
> preserved in the monorepo's git log; this file only documents changes from
> v0.3.0 onward.

## [Unreleased]

## [0.5.1] — 2026-05-05

### Fixed

- `desktop::icons::create_pixbuf` and `desktop::icons::pixbuf_from_file`
  now cache decoded pixbufs by `(icon, size)` / `(path, w, h)` for the
  lifetime of the process. The underlying
  `gdk_pixbuf_new_from_file_at_scale` path goes through glycin in modern
  gdk-pixbuf builds, and glycin leaks a few KiB of decoder state per
  call. heaptrack tracing on `nwg-dock` traced that to a 15.9 GiB peak
  RSS over 2.5 days uptime
  ([jasonherald/nwg-dock#83](https://github.com/jasonherald/nwg-dock/issues/83))
  via the dock's per-rebuild icon-load path. Caching means we only
  invoke the glycin path once per unique input; `gtk4::gdk_pixbuf::Pixbuf`
  is a GObject so cached entries are refcount-bump cheap on hit. Cache
  is `thread_local!` (Pixbuf is `!Send` so a `static Mutex<...>` won't
  satisfy `Sync`), bounded by unique inputs (~50 entries for a typical
  dock, well under 1 MiB total). Drawer and notifications consumers
  pick up the same fix automatically with a dep bump.

## [0.5.0] — 2026-05-04

### Added

- `watch_css_rebindable(path, provider) -> CssWatchHandle` — same setup as
  `watch_css`, but returns a handle that supports atomically rebinding the
  inotify watcher to a different CSS file path at runtime. Enables consumers
  (e.g., `nwg-dock`) to hot-reload a newly-configured `css-file` path without
  restarting and without losing hot-reload on subsequent edits to the new file.
  Fixes the silent watcher-stale bug described in
  [jasonherald/nwg-dock#77](https://github.com/jasonherald/nwg-dock/issues/77)
  (CR-2026-05-03-26).
- `CssWatchHandle::rebind(&mut self, new_path: impl AsRef<Path>) -> Result<(), CssRebindError>` —
  atomically tears down the current watcher, loads the CSS at `new_path` into
  the existing provider, and starts watching `new_path` instead. If either
  watcher setup or path resolution fails, the original watcher is preserved and
  `Err` is returned so the caller can surface the failure.
- `CssRebindError` — typed error for `CssWatchHandle::rebind`. Distinguishes
  `Io` (file not found / permission denied) from `WatcherSetup` (notify
  initialisation failure). Implements `std::error::Error`, `Display`, and
  `Debug`.

## [0.4.0] — 2026-04-29

### Added

- `WmEvent::WorkspaceChanged { id: i32, name: String }` — emitted by
  the Hyprland (`workspacev2`) and Sway (`WorkspaceEvent` with
  `change: focus`) backends when the focused workspace changes.
  Enables consumers like the workspace-switcher widget in
  jasonherald/nwg-dock#4 to react in-frame.
- `Compositor::focus_workspace(workspace: i32) -> Result<()>` — new
  trait method for switching the focused workspace. Distinct from the
  existing `move_to_workspace(window_id, workspace)` which moves a
  window. Implemented for Hyprland (`hyprctl dispatch workspace N`),
  Sway (`swaymsg workspace number N`), and `NullCompositor` (returns
  `DockError::NoCompositorDetected`).
- `HyprEvent::WorkspaceV2 { id, name }` — backend-internal Hyprland
  event variant; `parse_event` recognizes `workspacev2>>ID,NAME` lines.

### Changed

- **Breaking:** `Compositor::focus_workspace` is a new required trait
  method. No external `Compositor` impls exist outside this workspace
  today, so the impact is bounded; the minor bump signals the contract
  change.
- `init_or_null` now warn-logs when falling back to `NullCompositor`,
  listing the degraded features (event reactions, autohide, workspace
  switcher). Previously silent on the "no compositor detected" arm.
  Surfaces the fallback so consumers switching from `init_or_exit`
  (e.g., jasonherald/nwg-dock#4) don't leave users wondering why live
  features are inactive on unsupported compositors (Niri, river,
  Openbox).

## [0.3.1] — 2026-04-28

### Changed

- `WmOverride` now derives `serde::Serialize` and `serde::Deserialize`,
  with `#[serde(rename_all = "kebab-case")]` so variants serialize as
  `"hyprland"` / `"sway"` / `"uwsm"` — matching clap's `ValueEnum`
  lowercasing. Enables consumers (e.g.,
  [`nwg-dock`](https://github.com/jasonherald/nwg-dock) #33) to
  deserialize `WmOverride` from TOML/JSON config files.

## [0.3.0] — 2026-04-20

First standalone release. Extracts the shared library that underpins
[`nwg-dock`](https://github.com/jasonherald/nwg-dock),
[`nwg-drawer`](https://github.com/jasonherald/nwg-drawer), and
[`nwg-notifications`](https://github.com/jasonherald/nwg-notifications) from
the monorepo.

### Added

- `compositor` — compositor-neutral IPC abstraction built on the `Compositor`
  trait, with Hyprland and Sway backends auto-detected from
  `HYPRLAND_INSTANCE_SIGNATURE` / `SWAYSOCK` env vars. `init_or_exit` for
  tools that require a compositor; `init_or_null` for tools (like the
  drawer) that degrade gracefully on unsupported compositors (Niri, river,
  Openbox, etc.).
- `compositor::{WmClient, WmMonitor, WmWorkspace, WmEvent}` — neutral types
  covering the window / output / workspace model used by the three tools.
- `config::paths` — XDG data/config/cache directory resolution
  (`cache_dir`, `config_dir`, `find_data_home`, `ensure_dir`, `copy_file`,
  `load_text_lines`).
- `config::css` — GTK4 CSS provider loading + hot-reload with recursive
  `@import` graph resolution and cycle detection. Watcher handles in-place
  file edits across editors that rename-swap vs. truncate-in-place.
- `config::flags::normalize_legacy_flags` — converts pre-clap single-dash
  long flags (`-daemon` → `--daemon`) for backwards compatibility.
- `desktop::entry` — `.desktop` file parser with locale-aware Name/Comment
  resolution and `StartupWMClass` tracking for class-to-desktop-ID matching.
- `desktop::icons` — icon file lookup + display-name resolution that falls
  back through locale → base name → raw class.
- `desktop::categories` — FreeDesktop main-category assignment with
  multi-category support and secondary-category mapping (Audio/Video →
  AudioVideo, etc.).
- `desktop::preferred_apps` — user-configured `mime-type → desktop-id`
  overrides.
- `desktop::dirs` — `XDG_DATA_DIRS` app directory enumeration.
- `launch` — application launching via direct spawn or compositor `exec`,
  with a shared child-reaper thread so GUI-app processes don't zombify.
  Covers `.desktop` entry launches (field-code stripping, theme prepend,
  terminal handling) and shell-command launches.
- `layer_shell::create_fullscreen_backdrops` — per-monitor transparent
  backdrop surfaces for click-outside-to-close UI patterns.
- `pinning` — case-insensitive pin/unpin + atomic save/load for the
  shared `~/.cache/mac-dock-pinned` file used by the dock and drawer.
- `process::handle_dump_args` — `--dump-args <pid>` helper that reads
  `/proc/<pid>/cmdline` and shell-quotes it for `make upgrade` to
  capture a running instance's arguments before restarting it.
- `signals` — RT signal plumbing (SIGRTMIN+1..+6) for toggle/show/hide
  of the dock/drawer/notifications windows, plus SIGTERM handling and
  `send_signal_to_pid` for cross-instance signaling. `sigrtmin()` queries
  the runtime value so the library is correct on glibc and musl.
- `singleton` — per-user single-instance lock file with stale-PID recovery
  (validates `/proc/<pid>/exe` against our own binary).
- `DockError` + `Result` — unified error type re-exported at the crate
  root for `nwg_common::DockError` / `nwg_common::Result<T>`.

### Changed

- Public API surface is now explicitly sealed: `hyprland::ipc` types,
  `compositor::{hyprland, sway, null}` backends, and various internal
  helpers that don't cross crate boundaries are `pub(crate)` or private.
  Only items that consumers of the library legitimately need are exposed.
- Every public item carries rustdoc. `#![warn(missing_docs)]` is enabled
  at the crate root, so `cargo doc --no-deps -p nwg-common` runs
  warning-free and `cargo rustdoc -p nwg-common -- -D missing-docs`
  succeeds.
- `compositor::{WmClient, WmMonitor, WmWorkspace, WmEvent}` are now
  `#[non_exhaustive]`. External crates must construct via
  `Default::default()` + the new fluent `with_*` setters
  (`WmMonitor::default().with_id(1).with_name("DP-1") …`) rather than
  struct literals; future field additions won't break downstream
  construction or exhaustive matches. Same-crate usage is unaffected.

### Fixed

- `sigrtmin()` queries `libc::SIGRTMIN()` at runtime instead of
  hardcoding `34`, so the RT-signal offsets resolve correctly on musl
  (glibc reserves two NPTL slots and starts user RT signals at 34;
  musl reserves three and starts at 35).
