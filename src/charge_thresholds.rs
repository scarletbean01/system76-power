// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs,
    path::{Path, PathBuf},
};
use system76_power_zbus::ChargeProfile;

const POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";
const START_ATTR: &str = "charge_control_start_threshold";
const END_ATTR: &str = "charge_control_end_threshold";
const IDEAPAD_DRIVER_PATH: &str = "/sys/bus/platform/drivers/ideapad_acpi";
const CONSERVATION_ATTR: &str = "conservation_mode";

const UNSUPPORTED_ERROR: &str = "This system does not support charge thresholds";
const OUT_OF_RANGE_ERROR: &str = "Charge threshold out of range: should be 0-100";
const ORDER_ERROR: &str = "Charge end threshold must be strictly greater than start";

/// Threshold pair reported for conservation mode `1` (Lenovo: charge to ~60%).
const CONSERVATION_ON: (u8, u8) = (50, 60);
/// Threshold pair reported for conservation mode `0` (charge to full).
const CONSERVATION_OFF: (u8, u8) = (90, 100);
/// Highest end threshold that still maps to conservation mode being enabled.
const CONSERVATION_ON_MAX_END: u8 = 70;

/// How the platform enforces charge limits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mechanism {
    /// `charge_control_start/end_threshold` sysfs attributes. The start
    /// attribute is optional (some firmware only limits the end threshold).
    Thresholds {
        start: Option<PathBuf>,
        end:   PathBuf,
    },
    /// Lenovo `conservation_mode`: a single flag, `0` = full charge, `1` = ~60%.
    Conservation(PathBuf),
    /// No supported interface found.
    None,
}

/// Finds the generic charge threshold attributes on a battery.
///
/// Batteries are visited in name order so `BAT0` wins over `BAT1` on systems
/// with more than one pack.
fn find_thresholds() -> Option<(Option<PathBuf>, PathBuf)> {
    let dir = fs::read_dir(POWER_SUPPLY_PATH).ok()?;
    let mut batteries: Vec<PathBuf> = dir
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("BAT"))
        .map(|entry| entry.path())
        .collect();
    batteries.sort();

    for battery in batteries {
        let end = battery.join(END_ATTR);
        if !end.exists() {
            continue;
        }

        let start = battery.join(START_ATTR);
        let start = start.exists().then_some(start);

        return Some((start, end));
    }

    None
}

/// Finds a `conservation_mode` attribute exported by the ideapad driver.
fn find_conservation_mode() -> Option<PathBuf> {
    for entry in fs::read_dir(IDEAPAD_DRIVER_PATH).ok()?.flatten() {
        let path = entry.path().join(CONSERVATION_ATTR);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

fn detect() -> Mechanism {
    if let Some((start, end)) = find_thresholds() {
        return Mechanism::Thresholds { start, end };
    }

    if let Some(path) = find_conservation_mode() {
        return Mechanism::Conservation(path);
    }

    Mechanism::None
}

/// Thresholds reported for a raw `conservation_mode` value.
fn conservation_to_thresholds(value: &str) -> (u8, u8) {
    if value.trim() == "1" {
        CONSERVATION_ON
    } else {
        CONSERVATION_OFF
    }
}

/// `conservation_mode` value to write for a requested end threshold.
fn thresholds_to_conservation(end: u8) -> &'static str {
    if end <= CONSERVATION_ON_MAX_END {
        "1"
    } else {
        "0"
    }
}

#[must_use]
pub fn get_charge_profiles() -> Vec<ChargeProfile> {
    vec![
        ChargeProfile {
            id:          "full_charge".to_string(),
            title:       "Full Charge".to_string(),
            description: "Battery is charged to its full capacity for the longest possible use on \
                          battery power. Charging resumes when the battery falls below 96% charge."
                .to_string(),
            start:       90,
            end:         100,
        },
        ChargeProfile {
            id:          "balanced".to_string(),
            title:       "Balanced".to_string(),
            description: "Use this threshold when you unplug frequently but don't need the full \
                          battery capacity. Charging stops when the battery reaches 90% capacity \
                          and resumes when the battery falls below 85%."
                .to_string(),
            start:       86,
            end:         90,
        },
        ChargeProfile {
            id:          "max_lifespan".to_string(),
            title:       "Maximum Lifespan".to_string(),
            description: "Use this threshold if you rarely use the system on battery for extended \
                          periods. Charging stops when the battery reaches 60% capacity and \
                          resumes when the battery falls below 50%."
                .to_string(),
            start:       50,
            end:         60,
        },
    ]
}

fn read_threshold(path: &Path) -> anyhow::Result<u8> {
    let value = fs::read_to_string(path)?;

    Ok(value.trim().parse::<u8>()?)
}

pub(crate) fn get_charge_thresholds() -> anyhow::Result<(u8, u8)> {
    match detect() {
        Mechanism::Thresholds { start, end } => {
            let end = read_threshold(&end)?;
            let start = match start {
                Some(path) => read_threshold(&path)?,
                // Without a start attribute charging resumes as soon as the
                // battery drops below the end threshold.
                None => 0,
            };

            Ok((start, end))
        }

        Mechanism::Conservation(path) => Ok(conservation_to_thresholds(&fs::read_to_string(path)?)),

        Mechanism::None => Err(anyhow::anyhow!(UNSUPPORTED_ERROR)),
    }
}

pub(crate) fn set_charge_thresholds((start, end): (u8, u8)) -> anyhow::Result<()> {
    if start > 100 || end > 100 {
        return Err(anyhow::anyhow!(OUT_OF_RANGE_ERROR));
    } else if end <= start {
        return Err(anyhow::anyhow!(ORDER_ERROR));
    }

    match detect() {
        Mechanism::Thresholds { start: start_path, end: end_path } => {
            // The kernel rejects a start threshold above the current end
            // threshold, so raise the end first when a start attribute exists.
            if let Some(path) = start_path {
                fs::write(&end_path, "100")?;
                fs::write(path, start.to_string())?;
            }

            fs::write(&end_path, end.to_string())?;

            Ok(())
        }

        Mechanism::Conservation(path) => {
            fs::write(path, thresholds_to_conservation(end))?;

            Ok(())
        }

        Mechanism::None => Err(anyhow::anyhow!(UNSUPPORTED_ERROR)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservation_mode_maps_to_threshold_pair() {
        assert_eq!(conservation_to_thresholds("1"), (50, 60));
        // sysfs values carry a trailing newline
        assert_eq!(conservation_to_thresholds("1\n"), (50, 60));
        assert_eq!(conservation_to_thresholds("0"), (90, 100));
        assert_eq!(conservation_to_thresholds("0\n"), (90, 100));
    }

    #[test]
    fn end_threshold_maps_to_conservation_mode() {
        assert_eq!(thresholds_to_conservation(50), "1");
        assert_eq!(thresholds_to_conservation(60), "1");
        assert_eq!(thresholds_to_conservation(CONSERVATION_ON_MAX_END), "1");
        assert_eq!(thresholds_to_conservation(CONSERVATION_ON_MAX_END + 1), "0");
        assert_eq!(thresholds_to_conservation(90), "0");
        assert_eq!(thresholds_to_conservation(100), "0");
    }

    #[test]
    fn shipping_profiles_round_trip_to_expected_modes() {
        for profile in get_charge_profiles() {
            let expected = if profile.end <= CONSERVATION_ON_MAX_END { "1" } else { "0" };
            assert_eq!(thresholds_to_conservation(profile.end), expected, "{}", profile.id);
        }
    }
}

