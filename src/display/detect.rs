// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display server detection from a session environment.

use super::session::{SessionEnv, get_display_account, get_user_session_env};

/// Display server types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    GnomeWayland,
    KdeWayland,
    Hyprland,
    Wayland,
    X11,
    Unknown,
}

impl DisplayServer {
    /// Wayland desktops with a dedicated backend, matched against `XDG_CURRENT_DESKTOP`.
    const WAYLAND_DESKTOPS: &'static [(&'static str, DisplayServer)] =
        &[("GNOME", Self::GnomeWayland), ("KDE", Self::KdeWayland), ("Hyprland", Self::Hyprland)];

    /// Detect the display server a session environment belongs to.
    pub fn detect(env: &SessionEnv) -> Option<Self> {
        log::debug!("Detecting display server from {:?}", env);

        // The compositor's own environment never carries WAYLAND_DISPLAY (it only
        // exports it to its children), so the session type is the reliable signal.
        if env.is_wayland_session() {
            let desktop = Self::WAYLAND_DESKTOPS
                .iter()
                .find(|(name, _)| env.desktop_contains(name))
                .map(|&(_, server)| server);

            return Some(desktop.unwrap_or(Self::Wayland));
        }

        if env.contains("WAYLAND_DISPLAY") {
            return Some(Self::Wayland);
        }

        env.contains("DISPLAY").then_some(Self::X11)
    }
}

/// Detect which display server is currently running
pub fn detect_display_server() -> DisplayServer {
    // Direct invocation: this process carries the session environment.
    DisplayServer::detect(&SessionEnv::from_process())
        // System service: query the user session environment instead.
        .or_else(detect_from_user_session)
        .unwrap_or(DisplayServer::Unknown)
}

/// Detect display server from the user session environment
/// This is used when running as a system service without access to session variables
fn detect_from_user_session() -> Option<DisplayServer> {
    log::debug!("Attempting to detect display server from user session");

    let account = get_display_account()
        .inspect_err(|e| log::debug!("Failed to get display user: {}", e))
        .ok()?;
    let env = get_user_session_env(&account)
        .inspect_err(|e| log::debug!("Failed to get user session environment: {}", e))
        .ok()?;

    DisplayServer::detect(&env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_display_server() {
        // Just verify it doesn't crash
        let _ = detect_display_server();
    }

    fn session_env(pairs: &[(&str, &str)]) -> SessionEnv {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Mango's own environment (what we read from /proc) has no WAYLAND_DISPLAY,
    /// only the session type; that must still resolve to a Wayland backend.
    #[test]
    fn test_detect_wayland_without_wayland_display() {
        let mango = session_env(&[
            ("XDG_SESSION_TYPE", "wayland"),
            ("XDG_CURRENT_DESKTOP", "mango:wlroots"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        assert_eq!(DisplayServer::detect(&mango), Some(DisplayServer::Wayland));

        let tty = session_env(&[("XDG_SESSION_TYPE", "tty")]);
        assert_eq!(DisplayServer::detect(&tty), None);
    }

    #[test]
    fn test_detect_known_desktops_and_x11() {
        let detect = |pairs: &[(&str, &str)]| DisplayServer::detect(&session_env(pairs));

        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")]),
            Some(DisplayServer::GnomeWayland)
        );
        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "KDE")]),
            Some(DisplayServer::KdeWayland)
        );
        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "Hyprland")]),
            Some(DisplayServer::Hyprland)
        );
        // GNOME on X11 is not a Wayland desktop backend.
        assert_eq!(
            detect(&[
                ("XDG_SESSION_TYPE", "x11"),
                ("XDG_CURRENT_DESKTOP", "GNOME"),
                ("DISPLAY", ":0")
            ]),
            Some(DisplayServer::X11)
        );
    }
}
