// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display modes and selection of the mode matching a request.

use crate::config::ModeSpec;
use crate::errors::DisplayError;

/// Represents a display mode with resolution and refresh rate
#[derive(Debug, Clone)]
pub struct DisplayMode {
    /// Full mode string (e.g., "2560x1440@165.001+vrr")
    pub mode_string: String,
    /// Resolution as (width, height)
    pub resolution: (u32, u32),
    /// Refresh rate in Hz
    pub refresh_rate: f32,
    /// Whether VRR (Variable Refresh Rate) is supported
    pub has_vrr: bool,
    /// Whether this is the mode currently applied to the display
    pub is_current: bool,
}

/// Find the best matching mode for a target refresh rate
/// Prefers VRR modes if available, and selects closest matching rate
pub(super) fn find_best_mode<'a>(
    modes: &[&'a DisplayMode],
    target_rate: u32,
    prefer_vrr: bool,
) -> Option<&'a DisplayMode> {
    if modes.is_empty() {
        log::warn!("No modes available for matching");
        return None;
    }

    log::debug!(
        "Finding best mode for target {}Hz from {} available modes (prefer VRR: {})",
        target_rate,
        modes.len(),
        prefer_vrr
    );

    let target_f32 = target_rate as f32;

    // First, try to find VRR modes if preferred
    let vrr_modes: Option<Vec<&'a DisplayMode>> = if prefer_vrr {
        let vrr_modes: Vec<&'a DisplayMode> = modes.iter().copied().filter(|m| m.has_vrr).collect();
        if vrr_modes.is_empty() {
            log::debug!("No VRR modes found, using all {} modes", modes.len());
            None
        } else {
            log::debug!("Found {} VRR modes, preferring those", vrr_modes.len());
            Some(vrr_modes)
        }
    } else {
        None
    };

    // Find the closest match by refresh rate
    let best = vrr_modes.as_deref().unwrap_or(modes).iter().copied().min_by_key(|mode| {
        let diff = (mode.refresh_rate - target_f32).abs();
        (diff * 1000.0) as u32 // Convert to millihertz for integer comparison
    });

    if let Some(mode) = best {
        log::debug!(
            "Selected best mode: {} ({}x{} @ {:.2}Hz, VRR: {}) - diff: {:.2}Hz",
            mode.mode_string,
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate,
            mode.has_vrr,
            (mode.refresh_rate - target_f32).abs()
        );
    }

    best
}

/// Candidate modes for a refresh-rate-only change.
///
/// A refresh rate change must not alter the resolution, so when the backend
/// reports the active mode the candidates are restricted to modes of the same
/// resolution (always non-empty: the active mode itself is included). Falls
/// back to every mode when the active mode is unknown (e.g. gnome-randr).
pub(super) fn modes_for_rate_change(modes: &[DisplayMode]) -> Vec<&DisplayMode> {
    let Some(resolution) = modes.iter().find(|m| m.is_current).map(|m| m.resolution) else {
        return modes.iter().collect();
    };

    let same_resolution: Vec<&DisplayMode> =
        modes.iter().filter(|m| m.resolution == resolution).collect();

    log::debug!(
        "Keeping current resolution {}x{} ({} matching modes)",
        resolution.0,
        resolution.1,
        same_resolution.len()
    );

    same_resolution
}

/// Absolute difference between two refresh rates in millihertz, for integer comparison.
fn millihertz_diff(a: f32, b: f32) -> u32 {
    ((a - b).abs() * 1000.0) as u32
}

/// Select the mode for a refresh-rate-only change.
///
/// The resolution is kept (see [`modes_for_rate_change`]) and VRR modes are preferred.
pub(super) fn select_for_rate(
    available_modes: &[DisplayMode],
    rate: u32,
    display_name: &str,
) -> Result<DisplayMode, DisplayError> {
    let candidates = modes_for_rate_change(available_modes);

    find_best_mode(&candidates, rate, true)
        .cloned()
        .ok_or_else(|| mode_not_found(display_name, format!("{}Hz", rate)))
}

/// Select the best matching display mode from available modes based on specification
///
/// # Arguments
/// * `available_modes` - Slice of available DisplayMode options from the display
/// * `mode_spec` - The desired mode specification (explicit string or resolution+rate)
/// * `display_name` - Name of the display (for error messages)
/// * `prefer_vrr` - Whether to prefer VRR modes when using ResolutionAndRate
///
/// # Returns
/// The best matching DisplayMode (cloned), or [`DisplayError::ModeNotFound`]
pub(super) fn select_best_mode(
    available_modes: &[DisplayMode],
    mode_spec: &ModeSpec,
    display_name: &str,
    prefer_vrr: bool,
) -> Result<DisplayMode, DisplayError> {
    let (selected, spec) = match mode_spec {
        ModeSpec::ModeString(spec) => (select_by_mode_string(available_modes, spec), spec.clone()),
        ModeSpec::ResolutionAndRate(width, height, hz) => (
            select_by_resolution_and_rate(available_modes, (*width, *height), *hz, prefer_vrr),
            format!("{}x{}@{}Hz", width, height, hz),
        ),
    };

    selected.cloned().ok_or_else(|| mode_not_found(display_name, spec))
}

fn mode_not_found(display_name: &str, spec: String) -> DisplayError {
    DisplayError::ModeNotFound { display: display_name.to_string(), spec }
}

