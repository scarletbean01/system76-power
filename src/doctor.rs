// Copyright 2026 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! `system76-power doctor` — read-only audit of the configured power knobs.
//!
//! For every knob the profile code writes, `doctor` reads the kernel's current
//! state back and prints `OK` (in effect), `DRIFT` (configured but not in
//! effect) or `SKIP` (not auditable on this system) per knob. It never writes,
//! never needs root and never talks to the daemon, so it works with the daemon
//! stopped. [`run`] returns `true` when no check drifted.

use crate::{
    Profile, acpi_platform, config, cpufreq,
    power_supply::{PowerSource, get_power_source},
};
use std::{fs, io::ErrorKind, process::Command};

const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const CPU_BOOST: &str = "/sys/devices/system/cpu/cpufreq/boost";
const SCALING_DRIVER: &str = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver";
const SCALING_GOVERNOR: &str = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor";
const ENERGY_PREFERENCE: &str = "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference";
const AVAILABLE_PREFERENCES: &str =
    "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_available_preferences";
const LAPTOP_MODE: &str = "/proc/sys/vm/laptop_mode";
const DIRTY_WRITEBACK: &str = "/proc/sys/vm/dirty_writeback_centisecs";
const PCIE_ASPM_POLICY: &str = "/sys/module/pcie_aspm/parameters/policy";
const HDA_POWER_SAVE: &str = "/sys/module/snd_hda_intel/parameters/power_save";
const NET_PATH: &str = "/sys/class/net";
const RFKILL_PATH: &str = "/sys/class/rfkill";

/// Audit every configured power knob and print the report to stdout.
///
/// Returns `Ok(true)` when no check drifted, `Ok(false)` when at least one
/// knob drifted. Unreadable knobs and unsupported mechanisms are `SKIP`, never
/// errors.
pub fn run() -> anyhow::Result<bool> {
    // The CLI process starts from config defaults; read the on-disk
    // configuration that the daemon uses before auditing anything.
    let config = config::reload();

    let (profile, origin) = active_profile(&config);
    let name = profile.map_or_else(|| "unknown".to_owned(), |profile| format!("{:?}", profile));
    println!("Active profile: {} (from {})", name, origin);

    let mut tally = Tally::default();
    check_acpi_platform_profile(&mut tally, profile);
    check_boost(&mut tally, profile);
    check_governor(&mut tally, profile);
    check_epp(&mut tally, profile);
    check_laptop_mode(&mut tally, profile);
    check_dirty_writeback(&mut tally, profile);
    check_pcie_aspm(&mut tally, profile);
    check_hda_power_save(&mut tally, profile, &config);
    check_wifi_power_save(&mut tally, profile, &config);
    check_bluetooth_rfkill(&mut tally, profile, &config);
    check_smu_limits(&mut tally);

    println!("{} OK, {} DRIFT, {} SKIP", tally.ok, tally.drift, tally.skip);
    Ok(tally.drift == 0)
}

// ── Active profile determination ─────────────────────────────────────────────

/// The profile currently in effect, plus a description of how it was
/// determined.
fn active_profile(config: &config::Config) -> (Option<Profile>, String) {
    if acpi_platform::supported() {
        return match fs::read_to_string(PLATFORM_PROFILE) {
            Ok(value) => {
                let value = value.trim().to_owned();
                let profile = profile_from_platform_profile(&value);
                (profile, format!("{}={}", PLATFORM_PROFILE, value))
            }
            Err(why) => (None, format!("{} unreadable: {}", PLATFORM_PROFILE, why)),
        };
    }

    if config.auto_switch {
        match get_power_source() {
            Some(PowerSource::Battery) => {
                (Some(Profile::Battery), "power supply status on battery".to_owned())
            }
            Some(PowerSource::AC) => {
                (Some(Profile::Balanced), "power supply status on AC".to_owned())
            }
            None => (None, "power supply status undetermined".to_owned()),
        }
    } else {
        (None, "no platform profile and auto-switch disabled".to_owned())
    }
}

/// Map a `/sys/firmware/acpi/platform_profile` value to a [`Profile`].
fn profile_from_platform_profile(value: &str) -> Option<Profile> {
    match value {
        "low-power" | "quiet" => Some(Profile::Battery),
        "balanced" => Some(Profile::Balanced),
        "performance" => Some(Profile::Performance),
        _ => None,
    }
}

// ── Per-profile expectations (mirror the profile code) ───────────────────────

/// `/sys/devices/system/cpu/cpufreq/boost` value (`cpufreq::set_boost`).
fn expected_boost(profile: Profile) -> &'static str {
    match profile {
        Profile::Battery => "0",
        Profile::Balanced | Profile::Performance => "1",
    }
}

