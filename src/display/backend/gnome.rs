// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! GNOME Wayland backend.

use super::{DisplayManager, find_builtin_display_name, is_builtin_display_name};
use crate::display::command::run_checked;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// GNOME Wayland backend — uses `gnome-randr`
pub(super) struct GnomeManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Extract the built-in display's name and modes from `gnome-randr query`.
///
/// The display's section starts at an unindented line beginning with its name;
/// its modes are the indented lines after it that contain an `@`.
fn parse_gnome_output(stdout: &str) -> Result<(String, Vec<DisplayMode>), DisplayError> {
    let display_name = find_builtin_display_name(stdout.lines(), |line| {
        let trimmed = line.trim();
        is_builtin_display_name(trimmed) && !trimmed.contains("disconnected")
    })
    .ok_or(DisplayError::BuiltinDisplayNotFound)?;

    let modes = stdout
        .lines()
        .skip_while(|line| is_indented(line) || !line.starts_with(display_name.as_str()))
        .skip(1)
        .take_while(|line| is_indented(line))
        .filter(|line| line.contains('@'))
        .filter_map(parse_gnome_mode_line)
        .collect();

    Ok((display_name, modes))
}

fn is_indented(line: &str) -> bool {
    line.starts_with([' ', '\t'])
}

impl DisplayManager for GnomeManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying gnome-randr for display info (name + modes)");

        let output = run_checked(self.ctx, "gnome-randr", &["query"])?;

        parse_gnome_output(&String::from_utf8_lossy(&output.stdout))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        run_checked(
            self.ctx,
            "gnome-randr",
            &["modify", display_name, "--mode", &mode.mode_string],
        )?;

        Ok(())
    }
}

/// Parse a mode line from gnome-randr output
/// Example input: `"    2560x1440@165.001+vrr  2560x1440  165.00  [scales...]"`
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

    // gnome-randr does not mark the active mode in a machine-readable way here
    Some(DisplayMode {
        mode_string,
        resolution: (width, height),
        refresh_rate,
        has_vrr,
        is_current: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written to the layout `parse_gnome_output` documents (no captured
    /// `gnome-randr` output was available): a section per display, modes indented.
    const QUERY: &str = "\
HDMI-1 connected
    1920x1080@60.000    1920x1080    60.00
eDP-1 connected primary
    2560x1600@240.001+vrr    2560x1600    240.00
    2560x1600@60.002    2560x1600    60.00
DP-2 disconnected
    1280x720@60.000    1280x720    60.00
";

    #[test]
    fn test_parse_gnome_output_reads_only_the_builtin_section() {
        let (name, modes) = parse_gnome_output(QUERY).expect("built-in found");

        assert_eq!(name, "eDP-1");
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0].mode_string, "2560x1600@240.001+vrr");
        assert!(modes[0].has_vrr);
        assert_eq!(modes[1].resolution, (2560, 1600));
        assert_eq!(modes[1].refresh_rate, 60.002);
    }

    #[test]
    fn test_parse_gnome_output_without_builtin_display() {
        let query = "HDMI-1 connected\n    1920x1080@60.000    1920x1080    60.00\n";

        assert!(matches!(parse_gnome_output(query), Err(DisplayError::BuiltinDisplayNotFound)));
    }
}
