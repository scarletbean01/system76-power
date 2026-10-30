// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display refresh rate management
//!
//! This module provides functionality to control display refresh rates based on
//! power profiles. It supports GNOME Wayland (via gnome-randr), KDE Wayland
//! (kscreen-doctor), Hyprland (hyprctl), other wlroots compositors such as
//! Mango (wlr-randr) and X11 (xrandr).
//!
//! - [`session`]: finds the graphical session and its environment
//! - [`detect`]: maps a session environment to a [`DisplayServer`]
//! - [`mode`]: display modes and mode selection
//! - `backend`: one [`DisplayManager`](backend) implementation per display server

mod backend;
mod command;
mod detect;
mod mode;
mod session;

pub use detect::{DisplayServer, detect_display_server};
pub use mode::DisplayMode;
pub use session::{SessionContext, SessionEnv, UserAccount, get_session_context};

use crate::config::ModeSpec;
use crate::errors::DisplayError;

use backend::create_manager;
use mode::{select_best_mode, select_for_rate};

/// Set display refresh rate based on detected display server
///
/// Automatically detects GNOME Wayland, X11, or other Wayland compositors
/// and uses the appropriate method to set the refresh rate.
///
/// # Arguments
///
/// * `rate` - Target refresh rate in Hz (e.g., 60, 120, 144, 165)
///
/// # Errors
///
/// Returns an error if:
/// - Display server cannot be detected
/// - Display user cannot be found
/// - No suitable mode is found for the requested refresh rate
/// - The underlying command (gnome-randr/xrandr) fails
pub fn set_refresh_rate(rate: u32) -> Result<(), DisplayError> {
    log::info!("Display refresh rate change request: {}Hz", rate);

    log_outcome(
        "Display refresh rate change",
        apply_display_mode(|modes, display| select_for_rate(modes, rate, display)),
    )
}

/// Set display mode (resolution and refresh rate) based on ModeSpec
///
/// This function changes both resolution and refresh rate based on the provided mode specification.
/// It can work with either explicit mode strings or resolution + refresh rate combinations.
///
/// # Arguments
///
/// * `mode_spec` - The mode specification (either explicit mode string or resolution+rate)
///
/// # Errors
///
/// Returns an error if:
/// - Display server cannot be detected
/// - Display user cannot be found
/// - No suitable mode is found for the requested specification
/// - The underlying command (gnome-randr/xrandr) fails
pub fn set_display_mode(mode_spec: &ModeSpec) -> Result<(), DisplayError> {
    log::info!("Display mode change request: {:?}", mode_spec);

    log_outcome(
        "Display mode change",
        apply_display_mode(|modes, display| select_best_mode(modes, mode_spec, display, true)),
    )
}

/// The pipeline shared by every display change: find the session, pick the
/// backend, query the built-in display, let `select` choose a mode among its
/// modes, and apply it.
fn apply_display_mode(
    select: impl FnOnce(&[DisplayMode], &str) -> Result<DisplayMode, DisplayError>,
) -> Result<(), DisplayError> {
    let ctx = get_session_context()?;
    let manager = create_manager(&ctx)?;
    let (display_name, available_modes) = manager.get_display_info()?;

    if available_modes.is_empty() {
        return Err(DisplayError::ModeNotFound { display: display_name, spec: "any".to_string() });
    }

    log::info!("Target display: {} ({} modes)", display_name, available_modes.len());

    let mode = select(&available_modes, &display_name)?;

    log::info!(
        "Applying mode: {} ({}x{} @ {:.2}Hz, VRR: {})",
        mode.mode_string,
        mode.resolution.0,
        mode.resolution.1,
        mode.refresh_rate,
        mode.has_vrr
    );

    manager.apply_mode(&display_name, &mode)
}

/// Log how a display change ended, and pass the result through.
fn log_outcome(change: &str, result: Result<(), DisplayError>) -> Result<(), DisplayError> {
    match &result {
        Ok(()) => log::info!("{}: success", change),
        Err(e) => log::error!("{}: failed - {}", change, e),
    }

    result
}
