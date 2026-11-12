// Copyright 2022 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use crate::{util::write_value, Profile};
use concat_in_place::strcat;
use std::{
    fmt::Write,
    fs::{self, File},
    io::Read,
};

/// Directory holding one `policyN` subdirectory per cpufreq policy.
const CPUFREQ_DIR: &str = "/sys/devices/system/cpu/cpufreq";

/// Applies the governor, EPP, frequency cap and boost for `profile`.
///
/// `max_percent` caps `scaling_max_freq` relative to the boost ceiling
/// (`amd_pstate_max_freq`, else `cpuinfo_max_freq`), never above what the
/// current boost state allows; `battery_epp` is the configured Battery EPP.
pub fn set(profile: Profile, max_percent: u8, battery_epp: &str) {
    log::info!("Setting CPU frequency for profile: {:?}", profile);

    let policies = policy_dirs();
    let Some(first) = policies.first() else {
        log::error!("  No cpufreq policies found - cannot configure CPU frequency");
        return;
    };

    let mut core = Cpu::from_dir(first);
    let Some(driver) = core.scaling_driver().map(str::to_owned) else {
        log::error!("  Failed to detect CPU scaling driver - cannot configure CPU frequency");
        return;
    };
    log::info!("  Detected CPU scaling driver: {}", driver);

    // Decide the scaling governor and EPP preference for this profile.
    let governor = governor_for(profile, &driver);
    let mut epp = epp_for(profile, &driver, battery_epp);

    // The battery EPP is user-configurable; fall back if the CPU does not
    // advertise it.
    if matches!(profile, Profile::Battery) {
        if let Some(ref preference) = epp {
            if !epp_available(preference) {
                log::warn!(
                    "  EPP preference '{}' is not available; falling back to 'balance_power'",
                    preference
                );
                epp = Some("balance_power".to_string());
            }
        }
    }

    log::info!("  Selected governor: {}", governor);
    if let Some(ref pref) = epp {
        log::info!("  EPP preference: {}", pref);
    }

    // Boost first: with boost disabled, amd-pstate lowers `cpuinfo_max_freq`
    // to the nominal frequency. Reading it before re-enabling boost would cap
    // `scaling_max_freq` at nominal and leave the CPU stuck there.
    set_boost(profile, &policies);

    let max_percent = max_percent.min(100);
    let mut configured = 0;

    // One write per policy: a policy may span several CPUs, and offline CPUs
    // have no policy directory, so nothing is written to them.
    for dir in &policies {
        core.load_dir(dir);

        let (Some(min), Some(max)) = (core.frequency_minimum(), core.frequency_maximum()) else {
            log::warn!("  {}: no cpuinfo frequency range, skipping", dir);
            continue;
        };

        // The cap is relative to the boost ceiling, which amd-pstate exposes
        // independently of the boost state; never exceed what is reachable now.
        let reference = core.amd_pstate_max_freq().unwrap_or(max);
        let capped = (reference * usize::from(max_percent) / 100).min(max);

        // Set frequency limits for all drivers, including amd-pstate variants.
        // Even though amd-pstate-epp primarily uses EPP (Energy Performance Preference)
        // hints, the scaling_max_freq parameter must still be set to enforce the
        // maximum frequency cap. Without this, the CPU can get stuck at low frequencies.
        core.set_frequency_minimum(min);
        core.set_frequency_maximum(capped);

        // The governor must be set before EPP: amd-pstate-epp rejects non-
        // `performance` preferences while the `performance` governor is active.
        core.set_governor(governor);

        if let Some(preference) = &epp {
            core.set_epp(preference);
        }

        configured += 1;
    }

    log::info!(
        "  CPU frequency configured on {} of {} policies (cap {}%)",
        configured,
        policies.len(),
        max_percent
    );
}

/// The `policyN` directories under [`CPUFREQ_DIR`], sorted by policy number.
fn policy_dirs() -> Vec<String> {
    let Ok(dir) = fs::read_dir(CPUFREQ_DIR) else {
        return Vec::new();
    };

    let mut policies: Vec<(usize, String)> = dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let number = name.to_str()?.strip_prefix("policy")?.parse::<usize>().ok()?;
            Some((number, [CPUFREQ_DIR, "/", name.to_str()?, "/"].concat()))
        })
        .collect();

    policies.sort_unstable_by_key(|(number, _)| *number);
    policies.into_iter().map(|(_, path)| path).collect()
}

/// Scaling governor for the given profile and CPU scaling driver.
#[must_use]
pub fn governor_for(profile: Profile, driver: &str) -> &'static str {
    match profile {
        // Prefer battery life over efficiency.
        Profile::Battery => match driver {
            "amd-pstate" | "amd-pstate-epp" | "intel_pstate" => "powersave",
            _ => "conservative",
        },
        // The most energy-efficient profile.
        Profile::Balanced => match driver {
            "amd-pstate" => "ondemand",
            "amd-pstate-epp" | "intel_pstate" => "powersave",
            _ => "schedutil",
        },
        // Maximum performance.
        Profile::Performance => "performance",
    }
}

/// EPP (`energy_performance_preference`) for the given profile and driver, if
/// the driver supports it. The battery preference is user-configurable.
#[must_use]
pub fn epp_for(profile: Profile, driver: &str, battery_epp: &str) -> Option<String> {
    if driver != "amd-pstate-epp" {
        return None;
    }

    Some(match profile {
        Profile::Battery => battery_epp.to_owned(),
        Profile::Balanced => "balance_performance".to_string(),
        Profile::Performance => "performance".to_string(),
    })
}

