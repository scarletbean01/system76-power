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
mod detect;
mod mode;
mod session;

pub use detect::{DisplayServer, detect_display_server};
pub use mode::DisplayMode;
pub use session::{SessionContext, SessionEnv, get_session_context};

use std::io;

use crate::config::ModeSpec;
use crate::errors::DisplayError;

use backend::create_manager;
use mode::{find_best_mode, modes_for_rate_change, select_best_mode};

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
    log::info!("=== Display Refresh Rate Change Request ===");
    log::info!("Target refresh rate: {}Hz", rate);

    let ctx = get_session_context()?;
    let manager = create_manager(&ctx)?;
    let (display_name, available_modes) = manager.get_display_info()?;

    log::info!("Target display: {}", display_name);

    // A refresh rate change must not alter the resolution.
    let candidates = modes_for_rate_change(&available_modes);

    let best_mode = find_best_mode(&candidates, rate, true).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("No suitable mode found for {}Hz on {}", rate, display_name),
        )
    })?;

    log::info!(
        "Applying mode: {} ({}x{} @ {:.2}Hz, VRR: {})",
        best_mode.mode_string,
        best_mode.resolution.0,
        best_mode.resolution.1,
        best_mode.refresh_rate,
        best_mode.has_vrr
    );

    let result = manager.apply_mode(&display_name, best_mode);

    match &result {
        Ok(_) => log::info!("=== Display Refresh Rate Change: SUCCESS ==="),
        Err(e) => log::error!("=== Display Refresh Rate Change: FAILED - {} ===", e),
    }

    result
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
    log::info!("=== Display Mode Change Request ===");

    match mode_spec {
        ModeSpec::ModeString(mode_str) => {
            log::info!("Target mode: {} (explicit mode string)", mode_str);
        }
        ModeSpec::ResolutionAndRate(width, height, hz) => {
            log::info!("Target mode: {}x{}@{}Hz", width, height, hz);
        }
    }

    let ctx = get_session_context()?;
    let manager = create_manager(&ctx)?;
    let (display_name, available_modes) = manager.get_display_info()?;

    log::info!("Target display: {}", display_name);

    let best_mode = select_best_mode(&available_modes, mode_spec, &display_name, true)?;

    log::info!(
        "Applying mode: {} ({}x{} @ {:.2}Hz, VRR: {})",
        best_mode.mode_string,
        best_mode.resolution.0,
        best_mode.resolution.1,
        best_mode.refresh_rate,
        best_mode.has_vrr
    );

    let result = manager.apply_mode(&display_name, &best_mode);

    match &result {
        Ok(_) => log::info!("=== Display Mode Change: SUCCESS ==="),
        Err(e) => log::error!("=== Display Mode Change: FAILED - {} ===", e),
    }

    result
}
