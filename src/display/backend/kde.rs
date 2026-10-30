// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! KDE Wayland backend.

use super::{DisplayManager, is_builtin_display_name};
use crate::display::mode::ensure_modes_found;
use crate::display::session::run_command_as_user;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// Remove ANSI escape sequences (CSI: `ESC [ params final-byte`) from text.
///
/// `kscreen-doctor --outputs` colorizes its output unconditionally — it wraps the
/// currently active mode in `ESC [01;32m ... ESC [0;0m` even when stdout is a
/// pipe, and offers no `--no-color`/`NO_COLOR` option. Left in place, the escape
/// bytes make that token unparseable, which silently drops the active mode from
/// the discovered mode list (e.g. 2560x1600@60 disappearing while it is in use).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }

        if chars.peek() == Some(&'[') {
            chars.next();
            // Consume parameter bytes until the final byte (0x40..=0x7e)
            while let Some(&next) = chars.peek() {
                chars.next();
                if ('\x40'..='\x7e').contains(&next) {
                    break;
                }
            }
        }
    }

    out
}

/// KDE Wayland backend — uses `kscreen-doctor`
pub(super) struct KdeManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

impl DisplayManager for KdeManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying kscreen-doctor for display info (name + modes)");

        let output = run_command_as_user(self.ctx, "kscreen-doctor", &["--outputs"])?;

        if !output.status.success() {
            return Err(DisplayError::CommandFailed {
                command: "kscreen-doctor --outputs".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        log::debug!("kscreen-doctor output:\n{}", stdout);

        let (display_name, modes) =
            parse_kscreen_builtin_output(&stdout).ok_or(DisplayError::BuiltinDisplayNotFound)?;

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "kscreen-doctor", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // Build mode argument: output.<name>.mode.<W>x<H>@<Hz>
        let mode_arg = format!(
            "output.{}.mode.{}x{}@{}",
            display_name,
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate.round() as u32
        );

        // Also set VRR policy to automatic for high refresh rate modes
        let vrr_arg = format!("output.{}.vrrpolicy.automatic", display_name);

        log::debug!(
            "Executing: sudo -u {} env [vars...] kscreen-doctor {} {}",
            self.ctx.user,
            mode_arg,
            vrr_arg
        );

        let output = run_command_as_user(self.ctx, "kscreen-doctor", &[&mode_arg, &vrr_arg])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("kscreen-doctor command failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "kscreen-doctor".to_string(),
                stderr: stderr.into_owned(),
            });
        }

        log::info!(
            "Successfully applied mode {}x{}@{}Hz with VRR policy automatic",
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate.round() as u32
        );

        Ok(())
    }
}

/// Extract the built-in output's name and modes from `kscreen-doctor --outputs`.
///
/// Parses the multi-line output format:
/// ```text
/// Output: 1 eDP-1 <uuid>
///         enabled
///         Modes:  1:2560x1440@60.00*!  2:2560x1440@165.00  ...
/// Output: 2 HDMI-1 <uuid>
///         ...
/// ```
/// Returns `None` when no built-in output (eDP-*, LVDS-*) is listed.
fn parse_kscreen_builtin_output(stdout: &str) -> Option<(String, Vec<DisplayMode>)> {
    let mut display_name: Option<String> = None;
    let mut modes = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();

        if trimmed.contains("Output:") {
            // Any further output ends the built-in output's section.
            if display_name.is_some() {
                break;
            }

            display_name = trimmed
                .split_whitespace()
                .find(|part| is_builtin_display_name(part))
                .map(str::to_string);
        } else if display_name.is_some() {
            if let Some((_, modes_str)) = trimmed.split_once("Modes:") {
                log::debug!("Found modes line: {}", modes_str.trim());
                modes = parse_kscreen_modes(modes_str.trim());
            }
        }
    }

    display_name.map(|name| (name, modes))
}

/// Parse kscreen-doctor mode strings from the Modes: section
/// Example input: "1:2560x1440@60.00*!  2:2560x1440@165.00  3:1920x1200@60.00"
/// Each mode is formatted as "id:WxH@Hz" with optional *! suffixes
/// * = current mode, ! = preferred mode
fn parse_kscreen_modes(modes_str: &str) -> Vec<DisplayMode> {
    let mut modes = Vec::new();

    for mode_token in modes_str.split_whitespace() {
        // Mode format: "id:WxH@Hz*!" or similar
        // Strip the mode ID if present (e.g., "1:2560x1440@60.00*!" -> "2560x1440@60.00*!")
        let mode_part =
            if let Some((_id, rest)) = mode_token.split_once(':') { rest } else { mode_token };

        // Parse "WxH@Hz" (strip * and ! suffixes from rate)
        if let Some((res_part, rate_str)) = mode_part.split_once('@') {
            // Remove * and ! markers from rate string
            let rate_clean =
                rate_str.trim_end_matches('*').trim_end_matches('!').trim_end_matches('*');

            if let Some((width_str, height_str)) = res_part.split_once('x') {
                if let (Ok(width), Ok(height), Ok(rate)) =
                    (width_str.parse::<u32>(), height_str.parse::<u32>(), rate_clean.parse::<f32>())
                {
                    let mode_string = format!("{}x{}@{}", width, height, rate as u32);

                    log::debug!(
                        "Parsed KDE mode: {} → {}x{} @ {:.2}Hz",
                        mode_token,
                        width,
                        height,
                        rate
                    );

                    modes.push(DisplayMode {
                        mode_string,
                        resolution: (width, height),
                        refresh_rate: rate,
                        has_vrr: false, // kscreen-doctor doesn't report VRR in mode listing
                        // '*' marks the mode currently applied to the output
                        is_current: mode_token.contains('*'),
                    });
                }
            }
        }
    }

    modes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `kscreen-doctor --outputs` output colorizes the active mode with ANSI
    /// escapes. That token used to be dropped, losing the active mode entirely.
    #[test]
    fn test_strip_ansi_and_parse_kscreen_current_mode() {
        let colorized = "Modes:  1:2560x1600@240.00!  \
                         2:\x1b[01;32m2560x1600@60.00*\x1b[0;0m  \
                         3:1920x1200@240.00  19:2560x1440@59.96";
        let modes_str = strip_ansi(colorized).replace("Modes:", "");

        let modes = parse_kscreen_modes(&modes_str);

        assert_eq!(modes.len(), 4, "active mode must not be dropped");
        let current = modes.iter().find(|m| m.is_current).expect("active mode marked");
        assert_eq!(current.resolution, (2560, 1600));
        assert_eq!(current.refresh_rate, 60.0);
    }

    #[test]
    fn test_strip_ansi_removes_escapes_only() {
        assert_eq!(strip_ansi("\x1b[01;32m2560x1600@60.00*\x1b[0;0m"), "2560x1600@60.00*");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    /// Modes of a later output must not replace the built-in display's modes, and a
    /// missing built-in display must not be papered over with a guessed name.
    #[test]
    fn test_parse_kscreen_builtin_output() {
        let two_outputs = "Output: 1 eDP-2 uuid\n  enabled\n  Modes:  1:2560x1600@240.00*!\n\
                           Output: 2 HDMI-1 uuid\n  Modes:  1:1920x1080@60.00*!\n";
        let (name, modes) = parse_kscreen_builtin_output(two_outputs).expect("built-in found");

        assert_eq!(name, "eDP-2");
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].resolution, (2560, 1600));

        let external_only = "Output: 1 HDMI-1 uuid\n  Modes:  1:1920x1080@60.00*!\n";
        assert!(parse_kscreen_builtin_output(external_only).is_none());
    }
}
