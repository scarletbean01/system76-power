// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display modes and selection of the mode matching a request.

use std::io;

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

/// Ensure modes were found, returning an appropriate error if empty
///
/// This helper encapsulates the common "no modes found" error handling pattern
/// used by all display managers.
pub(super) fn ensure_modes_found(
    modes: &[DisplayMode],
    display_name: &str,
    tool_name: &str,
    raw_output: &str,
) -> Result<(), DisplayError> {
    if modes.is_empty() {
        log::warn!("No modes found! {} output:\n{}", tool_name, raw_output);
        return Err(DisplayError::ModeNotFound {
            display: display_name.to_string(),
            spec: "any".to_string(),
        });
    }
    Ok(())
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

    log::info!(
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
        log::info!(
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

    log::info!(
        "Keeping current resolution {}x{} ({} matching modes)",
        resolution.0,
        resolution.1,
        same_resolution.len()
    );

    same_resolution
}

/// Select the best matching display mode from available modes based on specification
///
/// This helper function encapsulates the common mode selection logic used by both
/// GNOME Wayland and X11 backends.
///
/// # Arguments
/// * `available_modes` - Slice of available DisplayMode options from the display
/// * `mode_spec` - The desired mode specification (explicit string or resolution+rate)
/// * `display_name` - Name of the display (for error messages)
/// * `prefer_vrr` - Whether to prefer VRR modes when using ResolutionAndRate
///
/// # Returns
/// The best matching DisplayMode (cloned), or an error if no suitable mode found
pub(super) fn select_best_mode(
    available_modes: &[DisplayMode],
    mode_spec: &ModeSpec,
    display_name: &str,
    prefer_vrr: bool,
) -> Result<DisplayMode, io::Error> {
    match mode_spec {
        ModeSpec::ModeString(mode_str) => {
            // Accept the "…Hz" suffix that `hyprctl monitors` prints, so specs
            // copied from it (or from `system76-power.conf` examples) still match
            // the canonical, suffix-free mode strings we store.
            let mode_str = mode_str.trim_end_matches("Hz").trim_end_matches("hz");

            // Try exact mode string match first
            if let Some(mode) = available_modes.iter().find(|m| m.mode_string == *mode_str) {
                return Ok(mode.clone());
            }

            log::warn!("Exact mode string '{}' not found, trying fuzzy match", mode_str);

            // Fuzzy match: strip VRR suffix and compare
            let mode_str_no_vrr = mode_str.replace("+vrr", "");
            if let Some(mode) = available_modes
                .iter()
                .find(|m| m.mode_string.replace("+vrr", "") == mode_str_no_vrr)
            {
                return Ok(mode.clone());
            }

            // More aggressive fuzzy match: parse resolution and rate, find closest match
            // This handles cases like "2560x1440@60.002" matching "2560x1440@60"
            log::warn!("VRR-stripped match failed, trying resolution+rate parsing");

            if let Some((res_part, rate_part)) = mode_str_no_vrr.split_once('@') {
                if let Some((width_str, height_str)) = res_part.split_once('x') {
                    if let (Ok(width), Ok(height), Ok(target_rate)) = (
                        width_str.parse::<u32>(),
                        height_str.parse::<u32>(),
                        rate_part.parse::<f32>(),
                    ) {
                        // Filter by resolution and find closest refresh rate
                        let filtered: Vec<_> = available_modes
                            .iter()
                            .filter(|m| m.resolution.0 == width && m.resolution.1 == height)
                            .collect();

                        if let Some(best) = filtered.iter().min_by_key(|m| {
                            let diff = (m.refresh_rate - target_rate).abs();
                            (diff * 1000.0) as u32
                        }) {
                            log::info!(
                                "Fuzzy matched '{}' to '{}' (diff: {:.3}Hz)",
                                mode_str,
                                best.mode_string,
                                (best.refresh_rate - target_rate).abs()
                            );
                            return Ok((*best).clone());
                        }
                    }
                }
            }

            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Mode '{}' not found on display {}", mode_str, display_name),
            ))
        }
        ModeSpec::ResolutionAndRate(width, height, hz) => {
            // Filter modes by resolution
            let filtered: Vec<&DisplayMode> = available_modes
                .iter()
                .filter(|m| m.resolution.0 == *width && m.resolution.1 == *height)
                .collect();

            if filtered.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "No modes found for resolution {}x{} on display {}",
                        width, height, display_name
                    ),
                ));
            }

            // Find best match by refresh rate (with optional VRR preference)
            find_best_mode(&filtered, *hz, prefer_vrr).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "No suitable mode found for {}x{}@{}Hz on display {}",
                        width, height, hz, display_name
                    ),
                )
            })
        }
    }
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