/// `/proc/sys/vm/laptop_mode` value (`daemon::profiles`).
fn expected_laptop_mode(profile: Profile) -> &'static str {
    match profile {
        Profile::Battery | Profile::Balanced => "2",
        Profile::Performance => "0",
    }
}

/// `/proc/sys/vm/dirty_writeback_centisecs` value
/// (`Dirty::set_max_lost_work`: 30s/15s/10s → 3000/1500/1000).
fn expected_dirty_writeback(profile: Profile) -> &'static str {
    match profile {
        Profile::Battery => "3000",
        Profile::Balanced => "1500",
        Profile::Performance => "1000",
    }
}

/// Active PCIe ASPM policy (`PcieAspm`).
fn expected_aspm(profile: Profile) -> &'static str {
    match profile {
        Profile::Battery => "powersupersave",
        Profile::Balanced | Profile::Performance => "default",
    }
}

// ── Parsing helpers ──────────────────────────────────────────────────────────

/// Extract the bracketed active policy from
/// `/sys/module/pcie_aspm/parameters/policy` (e.g. `default [powersupersave]`).
fn parse_active_policy(content: &str) -> Option<&str> {
    content.split_ascii_whitespace().find_map(|token| token.strip_prefix('[')?.strip_suffix(']'))
}

/// Expected `soft` rfkill state for Bluetooth, or `None` when the profile code
/// leaves Bluetooth alone (`daemon::profiles`: block on battery only).
fn bluetooth_expectation(profile: Profile) -> Option<&'static str> {
    matches!(profile, Profile::Battery).then_some("1")
}

/// Whether the CPU advertises `preference` in its available EPP list
/// (mirrors `cpufreq::epp_available`: unknown counts as available).
fn epp_available(preference: &str) -> bool {
    match fs::read_to_string(AVAILABLE_PREFERENCES) {
        Ok(available) => available.split_ascii_whitespace().any(|entry| entry == preference),
        Err(_) => true,
    }
}

