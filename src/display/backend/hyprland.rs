// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Hyprland backend.

use serde::Deserialize;

use super::{DisplayManager, is_builtin_display_name};
use crate::display::command::{run_checked, run_json};
use crate::display::session::find_compositor_pid;
use crate::display::{DisplayMode, SessionContext, SessionEnv, UserAccount};
use crate::errors::DisplayError;

/// A monitor as reported by `hyprctl monitors all -j`.
#[derive(Debug, Deserialize)]
struct Monitor {
    name: String,
    width: u32,
    height: u32,
    #[serde(rename = "refreshRate")]
    refresh_rate: f64,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default = "Monitor::default_scale")]
    scale: f64,
    #[serde(rename = "availableModes", default)]
    available_modes: Vec<String>,
}

impl Monitor {
    fn default_scale() -> f64 {
        1.0
    }

    /// Whether `mode` is the mode this monitor currently runs.
    fn is_running(&self, mode: &DisplayMode) -> bool {
        self.width == mode.resolution.0
            && self.height == mode.resolution.1
            && (self.refresh_rate - f64::from(mode.refresh_rate)).abs() < 1.0
    }

    /// The monitor's modes, with the running one marked.
    fn modes(&self) -> Vec<DisplayMode> {
        self.available_modes
            .iter()
            .filter_map(|mode| parse_hyprland_mode(mode))
            .map(|mode| DisplayMode { is_current: self.is_running(&mode), ..mode })
            .collect()
    }
}

/// An instance as reported by `hyprctl instances -j`.
#[derive(Debug, Deserialize)]
struct Instance {
    instance: String,
    pid: u64,
}

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
/// Asks `hyprctl instances -j` first, which works without
/// `HYPRLAND_INSTANCE_SIGNATURE` set. Falls back to the single instance
/// directory under `/run/user/<uid>/hypr/` when `hyprctl` is unavailable;
/// stale directories from crashed sessions rule that fallback out, so it is
/// only a last resort.
fn discover_hyprland_signature(account: &UserAccount, env: &SessionEnv) -> Option<String> {
    let ctx = SessionContext { account: account.clone(), env: env.clone() };

    signature_from_hyprctl(&ctx).or_else(|| signature_from_runtime_dir(account.uid))
}

fn signature_from_hyprctl(ctx: &SessionContext) -> Option<String> {
    let instances: Vec<Instance> = run_json(ctx, "hyprctl", &["instances", "-j"])
        .inspect_err(|e| log::debug!("hyprctl instances failed: {}", e))
        .ok()?;

    if let [only] = instances.as_slice() {
        return Some(only.instance.clone());
    }

    // Multiple instances (e.g. one left over from a crashed session):
    // identify ours by the compositor PID.
    let compositor_pid: u64 = find_compositor_pid(&ctx.account.name)?.parse().ok()?;
    let ours = instances.iter().find(|instance| instance.pid == compositor_pid);

    if ours.is_none() {
        log::debug!("Found {} Hyprland instances, none matched our compositor", instances.len());
    }

    ours.map(|instance| instance.instance.clone())
}

fn signature_from_runtime_dir(uid: u32) -> Option<String> {
    let mut instances = std::fs::read_dir(format!("/run/user/{}/hypr", uid))
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned());

    let first = instances.next()?;
    instances.next().is_none().then_some(first)
}

/// Hyprland Wayland backend — uses `hyprctl`
///
/// Queries `hyprctl monitors all -j` (JSON) for the built-in display's modes and
/// applies modes via the `hl.monitor` Lua API, rebuilding the full monitor
/// rule from the current position and scale so only the refresh rate changes.
pub(super) struct HyprlandManager<'a> {
    pub(super) ctx: &'a SessionContext,
}

/// Run `hyprctl monitors all -j`.
fn hyprctl_monitors(ctx: &SessionContext) -> Result<Vec<Monitor>, DisplayError> {
    run_json(ctx, "hyprctl", &["monitors", "all", "-j"])
}

