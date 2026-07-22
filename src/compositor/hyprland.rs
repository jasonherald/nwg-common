use super::traits::{Compositor, WmEventStream};
use super::types::{WmClient, WmEvent, WmMonitor, WmWorkspace};
use crate::error::{DockError, Result};
use crate::hyprland::events::{EventStream, HyprEvent};
use crate::hyprland::ipc;
use crate::hyprland::types::{HyprClient, HyprMonitor};
use std::cell::Cell;

/// Which `dispatch` payload syntax the running Hyprland accepts.
///
/// Hyprland 0.55 moved configuration to Lua; on a Lua-config session the
/// IPC `dispatch` payload is evaluated as Lua (`hl.dispatch(<payload>)`),
/// so the legacy textual dispatchers ("focuswindow address:…") fail
/// there, and the Lua forms ("hl.dsp.focus({ window = … })") fail on a
/// classic hyprlang-config session. Which one a session speaks is only
/// discoverable by trying, so the backend probes on first dispatch and
/// caches the answer (nwg-dock issue #90).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DispatchSyntax {
    Unknown,
    Legacy,
    Lua,
}

/// Classification of Hyprland's textual reply to a `dispatch` command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DispatchReply {
    /// "ok" — dispatched.
    Ok,
    /// The dispatcher itself was rejected: classic sessions answer
    /// "Invalid dispatcher" for Lua payloads, Lua sessions answer with a
    /// Lua evaluation error ("error: …") for legacy payloads. Worth
    /// retrying in the other syntax.
    WrongSyntax,
    /// Syntax accepted but the dispatch didn't apply (e.g. "No such
    /// window found" / "warning: … window not found"). Not retried —
    /// the target is gone, not the grammar.
    Failed,
}

fn classify_dispatch_reply(reply: &str) -> DispatchReply {
    let t = reply.trim();
    if t == "ok" {
        DispatchReply::Ok
    } else if t.starts_with("Invalid dispatcher") || t.starts_with("error:") {
        DispatchReply::WrongSyntax
    } else {
        DispatchReply::Failed
    }
}

/// Escapes a string for embedding in a double-quoted Lua string literal.
fn lua_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Hyprland compositor backend.
pub struct HyprlandBackend {
    dispatch_syntax: Cell<DispatchSyntax>,
}

impl HyprlandBackend {
    pub fn new() -> Result<Self> {
        ipc::instance_signature()?;
        Ok(Self {
            dispatch_syntax: Cell::new(DispatchSyntax::Unknown),
        })
    }

    /// Sends a dispatcher in whichever syntax the session speaks.
    ///
    /// Tries the cached (or, when unknown, the legacy) syntax first and
    /// falls back to the other on a `WrongSyntax` reply, caching whichever
    /// side the compositor accepted — so steady state is one IPC
    /// round-trip. A `Failed` reply (dispatcher recognized, target
    /// missing) is logged and treated as success, matching the previous
    /// fire-and-forget behavior: the window may simply have closed under
    /// the click.
    fn dispatch(&self, legacy: &str, lua: &str) -> Result<()> {
        let (first, second, first_syntax, second_syntax) = match self.dispatch_syntax.get() {
            DispatchSyntax::Lua => (lua, legacy, DispatchSyntax::Lua, DispatchSyntax::Legacy),
            DispatchSyntax::Legacy | DispatchSyntax::Unknown => {
                (legacy, lua, DispatchSyntax::Legacy, DispatchSyntax::Lua)
            }
        };

        let reply = ipc::hyprctl(&format!("dispatch {first}"))?;
        let reply = String::from_utf8_lossy(&reply);
        match classify_dispatch_reply(&reply) {
            DispatchReply::Ok => {
                self.dispatch_syntax.set(first_syntax);
                Ok(())
            }
            DispatchReply::Failed => {
                self.dispatch_syntax.set(first_syntax);
                log::debug!("dispatch '{first}' did not apply: {}", reply.trim());
                Ok(())
            }
            DispatchReply::WrongSyntax => {
                let reply2 = ipc::hyprctl(&format!("dispatch {second}"))?;
                let reply2 = String::from_utf8_lossy(&reply2);
                match classify_dispatch_reply(&reply2) {
                    DispatchReply::Ok => {
                        self.dispatch_syntax.set(second_syntax);
                        Ok(())
                    }
                    DispatchReply::Failed => {
                        self.dispatch_syntax.set(second_syntax);
                        log::debug!("dispatch '{second}' did not apply: {}", reply2.trim());
                        Ok(())
                    }
                    DispatchReply::WrongSyntax => Err(DockError::DispatchRejected {
                        command: first.to_string(),
                        reply: reply2.trim().to_string(),
                    }),
                }
            }
        }
    }
}

