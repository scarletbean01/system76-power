// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! wlroots Wayland backend.

use serde::Deserialize;

use super::{DisplayManager, is_builtin_display_name};
use crate::display::command::{run_checked, run_json};
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// An output as reported by `wlr-randr --json`.
#[derive(Debug, Deserialize)]
struct Output {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    modes: Vec<Mode>,
}

/// A mode of an [`Output`].
#[derive(Debug, Deserialize)]
struct Mode {
    width: u32,
    height: u32,
    refresh: f64,
    #[serde(default)]
    current: bool,
}

impl From<&Mode> for DisplayMode {
    fn from(mode: &Mode) -> Self {
        DisplayMode {
            // wlr-randr matches the refresh rate exactly, so keep the full precision.
            mode_string: format!("{}x{}@{:.6}", mode.width, mode.height, mode.refresh),
            resolution: (mode.width, mode.height),
            refresh_rate: mode.refresh as f32,
            has_vrr: false,
            is_current: mode.current,
        }
    }
}

/// wlroots Wayland backend — uses `wlr-randr`
///
/// Works with any compositor implementing `wlr-output-management` (Mango, sway,
/// labwc, river, wayfire, dwl, …). Requires the `wlr-randr` package.
pub(super) struct WlrootsManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Pick the enabled built-in output, falling back to the first enabled output.
fn select_output(outputs: &[Output]) -> Option<&Output> {
    let mut enabled = outputs.iter().filter(|output| output.enabled);

    enabled.clone().find(|output| is_builtin_display_name(&output.name)).or_else(|| enabled.next())
}

impl DisplayManager for WlrootsManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying wlr-randr for display info (name + modes)");

        let outputs: Vec<Output> = run_json(self.ctx, "wlr-randr", &["--json"])?;
        let output = select_output(&outputs).ok_or(DisplayError::SessionNotFound)?;

        Ok((output.name.clone(), output.modes.iter().map(DisplayMode::from).collect()))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // wlr-randr waits for the compositor to accept or reject the
        // configuration and exits non-zero on rejection.
        run_checked(
            self.ctx,
            "wlr-randr",
            &["--output", display_name, "--mode", &mode.mode_string],
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed `wlr-randr --json` output (extra fields are ignored).
    const OUTPUTS_JSON: &str = r#"[
        { "name": "HDMI-A-1", "enabled": true, "modes": [] },
        { "name": "eDP-2", "enabled": false, "modes": [] },
        { "name": "eDP-1", "description": "BOE", "enabled": true, "modes": [
            { "width": 2560, "height": 1600, "refresh": 240.000000, "preferred": true, "current": true },
            { "width": 2560, "height": 1600, "refresh": 59.999001, "preferred": true, "current": false }
        ] }
    ]"#;

    fn outputs() -> Vec<Output> {
        serde_json::from_str(OUTPUTS_JSON).expect("valid JSON")
    }

    #[test]
    fn test_mode_keeps_full_refresh_precision() {
        let outputs = outputs();
        let modes: Vec<DisplayMode> = outputs[2].modes.iter().map(DisplayMode::from).collect();

        assert_eq!(modes[0].mode_string, "2560x1600@240.000000");
        assert!(modes[0].is_current);
        assert_eq!(modes[1].mode_string, "2560x1600@59.999001");
        assert_eq!(modes[1].resolution, (2560, 1600));
        assert!(!modes[1].is_current);
    }

    #[test]
    fn test_select_output_prefers_enabled_builtin() {
        let outputs = outputs();

        // eDP-2 is built-in but disabled, so eDP-1 wins over it and over HDMI.
        assert_eq!(select_output(&outputs).unwrap().name, "eDP-1");
        // Without an enabled built-in, the first enabled output is used.
        assert_eq!(select_output(&outputs[..2]).unwrap().name, "HDMI-A-1");
        assert!(select_output(&[]).is_none());
    }

    #[test]
    fn test_missing_required_field_names_it() {
        let error = serde_json::from_str::<Vec<Output>>(r#"[{ "enabled": true }]"#).unwrap_err();

        assert!(error.to_string().contains("name"), "{}", error);
    }
}
