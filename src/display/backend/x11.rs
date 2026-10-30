// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! X11 backend.

use super::{DisplayManager, find_builtin_display_name, is_builtin_display_name};
use crate::display::command::run_checked;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// X11 backend — uses `xrandr`
pub(super) struct X11Manager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Extract the built-in display's name and modes from `xrandr --query`.
///
/// The display's section starts at its `<name> connected …` header line and
/// continues over the indented mode lines that follow it.
fn parse_xrandr_output(stdout: &str) -> Result<(String, Vec<DisplayMode>), DisplayError> {
    // The leading space keeps "disconnected" outputs from matching.
    let display_name = find_builtin_display_name(stdout.lines(), |line| {
        is_builtin_display_name(line) && line.contains(" connected")
    })
    .ok_or(DisplayError::BuiltinDisplayNotFound)?;

    let modes = stdout
        .lines()
        .skip_while(|line| line.split_whitespace().next() != Some(display_name.as_str()))
        .skip(1)
        .take_while(|line| line.starts_with([' ', '\t']))
        .flat_map(parse_xrandr_mode_line)
        .collect();

    Ok((display_name, modes))
}

impl DisplayManager for X11Manager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying xrandr for display info (name + modes)");

        let output = run_checked(self.ctx, "xrandr", &["--query"])?;

        parse_xrandr_output(&String::from_utf8_lossy(&output.stdout))
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

/// Parse an xrandr mode line into one mode per refresh rate.
///
/// Example input: `"   2560x1440     59.95*+  165.00"`. Each rate may carry a `*`
/// (currently active) and/or `+` (preferred) marker.
fn parse_xrandr_mode_line(line: &str) -> Vec<DisplayMode> {
    let mut tokens = line.split_whitespace();

    let Some(resolution) = tokens.next() else {
        return Vec::new();
    };
    let Some((width, height)) = resolution
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)))
    else {
        return Vec::new();
    };

    tokens
        .filter_map(|token| {
            let refresh_rate: f32 = token.trim_end_matches(['+', '*']).parse().ok()?;

            Some(DisplayMode {
                mode_string: format!("{}@{:.2}", resolution, refresh_rate),
                resolution: (width, height),
                refresh_rate,
                has_vrr: false, // X11 doesn't report VRR in mode strings
                is_current: token.contains('*'),
            })
        })
        .filter(|mode| mode.refresh_rate > 10.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `xrandr --query` layout.
    const QUERY: &str = "\
Screen 0: minimum 16 x 16, current 2560 x 1600, maximum 32767 x 32767
HDMI-1 disconnected (normal left inverted right x axis y axis)
eDP-1 connected primary 2560x1600+0+0 (normal left inverted right x axis y axis) 340mm x 220mm
   2560x1600    240.00*+  165.00    60.00  
   1920x1200    240.00    60.00  
DP-2 connected 1920x1080+2560+0 (normal left inverted right x axis y axis) 530mm x 300mm
   1920x1080     60.00*+
";

    #[test]
    fn test_parse_xrandr_output_reads_only_the_builtin_section() {
        let (name, modes) = parse_xrandr_output(QUERY).expect("built-in found");

        assert_eq!(name, "eDP-1");
        assert_eq!(modes.len(), 5);

        let current: Vec<_> = modes.iter().filter(|m| m.is_current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].mode_string, "2560x1600@240.00");

        assert!(modes.iter().all(|m| m.resolution != (1920, 1080)), "DP-2 modes leaked in");
    }

    #[test]
    fn test_parse_xrandr_output_ignores_disconnected_builtin() {
        let query = "eDP-1 disconnected (normal left inverted right x axis y axis)\n";

        assert!(matches!(parse_xrandr_output(query), Err(DisplayError::BuiltinDisplayNotFound)));
    }

    #[test]
    fn test_parse_xrandr_mode_line() {
        let modes = parse_xrandr_mode_line("   2560x1440     59.95*+  165.00");

        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0].mode_string, "2560x1440@59.95");
        assert!(modes[0].is_current);
        assert_eq!(modes[1].refresh_rate, 165.0);
        assert!(!modes[1].is_current);

        assert!(parse_xrandr_mode_line("not a mode line").is_empty());
    }
}