/// Interface names in `/sys/class/net` that are wireless.
fn wireless_interfaces() -> Vec<String> {
    let Ok(dir) = fs::read_dir(NET_PATH) else {
        return Vec::new();
    };

    dir.filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            path.join("phy80211").exists() || path.join("wireless").exists()
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Read a sysfs/procfs knob and trim it; the error carries a short reason.
fn read_trimmed(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map(|value| value.trim().to_owned()).map_err(|why| {
        format!("{}: {}", path, why)
    })
}

// ── Checks ───────────────────────────────────────────────────────────────────

fn check_acpi_platform_profile(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "ACPI platform profile";

    if !acpi_platform::supported() {
        return tally.skip(LABEL, "not supported by this kernel");
    }

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    let expected: &str = match profile {
        Profile::Balanced => "balanced",
        Profile::Performance => "performance",
        Profile::Battery => {
            // Mirror `acpi_platform::battery()`: the first `low-power`/`quiet`
            // choice, falling back to the first advertised choice.
            let mut first = None;
            let mut low_power = None;
            for choice in acpi_platform::choices() {
                if first.is_none() {
                    first = Some(choice);
                }
                if matches!(choice, "low-power" | "quiet") {
                    low_power = Some(choice);
                    break;
                }
            }
            match low_power.or(first) {
                Some(choice) => choice,
                None => return tally.skip(LABEL, "platform profile choices unreadable"),
            }
        }
    };

    report_value(tally, LABEL, read_trimmed(PLATFORM_PROFILE), expected);
}

fn check_boost(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "CPU boost";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    report_value(tally, LABEL, read_trimmed(CPU_BOOST), expected_boost(profile));
}

fn check_governor(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "CPU governor";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    let driver = match read_trimmed(SCALING_DRIVER) {
        Ok(driver) => driver,
        Err(reason) => return tally.skip(LABEL, &reason),
    };

    let expected = cpufreq::governor_for(profile, &driver);
    report_value(tally, LABEL, read_trimmed(SCALING_GOVERNOR), expected);
}

fn check_epp(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "CPU energy performance preference";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    let driver = match read_trimmed(SCALING_DRIVER) {
        Ok(driver) => driver,
        Err(reason) => return tally.skip(LABEL, &reason),
    };

    let Some(mut expected) = cpufreq::epp_for(profile, &driver) else {
        return tally.skip(LABEL, "scaling driver does not support EPP");
    };

    // Mirror `cpufreq::set()`: the battery preference falls back when the CPU
    // does not advertise it.
    if matches!(profile, Profile::Battery) && !epp_available(&expected) {
        expected = "balance_power".to_owned();
    }

    report_value(tally, LABEL, read_trimmed(ENERGY_PREFERENCE), &expected);
}

fn check_laptop_mode(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "Laptop mode";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    report_value(tally, LABEL, read_trimmed(LAPTOP_MODE), expected_laptop_mode(profile));
}

fn check_dirty_writeback(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "Dirty writeback";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    report_value(
        tally,
        LABEL,
        read_trimmed(DIRTY_WRITEBACK),
        expected_dirty_writeback(profile),
    );
}

fn check_pcie_aspm(tally: &mut Tally, profile: Option<Profile>) {
    const LABEL: &str = "PCIe ASPM policy";

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    let expected = expected_aspm(profile);
    match read_trimmed(PCIE_ASPM_POLICY) {
        Err(reason) => tally.skip(LABEL, &reason),
        Ok(content) => match parse_active_policy(&content) {
            None => tally.skip(LABEL, "cannot parse active policy"),
            Some(observed) => record_match(tally, LABEL, observed, expected),
        },
    }
}

fn check_hda_power_save(tally: &mut Tally, profile: Option<Profile>, config: &config::Config) {
    const LABEL: &str = "HDA audio power save";

    if !config.audio.power_save {
        return tally.skip(LABEL, "disabled in configuration");
    }

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    // The profile code writes 1 (with the controller enabled) everywhere
    // except performance, where it writes 0.
    let expected = if matches!(profile, Profile::Performance) { "0" } else { "1" };
    report_value(tally, LABEL, read_trimmed(HDA_POWER_SAVE), expected);
}

fn check_wifi_power_save(tally: &mut Tally, profile: Option<Profile>, config: &config::Config) {
    const LABEL: &str = "Wi-Fi power save";

    if !config.wifi.power_save {
        return tally.skip(LABEL, "disabled in configuration");
    }

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    let interfaces = wireless_interfaces();
    if interfaces.is_empty() {
        return tally.skip(LABEL, "no wireless interface");
    }

    let expected = if matches!(profile, Profile::Battery) { "on" } else { "off" };
    let mut states = Vec::with_capacity(interfaces.len());
    for interface in &interfaces {
        let output = match Command::new("iw")
            .args(["dev", interface.as_str(), "get", "power_save"])
            .output()
        {
            Ok(output) => output,
            Err(why) if why.kind() == ErrorKind::NotFound => {
                return tally.skip(LABEL, "no `iw` binary");
            }
            Err(why) => return tally.skip(LABEL, &format!("failed to run `iw`: {}", why)),
        };

        if !output.status.success() {
            return tally.skip(LABEL, &format!(
                "`iw dev {} get power_save` failed: {}",
                interface,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let state = stdout.rsplit(':').next().map(str::trim).unwrap_or("");
        if !matches!(state, "on" | "off") {
            return tally.skip(LABEL, &format!("unexpected `iw` output: {}", stdout.trim()));
        }

        states.push((interface.clone(), state.to_owned()));
    }

    record_match(tally, LABEL, &aggregate(&states), expected);
}

fn check_bluetooth_rfkill(tally: &mut Tally, profile: Option<Profile>, config: &config::Config) {
    const LABEL: &str = "Bluetooth rfkill";

    if !config.radio.bluetooth_off_on_battery {
        return tally.skip(LABEL, "disabled in configuration");
    }

    let Some(profile) = profile else {
        return tally.skip(LABEL, "profile unknown");
    };

    // `apply_device_policies` only soft-blocks Bluetooth on battery; it never
    // unblocks, so on any other profile the state is whatever the user set.
    let Some(expected) = bluetooth_expectation(profile) else {
        return tally.skip(LABEL, "unmanaged outside the battery profile");
    };

    let entries = match fs::read_dir(RFKILL_PATH) {
        Ok(dir) => dir.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(_) => return tally.skip(LABEL, "rfkill sysfs not available"),
    };

    let mut found = false;
    let mut states = Vec::new();
    for entry in entries {
        let path = entry.path();
        let is_bluetooth = fs::read_to_string(path.join("type"))
            .map(|kind| kind.trim() == "bluetooth")
            .unwrap_or(false);
        if !is_bluetooth {
            continue;
        }

        found = true;
        if let Ok(state) = fs::read_to_string(path.join("soft")) {
            states.push((
                entry.file_name().to_string_lossy().into_owned(),
                state.trim().to_owned(),
            ));
        }
    }

    if !found {
        return tally.skip(LABEL, "no bluetooth rfkill device");
    }
    if states.is_empty() {
        return tally.skip(LABEL, "cannot read rfkill state");
    }

    record_match(tally, LABEL, &aggregate(&states), expected);
}

fn check_smu_limits(tally: &mut Tally) {
    tally.skip("SMU/ryzenadj limits", "requires root to read the SMU table");
}

// ── Reporting helpers ────────────────────────────────────────────────────────

/// Outcome of a single knob check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Drift,
    Skip,
}

/// Classify a knob: present and equal to `expected`, present and different, or
/// absent/unreadable.
fn classify(observed: Option<&str>, expected: &str) -> Status {
    match observed {
        Some(value) if value == expected => Status::Ok,
        Some(_) => Status::Drift,
        None => Status::Skip,
    }
}

/// Record a present observed value against `expected`.
fn record_match(tally: &mut Tally, label: &str, observed: &str, expected: &str) {
    let status = classify(Some(observed), expected);
    tally.record(status, label, format!("{} (expected {})", observed, expected));
}

/// Record a knob read: unreadable knobs SKIP with the IO reason, otherwise the
/// observed value is compared against `expected`.
fn report_value(tally: &mut Tally, label: &str, observed: Result<String, String>, expected: &str) {
    match observed {
        Ok(observed) => record_match(tally, label, &observed, expected),
        Err(reason) => tally.skip(label, &reason),
    }
}

/// A single observed value when every entry agrees, otherwise a per-entry
/// listing (`wlan0=on, wlan1=off`).
fn aggregate(states: &[(String, String)]) -> String {
    if states.iter().all(|(_, state)| *state == states[0].1) {
        states[0].1.clone()
    } else {
        states
            .iter()
            .map(|(name, state)| format!("{}={}", name, state))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Running totals plus the per-knob stdout report.
#[derive(Default)]
struct Tally {
    ok:    usize,
    drift: usize,
    skip:  usize,
}

impl Tally {
    fn record(&mut self, status: Status, label: &str, detail: String) {
        match status {
            Status::Ok => {
                self.ok += 1;
                println!("OK     {}: {}", label, detail);
            }
            Status::Drift => {
                self.drift += 1;
                println!("DRIFT  {}: {}", label, detail);
            }
            Status::Skip => {
                self.skip += 1;
                println!("SKIP   {}: {}", label, detail);
            }
        }
    }

    fn skip(&mut self, label: &str, reason: &str) {
        self.record(Status::Skip, label, reason.to_owned());
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_profile_maps_to_profile() {
        assert!(matches!(
            profile_from_platform_profile("low-power"),
            Some(Profile::Battery)
        ));
        assert!(matches!(profile_from_platform_profile("quiet"), Some(Profile::Battery)));
        assert!(matches!(
            profile_from_platform_profile("balanced"),
            Some(Profile::Balanced)
        ));
        assert!(matches!(
            profile_from_platform_profile("performance"),
            Some(Profile::Performance)
        ));
        assert!(profile_from_platform_profile("custom").is_none());
        assert!(profile_from_platform_profile("").is_none());
    }

    #[test]
    fn boost_expectations() {
        assert_eq!(expected_boost(Profile::Battery), "0");
        assert_eq!(expected_boost(Profile::Balanced), "1");
        assert_eq!(expected_boost(Profile::Performance), "1");
    }

    #[test]
    fn laptop_mode_expectations() {
        assert_eq!(expected_laptop_mode(Profile::Battery), "2");
        assert_eq!(expected_laptop_mode(Profile::Balanced), "2");
        assert_eq!(expected_laptop_mode(Profile::Performance), "0");
    }

    #[test]
    fn dirty_writeback_expectations() {
        assert_eq!(expected_dirty_writeback(Profile::Battery), "3000");
        assert_eq!(expected_dirty_writeback(Profile::Balanced), "1500");
        assert_eq!(expected_dirty_writeback(Profile::Performance), "1000");
    }

    /// The profile code only ever soft-blocks Bluetooth on battery, so the
    /// doctor must not assert an unblocked state on AC (user may have turned
    /// Bluetooth off manually).
    #[test]
    fn bluetooth_expectation_only_battery() {
        assert_eq!(bluetooth_expectation(Profile::Battery), Some("1"));
        assert_eq!(bluetooth_expectation(Profile::Balanced), None);
        assert_eq!(bluetooth_expectation(Profile::Performance), None);
    }

    #[test]
    fn aspm_expectations() {
        assert_eq!(expected_aspm(Profile::Battery), "powersupersave");
        assert_eq!(expected_aspm(Profile::Balanced), "default");
        assert_eq!(expected_aspm(Profile::Performance), "default");
    }

    #[test]
    fn parse_active_policy_finds_bracketed_value() {
        assert_eq!(parse_active_policy("default [powersupersave]"), Some("powersupersave"));
        assert_eq!(parse_active_policy("[default] performance powersupersave"), Some("default"));
        assert_eq!(parse_active_policy("default performance powersupersave"), None);
        assert_eq!(parse_active_policy(""), None);
    }

    #[test]
    fn classify_observed_values() {
        assert_eq!(classify(Some("powersave"), "powersave"), Status::Ok);
        assert_eq!(classify(Some("1"), "0"), Status::Drift);
        assert_eq!(classify(None, "0"), Status::Skip);
    }
}