/// Whether the CPU advertises `preference` in its available EPP list.
fn epp_available(preference: &str) -> bool {
    const PATH: &str =
        "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_available_preferences";

    match fs::read_to_string(PATH) {
        Ok(available) => available.split_ascii_whitespace().any(|entry| entry == preference),
        // Unknown; let the write attempt decide.
        Err(_) => true,
    }
}

/// Controls CPU boost/turbo for AMD and generic cpufreq drivers.
/// This complements Intel PState's no_turbo parameter handled in daemon/profiles.rs
/// and provides boost control for AMD Ryzen CPUs via the generic cpufreq boost interface.
fn set_boost(profile: Profile, policies: &[String]) {
    let boost_value = match profile {
        Profile::Battery => "0", // Disable boost on battery to save power
        Profile::Balanced | Profile::Performance => "1", // Enable boost
    };

    // Try global boost path first (standard location)
    let global_boost_path = "/sys/devices/system/cpu/cpufreq/boost";
    if std::path::Path::new(global_boost_path).exists() {
        match fs::write(global_boost_path, boost_value) {
            Ok(()) => {
                log::info!("CPU boost set to {} for {:?} profile", boost_value, profile);
                return;
            }
            Err(e) => log::warn!("Failed to set CPU boost at {}: {}", global_boost_path, e),
        }
    }

    // Fallback: per-policy boost (some drivers only expose it there).
    let mut written = 0;
    for policy in policies {
        let path = [policy.as_str(), "boost"].concat();
        if !std::path::Path::new(&path).exists() {
            continue;
        }

        match fs::write(&path, boost_value) {
            Ok(()) => written += 1,
            Err(e) => log::warn!("Failed to set CPU boost at {}: {}", path, e),
        }
    }

    if written > 0 {
        log::info!(
            "CPU boost set to {} for {:?} profile on {} policies",
            boost_value,
            profile,
            written
        );
    } else {
        log::debug!("CPU boost control not available (neither global nor per-policy path found)");
    }
}

pub struct Cpu {
    /// Stores the path of the file being accessed.
    path: String,
    /// Know where to truncate the path.
    path_len: usize,
    /// Scratch space for read files
    read_buffer: Vec<u8>,
}

impl Cpu {
    #[must_use]
    pub fn new(core: usize) -> Self {
        let mut path = String::with_capacity(38);
        cpu_path(&mut path, core);

        Self { path_len: path.len(), path, read_buffer: Vec::with_capacity(16) }
    }

    pub fn load(&mut self, core: usize) {
        self.path.clear();
        cpu_path(&mut self.path, core);
        self.path_len = self.path.len();
    }

    /// Accesses the cpufreq directory `dir` (which must end in `/`), e.g. a
    /// `/sys/devices/system/cpu/cpufreq/policyN/` directory.
    #[must_use]
    pub fn from_dir(dir: &str) -> Self {
        Self { path_len: dir.len(), path: dir.to_owned(), read_buffer: Vec::with_capacity(16) }
    }

    pub fn load_dir(&mut self, dir: &str) {
        self.path.clear();
        self.path.push_str(dir);
        self.path_len = self.path.len();
    }

    #[must_use]
    pub fn frequency_maximum(&mut self) -> Option<usize> {
        self.get_value("cpuinfo_max_freq").and_then(|value| value.parse::<usize>().ok())
    }

    /// amd-pstate's highest (boost) frequency, independent of the boost state.
    #[must_use]
    pub fn amd_pstate_max_freq(&mut self) -> Option<usize> {
        self.get_value("amd_pstate_max_freq").and_then(|value| value.parse::<usize>().ok())
    }

    #[must_use]
    pub fn frequency_minimum(&mut self) -> Option<usize> {
        self.get_value("cpuinfo_min_freq").and_then(|value| value.parse::<usize>().ok())
    }

    #[must_use]
    pub fn scaling_driver(&mut self) -> Option<&str> {
        self.get_value("scaling_driver")
    }

    pub fn set_epp(&mut self, preference: &str) {
        log::debug!("  Setting EPP to '{}' on {}", preference, &self.path[..self.path_len]);
        self.set_value("energy_performance_preference", preference);
    }

    pub fn set_frequency_maximum(&mut self, frequency: usize) {
        log::debug!(
            "  Setting max frequency to {} kHz on {}",
            frequency,
            &self.path[..self.path_len]
        );
        self.set_value("scaling_max_freq", frequency);
    }

    pub fn set_frequency_minimum(&mut self, frequency: usize) {
        log::debug!(
            "  Setting min frequency to {} kHz on {}",
            frequency,
            &self.path[..self.path_len]
        );
        self.set_value("scaling_min_freq", frequency);
    }

    pub fn set_governor(&mut self, governor: &str) {
        log::debug!("  Setting governor to '{}' on {}", governor, &self.path[..self.path_len]);
        self.set_value("scaling_governor", governor);
    }

    fn set_value<V: std::fmt::Display>(&mut self, file: &str, value: V) {
        self.path.truncate(self.path_len);
        write_value(strcat!(&mut self.path, file), value);
    }

    fn get_value(&mut self, file: &str) -> Option<&str> {
        self.path.truncate(self.path_len);
        let mut file = match File::open(strcat!(&mut self.path, file)) {
            Ok(file) => file,
            Err(_) => return None,
        };

        self.read_buffer.clear();
        let _res = file.read_to_end(&mut self.read_buffer);

        std::str::from_utf8(&self.read_buffer).ok().map(str::trim)
    }
}

fn cpu_path(buffer: &mut String, core: usize) {
    let _ = write!(buffer, "/sys/devices/system/cpu/cpu{}/cpufreq/", core);
}
