// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Hyprland backend.

use super::{DisplayManager, is_builtin_display_name};
use crate::display::command::{failure_text, run_as_user, run_checked};
use crate::display::mode::ensure_modes_found;
use crate::display::session::find_compositor_pid;
use crate::display::{DisplayMode, SessionContext, SessionEnv, UserAccount};
use crate::errors::DisplayError;

/// Ensure the environment carries `HYPRLAND_INSTANCE_SIGNATURE` when the
/// session runs Hyprland.
///
/// `hyprctl` locates its IPC socket via `HYPRLAND_INSTANCE_SIGNATURE`, but the
/// compositor's own environment (the process we read env vars from) never sets
/// that variable — it is only exported to Hyprland's children.
pub(in crate::display) fn add_hyprland_signature(account: &UserAccount, env: &mut SessionEnv) {
    if !env.desktop_contains("Hyprland") || env.contains("HYPRLAND_INSTANCE_SIGNATURE") {
        return;
    }

    if let Some(signature) = discover_hyprland_signature(account, env) {
        log::debug!("Discovered HYPRLAND_INSTANCE_SIGNATURE: {}", signature);
        env.insert("HYPRLAND_INSTANCE_SIGNATURE", signature);
    }
}

/// Find the signature of the active Hyprland instance of `account`.
///
/// Asks `hyprctl instances -j`, which works without `HYPRLAND_INSTANCE_SIGNATURE`
/// set, and picks the instance whose PID matches the user's compositor process.
/// Falls back to the single instance directory under `/run/user/<uid>/hypr/`
/// when `hyprctl` is unavailable; stale directories from crashed sessions rule
/// that fallback out, so it is only a last resort.
fn discover_hyprland_signature(account: &UserAccount, env: &SessionEnv) -> Option<String> {
    let ctx = SessionContext { account: account.clone(), env: env.clone() };

    if let Ok(output) = run_as_user(&ctx, "hyprctl", &["instances", "-j"]) {
        if output.status.success() {
            match serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) {
                Ok(instances) => {
                    if instances.len() == 1 {
                        if let Some(sig) = instances[0]["instance"].as_str() {
                            return Some(sig.to_string());
                        }
                    }

                    // Multiple instances (e.g. one left over from a crashed
                    // session): identify ours by the compositor PID.
                    let compositor_pid =
                        find_compositor_pid(&account.name).and_then(|pid| pid.parse::<u64>().ok());

                    if let Some(pid) = compositor_pid {
                        if let Some(sig) = instances
                            .iter()
                            .find(|i| i["pid"].as_u64() == Some(pid))
                            .and_then(|i| i["instance"].as_str())
                        {
                            return Some(sig.to_string());
                        }
                    }

                    log::debug!(
                        "Found {} Hyprland instances, none matched our compositor",
                        instances.len()
                    );
                }
                Err(e) => log::debug!("Failed to parse 'hyprctl instances -j': {}", e),
            }
        } else {
            log::debug!("hyprctl instances failed: {}", failure_text(&output));
        }
    }

    let mut entries = std::fs::read_dir(format!("/run/user/{}/hypr", account.uid))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned());

    let first = entries.next()?;
    (entries.next().is_none()).then_some(first)
}

/// Hyprland Wayland backend — uses `hyprctl`
///
/// Queries `hyprctl monitors all -j` (JSON) for the built-in display's modes and
/// applies modes via the `hl.monitor` Lua API, rebuilding the full monitor
/// rule from the current position and scale so only the refresh rate changes.
pub(super) struct HyprlandManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Run `hyprctl monitors all -j` and parse the JSON array.
fn hyprctl_monitors(ctx: &SessionContext) -> Result<Vec<serde_json::Value>, DisplayError> {
    let output = run_checked(ctx, "hyprctl", &["monitors", "all", "-j"])?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    serde_json::from_str(&stdout).map_err(|e| DisplayError::CommandFailed {
        command: "hyprctl monitors all -j (parse)".to_string(),
        stderr: format!("invalid JSON: {}", e),
    })
}

