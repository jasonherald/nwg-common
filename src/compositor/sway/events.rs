use super::ipc::{MSG_SUBSCRIBE, read_response_with_type, send_message};
use crate::compositor::traits::WmEventStream;
use crate::compositor::types::WmEvent;
use crate::error::DockError;
use std::os::unix::net::UnixStream;

/// i3-ipc event type for output events (bit 31 set + type 7).
const EVENT_OUTPUT: u32 = 0x80000007;

/// i3-ipc event type for workspace events (bit 31 set + type 0).
const EVENT_WORKSPACE: u32 = 0x80000000;

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
    let id = current
        .get("num")
        .and_then(|v| v.as_i64())
        .map(|n| n as i32)?;
    let name = current
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(WmEvent::WorkspaceChanged { id, name })
}

pub(super) struct SwayEventStream {
    conn: UnixStream,
}

impl SwayEventStream {
    /// Subscribes to window, output, and workspace events, taking ownership of the connection.
    pub(super) fn connect(mut conn: UnixStream) -> crate::error::Result<Self> {
        // Subscribe to window, output, and workspace events
        // (output for monitor hotplug, workspace for focus changes)
        let payload = b"[\"window\",\"output\",\"workspace\"]";
        send_message(&mut conn, MSG_SUBSCRIBE, payload)?;
        let reply = super::ipc::read_response(&mut conn)?;
        // Check subscription success
        let result: serde_json::Value = serde_json::from_slice(&reply)?;
        if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
            let err = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("Sway event subscription failed");
            return Err(DockError::Ipc(std::io::Error::other(err.to_string())));
        }
        Ok(Self { conn })
    }
}

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
                    // Extract the container id as the window identifier
                    let id = event
                        .get("container")
                        .and_then(|c| c.get("id"))
                        .and_then(|v| v.as_i64())
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    return Ok(WmEvent::ActiveWindowChanged(id));
                }
                // Skip events like "title", "fullscreen_mode", "floating", etc.
                _ => continue,
            }
        }
    }
}

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
