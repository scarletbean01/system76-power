// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Wi-Fi power-save control.
//!
//! Sets the per-interface power-save state through `iw`, which works with any
//! cfg80211/mac80211 driver (including the MT7922) instead of reloading a
//! specific vendor module.

use std::{fs, process::Command};

/// Enables or disables 802.11 power save on every wireless interface.
pub fn set_power_save(on: bool) {
    let value = if on { "on" } else { "off" };
    let interfaces = wireless_interfaces();

    if interfaces.is_empty() {
        log::debug!("no wireless interfaces found for power-save control");
    }

    for interface in interfaces {
        match Command::new("iw")
            .args(["dev", &interface, "set", "power_save", value])
            .output()
        {
            Ok(output) if output.status.success() => {
                log::info!("iw dev {}: power_save {}", interface, value);
            }
            Ok(output) => log::warn!(
                "iw dev {} set power_save {} failed: {}",
                interface,
                value,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(why) => log::warn!("failed to run iw for {}: {}", interface, why),
        }
    }
}

/// Names of `/sys/class/net/*` entries that are wireless (`phy80211` present).
fn wireless_interfaces() -> Vec<String> {
    let Ok(dir) = fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };

    dir.filter_map(Result::ok)
        .filter(|entry| entry.path().join("phy80211").exists())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}
