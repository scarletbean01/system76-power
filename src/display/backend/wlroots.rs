// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! wlroots Wayland backend.

use super::{DisplayManager, is_builtin_display_name};
use crate::display::mode::ensure_modes_found;
use crate::display::session::run_command_as_user;
use crate::display::{DisplayMode, SessionContext};
use crate::errors::DisplayError;

/// wlroots Wayland backend — uses `wlr-randr`
///
/// Works with any compositor implementing `wlr-output-management` (Mango, sway,
/// labwc, river, wayfire, dwl, …). Requires the `wlr-randr` package.
pub(super) struct WlrootsManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Run `wlr-randr --json` and parse the array of outputs.
fn wlr_randr_outputs(ctx: &SessionContext) -> Result<Vec<serde_json::Value>, DisplayError> {
    let output = run_command_as_user(ctx, "wlr-randr", &["--json"])?;

    if !output.status.success() {
        return Err(DisplayError::CommandFailed {
            command: "wlr-randr --json".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|e| DisplayError::CommandFailed {
        command: "wlr-randr --json (parse)".to_string(),
        stderr: format!("invalid JSON: {}", e),
    })
}

/// Pick the enabled built-in output, falling back to the first enabled output.
fn select_wlr_randr_output(outputs: &[serde_json::Value]) -> Option<&serde_json::Value> {
    let mut enabled = outputs.iter().filter(|o| o["enabled"].as_bool().unwrap_or(false));

    enabled
        .clone()
        .find(|o| o["name"].as_str().is_some_and(is_builtin_display_name))
        .or_else(|| enabled.next())
}

/// Parse one entry of an output's `modes` array from `wlr-randr --json`.
fn parse_wlr_randr_mode(mode: &serde_json::Value) -> Option<DisplayMode> {
    let width = u32::try_from(mode["width"].as_u64()?).ok()?;
    let height = u32::try_from(mode["height"].as_u64()?).ok()?;
    let refresh = mode["refresh"].as_f64()?;

    Some(DisplayMode {
        // wlr-randr matches the refresh rate exactly, so keep the full precision.
        mode_string: format!("{}x{}@{:.6}", width, height, refresh),
        resolution: (width, height),
        refresh_rate: refresh as f32,
        has_vrr: false,
        is_current: mode["current"].as_bool().unwrap_or(false),
    })
}

impl DisplayManager for WlrootsManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying wlr-randr for display info (name + modes)");

        let outputs = wlr_randr_outputs(self.ctx)?;
        let output = select_wlr_randr_output(&outputs).ok_or(DisplayError::SessionNotFound)?;

        let display_name = output["name"]
            .as_str()
            .ok_or_else(|| DisplayError::ParseError { command: "wlr-randr --json".to_string() })?
            .to_string();

        let modes: Vec<DisplayMode> = output["modes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_wlr_randr_mode)
            .collect();

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "wlr-randr", &output.to_string())?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        log::debug!(
            "Executing: sudo -u {} env [vars...] wlr-randr --output {} --mode {}",
            self.ctx.user,
            display_name,
            mode.mode_string
        );

        // wlr-randr waits for the compositor to accept or reject the
        // configuration and exits non-zero on rejection.
        let output = run_command_as_user(
            self.ctx,
            "wlr-randr",
            &["--output", display_name, "--mode", &mode.mode_string],
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("wlr-randr failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "wlr-randr".to_string(),
                stderr: stderr.into_owned(),
            });
        }

        log::info!("Successfully applied mode {} to {}", mode.mode_string, display_name);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wlr_randr_mode() {
        let json = serde_json::json!(
            { "width": 2560, "height": 1600, "refresh": 59.999001, "current": true }
        );
        let mode = parse_wlr_randr_mode(&json).expect("valid mode");

        assert_eq!(mode.resolution, (2560, 1600));
        assert_eq!(mode.mode_string, "2560x1600@59.999001");
        assert!(mode.is_current);

        assert!(parse_wlr_randr_mode(&serde_json::json!({ "width": 1 })).is_none());
    }

    #[test]
    fn test_select_wlr_randr_output_prefers_enabled_builtin() {
        let outputs: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                { "name": "HDMI-A-1", "enabled": true },
                { "name": "eDP-2", "enabled": false },
                { "name": "eDP-3", "enabled": true }
            ]"#,
        )
        .unwrap();

        assert_eq!(select_wlr_randr_output(&outputs).unwrap()["name"], "eDP-3");
        assert_eq!(select_wlr_randr_output(&outputs[..2]).unwrap()["name"], "HDMI-A-1");
        assert!(select_wlr_randr_output(&[]).is_none());
    }
}