/// Match an explicit mode string, from the exact form down to the closest rate.
fn select_by_mode_string<'a>(modes: &'a [DisplayMode], spec: &str) -> Option<&'a DisplayMode> {
    // Accept the "…Hz" suffix that `hyprctl monitors` prints, so specs
    // copied from it (or from `system76-power.conf` examples) still match
    // the canonical, suffix-free mode strings we store.
    let spec = spec.trim_end_matches("Hz").trim_end_matches("hz");

    if let Some(mode) = modes.iter().find(|m| m.mode_string == spec) {
        return Some(mode);
    }

    log::warn!("Exact mode string '{}' not found, trying fuzzy match", spec);

    let spec_no_vrr = spec.replace("+vrr", "");
    if let Some(mode) = modes.iter().find(|m| m.mode_string.replace("+vrr", "") == spec_no_vrr) {
        return Some(mode);
    }

    // This handles cases like "2560x1440@60.002" matching "2560x1440@60".
    log::warn!("VRR-stripped match failed, trying resolution+rate parsing");

    let (resolution, rate) = parse_resolution_and_rate(&spec_no_vrr)?;
    let best = modes
        .iter()
        .filter(|m| m.resolution == resolution)
        .min_by_key(|m| millihertz_diff(m.refresh_rate, rate))?;

    log::info!(
        "Fuzzy matched '{}' to '{}' (diff: {:.3}Hz)",
        spec,
        best.mode_string,
        (best.refresh_rate - rate).abs()
    );

    Some(best)
}

/// Parse `"<width>x<height>@<rate>"` into its resolution and refresh rate.
fn parse_resolution_and_rate(spec: &str) -> Option<((u32, u32), f32)> {
    let (resolution, rate) = spec.split_once('@')?;
    let (width, height) = resolution.split_once('x')?;

    Some(((width.parse().ok()?, height.parse().ok()?), rate.parse().ok()?))
}

/// Best mode of the given resolution for a target rate, optionally preferring VRR.
fn select_by_resolution_and_rate(
    modes: &[DisplayMode],
    resolution: (u32, u32),
    hz: u32,
    prefer_vrr: bool,
) -> Option<&DisplayMode> {
    let same_resolution: Vec<&DisplayMode> =
        modes.iter().filter(|m| m.resolution == resolution).collect();

    find_best_mode(&same_resolution, hz, prefer_vrr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, rate: f32, is_current: bool) -> DisplayMode {
        DisplayMode {
            mode_string: format!("{}x{}@{}", width, height, rate as u32),
            resolution: (width, height),
            refresh_rate: rate,
            has_vrr: false,
            is_current,
        }
    }

    /// Battery profile sets 60Hz: the panel's 2560x1440@59.96 mode must not win
    /// over the active resolution's 2560x1600@60 mode.
    #[test]
    fn test_rate_change_keeps_active_resolution() {
        let modes = vec![
            mode(2560, 1600, 240.0, true),
            mode(2560, 1600, 60.0, false),
            mode(2560, 1440, 59.96, false),
        ];

        let candidates = modes_for_rate_change(&modes);
        let best = find_best_mode(&candidates, 60, true).expect("mode found");

        assert_eq!(best.resolution, (2560, 1600));
        assert_eq!(best.refresh_rate, 60.0);
    }

    #[test]
    fn test_rate_change_without_current_mode_uses_all_modes() {
        let modes = vec![mode(2560, 1600, 240.0, false), mode(2560, 1440, 60.0, false)];

        let candidates = modes_for_rate_change(&modes);
        let best = find_best_mode(&candidates, 60, true).expect("mode found");

        assert_eq!(candidates.len(), 2);
        assert_eq!(best.resolution, (2560, 1440));
    }

    /// Specs copied from `hyprctl monitors` output carry a "Hz" suffix that our
    /// stored mode strings do not; such specs must still resolve to the mode.
    fn modes() -> Vec<DisplayMode> {
        vec![mode(2560, 1600, 240.0, true), mode(2560, 1600, 60.0, false)]
    }

    #[test]
    fn test_select_best_mode_falls_back_to_the_closest_rate() {
        let spec = ModeSpec::ModeString("2560x1600@60.002+vrr".to_string());
        let best = select_best_mode(&modes(), &spec, "eDP-1", false).expect("fuzzy match");

        assert_eq!(best.refresh_rate, 60.0);
    }

    #[test]
    fn test_select_best_mode_by_resolution_and_rate() {
        let spec = ModeSpec::ResolutionAndRate(2560, 1600, 60);
        let best = select_best_mode(&modes(), &spec, "eDP-1", true).expect("match");

        assert_eq!(best.refresh_rate, 60.0);
    }

    /// A request nothing matches is a typed error naming the display and the request.
    #[test]
    fn test_unmatched_requests_are_mode_not_found() {
        let wrong_resolution = ModeSpec::ResolutionAndRate(800, 600, 60);
        let wrong_string = ModeSpec::ModeString("garbage".to_string());

        for spec in [wrong_resolution, wrong_string] {
            match select_best_mode(&modes(), &spec, "eDP-1", true) {
                Err(DisplayError::ModeNotFound { display, .. }) => assert_eq!(display, "eDP-1"),
                other => panic!("expected ModeNotFound, got {:?}", other),
            }
        }

        match select_for_rate(&[], 60, "eDP-1") {
            Err(DisplayError::ModeNotFound { spec, .. }) => assert_eq!(spec, "60Hz"),
            other => panic!("expected ModeNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_select_best_mode_accepts_hz_suffix() {
        let modes = vec![mode(2560, 1600, 240.0, true), mode(2560, 1600, 60.0, false)];

        for spec in ["2560x1600@240.00Hz", "2560x1600@240", "2560x1600@240.00"] {
            let best =
                select_best_mode(&modes, &ModeSpec::ModeString(spec.to_string()), "eDP-1", false)
                    .unwrap_or_else(|e| panic!("spec '{}' must resolve: {}", spec, e));

            assert_eq!(best.refresh_rate, 240.0, "spec '{}'", spec);
        }
    }
}