/// Parse an `availableModes` string like "2560x1600@240.00Hz" into a DisplayMode.
fn parse_hyprland_mode(mode_str: &str) -> Option<DisplayMode> {
    let spec = mode_str.strip_suffix("Hz")?;
    let (resolution_str, rate_str) = spec.split_once('@')?;
    let (width_str, height_str) = resolution_str.split_once('x')?;

    Some(DisplayMode {
        // Canonical form without the "Hz" suffix, matching the other backends.
        mode_string: spec.to_string(),
        resolution: (width_str.parse().ok()?, height_str.parse().ok()?),
        refresh_rate: rate_str.parse().ok()?,
        has_vrr: false,
        is_current: false,
    })
}

impl DisplayManager for HyprlandManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying hyprctl for display info (name + modes)");

        let monitors = hyprctl_monitors(self.ctx)?;

        // Prefer the built-in display, fall back to the first monitor.
        let monitor = monitors
            .iter()
            .find(|m| m["name"].as_str().is_some_and(is_builtin_display_name))
            .or_else(|| monitors.first())
            .ok_or(DisplayError::SessionNotFound)?;

        let display_name = monitor["name"]
            .as_str()
            .ok_or_else(|| DisplayError::ParseError {
                command: "hyprctl monitors all -j".to_string(),
            })?
            .to_string();

        let current_res = (monitor["width"].as_u64(), monitor["height"].as_u64());
        let current_rate = monitor["refreshRate"].as_f64();

        let modes: Vec<DisplayMode> = monitor["availableModes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| {
                        let mode = parse_hyprland_mode(s)?;
                        let is_current = current_res
                            == (Some(mode.resolution.0 as u64), Some(mode.resolution.1 as u64))
                            && current_rate
                                .map(|r| (r - mode.refresh_rate as f64).abs() < 1.0)
                                .unwrap_or(false);
                        Some(DisplayMode { is_current, ..mode })
                    })
                    .collect()
            })
            .unwrap_or_default();

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(
            &modes,
            &display_name,
            "hyprctl",
            &serde_json::to_string(&monitor).unwrap_or_default(),
        )?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // Re-query to rebuild the full monitor rule, preserving position and scale.
        let monitors = hyprctl_monitors(self.ctx)?;
        let monitor = monitors
            .iter()
            .find(|m| m["name"].as_str() == Some(display_name))
            .ok_or_else(|| DisplayError::CommandFailed {
                command: "hyprctl monitors all -j".to_string(),
                stderr: format!("monitor '{}' not found", display_name),
            })?;

        let x = monitor["x"].as_i64().unwrap_or(0);
        let y = monitor["y"].as_i64().unwrap_or(0);
        let scale = monitor["scale"].as_f64().unwrap_or(1.0);

        // Canonical Hyprland mode token; the reported "2560x1600@240.00Hz" form is
        // also accepted, but "Hz" is not part of the documented monitor-rule syntax.
        let mode_arg = mode.mode_string.trim_end_matches("Hz");

        // Hyprland >= 0.50 uses a non-legacy config parser: `hyprctl keyword`
        // rejects keywords (exit 0, no effect) and config changes must go through
        // the Lua API. Legacy parsers make `eval` fail, so fall back to `keyword`.
        let lua = format!(
            "hl.monitor({{ output = \"{}\", mode = \"{}\", position = \"{}x{}\", scale = {} }})",
            display_name,
            mode_arg,
            x,
            y,
            format_scale(scale)
        );

        // Keep every attempt's diagnostic so a failed verification can explain itself.
        let mut attempt_errors = String::new();

        if let Err(eval_failure) = run_checked(self.ctx, "hyprctl", &["eval", &lua]) {
            let DisplayError::CommandFailed { stderr: eval_error, .. } = eval_failure else {
                return Err(eval_failure);
            };

            log::debug!("hyprctl eval failed ({}), falling back to keyword monitor", eval_error);
            attempt_errors.push_str(&format!("eval: {}; ", eval_error));

            let rule = format!("{},{},{}x{},{}", display_name, mode_arg, x, y, format_scale(scale));

            if let Err(keyword_failure) =
                run_checked(self.ctx, "hyprctl", &["keyword", "monitor", &rule])
            {
                let DisplayError::CommandFailed { stderr: keyword_error, .. } = keyword_failure
                else {
                    return Err(keyword_failure);
                };

                attempt_errors.push_str(&format!("keyword: {}", keyword_error));
                log::error!("hyprctl apply failed: {}", attempt_errors);
                return Err(DisplayError::CommandFailed {
                    command: "hyprctl eval hl.monitor".to_string(),
                    stderr: attempt_errors,
                });
            }
        }

        // `hyprctl` exits 0 even when it rejects a rule, so confirm the mode
        // actually took effect before reporting success.
        match verify_applied_mode(self.ctx, display_name, mode) {
            Ok(()) => {}
            Err(DisplayError::CommandFailed { stderr, .. }) => {
                let stderr = format!("{}{}", attempt_errors, stderr);
                log::error!("=== Display Refresh Rate Change: FAILED - {} ===", stderr);
                return Err(DisplayError::CommandFailed {
                    command: "hyprctl monitor mode verification".to_string(),
                    stderr,
                });
            }
            Err(e) => return Err(e),
        }
        log::info!("Successfully applied mode {} to {}", mode.mode_string, display_name);

        Ok(())
    }
}

