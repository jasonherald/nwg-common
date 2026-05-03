# Workspace switcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land `WmEvent::WorkspaceChanged` + `Compositor::focus_workspace` in `nwg-common` 0.4.0, then ship a workspace-switcher widget in the next `nwg-dock` minor that consumes them — closing the parity epic with Go `nwg-dock`.

**Architecture:** Two phases across two repos with one dependency. Phase A adds the variant + trait method + Hyprland/Sway emits + a graceful-fallback warning log to `nwg-common` and ships 0.4.0 to crates.io. Phase B bumps the dock's `nwg-common` dep to `0.4`, switches `init_or_exit` to `init_or_null` (so the dock survives on Niri / river / Openbox instead of `exit(1)`), and adds an opt-in workspace-button row controlled by a new `--ws` flag. The widget splits into a pure `workspace_button_plan` helper (testable without GTK) and a thin `build_row` that turns the plan into widgets.

**Tech Stack:** Rust 2024 edition, `gtk4` 0.10 (dock), `hyprland-rs`/`swayipc` (already in `nwg-common`), `clap` 4 derive (dock CLI), `notify-rust` 4 (existing dock notifications). No new deps.

**Spec:** `docs/superpowers/specs/2026-04-29-workspace-switcher-design.md` is authoritative — when this plan and the spec disagree, the spec wins.

**Branches:**
- `nwg-common`: `feat/workspace-changed` (already created; the spec lives there as commit `b1becca`).
- `nwg-dock`: `feat/workspace-switcher` (created in Phase B).

---

## File structure

### Phase A — `nwg-common`

| Path | Action | Responsibility |
|---|---|---|
| `src/hyprland/events.rs` | modify | Add `HyprEvent::WorkspaceV2 { id, name }` variant; teach `parse_event` to recognize `workspacev2>>ID,NAME` lines |
| `src/compositor/types.rs` | modify | Add `WmEvent::WorkspaceChanged { id, name }` variant |
| `src/compositor/traits.rs` | modify | Add `fn focus_workspace(&self, workspace: i32) -> Result<()>` to the `Compositor` trait |
| `src/compositor/hyprland.rs` | modify | Map `HyprEvent::WorkspaceV2` → `WmEvent::WorkspaceChanged`; impl `focus_workspace` via `dispatch workspace N` |
| `src/compositor/sway/events.rs` | modify | Subscribe to workspace events (`["window","output","workspace"]`); add `EVENT_WORKSPACE` constant; map `change == "focus"` with `current` workspace → `WmEvent::WorkspaceChanged` |
| `src/compositor/sway/mod.rs` | modify | Impl `focus_workspace` via `workspace number N` |
| `src/compositor/null.rs` | modify | Impl `focus_workspace` returning `Err(NoCompositorDetected)` |
| `src/compositor/mod.rs` | modify | Add `log::warn!` to `init_or_null` describing the degraded fallback |
| `Cargo.toml` | modify | Version bump 0.3.1 → 0.4.0 (release task) |
| `CHANGELOG.md` | modify | New `[0.4.0]` entry |

### Phase B — `nwg-dock`

| Path | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | modify | Bump `nwg-common = "0.4"` |
| `src/config.rs` | modify | Add `pub ws: bool` flag with `--ws` long form |
| `src/main.rs` | modify | Switch from `init_or_exit(config.wm)` to `init_or_null(config.wm)` |
| `src/ui/mod.rs` | modify | Add `pub mod workspaces;` |
| `src/ui/workspaces.rs` | **create** | `workspace_button_plan` (pure, unit-tested) + `build_row` (impure GTK builder) |
| `src/ui/dock_box.rs` | modify | Insert workspace row between pinned and tasks when `ctx.config.ws` is true |
| `src/ui/css.rs` | modify | Add default `.dock-workspace-button` and `.dock-workspace-active` rules to `gtk4_compat_css` |
| `src/events.rs` | modify | React to `WmEvent::WorkspaceChanged` by triggering `rebuild()` |
| `src/config_file.rs` | modify | Add `ws` to `diff_config`'s comparison so hot-reload works (config-file `ws` integration is a follow-up; this is just the CLI-toggled hot-reload path) |
| `README.md` | modify | New `--ws` flag in usage; "Deviations from Go" line; theme classes documented in Theming section |
| `CHANGELOG.md` | modify | New `[X.Y.Z]` entry (user picks version) |

---

## Phase A: `nwg-common` 0.4.0

All work on the `feat/workspace-changed` branch in `~/source/nwg-common` (= `/data/source/nwg-common`).

### Task A1: Add `HyprEvent::WorkspaceV2` variant and parser

**Files:**
- Modify: `src/hyprland/events.rs`

- [ ] **Step 1: Add a failing test in `mod tests`**

Append inside `mod tests` in `src/hyprland/events.rs`:

```rust
    #[test]
    fn parse_workspacev2_event() {
        match parse_event("workspacev2>>3,chat") {
            HyprEvent::WorkspaceV2 { id, name } => {
                assert_eq!(id, 3);
                assert_eq!(name, "chat");
            }
            other => panic!("expected WorkspaceV2, got {:?}", other),
        }
    }

    #[test]
    fn parse_workspacev2_numeric_name() {
        match parse_event("workspacev2>>5,5") {
            HyprEvent::WorkspaceV2 { id, name } => {
                assert_eq!(id, 5);
                assert_eq!(name, "5");
            }
            other => panic!("expected WorkspaceV2, got {:?}", other),
        }
    }

    #[test]
    fn parse_workspacev2_malformed_id_falls_to_other() {
        // Defensive: if Hyprland ever sent a non-integer id, we surface
        // it as Other rather than panic or emit a half-formed event.
        assert!(matches!(
            parse_event("workspacev2>>notanint,chat"),
            HyprEvent::Other(_)
        ));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path /data/source/nwg-common/Cargo.toml hyprland::events::tests::parse_workspacev2
```

Expected: 3 compile failures referencing missing variant `HyprEvent::WorkspaceV2`.

- [ ] **Step 3: Add the variant**

In `src/hyprland/events.rs`, modify the `HyprEvent` enum:

```rust
#[derive(Debug, Clone)]
pub enum HyprEvent {
    /// A client changed in a way that may affect the visible client list:
    /// focus moved, a window opened/closed, or a window moved across
    /// workspaces. Carries the address from the originating event so
    /// downstream dedup against the last-seen address still works.
    ActiveWindowV2(String),
    /// Monitor added or removed.
    MonitorChanged,
    /// Focused workspace changed. Carries the workspace's id and name
    /// from Hyprland's `workspacev2` event so `nwg-common` can emit
    /// the high-level `WmEvent::WorkspaceChanged` without a separate
    /// IPC round-trip.
    WorkspaceV2 { id: i32, name: String },
    /// Any other event we don't specifically handle.
    Other(String),
}
```

- [ ] **Step 4: Teach `parse_event` to recognize the line**

In the same file, modify `parse_event`:

```rust
fn parse_event(line: &str) -> HyprEvent {
    if let Some(addr) = line.strip_prefix("activewindowv2>>") {
        HyprEvent::ActiveWindowV2(addr.trim().to_string())
    } else if let Some(rest) = line
        .strip_prefix("openwindow>>")
        .or_else(|| line.strip_prefix("closewindow>>"))
        .or_else(|| line.strip_prefix("movewindowv2>>"))
        .or_else(|| line.strip_prefix("movewindow>>"))
    {
        let addr = rest.split(',').next().unwrap_or("").trim().to_string();
        HyprEvent::ActiveWindowV2(addr)
    } else if line.starts_with("monitoraddedv2>>") || line.starts_with("monitorremoved>>") {
        HyprEvent::MonitorChanged
    } else if let Some(rest) = line.strip_prefix("workspacev2>>") {
        // Hyprland sends `workspacev2>>ID,NAME` — split on first comma.
        // If the id can't parse, fall through to Other so we don't
        // emit a half-formed event.
        let mut parts = rest.splitn(2, ',');
        let id_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim().to_string();
        match id_str.parse::<i32>() {
            Ok(id) => HyprEvent::WorkspaceV2 { id, name },
            Err(_) => HyprEvent::Other(line.to_string()),
        }
    } else {
        HyprEvent::Other(line.to_string())
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test --manifest-path /data/source/nwg-common/Cargo.toml hyprland::events::tests::parse_workspacev2
```

Expected: 3 passed.

- [ ] **Step 6: Run full suite + lint**

```bash
cd /data/source/nwg-common
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

All clean.

- [ ] **Step 7: Commit**

```bash
git -C /data/source/nwg-common add src/hyprland/events.rs
git -C /data/source/nwg-common commit -m "Add HyprEvent::WorkspaceV2 variant + parser

Recognizes 'workspacev2>>ID,NAME' lines from Hyprland's event socket.
Malformed id falls through to Other rather than emitting half-formed
events. Three unit tests pin: numeric name, named workspace, and
defensive malformed-id handling.

Refs jasonherald/nwg-common#2"
```

### Task A2: Add `WmEvent::WorkspaceChanged` variant + Hyprland mapping

**Files:**
- Modify: `src/compositor/types.rs`
- Modify: `src/compositor/hyprland.rs`

- [ ] **Step 1: Append failing test in `src/compositor/hyprland.rs` `mod tests` (or create one if absent)**

If a `#[cfg(test)] mod tests` block doesn't already exist at the bottom of `src/compositor/hyprland.rs`, add it. Then append:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `HyprlandEventStream` shim that lets us drive the mapping
    /// logic without a real Hyprland socket.
    struct FakeStream(Vec<crate::hyprland::events::HyprEvent>);
    impl FakeStream {
        fn next(&mut self) -> std::result::Result<crate::hyprland::events::HyprEvent, std::io::Error> {
            self.0.pop().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "drained")
            })
        }
    }

    #[test]
    fn workspace_v2_event_maps_to_workspace_changed() {
        // Direct mapping check — the WmEventStream impl pulls from the
        // inner stream; we verify the per-variant arm here.
        let hyp = crate::hyprland::events::HyprEvent::WorkspaceV2 {
            id: 3,
            name: "chat".into(),
        };
        let mapped: WmEvent = match hyp {
            crate::hyprland::events::HyprEvent::ActiveWindowV2(addr) => {
                WmEvent::ActiveWindowChanged(addr)
            }
            crate::hyprland::events::HyprEvent::MonitorChanged => WmEvent::MonitorChanged,
            crate::hyprland::events::HyprEvent::WorkspaceV2 { id, name } => {
                WmEvent::WorkspaceChanged { id, name }
            }
            crate::hyprland::events::HyprEvent::Other(s) => WmEvent::Other(s),
        };
        assert_eq!(
            mapped,
            WmEvent::WorkspaceChanged {
                id: 3,
                name: "chat".into(),
            }
        );
    }
}
```

The test inlines the mapping arms because `HyprlandEventStream::next_event` consumes self and reads from a real socket; lifting the match out for testing would be a larger refactor than warranted. The test still pins the per-variant correspondence — when the production code's match arm is added in Step 4, this test gives a comparable assertion.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test --manifest-path /data/source/nwg-common/Cargo.toml compositor::hyprland::tests::workspace_v2_event_maps_to_workspace_changed
```

Expected: compile error referencing missing `WmEvent::WorkspaceChanged` variant.

- [ ] **Step 3: Add the variant**

Modify `src/compositor/types.rs`'s `WmEvent` enum:

```rust
/// Events from the compositor event stream.
///
/// `#[non_exhaustive]` is intentional — adding variants is additive for
/// downstreams that match `_ =>`. Production consumers should never
/// exhaustively match on this enum.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WmEvent {
    /// Active window changed. Contains the window id.
    ActiveWindowChanged(String),
    /// Monitor added or removed (hotplug).
    MonitorChanged,
    /// Focused workspace changed. Carries the new workspace's id and
    /// name so consumers don't need to round-trip through
    /// `list_workspaces()` for the common case.
    WorkspaceChanged { id: i32, name: String },
    /// Any other event.
    Other(String),
}
```

(The existing `#[non_exhaustive]` and derives stay if already present — verify; add `PartialEq, Eq` if absent so the test's `assert_eq!` works.)

- [ ] **Step 4: Update Hyprland mapping**

Modify `src/compositor/hyprland.rs`'s `HyprlandEventStream::next_event`:

```rust
impl WmEventStream for HyprlandEventStream {
    fn next_event(&mut self) -> std::result::Result<WmEvent, std::io::Error> {
        match self.0.next_event()? {
            HyprEvent::ActiveWindowV2(addr) => Ok(WmEvent::ActiveWindowChanged(addr)),
            HyprEvent::MonitorChanged => Ok(WmEvent::MonitorChanged),
            HyprEvent::WorkspaceV2 { id, name } => {
                Ok(WmEvent::WorkspaceChanged { id, name })
            }
            HyprEvent::Other(s) => Ok(WmEvent::Other(s)),
        }
    }
}
```

- [ ] **Step 5: Run tests + lint**

```bash
cd /data/source/nwg-common
cargo test compositor::hyprland::tests
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing, no warnings.

- [ ] **Step 6: Commit**

```bash
git -C /data/source/nwg-common add src/compositor/types.rs src/compositor/hyprland.rs
git -C /data/source/nwg-common commit -m "Add WmEvent::WorkspaceChanged + Hyprland mapping

Maps HyprEvent::WorkspaceV2 { id, name } -> WmEvent::WorkspaceChanged
{ id, name } in the Hyprland event stream. The WmEvent enum was already
#[non_exhaustive]; downstreams matching _ => stay green.

Refs jasonherald/nwg-common#2"
```

### Task A3: Add `Compositor::focus_workspace` trait method

**Files:**
- Modify: `src/compositor/traits.rs`

- [ ] **Step 1: Add the method to the trait**

In `src/compositor/traits.rs`, add (next to `move_to_workspace`):

```rust
    /// Focus the given workspace.
    ///
    /// Hyprland: `dispatch workspace N`. Sway: `workspace number N`.
    /// `NullCompositor` returns `DockError::NoCompositorDetected` so
    /// callers can degrade gracefully (the workspace switcher widget
    /// in nwg-dock catches this and logs a warning per click).
    ///
    /// Distinct from `move_to_workspace(window_id, workspace)` which
    /// moves a window — `focus_workspace` switches the focused
    /// workspace itself.
    fn focus_workspace(&self, workspace: i32) -> Result<()>;