impl Compositor for HyprlandBackend {
    fn list_clients(&self) -> Result<Vec<WmClient>> {
        Ok(ipc::list_clients()?.into_iter().map(to_wm_client).collect())
    }

    fn list_monitors(&self) -> Result<Vec<WmMonitor>> {
        Ok(ipc::list_monitors()?
            .into_iter()
            .map(to_wm_monitor)
            .collect())
    }

    fn get_active_window(&self) -> Result<WmClient> {
        Ok(to_wm_client(ipc::get_active_window()?))
    }

    fn get_cursor_position(&self) -> Option<(i32, i32)> {
        let reply = match ipc::hyprctl("j/cursorpos") {
            Ok(r) => r,
            Err(e) => {
                log::debug!("Failed to get cursor position: {}", e);
                return None;
            }
        };
        let val: serde_json::Value = match serde_json::from_slice(&reply) {
            Ok(v) => v,
            Err(e) => {
                log::debug!("Failed to parse cursor position: {}", e);
                return None;
            }
        };
        let x = val.get("x")?.as_i64()? as i32;
        let y = val.get("y")?.as_i64()? as i32;
        Some((x, y))
    }

    // The Lua forms below were each verified against a Hyprland 0.56
    // Lua-config session (real window, state inspected via j/clients):
    // wrong guesses reply "ok" without acting, so name-by-analogy is
    // not enough — e.g. movetoworkspace maps to hl.dsp.window.move, not
    // hl.dsp.workspace.move (which moves a workspace between monitors),
    // and "workspace N" maps to hl.dsp.focus, not workspace.change_id.

    fn focus_window(&self, id: &str) -> Result<()> {
        self.dispatch(
            &format!("focuswindow address:{id}"),
            &format!("hl.dsp.focus({{ window = \"address:{id}\" }})"),
        )
    }

    fn close_window(&self, id: &str) -> Result<()> {
        self.dispatch(
            &format!("closewindow address:{id}"),
            &format!("hl.dsp.window.close({{ window = \"address:{id}\" }})"),
        )
    }

    fn toggle_floating(&self, id: &str) -> Result<()> {
        self.dispatch(
            &format!("togglefloating address:{id}"),
            &format!("hl.dsp.window.float({{ window = \"address:{id}\" }})"),
        )
    }

    fn toggle_fullscreen(&self, id: &str) -> Result<()> {
        self.dispatch(
            &format!("fullscreen address:{id}"),
            &format!("hl.dsp.window.fullscreen({{ window = \"address:{id}\" }})"),
        )
    }

    fn move_to_workspace(&self, id: &str, workspace: i32) -> Result<()> {
        self.dispatch(
            &format!("movetoworkspace {workspace},address:{id}"),
            &format!(
                "hl.dsp.window.move({{ workspace = {workspace}, window = \"address:{id}\" }})"
            ),
        )
    }

    fn focus_workspace(&self, workspace: i32) -> Result<()> {
        self.dispatch(
            &format!("workspace {workspace}"),
            &format!("hl.dsp.focus({{ workspace = {workspace} }})"),
        )
    }

    fn toggle_special_workspace(&self, name: &str) -> Result<()> {
        self.dispatch(
            &format!("togglespecialworkspace {name}"),
            &format!(
                "hl.dsp.workspace.toggle_special(\"{}\")",
                lua_string_escape(name)
            ),
        )
    }

    fn raise_active(&self) -> Result<()> {
        self.dispatch("bringactivetotop", "hl.dsp.window.bring_to_top()")
    }

    fn exec(&self, cmd: &str) -> Result<()> {
        let sanitized = super::sanitize_exec_command(cmd);
        self.dispatch(
            &format!("exec {sanitized}"),
            &format!("hl.dsp.exec_cmd(\"{}\")", lua_string_escape(&sanitized)),
        )
    }