/// Query the monitor called `name`.
fn hyprctl_monitor(ctx: &SessionContext, name: &str) -> Result<Monitor, DisplayError> {
    hyprctl_monitors(ctx)?.into_iter().find(|monitor| monitor.name == name).ok_or_else(|| {
        DisplayError::CommandFailed {
            command: "hyprctl monitors all -j".to_string(),
            stderr: format!("monitor '{}' not found", name),
        }
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
            .find(|monitor| is_builtin_display_name(&monitor.name))
            .or_else(|| monitors.first())
            .ok_or(DisplayError::SessionNotFound)?;

        Ok((monitor.name.clone(), monitor.modes()))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // Re-query to rebuild the full monitor rule, preserving position and scale.
        let monitor = hyprctl_monitor(self.ctx, display_name)?;

        // Canonical Hyprland mode token; the reported "2560x1600@240.00Hz" form is
        // also accepted, but "Hz" is not part of the documented monitor-rule syntax.
        let mode_arg = mode.mode_string.trim_end_matches("Hz");
        let scale = format_scale(monitor.scale);
        let (x, y) = (monitor.x, monitor.y);

        let lua = format!(
            "hl.monitor({{ output = \"{}\", mode = \"{}\", position = \"{}x{}\", scale = {} }})",
            display_name, mode_arg, x, y, scale
        );
        let rule = format!("{},{},{}x{},{}", display_name, mode_arg, x, y, scale);

        // Keep every attempt's diagnostic so a failed verification can explain itself.
        let attempt_errors = apply_monitor_rule(self.ctx, &lua, &rule)?;

        // `hyprctl` exits 0 even when it rejects a rule, so confirm the mode
        // actually took effect before reporting success.
        verify_applied_mode(self.ctx, display_name, mode).map_err(|e| match e {
            DisplayError::CommandFailed { stderr, .. } => DisplayError::CommandFailed {
                command: "hyprctl monitor mode verification".to_string(),
                stderr: format!("{}{}", attempt_errors, stderr),
            },
            other => other,
        })?;

        log::debug!("Applied mode {} to {}", mode.mode_string, display_name);

        Ok(())
    }
}

/// Apply a monitor rule, returning the diagnostics of the attempts that failed.
///
/// Hyprland >= 0.50 uses a non-legacy config parser: `hyprctl keyword`
/// rejects keywords (exit 0, no effect) and config changes must go through
/// the Lua API. Legacy parsers make `eval` fail, so fall back to `keyword`.
fn apply_monitor_rule(ctx: &SessionContext, lua: &str, rule: &str) -> Result<String, DisplayError> {
    let eval_error = match run_checked(ctx, "hyprctl", &["eval", lua]) {
        Ok(_) => return Ok(String::new()),
        Err(DisplayError::CommandFailed { stderr, .. }) => stderr,
        Err(other) => return Err(other),
    };

    log::debug!("hyprctl eval failed ({}), falling back to keyword monitor", eval_error);

    match run_checked(ctx, "hyprctl", &["keyword", "monitor", rule]) {
        Ok(_) => Ok(format!("eval: {}; ", eval_error)),
        Err(DisplayError::CommandFailed { stderr: keyword_error, .. }) => {
            let stderr = format!("eval: {}; keyword: {}", eval_error, keyword_error);
            log::error!("hyprctl apply failed: {}", stderr);

            Err(DisplayError::CommandFailed {
                command: "hyprctl eval hl.monitor".to_string(),
                stderr,
            })
        }
        Err(other) => Err(other),
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
    let monitor = hyprctl_monitor(ctx, display_name)?;

    if monitor.is_running(mode) {
        return Ok(());
    }

    Err(DisplayError::CommandFailed {
        command: "hyprctl monitor mode verification".to_string(),
        stderr: format!(
            "requested {}x{}@{:.2}Hz but display reports {}x{}@{:.2}Hz",
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate,
            monitor.width,
            monitor.height,
            monitor.refresh_rate
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

    /// Trimmed `hyprctl monitors all -j` output: extra fields must be ignored.
    const MONITORS_JSON: &str = r#"[{
        "id": 0, "name": "eDP-1", "make": "BOE", "width": 2560, "height": 1600,
        "refreshRate": 240.00000, "x": 0, "y": 0, "scale": 1.60, "focused": true,
        "availableModes": ["2560x1600@240.00Hz", "2560x1600@60.00Hz", "1920x1200@240.00Hz"]
    }, {
        "id": 1, "name": "HDMI-A-1", "width": 1920, "height": 1080,
        "refreshRate": 60.00000, "availableModes": ["1920x1080@60.00Hz"]
    }]"#;

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
    fn test_monitor_modes_mark_the_running_mode() {
        let monitors: Vec<Monitor> = serde_json::from_str(MONITORS_JSON).expect("valid JSON");
        let modes = monitors[0].modes();

        assert_eq!(modes.len(), 3);
        let current: Vec<_> = modes.iter().filter(|m| m.is_current).collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].mode_string, "2560x1600@240.00");

        assert_eq!((monitors[0].x, monitors[0].y, monitors[0].scale), (0, 0, 1.6));
        // Optional fields default instead of failing the whole parse.
        assert_eq!((monitors[1].x, monitors[1].scale), (0, 1.0));
    }

    #[test]
    fn test_monitor_missing_required_field_names_it() {
        let error = serde_json::from_str::<Vec<Monitor>>(r#"[{ "name": "eDP-1", "width": 1 }]"#)
            .unwrap_err();

        assert!(error.to_string().contains("height"), "{}", error);
    }

    #[test]
    fn test_parse_instances() {
        let instances: Vec<Instance> = serde_json::from_str(
            r#"[{ "instance": "abc_123", "time": 1, "pid": 4242, "wl_socket": "wayland-1" }]"#,
        )
        .expect("valid JSON");

        assert_eq!(instances[0].instance, "abc_123");
        assert_eq!(instances[0].pid, 4242);
    }

    #[test]
    fn test_format_scale() {
        assert_eq!(format_scale(1.6), "1.6");
        assert_eq!(format_scale(1.0), "1");
        assert_eq!(format_scale(2.0), "2");
        assert_eq!(format_scale(1.0 / 3.0), "0.333333");
    }
}