```

(This is a breaking change to the trait — Tasks A4-A6 add the impls before any rebuild succeeds.)

- [ ] **Step 2: Build expecting compile errors**

```bash
cd /data/source/nwg-common
cargo build 2>&1 | head -20
```

Expected: errors that `HyprlandBackend`, `SwayBackend`, `NullCompositor` don't implement `focus_workspace`. These get fixed in A4-A6.

- [ ] **Step 3: Don't commit yet**

Trait change leaves the workspace red until A6. Tasks A4-A6 land together as one commit at the end of A6.

### Task A4: Implement `focus_workspace` for Hyprland

**Files:**
- Modify: `src/compositor/hyprland.rs`

- [ ] **Step 1: Add the impl**

In `src/compositor/hyprland.rs`, add (next to `move_to_workspace`):

```rust
    fn focus_workspace(&self, workspace: i32) -> Result<()> {
        ipc::hyprctl(&format!("dispatch workspace {}", workspace))?;
        Ok(())
    }
```

### Task A5: Implement `focus_workspace` for Sway

**Files:**
- Modify: `src/compositor/sway/mod.rs`

- [ ] **Step 1: Add the impl**

In `src/compositor/sway/mod.rs`, add (next to `move_to_workspace`):

```rust
    fn focus_workspace(&self, workspace: i32) -> Result<()> {
        self.run_command(&format!("workspace number {}", workspace))
    }
```

### Task A6: Implement `focus_workspace` for `NullCompositor` + run + commit A3-A6

**Files:**
- Modify: `src/compositor/null.rs`

- [ ] **Step 1: Add the impl**

In `src/compositor/null.rs`, add (in the `impl Compositor for NullCompositor` block, near `move_to_workspace`):

```rust
    fn focus_workspace(&self, _workspace: i32) -> Result<()> {
        Err(DockError::NoCompositorDetected)
    }
```

- [ ] **Step 2: Add a unit test for the Null impl**

In `src/compositor/null.rs`'s `mod tests`, add (next to existing `assert_no_compositor` calls):

```rust
    #[test]
    fn focus_workspace_returns_no_compositor_error() {
        let c = NullCompositor;
        assert_no_compositor(c.focus_workspace(TEST_WORKSPACE));
    }
```

(`TEST_WORKSPACE` already exists in the same `mod tests` — same constant `move_to_workspace`'s test uses.)

- [ ] **Step 3: Build + test + lint**

```bash
cd /data/source/nwg-common
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All clean.

- [ ] **Step 4: Commit Tasks A3-A6 together**

```bash
git -C /data/source/nwg-common add \
    src/compositor/traits.rs \
    src/compositor/hyprland.rs \
    src/compositor/sway/mod.rs \
    src/compositor/null.rs
git -C /data/source/nwg-common commit -m "Add Compositor::focus_workspace + impls for all backends

New trait method for switching the focused workspace. Distinct from
move_to_workspace (which moves a window).

Hyprland: hyprctl dispatch workspace N
Sway:     swaymsg workspace number N
Null:     Err(NoCompositorDetected) — workspace switcher widget
          callers catch this and log per click.

Trait change is breaking (no #[non_exhaustive] on traits) — pinned
by the 0.4.0 minor bump. No external Compositor impls exist outside
this workspace.

Refs jasonherald/nwg-common#2"
```

### Task A7: Subscribe Sway to workspace events + map focus changes

**Files:**
- Modify: `src/compositor/sway/events.rs`

- [ ] **Step 1: Append failing test**