/// Verify the requested mode was actually applied.
///
/// `hyprctl` exits 0 even when it rejects a monitor rule, so a successful exit
/// status proves nothing. Re-query the monitor and compare the active
/// resolution and refresh rate against the requested mode.
fn verify_applied_mode(
    ctx: &SessionContext,
    display_name: &str,
    mode: &DisplayMode,
) -> Result<(), DisplayError> {
    let monitors = hyprctl_monitors(ctx)?;
    let monitor =
        monitors.iter().find(|m| m["name"].as_str() == Some(display_name)).ok_or_else(|| {
            DisplayError::CommandFailed {
                command: "hyprctl monitors all -j".to_string(),
                stderr: format!("monitor '{}' not found after applying mode", display_name),
            }
        })?;

    let width = monitor["width"].as_u64();
    let height = monitor["height"].as_u64();
    let rate = monitor["refreshRate"].as_f64();

    let resolution_matches =
        width == Some(mode.resolution.0 as u64) && height == Some(mode.resolution.1 as u64);
    let rate_matches = rate.map(|r| (r - mode.refresh_rate as f64).abs() < 1.0).unwrap_or(false);

    if resolution_matches && rate_matches {
        return Ok(());
    }

    Err(DisplayError::CommandFailed {
        command: "hyprctl monitor mode verification".to_string(),
        stderr: format!(
            "requested {}x{}@{:.2}Hz but display reports {}x{}@{:.2}Hz",
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate,
            width.unwrap_or(0),
            height.unwrap_or(0),
            rate.unwrap_or(0.0)
        ),
    })
}

/// Format a Hyprland scale without a trailing ".0" (Hyprland expects e.g. "1.6" or "1").
fn format_scale(scale: f64) -> String {
    if (scale - scale.round()).abs() < f64::EPSILON {
        format!("{}", scale.round() as i64)
    } else {
        let formatted = format!("{:.6}", scale);
        formatted.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hyprctl` reports modes as "2560x1600@240.00Hz"; the stored mode string must
    /// be the canonical, suffix-free form used to build monitor rules.
    #[test]
    fn test_parse_hyprland_mode() {
        let mode = parse_hyprland_mode("2560x1600@240.00Hz").expect("valid mode");
        assert_eq!(mode.resolution, (2560, 1600));
        assert_eq!(mode.refresh_rate, 240.0);
        assert_eq!(mode.mode_string, "2560x1600@240.00");
        assert!(!mode.is_current);

        assert!(parse_hyprland_mode("2560x1600@240.00").is_none(), "missing Hz suffix");
        assert!(parse_hyprland_mode("garbage").is_none());
    }

    #[test]
    fn test_format_scale() {
        assert_eq!(format_scale(1.6), "1.6");
        assert_eq!(format_scale(1.0), "1");
        assert_eq!(format_scale(2.0), "2");
        assert_eq!(format_scale(1.0 / 3.0), "0.333333");
    }
}
