# Workspace switcher: `WmEvent::WorkspaceChanged` + dock widget — design

**Status:** Approved 2026-04-29. Implementation pending across two repos.
**Issues:**
- [`jasonherald/nwg-common#2`](https://github.com/jasonherald/nwg-common/issues/2) — `WmEvent::WorkspaceChanged` variant + emits + new trait method.
- [`jasonherald/nwg-dock#4`](https://github.com/jasonherald/nwg-dock/issues/4) — workspace switcher widget consuming the new event.
- [`jasonherald/nwg-dock#3`](https://github.com/jasonherald/nwg-dock/issues/3) — parity epic; this work closes both gaps it tracks.

## Summary

Two coordinated changes that together close the parity gap with the Go `nwg-piotr/nwg-dock` workspace switcher row. **Phase A** lands in `nwg-common` 0.4.0 and adds `WmEvent::WorkspaceChanged { id: i32, name: String }`, a new `Compositor::focus_workspace(i32)` trait method (implemented for Hyprland / Sway / Null), and a one-line warning log when `init_or_null` falls back to `NullCompositor` so users on unsupported compositors know they're running degraded. **Phase B** lands in the next `nwg-dock` minor: it bumps the `nwg-common` dep to `0.4`, switches `init_or_exit` to `init_or_null` (so the dock survives on Niri / river / Openbox instead of `exit(1)`), and adds an opt-in workspace-button row between the pinned-apps and tasks rows, controlled by a new `--ws` flag.

## Goals

- Close the two open gaps in the [parity epic](https://github.com/jasonherald/nwg-dock/issues/3).
- Give Sway and Hyprland users feature parity with the Go dock's workspace row, finally.
- Make the dock survive gracefully on unsupported compositors (no more hard-exit on Niri / river).
- Land both pieces under the project's existing PR-based workflow with CodeRabbit review.

## Non-goals

- **Niri / river / Openbox backend implementations.** Out of scope. The dock will *survive* on those compositors via `NullCompositor`, but no live workspace data, no autohide, no event reactions. A native Niri backend is tracked as `upstream-migrated` #32 in the dock and can pick up the trait method we add here.
- **Old Hyprland (pre-`workspacev2`).** We use `HyprEvent::WorkspaceV2` exclusively. Hyprland v0.30+ has had it for a year+; older versions get `WmEvent::Other(...)` for workspace events, which the dock simply doesn't react to. No fallback to the v1 `workspace` event.
- **Workspace renaming.** The widget reads `name` from the event for display, but the user changing a workspace's name mid-session isn't a special case — next `WorkspaceChanged` event carries the new name and the rebuild picks it up.
- **Drag-to-reorder workspace buttons.** Out of scope; the row is a plain button strip.
- **Per-workspace icon customization.** Numeric label only in v1; users can override via CSS if they want.

## Decisions

| # | Decision | Choice |
|---|---|---|
| 1 | `WorkspaceChanged` payload | Struct variant `{ id: i32, name: String }`. Both fields available free from both backend events; the small payload is worth the future-proof flexibility (consumers don't have to round-trip through `list_workspaces()` for the common case). |
| 2 | Trait method name | `focus_workspace(workspace: i32)` — distinct from the existing `move_to_workspace(window_id, workspace)` (different operation). Verb matches the Sway "focus" change type and Hyprland's "active workspace" semantics. |
| 3 | Hyprland source event | `HyprEvent::WorkspaceV2 { id, name }`. Drops support for old Hyprland missing v2 (unaffected pre-v0.30; v0.30 shipped 2024-04). |
| 4 | Sway source event | `Event::Workspace` with `change: Focus` and `current: Some(ws)` → `id: ws.num, name: ws.name`. `current: None` is dropped with `log::debug!` (defensive — protocol shouldn't allow it). |
| 5 | NullCompositor fallback log | One-line `log::warn!` in `init_or_null` listing what's degraded ("autohide, workspace switcher, live event reactions"). Not new behavior — existing function stays silent today; this just surfaces it. |
| 6 | Dock degraded-mode strategy | `main.rs` switches from `init_or_exit(config.wm)` to `init_or_null(config.wm)`. Pinned apps still render (they read `.desktop` files, not compositor IPC). Click-to-launch works. Autohide / running-tasks / workspace switcher silently absent. |
| 7 | Workspace widget default | **Opt-in via `--ws`. Default OFF.** Diverges from Go nwg-dock (which defaults ON, opt-out via `-nows`). Existing nwg-dock users updating from 0.3.1 see no UI change unless they opt in. Documented in the "Deviations from Go nwg-dock" README section. |
| 8 | Widget layout position | `[launcher] [pinned] [workspaces] [tasks]` — between pinned and tasks rows. Stacks vertically when `position` is `left`/`right`. |
| 9 | Button label | Workspace number (1..=`num_ws`). Active button gets a `.dock-workspace-active` CSS class for visual distinction. |
| 10 | Reaction strategy | On `WmEvent::WorkspaceChanged`, trigger a `rebuild()` that redraws the workspace row with the new active class. Lightest-touch implementation; we already have `rebuild()`, no new event-routing infra. |
| 11 | CSS class names | `.dock-workspace-button` (each button), `.dock-workspace-active` (the focused one). Match existing `.dock-button` / `.dock-launching` naming convention. Default styles ship in the embedded GTK4 compat CSS. |
| 12 | Hot-reload of `--ws` and `num-ws` | Both hot-reloadable via the existing `apply_config_change` rebuild path. Adding `ws` to the existing dock config-file schema is a follow-up — flag-only support v1. |
| 13 | Version cuts | nwg-common: minor → **0.4.0** (per the issue's release coordination — additive variant + new trait method warrants the minor bump even though `#[non_exhaustive]` already shields downstreams). nwg-dock: user picks at PR merge time, but the natural shape is a minor (e.g., 0.4.0) since the workspace widget is a notable new feature. |

## Architecture

### Cross-repo layout

```text
nwg-common (Phase A) ─────────────────────► 0.4.0 published to crates.io
   │
   ├── src/compositor/types.rs         (+ WorkspaceChanged variant)
   ├── src/compositor/traits.rs        (+ focus_workspace method)
   ├── src/compositor/hyprland.rs      (+ emit WorkspaceV2 → WorkspaceChanged
   │                                    + impl focus_workspace)
   ├── src/compositor/sway/events.rs   (+ emit Workspace.Focus → WorkspaceChanged)
   ├── src/compositor/sway/mod.rs      (+ impl focus_workspace)
   ├── src/compositor/null.rs          (+ no-op focus_workspace returning Err
   │                                    + warn log in init_or_null)
   └── src/compositor/mod.rs           (init_or_null gains the warn log)

nwg-dock (Phase B) ────────────────────────► consumes nwg-common 0.4.0
   │
   ├── Cargo.toml                      (nwg-common = "0.4")
   ├── src/config.rs                   (+ pub ws: bool flag, --ws)
   ├── src/main.rs                     (init_or_exit → init_or_null)
   ├── src/ui/mod.rs                   (+ pub mod workspaces)
   ├── src/ui/workspaces.rs            (NEW — widget impl)
   ├── src/ui/dock_box.rs              (slot the widget between pinned & tasks)
   ├── src/ui/css.rs                   (+ default .dock-workspace-* CSS)
   ├── src/events.rs                   (+ react to WmEvent::WorkspaceChanged)
   └── README.md                       (+ flag docs, parity-deviation note)
```

### Key invariant

Backwards-compat is preserved at every step:
- `nwg-common` 0.4.0 adds a variant to a `#[non_exhaustive]` enum — downstreams matching `_ =>` keep working without code change.
- `nwg-common` 0.4.0 adds a trait method, which IS a breaking change for any *external* `Compositor` impl. None exist outside the workspace today, but the bump to 0.4 is what acknowledges this.
- `nwg-dock` next-minor adds an opt-in flag and changes the unsupported-compositor exit behavior to graceful fallback. Both are user-visible improvements, not regressions.
- Existing dock CSS keeps working; the new classes only fire when the new flag is on.

## Components

### Phase A — `nwg-common`

**`WmEvent::WorkspaceChanged`** (in `src/compositor/types.rs`):

```rust
pub enum WmEvent {
    ActiveWindowChanged(String),
    MonitorChanged,
    /// Focused workspace changed. Carries the new workspace's id and name.
    /// Both backends (Hyprland workspacev2, Sway WorkspaceEvent.focus) provide
    /// these fields free, so consumers don't need to round-trip through
    /// `list_workspaces()` for the common case.
    WorkspaceChanged { id: i32, name: String },
    Other(String),
}
```

**`Compositor::focus_workspace`** (in `src/compositor/traits.rs`):

```rust
/// Focus the given workspace. Hyprland: `dispatch workspace N`.
/// Sway: `workspace number N`. NullCompositor: returns an error
/// (NoCompositorDetected) so callers can degrade gracefully.
fn focus_workspace(&self, workspace: i32) -> Result<()>;
```

**Backend implementations:**
- Hyprland: `hyprctl dispatch workspace N` via the existing dispatch path.
- Sway: `swaymsg workspace number N` via the existing IPC client.
- Null: `Err(DockError::NoCompositorDetected)` — same shape as other Null trait methods.

**Emit additions:**
- Hyprland `events::map`: extend the existing `match` on `HyprEvent` to include `WorkspaceV2 { id, name } => WmEvent::WorkspaceChanged { id, name }`.
- Sway `events::map`: extend the existing `match` on `Event::Workspace` to include the focus-change branch (`change: Focus`, `current: Some(ws)`).

**`init_or_null` log surface** (in `src/compositor/mod.rs`):

```rust
pub fn init_or_null(wm_override: Option<WmOverride>) -> Box<dyn Compositor> {
    match detect(wm_override) {
        Ok(kind) => match create(kind) {
            Ok(c) => c,
            Err(e) => {
                log::warn!(
                    "Compositor backend failed: {} — falling back to NullCompositor. \
                     Live features (event reactions, autohide, workspace switcher) \
                     will be inactive.", e
                );
                Box::new(NullCompositor)
            }
        },
        Err(_) => {
            log::warn!(
                "No supported compositor detected (no HYPRLAND_INSTANCE_SIGNATURE / \
                 SWAYSOCK in env). Falling back to NullCompositor — live features \
                 (event reactions, autohide, workspace switcher) will be inactive. \
                 Pinned apps and click-to-launch still work."
            );
            Box::new(NullCompositor)
        }
    }
}
```

### Phase B — `nwg-dock`

**`--ws` flag** (in `src/config.rs`):

```rust
/// Show the workspace switcher row between pinned and tasks (off by default).
/// Diverges from Go nwg-dock which defaults the row ON; we keep existing
/// users' dock layout unchanged unless they opt in.
#[arg(long)]
pub ws: bool,
```

**Workspace widget (new `src/ui/workspaces.rs`):**

Split into a pure plan-builder (testable without GTK) and a thin impure widget-builder (integration-tested):

```rust
/// One workspace button's render plan. Pure data — produced from the
/// compositor's list and rendered by `build_row` below.
pub struct WorkspaceButton {
    pub n: i32,
    pub label: String,
    pub is_active: bool,
}

/// Pure plan builder. Given the configured count and the currently-focused
/// workspace id (None when no compositor or empty list), returns a vector
/// of buttons to render. Unit-testable — no GTK init required.
pub fn workspace_button_plan(num_ws: i32, active_id: Option<i32>) -> Vec<WorkspaceButton> {
    (1..=num_ws)
        .map(|n| WorkspaceButton {
            n,
            label: n.to_string(),
            is_active: Some(n) == active_id,
        })
        .collect()
}

/// Impure builder — turns a plan into a `gtk4::Box`. Wires the click
/// handler to `compositor.focus_workspace(n)`. Tested via integration
/// (headless Sway harness).
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
```

The dock's rebuild path queries `list_workspaces()` once (same query the right-click "Move to workspace" submenu already uses), finds the focused workspace's id, calls `workspace_button_plan(num_ws, active_id)` to get the plan, and passes it to `build_row` along with the orientation and compositor handle.

**Layout slot** (in `src/ui/dock_box.rs`): the existing builder concatenates `[launcher][pinned][tasks]` into the alignment box. Add the workspace row between pinned and tasks when `ctx.config.ws` is true:

```rust
if ctx.config.ws {
    main_box.append(&workspaces::build_workspace_row(ctx));
}
```

**Event reactivity** (in `src/events.rs`): the existing event-listener loop already calls `rebuild()` on most events. Extend its match arms to include `WmEvent::WorkspaceChanged { .. }` → `rebuild()`. One-line addition.

**Default CSS** (in `src/ui/css.rs`):

```css
.dock-workspace-button {
    min-height: 0;
    min-width: 0;
    margin: 0 2px;
}
.dock-workspace-active {
    background-color: rgba(255, 255, 255, 0.15);
    border-radius: 4px;
}
```

(Subtle defaults so the active marker is visible even with no user CSS; users can override via the existing `style.css` hot-reload path.)

**`init_or_null` switch** (in `src/main.rs`):

```rust
let compositor: Rc<dyn nwg_common::compositor::Compositor> =
    Rc::from(nwg_common::compositor::init_or_null(config.wm));
```

The warning log fires from `nwg-common` itself; the dock just consumes whichever compositor came back.

## Data flow

### Workspace-switch happy path

```text
User keybind / external switch / widget click
                  ↓
           compositor IPC
                  ↓
       backend event mapper
   (Hyprland: WorkspaceV2 → WorkspaceChanged)
   (Sway:     Workspace.Focus → WorkspaceChanged)
                  ↓
          WmEventStream
                  ↓
       events::start_event_listener
                  ↓
              rebuild()
                  ↓
     workspaces::build_workspace_row
   (queries list_workspaces, marks active)
                  ↓
           visible refresh
```

### Widget click → workspace switch

```text
User clicks workspace button N
              ↓
    btn.connect_clicked closure
              ↓
   compositor.focus_workspace(N)
              ↓
       backend dispatches:
   Hyprland: hyprctl dispatch workspace N
   Sway:     swaymsg workspace number N
              ↓
   compositor fires its own workspace event
   (loop closes via the happy path above)
```

### Unsupported-compositor cold path

```text
$ nwg-dock        (on Niri, river, etc.)
       ↓
init_or_null(config.wm)
       ↓
   detect() → Err(NoCompositorDetected)
       ↓
   log::warn!("...degraded features...")
       ↓
   Box::new(NullCompositor)
       ↓
activate_dock continues normally
       ↓
visible result:
- pinned apps render (from .desktop files)
- click-to-launch works
- no running-task indicators
- no autohide (cursor poller never fires — stream is empty)
- no workspace widget (--ws is off by default; if user passes --ws,
  list_workspaces returns empty → row renders zero buttons)
```

## Error handling

| Failure | Phase A | Phase B |
|---|---|---|
| Hyprland event has malformed payload | Fall through to `WmEvent::Other(...)` (mirrors existing pattern) | n/a — receives `Other` and ignores |
| Sway focus event with `current: None` | `log::debug!`, drop the event (defensive — protocol shouldn't allow this) | n/a |
| `focus_workspace` IPC fails (compositor busy, malformed reply) | Returns `Err`, propagated to caller | Widget `connect_clicked` logs `log::warn!` and silently fails — no notification toast (the user clicked, IPC error is rare and recoverable on next click) |
| `list_workspaces` returns `Err` during widget build | n/a | Treat as empty list — no buttons, row renders empty. Logged at `debug` level since Null backend returns this normally. |
| `init_or_null` falls back to NullCompositor | One-line `log::warn!` listing degraded features | Dock continues. User sees pinned apps only. |

The whole pipeline degrades gracefully: bad data drops to logs, no panics, no notifications spamming the user.

## Testing

### Phase A — `nwg-common`

**Unit tests:**
- `compositor::hyprland::tests::workspace_v2_event_maps_to_workspace_changed` — feed a `HyprEvent::WorkspaceV2 { id: 3, name: "chat".into() }`, assert the mapper produces `WmEvent::WorkspaceChanged { id: 3, name: "chat".into() }`.
- `compositor::sway::events::tests::workspace_focus_event_maps_to_workspace_changed` — synthesize a `swayipc::Event::Workspace` with `change: Focus` and a `current` workspace, assert the same shape.
- `compositor::sway::events::tests::workspace_focus_with_no_current_drops_silently` — `current: None` produces no event (or `Other`, whichever is cleanest in the existing handler shape).
- `compositor::null::tests::focus_workspace_returns_err_no_compositor` — assert the Null impl returns the right error variant.
- `compositor::tests::init_or_null_logs_on_unknown_compositor` — `env_logger` test setup, assert the warn line is emitted when no compositor env vars are present.

**Integration:** existing test suite must stay green; the variant addition shouldn't break any `_ =>` arms in test assertions.

**Release:** `cargo publish --dry-run` clean, branch + PR, user picks version (target 0.4.0 per the parity issue), tag follows merge, then `cargo publish` with explicit go-ahead.

### Phase B — `nwg-dock`

**Unit tests** (all on the pure `workspace_button_plan` helper — no GTK init required):
- `config::tests::ws_flag_default_off` — `parse_from(["test"])` produces `ws == false`.
- `config::tests::ws_flag_on` — `parse_from(["test", "--ws"])` produces `ws == true`.
- `ui::workspaces::tests::plan_returns_num_ws_buttons` — `workspace_button_plan(5, None)` returns a vec of length 5 with labels "1".."5", none active.
- `ui::workspaces::tests::plan_marks_active_workspace` — `workspace_button_plan(5, Some(3))` returns 5 entries, only `n == 3` has `is_active == true`.
- `ui::workspaces::tests::plan_zero_num_ws_returns_empty` — `workspace_button_plan(0, _)` returns empty vec (degenerate case).
- `ui::workspaces::tests::plan_active_outside_range_marks_none_active` — `workspace_button_plan(10, Some(11))` returns 10 entries, all `is_active == false` (user is on a workspace beyond the configured count; documented behavior matching Go dock).

**Integration tests** (extending `tests/integration/test_runner.sh`):
- Cold start with `--ws`: assert the workspace row appears in the dock window tree.
- Cold start without `--ws`: assert no workspace row.
- (Cannot easily simulate a full workspace-switch event from headless Sway; skip that for v1.)

**Manual smoke test:**
- Run on Hyprland with `--ws --num-ws 5`, click each button, verify focus switches.
- Run on Sway same scenario.
- Run on a known-unsupported environment (just unset `HYPRLAND_INSTANCE_SIGNATURE` and `SWAYSOCK`); assert the dock comes up with pinned apps and the warning appears in the log.

**Release:** Same shape as Phase A — branch + PR, user picks version, tag, publish.

### Edge cases explicitly covered

- `num_ws = 0` → row renders zero buttons (degenerate but valid).
- `active workspace > num_ws` (user is on workspace 11 but `--num-ws 10`) → no button has the active class. Documented behavior; matches Go dock.
- `--ws` without `--num-ws` → uses the default `num_ws = 10`.
- Workspace name contains markup characters (`<`, `&`) → Button label uses `set_label` which auto-escapes; no XSS-style concern.
- Rapid workspace switching → debounced naturally by `rebuild()`'s reentrancy guard (existing infra).

## Migration / compatibility

- **`nwg-common` 0.3.x → 0.4.0:** breaking for external `Compositor` impls (none exist) due to the new trait method. Additive for matchers using `_ =>`. Downstream crates pin `0.4` on their next release; this is the first cross-repo coordination exercise post-split.
- **`nwg-dock` users:** no UI change unless they opt into `--ws`. Existing autostart lines unaffected. Dock now survives on unsupported compositors (regression-positive: niri / river users could get a Null-mode dock instead of `exit(1)`).
- **CSS:** new classes are *additive* — no existing class is removed or repurposed.

## Documentation updates

- `nwg-common`:
  - CHANGELOG entry for 0.4.0 with the new variant + trait method + warning-log changes.
  - Rustdoc on the new types.
- `nwg-dock`:
  - README: new `--ws` flag in usage examples; "Deviations from Go nwg-dock" section gains the line *"workspace switcher defaults OFF (Go: ON, opt-out via `-nows`); enable with `--ws`."*
  - CHANGELOG entry for the next minor with the widget + degradation behavior.
  - Theme classes (`.dock-workspace-button`, `.dock-workspace-active`) documented in the README's Theming section.

## Open questions

None. All decisions recorded in the *Decisions* table.
