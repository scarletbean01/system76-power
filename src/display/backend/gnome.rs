// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! GNOME Wayland backend.

use super::{DisplayManager, find_builtin_display_name, is_builtin_display_name};
use crate::display::mode::ensure_modes_found;
use crate::display::session::run_command_as_user;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// GNOME Wayland backend — uses `gnome-randr`
pub(super) struct GnomeManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

impl DisplayManager for GnomeManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying gnome-randr for display info (name + modes)");

        let output = run_command_as_user(self.ctx, "gnome-randr", &["query"])?;

        if !output.status.success() {
            return Err(DisplayError::CommandFailed {
                command: "gnome-randr query".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Find the built-in display name (eDP-*)
        let display_name = find_builtin_display_name(stdout.lines(), |line| {
            let trimmed = line.trim();
            is_builtin_display_name(trimmed) && !trimmed.contains("disconnected")
        })
        .ok_or(DisplayError::BuiltinDisplayNotFound)?;

        // Collect modes for the detected display
        let mut modes = Vec::new();
        let mut in_display_section = false;

        log::debug!("Parsing gnome-randr output for display '{}'", display_name);

        for line in stdout.lines() {
            let trimmed = line.trim_start();

            if !line.starts_with(' ') && !line.starts_with('\t') {
                if line.starts_with(display_name.as_str()) {
                    log::debug!("Found display section: {}", line);
                    in_display_section = true;
                    continue;
                } else if in_display_section {
                    log::debug!("Exiting display section (found different display)");
                    break;
                }
            }

            if in_display_section && trimmed.len() < line.len() && trimmed.contains('@') {
                if let Some(mode) = parse_gnome_mode_line(line) {
                    modes.push(mode);
                }
            }
        }

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "gnome-randr", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        log::debug!(
            "Executing: sudo -u {} env [vars...] gnome-randr modify {} --mode {}",
            self.ctx.user,
            display_name,
            mode.mode_string
        );
        let output = run_command_as_user(
            self.ctx,
            "gnome-randr",
            &["modify", display_name, "--mode", &mode.mode_string],
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("gnome-randr modify failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "gnome-randr modify".to_string(),
                stderr: stderr.into_owned(),
            });
        }
        Ok(())
    }
}

/// Parse a mode line from gnome-randr output
/// Example input: "    2560x1440@165.001+vrr	2560x1440 	165.00    	[scales...]"
fn parse_gnome_mode_line(line: &str) -> Option<DisplayMode> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Extract the mode string (first whitespace-separated token)
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let mode_string = parts[0].to_string();

    // Parse mode string: resolution@rate+flags
    // Examples: "2560x1440@165.001+vrr", "1920x1080@60.002"
    let has_vrr = mode_string.contains("+vrr");
    let clean = mode_string.replace("+vrr", "");

    // Split into resolution and rate parts
    let (res_part, rate_part) = clean.split_once('@')?;
    let (width_str, height_str) = res_part.split_once('x')?;

    // Parse numeric values
    let width = width_str.parse::<u32>().ok()?;
    let height = height_str.parse::<u32>().ok()?;
    let refresh_rate = rate_part.parse::<f32>().ok()?;

    log::debug!(
        "Parsed mode: {} → {}x{} @ {:.2}Hz (VRR: {})",
        mode_string,
        width,
        height,
        refresh_rate,
        has_vrr
    );

    // gnome-randr does not mark the active mode in a machine-readable way here
    Some(DisplayMode {
        mode_string,
        resolution: (width, height),
        refresh_rate,
        has_vrr,
        is_current: false,
    })
}