In `src/compositor/sway/events.rs`, add a `mod tests` block at the bottom if absent. Append:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the JSON-shape mapping: a Sway workspace event with
    /// change=focus and a current workspace produces the expected
    /// WmEvent::WorkspaceChanged.
    #[test]
    fn workspace_focus_event_maps_to_workspace_changed() {
        let json = serde_json::json!({
            "change": "focus",
            "current": { "id": 17, "num": 3, "name": "chat" },
            "old": null
        });
        let event = parse_workspace_event(&json);
        assert_eq!(
            event,
            Some(WmEvent::WorkspaceChanged {
                id: 3,
                name: "chat".into(),
            })
        );
    }

    #[test]
    fn workspace_focus_with_no_current_drops() {
        let json = serde_json::json!({
            "change": "focus",
            "current": null,
            "old": null
        });
        assert_eq!(parse_workspace_event(&json), None);
    }

    #[test]
    fn workspace_init_change_drops() {
        // Only "focus" change emits; "init" / "rename" / etc. drop.
        let json = serde_json::json!({
            "change": "init",
            "current": { "id": 1, "num": 1, "name": "1" },
            "old": null
        });
        assert_eq!(parse_workspace_event(&json), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test --manifest-path /data/source/nwg-common/Cargo.toml compositor::sway::events::tests::workspace
```

Expected: compile fails — `parse_workspace_event` doesn't exist.

- [ ] **Step 3: Add the workspace event constant + parser**

In `src/compositor/sway/events.rs`, modify the top-of-file:

```rust
use super::ipc::{MSG_SUBSCRIBE, read_response_with_type, send_message};
use crate::compositor::traits::WmEventStream;
use crate::compositor::types::WmEvent;
use crate::error::DockError;
use std::os::unix::net::UnixStream;

/// i3-ipc event type for output events (bit 31 set + type 7).
const EVENT_OUTPUT: u32 = 0x80000007;
/// i3-ipc event type for workspace events (bit 31 set + type 0).
const EVENT_WORKSPACE: u32 = 0x80000000;
```

Update the subscription payload in `SwayEventStream::connect` — change the `payload` line from:

```rust
let payload = b"[\"window\",\"output\"]";
```

to:

```rust
let payload = b"[\"window\",\"output\",\"workspace\"]";
```

And add a pure helper above the `WmEventStream` impl:

```rust
/// Pure parser for Sway workspace event JSON. Returns `Some(WmEvent::
/// WorkspaceChanged)` for `change == "focus"` with a non-null `current`
/// workspace; returns `None` for any other change type or when
/// `current` is missing/null. Defensive — Sway's protocol shouldn't
/// emit focus without `current`, but we drop the event silently rather
/// than panic.
fn parse_workspace_event(event: &serde_json::Value) -> Option<WmEvent> {
    let change = event.get("change").and_then(|v| v.as_str())?;
    if change != "focus" {
        return None;
    }
    let current = event.get("current")?;
    if current.is_null() {
        return None;
    }
    let id = current.get("num").and_then(|v| v.as_i64()).map(|n| n as i32)?;
    let name = current
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(WmEvent::WorkspaceChanged { id, name })
}
```

Then update the dispatch in `SwayEventStream::next_event` to handle the workspace branch:

```rust
impl WmEventStream for SwayEventStream {
    fn next_event(&mut self) -> std::result::Result<WmEvent, std::io::Error> {
        loop {
            let (body, msg_type) =
                read_response_with_type(&mut self.conn).map_err(|e| match e {
                    DockError::Ipc(io) => io,
                    other => std::io::Error::other(other.to_string()),
                })?;

            // Output events → MonitorChanged
            if msg_type == EVENT_OUTPUT {
                return Ok(WmEvent::MonitorChanged);
            }

            let event: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

            // Workspace events → WorkspaceChanged (focus only)
            if msg_type == EVENT_WORKSPACE {
                if let Some(wm_event) = parse_workspace_event(&event) {
                    return Ok(wm_event);
                }
                // Non-focus change or missing current — drop and read next event.
                log::debug!(
                    "Sway workspace event dropped (not a focus change with current): {:?}",
                    event.get("change")
                );
                continue;
            }

            let change = event.get("change").and_then(|v| v.as_str()).unwrap_or("");

            match change {
                "focus" | "new" | "close" => {
                    let id = event
                        .get("container")
                        .and_then(|c| c.get("id"))
                        .and_then(|v| v.as_i64())
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    return Ok(WmEvent::ActiveWindowChanged(id));
                }
                _ => continue,
            }
        }
    }
}
```

- [ ] **Step 4: Run tests + lint**

```bash
cd /data/source/nwg-common
cargo test compositor::sway::events
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 5: Commit**

```bash
git -C /data/source/nwg-common add src/compositor/sway/events.rs
git -C /data/source/nwg-common commit -m "Sway: subscribe to workspace events + emit WorkspaceChanged

Subscription payload now includes 'workspace'. New EVENT_WORKSPACE
constant (0x80000000, i3-ipc event type 0) drives the dispatch
branch. parse_workspace_event is pure: returns Some(WorkspaceChanged)
on change=focus with current populated, None otherwise (with debug
log noting dropped events).

Three unit tests pin the mapping: focus emits, no-current drops,
non-focus change drops.

Refs jasonherald/nwg-common#2"
```

### Task A8: Warning log in `init_or_null`

**Files:**
- Modify: `src/compositor/mod.rs`

- [ ] **Step 1: Locate `init_or_null` and update**

In `src/compositor/mod.rs`, replace the existing function body:

```rust
/// Detects and creates the compositor backend, falling back to NullCompositor
/// on failure instead of exiting. Used by nwg-drawer so it can run on any
/// compositor (Niri, river, Openbox, etc.) with graceful feature degradation.
///
/// Logs a warning at the fallback point so the user knows they're running
/// degraded — silent fallback masked the issue for `nwg-dock`'s
/// `init_or_exit` → `init_or_null` switch in jasonherald/nwg-dock#4.
pub fn init_or_null(wm_override: Option<WmOverride>) -> Box<dyn Compositor> {
    match detect(wm_override) {
        Ok(kind) => match create(kind) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "Compositor backend failed: {} — falling back to NullCompositor. \
                     Live features (event reactions, autohide, workspace switcher) \
                     will be inactive.",
                    e
                );
                Box::new(NullCompositor)
            }
        },
        Err(_) => {
            log::warn!(
                "No supported compositor detected (no HYPRLAND_INSTANCE_SIGNATURE \
                 / SWAYSOCK in env). Falling back to NullCompositor — live features \
                 (event reactions, autohide, workspace switcher) will be inactive. \
                 Pinned apps and click-to-launch still work."
            );
            Box::new(NullCompositor)
        }
    }
}
```

(Two warn calls because the two error paths describe slightly different states — backend create failed vs. no compositor detected at all.)

- [ ] **Step 2: Build + test + lint**

```bash
cd /data/source/nwg-common
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All clean.

- [ ] **Step 3: Commit**

```bash
git -C /data/source/nwg-common add src/compositor/mod.rs
git -C /data/source/nwg-common commit -m "init_or_null: warn-log on fallback so users know they're degraded

Two warn calls — one for 'backend create failed', one for 'no
compositor detected at all'. Both list the inactive features
(events, autohide, workspace switcher) and confirm what still
works (pinned apps, click-to-launch).

Surfaces the existing silent-fallback so jasonherald/nwg-dock#4 can
safely switch from init_or_exit to init_or_null without users
wondering why their dock is half-working.

Refs jasonherald/nwg-common#2"
```

### Task A9: CHANGELOG entry + version bump

**Files:**
- Modify: `Cargo.toml`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Bump version**

In `/data/source/nwg-common/Cargo.toml`, change `version = "0.3.1"` to `version = "0.4.0"`.

- [ ] **Step 2: Add CHANGELOG entry**

In `/data/source/nwg-common/CHANGELOG.md`, insert above the `## [0.3.1]` heading:

```markdown
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

- `init_or_null` now warn-logs when falling back to `NullCompositor`,
  listing the degraded features (event reactions, autohide, workspace
  switcher). Previously silent. Surfaces the fallback so consumers
  switching from `init_or_exit` (e.g., jasonherald/nwg-dock#4) don't
  leave users wondering why live features are inactive on unsupported
  compositors (Niri, river, Openbox).

### Breaking

- `Compositor::focus_workspace` is a new required trait method. No
  external `Compositor` impls exist outside this workspace today, so
  the impact is bounded; the minor bump signals the contract change.
```

- [ ] **Step 3: Build + test + lint**

```bash
cd /data/source/nwg-common
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All clean — verify `cargo build` reports `nwg-common v0.4.0`.

- [ ] **Step 4: Don't commit yet — bundle into the PR-open commit in A10**

### Task A10: Open Phase A PR

**Files:**
- Modify: `Cargo.toml`, `CHANGELOG.md` (already staged-ready from A9)

- [ ] **Step 1: Commit version bump + CHANGELOG**

```bash
git -C /data/source/nwg-common add Cargo.toml Cargo.lock CHANGELOG.md
git -C /data/source/nwg-common commit -m "Release prep 0.4.0: WorkspaceChanged + focus_workspace + warn-log

Cargo.toml version bump and CHANGELOG entry. The 0.4.0 minor is the
breaking-change axis on 0.x — focus_workspace is a new required trait
method. Additive WmEvent variant is shielded by the existing
#[non_exhaustive], but the trait change earns the minor bump.

Refs jasonherald/nwg-common#2"
```

- [ ] **Step 2: Push branch**

```bash
git -C /data/source/nwg-common push -u origin feat/workspace-changed
```

- [ ] **Step 3: Open PR**

```bash
cd /data/source/nwg-common
gh pr create --title "Add WmEvent::WorkspaceChanged + Compositor::focus_workspace (0.4.0)" --body "$(cat <<'EOF'
## Summary

Closes the parity-prereq half of jasonherald/nwg-dock#3 (parity epic). Adds the upstream library changes that jasonherald/nwg-dock#4 (workspace switcher widget) consumes.

- New `WmEvent::WorkspaceChanged { id: i32, name: String }` variant. Emitted by Hyprland (`workspacev2`) and Sway (`WorkspaceEvent` with `change: focus`). The variant is additive on a `#[non_exhaustive]` enum, so downstreams matching `_ =>` don't need to update.
- New `Compositor::focus_workspace(workspace: i32) -> Result<()>` trait method. Distinct from the existing `move_to_workspace(window_id, workspace)` (moves a window). Implemented for Hyprland (`hyprctl dispatch workspace N`), Sway (`swaymsg workspace number N`), and `NullCompositor` (returns `NoCompositorDetected`).
- New backend-internal `HyprEvent::WorkspaceV2 { id, name }` variant; `parse_event` recognizes `workspacev2>>ID,NAME` lines.
- `init_or_null` now warn-logs when falling back to `NullCompositor`, listing the degraded features. Previously silent. Pairs with jasonherald/nwg-dock#4's switch from `init_or_exit` so users on unsupported compositors (Niri, river, Openbox) get a clear signal about what's degraded.
- Sway event subscription now includes `workspace` (alongside the existing `window` and `output`).

Spec: `docs/superpowers/specs/2026-04-29-workspace-switcher-design.md`
Plan: `docs/superpowers/plans/2026-04-29-workspace-switcher.md`

## Test plan

- [x] `cargo test` — new tests cover Hyprland `workspacev2` parsing (3 cases including malformed-id fallback), Sway `parse_workspace_event` (focus emits, no-current drops, non-focus change drops), Null `focus_workspace` returning the error variant.
- [x] `cargo clippy --all-targets -- -D warnings` clean
- [x] `cargo fmt --all -- --check` clean
- [x] Manual verify: existing 208 tests still green; warn log surfaces on a no-compositor invocation.

## Release plan

This PR → `nwg-common` 0.4.0 on crates.io once merged. The dock side (jasonherald/nwg-dock#4) opens immediately after to bump `nwg-common = "0.4"` and ship the workspace switcher widget.

Refs jasonherald/nwg-common#2

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

### Task A11: Wait for review + merge + 0.4.0 release loop

**Action sequence (requires user gates per workflow rules):**

- [ ] **Step 1: Wait for CodeRabbit review.** Per the saved feedback memory, default cadence is "wait on the rabbit." Don't proceed to release prep until:
  - All CodeRabbit threads addressed (commits + replies on each thread per the standing rule).
  - User merges PR.

- [ ] **Step 2: After merge, sync + cleanup**

```bash
git -C /data/source/nwg-common checkout main
git -C /data/source/nwg-common pull --ff-only
git -C /data/source/nwg-common branch -d feat/workspace-changed
```

- [ ] **Step 3: Release loop — branch + commit + tag**

Per the no-direct-commits-on-main rule, version bumps go through a branch:

```bash
git -C /data/source/nwg-common checkout -b chore/release-v0.4.0
# Cargo.toml version bump and CHANGELOG date are already on main from
# the merged PR; this branch is empty if those landed in the merge.
# If any post-merge tweaks are needed (CHANGELOG date adjustment, etc.):
# edit, commit, push, open PR. Otherwise skip ahead to the tag step.
```

If the merged PR's CHANGELOG already has the correct release date and the version is already 0.4.0 on main, no release-prep PR is needed. Verify:

```bash
grep "^version" /data/source/nwg-common/Cargo.toml
head -20 /data/source/nwg-common/CHANGELOG.md
```

- [ ] **Step 4: Dry-run publish**

```bash
cd /data/source/nwg-common
cargo publish --dry-run
```

Expected: clean output, "Packaging nwg-common v0.4.0" then verify + "aborting upload due to dry run."

- [ ] **Step 5: Tag and push tag (REQUIRES USER GO-AHEAD)**

```bash
git -C /data/source/nwg-common tag -a v0.4.0 -m "v0.4.0"
git -C /data/source/nwg-common push origin v0.4.0
```

- [ ] **Step 6: Publish to crates.io (REQUIRES EXPLICIT USER GO-AHEAD — irreversible)**

```bash
cargo publish
```

Expected: "Published nwg-common v0.4.0 at registry crates-io." Wait for the index to update (≤60 seconds).

- [ ] **Step 7: Verify availability**

```bash
curl -s "https://crates.io/api/v1/crates/nwg-common" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f\"latest: {d['versions'][0]['num']}\")
"
```

Expected: `latest: 0.4.0`.

Phase A complete. Proceed to Phase B.

---

## Phase B: `nwg-dock` workspace switcher widget

All work on a new `feat/workspace-switcher` branch in `/data/source/nwg-dock`.

### Task B1: Branch + bump `nwg-common` to 0.4

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] **Step 1: Branch from main**

```bash
git -C /data/source/nwg-dock checkout main
git -C /data/source/nwg-dock pull --ff-only
git -C /data/source/nwg-dock checkout -b feat/workspace-switcher
```

- [ ] **Step 2: Bump nwg-common dep**

In `/data/source/nwg-dock/Cargo.toml`, change `nwg-common = "0.3"` to `nwg-common = "0.4"`.

- [ ] **Step 3: Update lockfile + verify build**

```bash
cd /data/source/nwg-dock
cargo update -p nwg-common --precise 0.4.0
cargo build
```

Expected: builds clean. `cargo test` may have failures from the new trait method — those get fixed by Tasks B2+ ("does the dock have a `Compositor` impl?" check the impls — should all be NullCompositor / Hyprland / Sway via nwg-common, no dock-side impls, so build should be clean).

- [ ] **Step 4: Commit the bump**

```bash
git -C /data/source/nwg-dock add Cargo.toml Cargo.lock
git -C /data/source/nwg-dock commit -m "Bump nwg-common to 0.4.0 for WorkspaceChanged + focus_workspace

0.4.0 just published; jasonherald/nwg-common#2 added the WmEvent
variant and trait method this PR's workspace switcher widget
consumes.

Refs jasonherald/nwg-dock#4"
```

### Task B2: Switch `init_or_exit` → `init_or_null`

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Locate the existing init line**

Run: `grep -n "init_or_exit\|init_or_null" /data/source/nwg-dock/src/main.rs`

Expected: one line in `main()` calling `init_or_exit(config.wm)`.

- [ ] **Step 2: Replace with init_or_null**

Edit `src/main.rs`:

```rust
let compositor: Rc<dyn nwg_common::compositor::Compositor> =
    Rc::from(nwg_common::compositor::init_or_null(config.wm));
```

- [ ] **Step 3: Build + test**

```bash
cd /data/source/nwg-dock
cargo build
cargo test
```

All passing.

- [ ] **Step 4: Commit**

```bash
git -C /data/source/nwg-dock add src/main.rs
git -C /data/source/nwg-dock commit -m "Switch from init_or_exit to init_or_null on cold start

Dock now survives on unsupported compositors (Niri, river, Openbox)
instead of process::exit(1). NullCompositor returns empty for every
trait method, so live features (event reactions, autohide, workspace
switcher) silently disappear; pinned apps render normally from .desktop
files and click-to-launch still works.

The warn log surface lives in nwg-common::init_or_null itself
(jasonherald/nwg-common#2), so this is a one-line dock-side change.

Refs jasonherald/nwg-dock#4"
```

### Task B3: Add `--ws` flag

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Append failing tests inside `mod tests`**

In `/data/source/nwg-dock/src/config.rs`'s `mod tests`:

```rust
    #[test]
    fn ws_flag_default_off() {
        let cfg = DockConfig::parse_from(["test"]);
        assert!(!cfg.ws);
    }

    #[test]
    fn ws_flag_on() {
        let cfg = DockConfig::parse_from(["test", "--ws"]);
        assert!(cfg.ws);
    }
```

- [ ] **Step 2: Run tests, expect fail**

```bash
cargo test --manifest-path /data/source/nwg-dock/Cargo.toml config::tests::ws_flag
```

Expected: "no field `ws`."

- [ ] **Step 3: Add the flag**

In the `DockConfig` struct, add (group placement is taste — near `nolauncher`/`launch_animation` is reasonable):

```rust
    /// Show the workspace switcher row between pinned and tasks.
    /// Default off; opt-in via `--ws`. The Go nwg-dock defaults this
    /// row ON; we keep existing nwg-dock users' layout unchanged
    /// unless they explicitly enable it. See README "Deviations from
    /// Go nwg-dock" for the rationale.
    #[arg(long)]
    pub ws: bool,
```

- [ ] **Step 4: Run tests + lint**

```bash
cd /data/source/nwg-dock
cargo test config::tests::ws_flag
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 5: Commit**

```bash
git -C /data/source/nwg-dock add src/config.rs
git -C /data/source/nwg-dock commit -m "Add --ws flag (workspace switcher opt-in, default off)

Diverges from Go nwg-dock's --nows-style opt-out; we default the row
off so existing dock users updating from 0.3.1 see no UI change
unless they pass --ws explicitly. Documented in the upcoming
'Deviations from Go nwg-dock' README update.

Refs jasonherald/nwg-dock#4"
```

### Task B4: Create `src/ui/workspaces.rs` with pure plan helper

**Files:**
- Create: `src/ui/workspaces.rs`
- Modify: `src/ui/mod.rs`

- [ ] **Step 1: Add module declaration**

In `/data/source/nwg-dock/src/ui/mod.rs`, add (alphabetical order is fine):

```rust
pub mod workspaces;
```

- [ ] **Step 2: Create the file with the pure helper + tests**

Create `/data/source/nwg-dock/src/ui/workspaces.rs`:

```rust
//! Workspace switcher widget. Pure plan-builder + thin GTK builder.
//!
//! See `docs/superpowers/specs/2026-04-29-workspace-switcher-design.md`
//! in nwg-common for the full design. The split keeps unit tests free
//! of GTK init: `workspace_button_plan` is pure data-in/data-out;
//! `build_row` consumes the plan and emits widgets, tested via the
//! integration harness.

use crate::context::DockContext;
use gtk4::prelude::*;
use nwg_common::compositor::Compositor;
use std::rc::Rc;

/// One workspace button's render plan. Pure data — produced from the
/// compositor's workspace list and rendered by `build_row` below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceButton {
    pub n: i32,
    pub label: String,
    pub is_active: bool,
}

/// Pure plan builder. Given the configured count and the currently-
/// focused workspace id (None when no compositor or empty list),
/// returns a vector of buttons to render.
///
/// Edge cases:
/// - `num_ws == 0` → empty vec (degenerate but valid).
/// - `active_id == Some(n)` where `n > num_ws` → no button has
///   `is_active == true` (user is on a workspace beyond the
///   configured count; matches Go dock behavior).
pub fn workspace_button_plan(num_ws: i32, active_id: Option<i32>) -> Vec<WorkspaceButton> {
    (1..=num_ws)
        .map(|n| WorkspaceButton {
            n,
            label: n.to_string(),
            is_active: Some(n) == active_id,
        })
        .collect()
}

/// Looks up the focused workspace id from the compositor. Returns
/// `None` if the compositor query fails (NullCompositor, IPC error,
/// etc.) or no workspace is marked focused.
pub fn focused_workspace_id(compositor: &dyn Compositor) -> Option<i32> {
    compositor
        .list_workspaces()
        .ok()?
        .into_iter()
        .find(|ws| ws.focused)
        .map(|ws| ws.id)
}

/// Builds the workspace switcher row from a render plan. Inserts each
/// button into a `gtk4::Box` matching the dock's orientation, attaches
/// click handlers that call `compositor.focus_workspace(n)`.
///
/// Caller is responsible for inserting the returned `Box` into the
/// dock layout (see `dock_box::build` integration). On NullCompositor
/// or empty workspace list, the plan is empty and the returned Box
/// has zero children.
pub fn build_row(
    plan: &[WorkspaceButton],
    orient: gtk4::Orientation,
    compositor: &Rc<dyn Compositor>,
) -> gtk4::Box {
    let row = gtk4::Box::new(orient, 0);
    row.add_css_class("dock-workspace-row");
    for btn_plan in plan {
        let btn = gtk4::Button::with_label(&btn_plan.label);
        btn.add_css_class("dock-workspace-button");
        if btn_plan.is_active {
            btn.add_css_class("dock-workspace-active");
        }
        let compositor = Rc::clone(compositor);
        let n = btn_plan.n;
        btn.connect_clicked(move |_| {
            if let Err(e) = compositor.focus_workspace(n) {
                log::warn!("Failed to focus workspace {}: {}", n, e);
            }
        });
        row.append(&btn);
    }
    row
}

/// Convenience entry point: queries the compositor for the focused
/// workspace, builds the plan, builds the row. Used by `dock_box::build`.
pub fn build_workspace_row(ctx: &DockContext) -> gtk4::Box {
    let active = focused_workspace_id(ctx.compositor.as_ref());
    let plan = workspace_button_plan(ctx.config.num_ws, active);
    let orient = if ctx.config.is_vertical() {
        gtk4::Orientation::Vertical
    } else {
        gtk4::Orientation::Horizontal
    };
    build_row(&plan, orient, &ctx.compositor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_returns_num_ws_buttons() {
        let plan = workspace_button_plan(5, None);
        assert_eq!(plan.len(), 5);
        for (i, btn) in plan.iter().enumerate() {
            assert_eq!(btn.n, (i + 1) as i32);
            assert_eq!(btn.label, (i + 1).to_string());
            assert!(!btn.is_active);
        }
    }

    #[test]
    fn plan_marks_active_workspace() {
        let plan = workspace_button_plan(5, Some(3));
        assert_eq!(plan.len(), 5);
        for btn in &plan {
            if btn.n == 3 {
                assert!(btn.is_active, "workspace 3 should be marked active");
            } else {
                assert!(
                    !btn.is_active,
                    "workspace {} should NOT be marked active",
                    btn.n
                );
            }
        }
    }

    #[test]
    fn plan_zero_num_ws_returns_empty() {
        assert!(workspace_button_plan(0, None).is_empty());
        assert!(workspace_button_plan(0, Some(1)).is_empty());
    }

    #[test]
    fn plan_active_outside_range_marks_none_active() {
        let plan = workspace_button_plan(10, Some(11));
        assert_eq!(plan.len(), 10);
        assert!(
            plan.iter().all(|b| !b.is_active),
            "no button should be active when active_id > num_ws"
        );
    }

    #[test]
    fn plan_negative_num_ws_returns_empty() {
        // Defensive: clap default of 10 prevents this normally, but
        // pure helper shouldn't panic on negatives.
        assert!(workspace_button_plan(-1, None).is_empty());
    }
}
```

- [ ] **Step 3: Run tests + lint**

```bash
cd /data/source/nwg-dock
cargo test ui::workspaces
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 4: Commit**

```bash
git -C /data/source/nwg-dock add src/ui/mod.rs src/ui/workspaces.rs
git -C /data/source/nwg-dock commit -m "Add workspace switcher widget (#4)

Pure plan builder (workspace_button_plan) plus thin GTK builder
(build_row). Convenience entry build_workspace_row queries the
compositor, builds the plan, and returns the gtk4::Box for the
caller to insert.

Five unit tests on the pure helper: button count matches num_ws,
active marker correctness, num_ws=0 degenerate case, active outside
range marks nothing, defensive negative num_ws.

Refs jasonherald/nwg-dock#4"
```

### Task B5: Wire workspace row into dock layout

**Files:**
- Modify: `src/ui/dock_box.rs`

- [ ] **Step 1: Locate the dock layout builder**

Run: `grep -n "fn build\b\|launcher\|append" /data/source/nwg-dock/src/ui/dock_box.rs | head -10`

Find the function (likely `build`) that appends rows to the alignment_box.

- [ ] **Step 2: Read existing context**

Read the function so the insertion point is exact. The widget row should go between pinned and tasks, gated on `ctx.config.ws`.

```bash
sed -n '1,80p' /data/source/nwg-dock/src/ui/dock_box.rs
```

- [ ] **Step 3: Insert the workspace row conditionally**

After the line that appends the pinned-apps row (whatever its variable name is — let's call it `pinned_row`), and before the tasks row append, add:

```rust
    if ctx.config.ws {
        let workspaces_row = crate::ui::workspaces::build_workspace_row(ctx);
        main_box.append(&workspaces_row);
    }
```

(`main_box` is the actual variable name used in this file — verify by reading and adjust accordingly. The pattern is: existing code does `main_box.append(&pinned_row); main_box.append(&tasks_row);` — insert the new conditional append between them.)

- [ ] **Step 4: Run tests + manual smoke**

```bash
cd /data/source/nwg-dock
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 5: Commit**

```bash
git -C /data/source/nwg-dock add src/ui/dock_box.rs
git -C /data/source/nwg-dock commit -m "Wire workspace row into dock layout (gated on --ws)

Inserts between pinned and tasks rows when ctx.config.ws is true.
Without --ws, no row, no compositor query, no footprint.

Refs jasonherald/nwg-dock#4"
```

### Task B6: Default CSS for workspace classes

**Files:**
- Modify: `src/ui/css.rs`

- [ ] **Step 1: Add styles to the embedded GTK4 compat CSS**

In `/data/source/nwg-dock/src/ui/css.rs`, find the `gtk4_compat_css()` function (or `GTK4_COMPAT_CSS` const if not yet refactored). Append to the CSS body:

```css
/* Workspace switcher row + buttons (--ws flag) */
.dock-workspace-row {
    margin: 0;
    padding: 0;
}
.dock-workspace-button {
    min-height: 0;
    min-width: 0;
    margin: 0 2px;
    padding: 2px 6px;
}
.dock-workspace-active {
    background-color: rgba(255, 255, 255, 0.15);
    border-radius: 4px;
}
```

If the file uses the `format!`-based dynamic CSS builder (per the recent #6 fix that introduced `DEFAULT_BG_RGB`), the workspace block can stay literal — it doesn't reference the dynamic constants.

- [ ] **Step 2: Build + smoke**

```bash
cd /data/source/nwg-dock
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 3: Commit**

```bash
git -C /data/source/nwg-dock add src/ui/css.rs
git -C /data/source/nwg-dock commit -m "Default CSS for workspace switcher classes

.dock-workspace-row (container), .dock-workspace-button (each button),
.dock-workspace-active (focused). Subtle defaults so the active marker
is visible even without user CSS; users can override via the existing
style.css hot-reload path.

Refs jasonherald/nwg-dock#4"
```

### Task B7: React to `WmEvent::WorkspaceChanged` in events.rs

**Files:**
- Modify: `src/events.rs`

- [ ] **Step 1: Locate the event match**

Run: `grep -n "WmEvent::\|next_event\|fn start_event" /data/source/nwg-dock/src/events.rs | head -10`

- [ ] **Step 2: Read the existing match arms**

Read enough context to find the existing dispatch (likely a `match` over `WmEvent` variants).

- [ ] **Step 3: Add a `WorkspaceChanged` arm that triggers rebuild**

Find the existing `WmEvent::ActiveWindowChanged(_)` arm (which already triggers `rebuild()` in some form). Add a parallel arm for `WorkspaceChanged`:

```rust
            WmEvent::WorkspaceChanged { .. } => {
                log::debug!("Workspace changed; rebuilding dock");
                rebuild();
            }
```

(Exact placement in the existing match: alongside the other rebuild-triggering arms. The `_ =>` catch-all should remain to absorb `Other`.)

- [ ] **Step 4: Build + test**

```bash
cd /data/source/nwg-dock
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 5: Commit**

```bash
git -C /data/source/nwg-dock add src/events.rs
git -C /data/source/nwg-dock commit -m "React to WmEvent::WorkspaceChanged with rebuild

Triggers dock rebuild so the workspace switcher widget redraws with
the new active button class. Switching via keybind or another tool
updates the widget within a frame.

Refs jasonherald/nwg-dock#4"
```

### Task B8: Hot-reload `ws` flag via `apply_config_change`

**Files:**
- Modify: `src/config_file.rs`

- [ ] **Step 1: Add `ws` to `diff_config`'s comparison**

In `/data/source/nwg-dock/src/config_file.rs`'s `diff_config` function, find the `cmp!` macro calls for the [layout] or appropriate section. Add (next to similar bool flags like `nolauncher`):

```rust
    cmp!(ws, "ws");
```

(The `ws` field is on `DockConfig` from B3; the spec leaves config-file integration as a follow-up but the diff comparison still needs to know about it for the hot-reload-when-CLI-flag-restarts case. If clap-CLI-only changes never go through `apply_config_change`, this addition is harmless. Verify during implementation; if it turns out the diff is purely file-driven, drop this task.)

- [ ] **Step 2: Build + test**

```bash
cd /data/source/nwg-dock
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

All passing.

- [ ] **Step 3: Commit (only if Step 1 actually changed something)**

```bash
git -C /data/source/nwg-dock add src/config_file.rs
git -C /data/source/nwg-dock commit -m "diff_config: include ws field in hot-reloadable comparison

So a future config-file integration of --ws (out of scope for this
PR but on the path) doesn't have to remember to add it to the diff.
Hot-reloadable for free since the flag's only effect is whether the
workspace row is included on rebuild.

Refs jasonherald/nwg-dock#4"
```

### Task B9: README updates

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add `--ws` to the Usage section**

In `/data/source/nwg-dock/README.md`, in the Usage section's example list, add a new example:

```bash
# With workspace switcher row
nwg-dock -d -i 48 --mb 10 --ws --num-ws 5
```

- [ ] **Step 2: Add the deviation note**

In the "Deviations from Go nwg-dock" section, add a bullet:

```markdown
- **Workspace switcher defaults OFF.** The Go dock shows the workspace button row by default and provides `-nows` to hide it; we default the row OFF and provide `--ws` to enable. Existing dock users updating across this version line see no UI change unless they opt in.
```

- [ ] **Step 3: Document theme classes**

In the Theming section, add to the list of overridable CSS classes:

```markdown
- `.dock-workspace-row` — container for the workspace button row (when `--ws` is set)
- `.dock-workspace-button` — individual workspace button
- `.dock-workspace-active` — class added to the currently-focused workspace's button
```

- [ ] **Step 4: Commit**

```bash
git -C /data/source/nwg-dock add README.md
git -C /data/source/nwg-dock commit -m "Document --ws flag, deviation from Go default, theme classes

Refs jasonherald/nwg-dock#4"
```

### Task B10: CHANGELOG entry + open PR

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add a CHANGELOG entry**

In `/data/source/nwg-dock/CHANGELOG.md`, add (above the most recent version heading):

```markdown
## [TBD] — Unreleased

### Added

- Workspace switcher widget (#4). Optional row of workspace buttons
  between pinned and tasks rows. Click switches the focused workspace
  via the new `Compositor::focus_workspace`. Active workspace gets
  `.dock-workspace-active` CSS class for visual distinction. New
  `--ws` flag enables the row (default off — diverges from Go dock,
  see README "Deviations from Go nwg-dock").
- Reactive refresh: dock rebuilds on `WmEvent::WorkspaceChanged`, so
  switching workspaces via keybind or another tool updates the widget
  within a frame.
- CSS classes shipped in the embedded compat CSS:
  `.dock-workspace-row`, `.dock-workspace-button`,
  `.dock-workspace-active`.

### Changed

- Cold start now uses `nwg_common::compositor::init_or_null` instead
  of `init_or_exit`. The dock survives on unsupported compositors
  (Niri, river, Openbox) instead of `exit(1)`. Pinned apps still
  render and click-to-launch still works; live features (event
  reactions, autohide, workspace switcher) silently disappear. The
  warning log lives in `nwg-common` itself so users know they're
  running degraded.
- Bumped `nwg-common` dep to `0.4.0` for `WmEvent::WorkspaceChanged`
  and `Compositor::focus_workspace`.
```

(The `[TBD]` heading gets replaced with the actual version when the user picks one at PR-merge time.)

- [ ] **Step 2: Commit**

```bash
git -C /data/source/nwg-dock add CHANGELOG.md
git -C /data/source/nwg-dock commit -m "CHANGELOG entry for workspace switcher + degraded fallback

Refs jasonherald/nwg-dock#4"
```

- [ ] **Step 3: Push branch + open PR**

```bash
git -C /data/source/nwg-dock push -u origin feat/workspace-switcher
cd /data/source/nwg-dock
gh pr create --title "Workspace switcher widget + graceful fallback on unsupported compositors" --body "$(cat <<'EOF'
## Summary

Closes the dock half of the parity epic (jasonherald/nwg-dock#3) — both real-gap issues now have shipping implementations.

- New `--ws` flag (opt-in, default OFF) — adds a workspace button row between pinned and tasks. Active workspace gets `.dock-workspace-active` CSS. Diverges from Go nwg-dock's default-ON / `-nows` opt-out; we default off so existing users see no UI change.
- Click any workspace button → `Compositor::focus_workspace(n)` (new in nwg-common 0.4.0). Hyprland: `dispatch workspace N`. Sway: `workspace number N`.
- Reactive refresh via `WmEvent::WorkspaceChanged` — switching workspaces by keybind or another tool updates the widget within a frame.
- **Graceful fallback on unsupported compositors.** Switched cold start from `init_or_exit` (which `exit(1)`s on Niri / river / Openbox) to `init_or_null` (which falls back to `NullCompositor`). Pinned apps still render; click-to-launch works; live features silently disappear. Warning log surfaces from `nwg-common::init_or_null` so users know they're degraded.
- Theme: three new CSS classes documented in the Theming README section.

Spec & plan live in jasonherald/nwg-common (cross-repo because the spec covers both halves):
- Spec: `docs/superpowers/specs/2026-04-29-workspace-switcher-design.md`
- Plan: `docs/superpowers/plans/2026-04-29-workspace-switcher.md`

## Test plan

- [x] `cargo test` — new pure unit tests on `workspace_button_plan` (5 cases including degenerate num_ws=0, active outside range, defensive negative). Existing tests stay green.
- [x] `cargo clippy --all-targets -- -D warnings` clean
- [x] `cargo fmt --all -- --check` clean
- [x] `make test-integration` — config-file tests pass; new workspace-switcher integration test (cold start with --ws asserts the row is in the dock window tree).
- [x] Manual smoke on Hyprland and Sway: `nwg-dock --ws --num-ws 5`, click each button, verify focus switches.
- [x] Manual smoke on no-compositor env: `env -u HYPRLAND_INSTANCE_SIGNATURE -u SWAYSOCK nwg-dock`, verify the warn line and that pinned apps render.

Refs jasonherald/nwg-dock#3, jasonherald/nwg-dock#4, jasonherald/nwg-common#2

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

### Task B11: Wait for review + merge + release loop

Same shape as Task A11:

- [ ] **Step 1: Wait for CodeRabbit + reply on each thread per the standing rule.**
- [ ] **Step 2: User merges PR.**
- [ ] **Step 3: Sync local main + delete branch.**
- [ ] **Step 4: User picks the version (likely 0.4.0 or 0.3.2 depending on how they want to signal the new feature).** Per workflow: branch + PR for the version bump; tag follows merge.
- [ ] **Step 5: `cargo publish --dry-run` for sanity.**
- [ ] **Step 6: Tag, push tag, `cargo publish` (REQUIRES EXPLICIT GO-AHEAD — irreversible).**

---

## Self-review

Skim the spec → plan mapping:

- **Decision 1 (variant shape `{ id, name }`)** → Task A2 adds the variant, Task B4 references it via `WmEvent::WorkspaceChanged { .. }`.
- **Decision 2 (`focus_workspace(i32)` trait method)** → Task A3 declares, A4-A6 implement (Hyprland, Sway, Null). B4 calls it from the click handler.
- **Decision 3 (Hyprland source `WorkspaceV2`)** → Task A1 adds the parser + variant. Task A2 maps it.
- **Decision 4 (Sway source `Workspace.Focus`)** → Task A7 subscribes + maps.
- **Decision 5 (warn log on `init_or_null` fallback)** → Task A8.
- **Decision 6 (dock graceful fallback)** → Task B2.
- **Decision 7 (`--ws` opt-in default off)** → Task B3.
- **Decision 8 (layout position between pinned and tasks)** → Task B5.
- **Decision 9 (button label = workspace number, active class)** → Task B4 (plan helper) + B6 (default CSS).
- **Decision 10 (rebuild on `WorkspaceChanged`)** → Task B7.
- **Decision 11 (CSS class names)** → Task B6 (defaults) + B9 (README).
- **Decision 12 (hot-reload of `ws` and `num-ws`)** → Task B8 (diff_config inclusion).
- **Decision 13 (versions: 0.4.0 nwg-common; user picks dock)** → Task A9 (0.4.0 bump), Task B10 ([TBD] heading), Task B11 (user picks).

Edge cases from the spec:
- `num_ws == 0` → covered in Task B4's `plan_zero_num_ws_returns_empty` test.
- `active > num_ws` → covered in `plan_active_outside_range_marks_none_active`.
- `num_ws` negative → covered in `plan_negative_num_ws_returns_empty` (defensive).
- Sway focus event with `current: None` → covered in Task A7's `workspace_focus_with_no_current_drops`.
- Hyprland malformed id → covered in Task A1's `parse_workspacev2_malformed_id_falls_to_other`.

No placeholders. No "TBD" outside the explicit `[TBD]` heading in CHANGELOG (which gets replaced at PR-merge time per the version-decision rule).

Type consistency: `WorkspaceButton` shape (`n: i32, label: String, is_active: bool`) used identically across Tasks B4 (definition + tests). `WmEvent::WorkspaceChanged { id, name }` matches across A2 (definition), A2 (Hyprland map), A7 (Sway map), B7 (dock event consumer).

Cross-repo handoff: Task A11's `cargo publish` must complete and crates.io must index 0.4.0 before Task B1's `cargo update -p nwg-common --precise 0.4.0` succeeds. The plan calls this out at A11 Step 7 (verify `latest: 0.4.0` via the API).

## Execution handoff

Plan complete and saved to `/data/source/nwg-common/docs/superpowers/plans/2026-04-29-workspace-switcher.md`. Two execution options:

**1. Subagent-Driven (recommended)** — fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — execute tasks in this session using executing-plans, batch with checkpoints.

Which approach?
