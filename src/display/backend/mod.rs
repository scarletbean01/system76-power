// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display backends: one [`DisplayManager`] per display server.

mod gnome;
mod hyprland;
mod kde;
mod wlroots;
mod x11;

use crate::display::{DisplayMode, DisplayServer, SessionContext, SessionEnv};
use crate::errors::DisplayError;

use gnome::GnomeManager;
use hyprland::HyprlandManager;
use kde::KdeManager;
use wlroots::WlrootsManager;
use x11::X11Manager;

pub(in crate::display) use hyprland::add_hyprland_signature;

/// Abstraction over GNOME Wayland and X11 display backends.
///
/// Each implementation queries the display and applies modes using the
/// appropriate tool (gnome-randr for GNOME Wayland, xrandr for X11).
pub(super) trait DisplayManager {
    /// Query the built-in display name and all available modes in one call.
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError>;

    /// Apply a specific mode to the named display.
    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError>;
}

/// Whether an output name belongs to a laptop's built-in panel (eDP-*, LVDS-*).
fn is_builtin_display_name(name: &str) -> bool {
    name.starts_with("eDP") || name.starts_with("LVDS")
}

/// Construct the correct `DisplayManager` for the active session.
///
/// Returns an error for display servers that cannot be detected (Unknown).
pub(super) fn create_manager(
    ctx: &SessionContext,
) -> Result<Box<dyn DisplayManager + '_>, DisplayError> {
    let display_server = DisplayServer::detect(&ctx.env)
        .or_else(|| DisplayServer::detect(&SessionEnv::from_process()))
        .unwrap_or(DisplayServer::Unknown);

    log::info!("Detected display server: {:?}", display_server);

    match display_server {
        DisplayServer::GnomeWayland => {
            log::info!("Using GNOME Wayland backend (gnome-randr)");
            Ok(Box::new(GnomeManager { ctx }))
        }
        DisplayServer::KdeWayland => {
            log::info!("Using KDE Wayland backend (kscreen-doctor)");
            Ok(Box::new(KdeManager { ctx }))
        }
        DisplayServer::Hyprland => {
            log::info!("Using Hyprland backend (hyprctl)");
            Ok(Box::new(HyprlandManager { ctx }))
        }
        DisplayServer::X11 => {
            log::info!("Using X11 backend (xrandr)");
            Ok(Box::new(X11Manager { ctx }))
        }
        DisplayServer::Wayland => {
            log::info!("Using wlroots backend (wlr-randr)");
            Ok(Box::new(WlrootsManager { ctx }))
        }
        DisplayServer::Unknown => {
            log::error!("Could not detect display server");
            Err(DisplayError::SessionNotFound)
        }
    }
}

/// Find the built-in display name from command output lines
///
/// Searches for display names matching common laptop display patterns (eDP-*, LVDS-*).
/// Returns `None` when no built-in display is listed, so callers never act on a guessed name.
///
/// # Arguments
/// * `lines` - Iterator over output lines from display query command
/// * `line_filter` - Filter function applied to each line (e.g., check "connected")
fn find_builtin_display_name<'a>(
    lines: impl Iterator<Item = &'a str>,
    line_filter: impl Fn(&str) -> bool,
) -> Option<String> {
    let name = lines
        .filter(|line| line_filter(line))
        .flat_map(str::split_whitespace)
        .find(|part| is_builtin_display_name(part))?;

    log::debug!("Found built-in display: {}", name);
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_builtin_display_name_has_no_default() {
        let lines = ["HDMI-1 connected", "DP-1 disconnected"];
        assert_eq!(find_builtin_display_name(lines.into_iter(), |_| true), None);

        let lines = ["HDMI-1 connected", "LVDS-1 connected primary"];
        assert_eq!(
            find_builtin_display_name(lines.into_iter(), |line| line.contains(" connected")),
            Some("LVDS-1".to_string())
        );
    }
}