    fn event_stream(&self) -> Result<Box<dyn WmEventStream>> {
        Ok(Box::new(HyprlandEventStream(EventStream::connect()?)))
    }

    fn supports_cursor_position(&self) -> bool {
        true
    }
}

struct HyprlandEventStream(EventStream);

/// Maps a low-level [`HyprEvent`] to the high-level cross-compositor
/// [`WmEvent`]. Pure — no I/O, no state. Lives outside the
/// `WmEventStream` impl so unit tests exercise the same code path the
/// production stream does (the impl below just calls this and wraps
/// in `Ok`).
fn map_hypr_event(event: HyprEvent) -> WmEvent {
    match event {
        HyprEvent::ActiveWindowV2(addr) => WmEvent::ActiveWindowChanged(addr),
        HyprEvent::MonitorChanged => WmEvent::MonitorChanged,
        HyprEvent::WorkspaceV2 { id, name } => WmEvent::WorkspaceChanged { id, name },
        HyprEvent::Other(s) => WmEvent::Other(s),
    }
}

impl WmEventStream for HyprlandEventStream {
    fn next_event(&mut self) -> std::result::Result<WmEvent, std::io::Error> {
        Ok(map_hypr_event(self.0.next_event()?))
    }
}

/// Converts a Hyprland client to a compositor-neutral WmClient.
///
/// Fields intentionally not carried over (not needed by dock/drawer UI):
/// mapped, hidden, at, size, initial_title, xwayland,
/// pinned, fake_fullscreen, fullscreen_mode, grouped, swallowing.
fn to_wm_client(c: HyprClient) -> WmClient {
    WmClient {
        id: c.address,
        class: c.class,
        initial_class: c.initial_class,
        title: c.title,
        pid: c.pid,
        workspace: WmWorkspace {
            id: c.workspace.id,
            name: c.workspace.name,
        },
        floating: c.floating,
        monitor_id: c.monitor,
        fullscreen: c.fullscreen != 0,
    }
}

