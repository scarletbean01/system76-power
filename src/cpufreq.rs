// Copyright 2022 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use crate::{util::write_value, Profile};
use concat_in_place::strcat;
use std::{
    fmt::Write,
    fs::{self, File},
    io::Read,
};

pub fn set(profile: Profile, max_percent: u8) {
    let mut core = Cpu::new(0);

    let min_freq = core.frequency_minimum();
    let max_freq = core.frequency_maximum();

    log::info!("Setting CPU frequency for profile: {:?}", profile);
    log::info!("  CPU frequency range: min={:?} kHz, max={:?} kHz", min_freq, max_freq);

    if let Some(driver) = core.scaling_driver() {
        log::info!("  Detected CPU scaling driver: {}", driver);

        // Decide the scaling governor and EPP preference for this profile.
        let governor = governor_for(profile, driver);
        let mut epp = epp_for(profile, driver);

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

        if let Some((cpus, (min, max))) = num_cpus().zip(min_freq.zip(max_freq)) {
            let max = max * max_percent.min(100) as usize / 100;
            log::info!(
                "  Applying frequency limits: min={} kHz, max={} kHz ({}% of {} kHz)",
                min,
                max,
                max_percent,
                max_freq.unwrap_or(0)
            );
            log::info!("  Total CPUs to configure: {}", cpus + 1);

            for cpu in 0..=cpus {
                core.load(cpu);

                // Set frequency limits for all drivers, including amd-pstate variants.
                // Even though amd-pstate-epp primarily uses EPP (Energy Performance Preference)
                // hints, the scaling_max_freq parameter must still be set to enforce the
                // maximum frequency cap. Without this, the CPU can get stuck at low frequencies.
                core.set_frequency_minimum(min);
                core.set_frequency_maximum(max);

                core.set_governor(governor);

                if let Some(preference) = &epp {
                    core.set_epp(preference);
                }
            }

            log::info!("  CPU frequency configuration completed for {} cores", cpus + 1);
        } else {
            log::warn!("  Failed to get CPU count or frequency range - skipping CPU configuration");
        }

        // Set CPU boost for AMD/generic cpufreq (complements Intel PState no_turbo)
        set_boost(profile);
    } else {
        log::error!("  Failed to detect CPU scaling driver - cannot configure CPU frequency");
    }
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
pub fn epp_for(profile: Profile, driver: &str) -> Option<String> {
    if driver != "amd-pstate-epp" {
        return None;
    }

    Some(match profile {
        Profile::Battery => crate::config::current().cpu.battery_epp.clone(),
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
fn set_boost(profile: Profile) {
    let boost_value = match profile {
        Profile::Battery => b"0", // Disable boost on battery to save power
        Profile::Balanced | Profile::Performance => b"1", // Enable boost
    };

    // Try global boost path first (standard location)
    let global_boost_path = "/sys/devices/system/cpu/cpufreq/boost";
    if std::path::Path::new(global_boost_path).exists() {
        match fs::write(global_boost_path, boost_value) {
            Ok(()) => {
                log::info!(
                    "CPU boost set to {} for {:?} profile",
                    std::str::from_utf8(boost_value).unwrap_or("?"),
                    profile
                );
                return;
            }
            Err(e) => log::warn!("Failed to set CPU boost at {}: {}", global_boost_path, e),
        }
    }

    // Fallback: try per-CPU boost path (some systems use this)
    let per_cpu_boost_path = "/sys/devices/system/cpu/cpu0/cpufreq/boost";
    if std::path::Path::new(per_cpu_boost_path).exists() {
        match fs::write(per_cpu_boost_path, boost_value) {
            Ok(()) => log::info!(
                "CPU boost set to {} for {:?} profile (via per-CPU path)",
                std::str::from_utf8(boost_value).unwrap_or("?"),
                profile
            ),
            Err(e) => log::warn!("Failed to set CPU boost at {}: {}", per_cpu_boost_path, e),
        }
    } else {
        log::debug!("CPU boost control not available (neither global nor per-CPU path found)");
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

    #[must_use]
    pub fn frequency_maximum(&mut self) -> Option<usize> {
        self.get_value("cpuinfo_max_freq").and_then(|value| value.parse::<usize>().ok())
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

#[must_use]
pub fn num_cpus() -> Option<usize> {
    let info = fs::read_to_string("/sys/devices/system/cpu/possible").ok()?;
    info.split('-').nth(1)?.trim_end().parse::<usize>().ok()
}

fn cpu_path(buffer: &mut String, core: usize) {
    let _ = write!(buffer, "/sys/devices/system/cpu/cpu{}/cpufreq/", core);
}
