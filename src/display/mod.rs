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

/// Whether a failed display change is worth retrying later.
///
/// Session and command failures are usually transient (the compositor is
/// still starting); a missing mode or unparseable output will not fix itself.
#[must_use]
pub fn is_retryable(error: &DisplayError) -> bool {
    !matches!(error, DisplayError::ModeNotFound { .. } | DisplayError::ParseError { .. })
}

/// A cheap fingerprint of the running graphical sessions, or `None` when
/// there are none.
///
/// It covers every Wayland socket (`/run/user/<uid>/wayland-N`) and X11
/// socket (`/tmp/.X11-unix/X*`) by path and inode, so it changes whenever a
/// compositor or X server starts or restarts (login, re-login, crash). Only
/// directory listings and `stat`s: no processes are spawned, nothing is
/// logged, so it is safe to poll.
#[must_use]
pub fn session_fingerprint() -> Option<u64> {
    use std::{
        hash::{DefaultHasher, Hash, Hasher},
        os::unix::fs::{FileTypeExt, MetadataExt},
        path::Path,
    };

    fn sockets(dir: &Path, matches: impl Fn(&str) -> bool, out: &mut Vec<(String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !matches(name) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.file_type().is_socket() {
                    out.push((entry.path().to_string_lossy().into_owned(), meta.ino()));
                }
            }
        }
    }

    let mut found = Vec::new();

    if let Ok(users) = std::fs::read_dir("/run/user") {
        for user in users.filter_map(Result::ok) {
            sockets(
                &user.path(),
                |name| {
                    name.strip_prefix("wayland-")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                },
                &mut found,
            );
        }
    }

    sockets(Path::new("/tmp/.X11-unix"), |name| name.starts_with('X'), &mut found);

    if found.is_empty() {
        return None;
    }

    found.sort_unstable();
    let mut hasher = DefaultHasher::new();
    found.hash(&mut hasher);
    Some(hasher.finish())
}

/// Log how a display change ended, and pass the result through.
fn log_outcome(change: &str, result: Result<(), DisplayError>) -> Result<(), DisplayError> {
    match &result {
        Ok(()) => log::info!("{}: success", change),
        Err(e) => log::error!("{}: failed - {}", change, e),
    }

    result
}