/// Converts a Hyprland monitor to a compositor-neutral WmMonitor.
///
/// Hyprland's `j/monitors` reports native pixel dimensions (before transform
/// or scale), but cursor coordinates from `j/cursorpos` use the logical layout
/// space (after transform and scale). Convert to logical dimensions so bounds
/// checking in the cursor poller works correctly for rotated and scaled monitors.
fn to_wm_monitor(m: HyprMonitor) -> WmMonitor {
    // Transforms 1,3,5,7 swap width↔height (90°, 270°, and their flipped variants)
    let (pw, ph) = if matches!(m.transform, 1 | 3 | 5 | 7) {
        (m.height, m.width)
    } else {
        (m.width, m.height)
    };
    let scale = if m.scale > 0.0 { m.scale } else { 1.0 };
    WmMonitor {
        id: m.id,
        name: m.name,
        width: (pw as f64 / scale).round() as i32,
        height: (ph as f64 / scale).round() as i32,
        x: m.x,
        y: m.y,
        scale,
        focused: m.focused,
        active_workspace: WmWorkspace {
            id: m.active_workspace.id,
            name: m.active_workspace.name,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyprland::types::WorkspaceRef;

    // ─── dispatch reply classification ─────────────────────────────────────
    // Reply strings below are verbatim from Hyprland 0.56 probing: a
    // classic (hyprlang) session answering Lua payloads, and a Lua
    // session answering legacy payloads.

    #[test]
    fn classify_ok() {
        assert_eq!(classify_dispatch_reply("ok"), DispatchReply::Ok);
        assert_eq!(classify_dispatch_reply("ok\n"), DispatchReply::Ok);
    }

    #[test]
    fn classify_wrong_syntax_on_classic_session() {
        // Classic session rejecting a Lua-form payload.
        assert_eq!(
            classify_dispatch_reply("Invalid dispatcher"),
            DispatchReply::WrongSyntax
        );
    }

    #[test]
    fn classify_wrong_syntax_on_lua_session() {
        // Lua session rejecting a legacy payload: the payload is evaluated
        // as Lua and fails to parse.
        let reply = "error: [string \"return hl.dispatch(focuswindow address:0xdead...\"]:1: ')' expected near 'address'\n\n → Note: dispatch in lua is a shorthand for hl.dispatch(...), your syntax might need to be updated.";
        assert_eq!(classify_dispatch_reply(reply), DispatchReply::WrongSyntax);
    }

    #[test]
    fn classify_semantic_failure_not_retried() {
        // Right syntax, missing target — classic and Lua flavors.
        assert_eq!(
            classify_dispatch_reply("No such window found"),
            DispatchReply::Failed
        );
        assert_eq!(
            classify_dispatch_reply("warning: =[C]:-1: hl.focus: window not found"),
            DispatchReply::Failed
        );
    }

    // ─── Lua string escaping ───────────────────────────────────────────────

    #[test]
    fn lua_escape_passthrough() {
        assert_eq!(
            lua_string_escape("firefox --new-window"),
            "firefox --new-window"
        );
    }

    #[test]
    fn lua_escape_quotes_backslashes_newlines() {
        assert_eq!(lua_string_escape("a\"b\\c\nd\re"), "a\\\"b\\\\c\\nd\\re");
    }

    fn test_hypr_monitor(w: i32, h: i32, scale: f64, transform: i32) -> HyprMonitor {
        HyprMonitor {
            width: w,
            height: h,
            scale,
            transform,
            name: "DP-1".into(),
            active_workspace: WorkspaceRef::default(),
            ..Default::default()
        }
    }

    #[test]
    fn to_wm_monitor_no_transform_no_scale() {
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1440, 1.0, 0));
        assert_eq!(wm.width, 2560);
        assert_eq!(wm.height, 1440);
    }

    #[test]
    fn to_wm_monitor_scaled() {
        // 4K at 1.5x scale → logical 2560×1440
        let wm = to_wm_monitor(test_hypr_monitor(3840, 2160, 1.5, 0));
        assert_eq!(wm.width, 2560);
        assert_eq!(wm.height, 1440);
    }

    #[test]
    fn to_wm_monitor_rotated_90() {
        // 2560×1600 native, rotated 90° → logical 1600×2560
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1600, 1.0, 1));
        assert_eq!(wm.width, 1600);
        assert_eq!(wm.height, 2560);
    }

    #[test]
    fn to_wm_monitor_rotated_270() {
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1600, 1.0, 3));
        assert_eq!(wm.width, 1600);
        assert_eq!(wm.height, 2560);
    }

    #[test]
    fn to_wm_monitor_rotated_and_scaled() {
        // 3840×2160 native, rotated 90°, scale 1.5 → logical 1440×2560
        let wm = to_wm_monitor(test_hypr_monitor(3840, 2160, 1.5, 1));
        assert_eq!(wm.width, 1440);
        assert_eq!(wm.height, 2560);
    }

    #[test]
    fn to_wm_monitor_180_no_swap() {
        // 180° rotation doesn't swap dimensions
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1440, 1.0, 2));
        assert_eq!(wm.width, 2560);
        assert_eq!(wm.height, 1440);
    }

    #[test]
    fn to_wm_monitor_flipped_rotated_90() {
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1600, 1.0, 5));
        assert_eq!(wm.width, 1600);
        assert_eq!(wm.height, 2560);
    }

    #[test]
    fn to_wm_monitor_flipped_rotated_270() {
        let wm = to_wm_monitor(test_hypr_monitor(2560, 1600, 1.0, 7));
        assert_eq!(wm.width, 1600);
        assert_eq!(wm.height, 2560);
    }

    #[test]
    fn to_wm_monitor_zero_scale_falls_back() {
        let wm = to_wm_monitor(test_hypr_monitor(1920, 1080, 0.0, 0));
        assert_eq!(wm.width, 1920);
        assert_eq!(wm.height, 1080);
    }

    #[test]
    fn workspace_v2_event_maps_to_workspace_changed() {
        // Exercises the production mapping function directly so the test
        // can't drift from the WmEventStream impl (the impl just calls
        // map_hypr_event + wraps in Ok).
        let mapped = map_hypr_event(crate::hyprland::events::HyprEvent::WorkspaceV2 {
            id: 3,
            name: "chat".into(),
        });
        assert_eq!(
            mapped,
            WmEvent::WorkspaceChanged {
                id: 3,
                name: "chat".into(),
            }
        );
    }
}
