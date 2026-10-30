// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! X11 backend.

use super::{DisplayManager, find_builtin_display_name, is_builtin_display_name};
use crate::display::command::run_checked;
use crate::display::mode::ensure_modes_found;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// X11 backend — uses `xrandr`
pub(super) struct X11Manager<'a> {
    pub(super) ctx: &'a SessionContext,
}

impl DisplayManager for X11Manager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying xrandr for display info (name + modes)");

        let output = run_checked(self.ctx, "xrandr", &["--query"])?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Find the built-in display name (eDP-* or LVDS-*)
        // The leading space keeps "disconnected" outputs from matching.
        let display_name = find_builtin_display_name(stdout.lines(), |line| {
            is_builtin_display_name(line) && line.contains(" connected")
        })
        .ok_or(DisplayError::BuiltinDisplayNotFound)?;

        // Collect modes for the detected display
        let mut modes = Vec::new();
        let mut in_display_section = false;
        #[allow(unused_assignments)]
        let mut current_resolution = String::new();

        log::debug!("Parsing xrandr output for display '{}'", display_name);

        for line in stdout.lines() {
            if line.starts_with(display_name.as_str()) && line.contains("connected") {
                log::debug!("Found display section: {}", line);
                in_display_section = true;
                continue;
            }

            if in_display_section && !line.starts_with(' ') && !line.starts_with('\t') {
                log::debug!("Exiting display section");
                break;
            }

            if in_display_section {
                let trimmed = line.trim_start();

                if trimmed.len() < line.len() && trimmed.contains('x') {
                    if let Some(first_token) = trimmed.split_whitespace().next() {
                        if first_token.contains('x') {
                            current_resolution = first_token.to_string();

                            if let Some(mode) = parse_xrandr_mode_line(line, &current_resolution) {
                                modes.push(mode);
                            }

                            let parts: Vec<&str> = trimmed.split_whitespace().collect();
                            for part in parts.iter().skip(1) {
                                let is_current = part.contains('*');
                                let rate_str = part.trim_end_matches('+').trim_end_matches('*');
                                if let Ok(rate) = rate_str.parse::<f32>() {
                                    if rate > 10.0 {
                                        let (width_str, height_str) =
                                            current_resolution.split_once('x').unwrap();
                                        if let (Ok(width), Ok(height)) =
                                            (width_str.parse::<u32>(), height_str.parse::<u32>())
                                        {
                                            let mode_string =
                                                format!("{}@{:.2}", current_resolution, rate);
                                            modes.push(DisplayMode {
                                                mode_string,
                                                resolution: (width, height),
                                                refresh_rate: rate,
                                                has_vrr: false,
                                                is_current,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "xrandr", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        run_checked(
            self.ctx,
            "xrandr",
            &[
                "--output",
                display_name,
                "--mode",
                &format!("{}x{}", mode.resolution.0, mode.resolution.1),
                "--rate",
                &format!("{:.2}", mode.refresh_rate),
            ],
        )?;

        Ok(())
    }
}

/// Parse xrandr mode line
/// Example input: "   2560x1440     59.95*+  165.00"
fn parse_xrandr_mode_line(line: &str, resolution: &str) -> Option<DisplayMode> {
    let trimmed = line.trim();

    // Check if line starts with resolution
    if !trimmed.starts_with(resolution) {
        return None;
    }

    // Extract refresh rates from the rest of the line
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    // Find refresh rates (numbers with optional + or *)
    // The first rate is usually the active one (marked with *)
    for i in 1..parts.len() {
        let is_current = parts[i].contains('*');
        let rate_str = parts[i].trim_end_matches('+').trim_end_matches('*');
        if let Ok(rate) = rate_str.parse::<f32>() {
            let mode_string = format!("{}@{:.2}", resolution, rate);

            log::debug!("Parsed X11 mode: {} @ {:.2}Hz", resolution, rate);

            let (width_str, height_str) = resolution.split_once('x')?;
            let width = width_str.parse::<u32>().ok()?;
            let height = height_str.parse::<u32>().ok()?;

            return Some(DisplayMode {
                mode_string,
                resolution: (width, height),
                refresh_rate: rate,
                has_vrr: false, // X11 doesn't report VRR in mode strings
                is_current,
            });
        }
    }

    None
}
