// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Applies the Battery / Balanced / Performance power profiles.
//!
//! Every profile runs the same pipeline ([`apply`]); what differs between them
//! is the data in [`ProfileSettings`] (fixed per profile) and the user's
//! [`Config`] (SMU limits, device policies, blacklists).

use crate::{
    config::{Config, RyzenLimits, UsbConfig},
    errors::{BacklightError, ModelError, ProfileError, RyzenadjError, ScsiHostError},
    kernel_parameters::{DeviceList, Dirty, KernelParameter, PcieAspm},
    radeon::RadeonDevice,
    snd::SoundDevice,
    sys_devices, Profile,
};
use intel_pstate::{PState, PStateError, PStateValues};
use std::{
    fs,
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};
use sysfs_class::{Backlight, Brightness, Leds, RuntimePowerManagement, ScsiHost, SysClass};

/// Instead of returning on the first error, we want to collect all errors that occur while
/// setting a profile. Even if one parameter fails to set, we'll still be able to set other
/// parameters successfully.
macro_rules! catch {
    ($errors:ident, $result:expr) => {
        if let Err(why) = $result {
            $errors.push(why.into());
        }
    };
}

// ── Per-profile settings ─────────────────────────────────────────────────────

/// How a backlight value is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklightAction {
    /// Dim to this percentage if the current level is brighter; never raise.
    DimTo(u64),
    /// Set to exactly this raw brightness value.
    Set(u64),
}

/// Fixed, per-profile tuning knobs. User-configurable values live in [`Config`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileSettings {
    /// Maximum seconds of dirty data before writeback (`vm.dirty_*_centisecs`).
    pub dirty_secs:          u32,
    /// Radeon/amdgpu `(power_profile, power_dpm_state, power_dpm_force_performance_level)`.
    pub radeon:              (&'static str, &'static str, &'static str),
    /// SATA link power-management policies, tried in order.
    pub scsi_policies:       &'static [&'static str],
    /// cpufreq `scaling_max_freq` cap, as a percentage of `cpuinfo_max_freq`.
    pub cpu_max_percent:     u8,
    /// Intel P-State `max_perf_pct`.
    pub pstate_max_perf_pct: u8,
    /// Turbo/boost allowed (Intel `no_turbo` inverse, HWP dynamic boost).
    pub turbo:               bool,
    /// `pcie_aspm` module policy.
    pub pcie_aspm:           &'static str,
    /// Runtime PM for PCI and I2C devices.
    pub runtime_pm:          RuntimePowerManagement,
    /// Screen and keyboard backlight actions, applied only when dimming is requested.
    pub backlight:           Option<(BacklightAction, BacklightAction)>,
}

/// The settings table for `profile`.
#[must_use]
pub const fn settings(profile: Profile) -> ProfileSettings {
    match profile {
        Profile::Battery => ProfileSettings {
            dirty_secs:          30,
            radeon:              ("low", "battery", "low"),
            scsi_policies:       &["min_power", "min_power"],
            // ~60% keeps a Ryzen responsive without boost.
            cpu_max_percent:     60,
            // Matches the cpufreq cap; with intel_pstate the stricter of the
            // two would otherwise win silently.
            pstate_max_perf_pct: 60,
            turbo:               false,
            pcie_aspm:           "powersupersave",
            runtime_pm:          RuntimePowerManagement::On,
            backlight:           Some((BacklightAction::DimTo(10), BacklightAction::Set(0))),
        },
        Profile::Balanced => ProfileSettings {
            dirty_secs:          15,
            radeon:              ("auto", "performance", "auto"),
            scsi_policies:       &["med_power_with_dipm", "medium_power"],
            cpu_max_percent:     100,
            pstate_max_perf_pct: 100,
            turbo:               true,
            pcie_aspm:           "default",
            runtime_pm:          RuntimePowerManagement::On,
            backlight:           Some((BacklightAction::DimTo(40), BacklightAction::DimTo(50))),
        },
        Profile::Performance => ProfileSettings {
            dirty_secs:          10,
            radeon:              ("high", "performance", "auto"),
            scsi_policies:       &["med_power_with_dipm", "max_performance"],
            cpu_max_percent:     100,
            pstate_max_perf_pct: 100,
            turbo:               true,
            pcie_aspm:           "default",
            runtime_pm:          RuntimePowerManagement::Off,
            backlight:           None,
        },
    }
}

// ── Profile application ──────────────────────────────────────────────────────

/// Applies `profile`, returning every error encountered. A failing step never
/// prevents the remaining steps from running.
///
/// This does blocking sysfs I/O and spawns helper processes; call it from a
/// blocking context (e.g. `tokio::task::spawn_blocking`).
#[must_use]
pub fn apply(profile: Profile, config: &Config, set_brightness: bool) -> Vec<ProfileError> {
    log::info!("=== Applying {} profile ===", profile.name());

    let settings = settings(profile);
    let mut errors: Vec<ProfileError> = Vec::new();

    // Use the ACPI Platform Profile if the hardware is supported by the kernel.
    if crate::acpi_platform::supported() {
        log::info!("Setting ACPI platform profile for {}", profile.name());
        match profile {
            Profile::Battery => crate::acpi_platform::battery(),
            Profile::Balanced => crate::acpi_platform::balanced(),
            Profile::Performance => crate::acpi_platform::performance(),
        }
    }

    // How often dirty pages are written back: longer intervals save power at
    // the risk of losing more data on sudden power loss.
    log::debug!("Setting dirty writeback to {} seconds", settings.dirty_secs);
    Dirty::default().set_max_lost_work(settings.dirty_secs);

    // Radeon/amdgpu power profiles. Probe once: each probe stats several files.
    let gpus: Vec<RadeonDevice> = RadeonDevice::get_devices().collect();
    log::info!("Found {} AMD GPU device(s)", gpus.len());
    let (power_profile, dpm_state, dpm_perf) = settings.radeon;
    for gpu in &gpus {
        gpu.set_profiles(power_profile, dpm_state, dpm_perf);
    }

    // SCSI / SATA link power management.
    for error in scsi_host_link_time_pm_policy(settings.scsi_policies) {
        errors.push(error.into());
    }

    if set_brightness {
        if let Some((screen, keyboard)) = settings.backlight {
            for error in iterate_backlights(Backlight::iter(), screen) {
                errors.push(error.into());
            }
            for error in iterate_backlights(Leds::iter_keyboards(), keyboard) {
                errors.push(error.into());
            }
        }
    }

    // CPU frequency scaling: governor, EPP, frequency cap and boost.
    crate::cpufreq::set(profile, settings.cpu_max_percent, &config.cpu.battery_epp);

    // Intel P-State values, if they exist.
    catch!(
        errors,
        pstate_values(
            PStateValues::default()
                .hwp_dynamic_boost(settings.turbo)
                .min_perf_pct(0)
                .max_perf_pct(settings.pstate_max_perf_pct)
                .no_turbo(!settings.turbo)
        )
    );

    // PCI device runtime PM (may cause issues on certain systems, hence opt-out).
    if config.pci.runtime_pm {
        pci_device_runtime_pm(settings.runtime_pm, config);
    }

    PcieAspm.set(settings.pcie_aspm.as_bytes());

    // I2C client runtime PM (touchpad, sensors): off for lowest latency on
    // Performance, on otherwise.
    for device in sys_devices::i2c::devices() {
        device.set_runtime_pm(settings.runtime_pm);
    }

    // Model-specific Intel PL1/PL2/TCC limits.
    if let Some(model_profiles) = MODEL_PROFILES.as_ref() {
        catch!(errors, model_profiles.get(profile).set());
    }

    // AMD SMU limits (sustained / boost / Tctl) from the config.
    if let Some(ryzenadj) = RYZENADJ.as_deref() {
        catch!(errors, set_ryzen_limits(ryzenadj, config.cpu.ryzen_limits(profile)));
    }

    // amdgpu panel power savings (ABM): dims the backlight while boosting
    // contrast. Battery only; other profiles favour colour accuracy.
    sys_devices::drm::set_panel_power_savings(match profile {
        Profile::Battery => config.profile.battery_panel_power_savings,
        Profile::Balanced | Profile::Performance => 0,
    });

    apply_device_policies(profile, config);

    // Model runtime-PM quirks go last so the generic runtime PM above cannot
    // undo them.
    let (vendor, model) = &*DMI;
    if let Err(why) = crate::runtime_pm::runtime_pm_quirks(vendor, model) {
        log::warn!("Failed to apply runtime power management quirks: {}", why);
    }

    if errors.is_empty() {
        log::info!("=== {} profile applied successfully ===", profile.name());
    } else {
        log::warn!("=== {} profile applied with {} error(s) ===", profile.name(), errors.len());
    }

    errors
}

// ── Cached system facts ──────────────────────────────────────────────────────

/// `(sys_vendor, product_version)` from DMI.
static DMI: LazyLock<(String, String)> = LazyLock::new(|| {
    let read = |file: &str| {
        fs::read_to_string(Path::new("/sys/class/dmi/id").join(file))
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    (read("sys_vendor"), read("product_version"))
});

/// Model-specific PL1/PL2/TCC profiles, if this model has any.
static MODEL_PROFILES: LazyLock<Option<ModelProfiles>> =
    LazyLock::new(|| ModelProfiles::new(&DMI.1));

/// Path to `ryzenadj`, when running on an AMD CPU with `ryzenadj` installed.
static RYZENADJ: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let is_amd = fs::read_to_string("/proc/cpuinfo").is_ok_and(|cpuinfo| {
        cpuinfo
            .lines()
            .find(|line| line.starts_with("vendor_id"))
            .is_some_and(|line| line.contains("AuthenticAMD"))
    });

    if !is_amd {
        log::debug!("not an AMD CPU; SMU limits will not be applied");
        return None;
    }

    let path = find_in_path("ryzenadj");
    match path {
        Some(ref path) => log::info!("using {} for AMD SMU limits", path.display()),
        None => log::warn!("ryzenadj not found in PATH; AMD SMU limits will not be applied"),
    }
    path
});

/// Looks `program` up in `$PATH`, falling back to the standard system dirs
/// (systemd services may run with a minimal environment).
fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin".to_owned());

    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| candidate.is_file())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Applies AMD SMU power limits through `ryzenadj`.
///
/// A failure (e.g. unsupported SMU or locked PCI config space) is returned so
/// it is reported to the caller; the rest of the profile still applies.
fn set_ryzen_limits(ryzenadj: &Path, limits: RyzenLimits) -> Result<(), RyzenadjError> {
    let output = Command::new(ryzenadj)
        .arg(format!("--stapm-limit={}", limits.stapm_mw))
        .arg(format!("--fast-limit={}", limits.fast_mw))
        .arg(format!("--slow-limit={}", limits.slow_mw))
        .arg(format!("--tctl-temp={}", limits.tctl_c))
        .output()
        .map_err(RyzenadjError::Spawn)?;

    if !output.status.success() {
        return Err(RyzenadjError::Exit {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    log::info!(
        "Set Ryzen limits: STAPM={}mW, Fast={}mW, Slow={}mW, Tctl={}°C",
        limits.stapm_mw,
        limits.fast_mw,
        limits.slow_mw,
        limits.tctl_c
    );
    Ok(())
}

/// Controls the Intel [`PState`] values.
fn pstate_values(values: PStateValues) -> Result<(), PStateError> {
    if let Ok(pstate) = PState::new() {
        pstate.set_values(values)?;
    }

    Ok(())
}

/// Applies `action` to every backlight in `iterator`, returning one error per
/// backlight that could not be set.
fn iterate_backlights<B: Brightness>(
    iterator: impl Iterator<Item = io::Result<B>>,
    action: BacklightAction,
) -> Vec<BacklightError> {
    let mut errors = Vec::new();

    for backlight in iterator {
        match backlight {
            Ok(backlight) => {
                let result = match action {
                    BacklightAction::DimTo(percent) => backlight.set_if_lower_than(percent),
                    BacklightAction::Set(value) => backlight.set_brightness(value),
                };

                if let Err(why) = result {
                    errors.push(BacklightError::Set(backlight.id().to_owned(), why));
                }
            }
            Err(why) => log::warn!("failed to iterate backlight: {}", why),
        }
    }

    errors
}

/// Iterates over all PCI devices, setting runtime power management.
///
/// - Enabling (`On`): devices bound to a blacklisted driver are left alone.
///   Driverless devices are deliberately NOT skipped: that is what lets an
///   unused discrete GPU reach D3.
/// - Disabling (`Off`): GPUs and their companion functions are left alone, so
///   an idle dGPU can still suspend and the graphics module keeps control of
///   its power state.
fn pci_device_runtime_pm(pm: RuntimePowerManagement, config: &Config) {
    for device in sys_devices::pci::devices() {
        match pm {
            RuntimePowerManagement::On => {
                if let Some(driver) = device.driver_name() {
                    if config.pci.blacklist_drivers.iter().any(|entry| entry == &driver) {
                        log::debug!("leaving blacklisted driver '{}' out of PCI runtime PM", driver);
                        continue;
                    }
                }
            }
            RuntimePowerManagement::Off => {
                if device.belongs_to_gpu() {
                    log::debug!("leaving GPU device {} out of PCI runtime PM", device.name());
                    continue;
                }
            }
        }

        device.set_runtime_pm(pm);
    }
}

/// Applies the USB, audio, Wi-Fi and Bluetooth radio policies for a profile.
fn apply_device_policies(profile: Profile, config: &Config) {
    // USB autosuspend: `auto` on battery/balanced (minus blacklists), `on` on performance.
    if config.usb.autosuspend {
        usb_runtime_pm(&config.usb, !matches!(profile, Profile::Performance));
    }

    // HDA power-save timeout.
    if config.audio.power_save {
        let enable = !matches!(profile, Profile::Performance);
        for device in SoundDevice::get_devices() {
            device.set_power_save(u32::from(enable), enable);
        }
    }

    // Wi-Fi power-save.
    if config.wifi.power_save {
        crate::wifi::set_power_save(matches!(profile, Profile::Battery));
    }

    // Bluetooth soft-block on battery only (opt-in).
    // We only *block* automatically; we never unblock, because the user may
    // have turned Bluetooth off manually and expects it to stay off.
    if config.radio.bluetooth_off_on_battery && matches!(profile, Profile::Battery) {
        for device in sys_devices::rfkill::devices() {
            if device.kind().as_deref() == Some("bluetooth") {
                device.set_soft(true);
            }
        }
    }
}

/// Sets USB runtime PM on every USB device node.
///
/// Blacklisted devices are forced to `on` rather than skipped, so a device
/// that was autosuspended earlier (by us, udev or the kernel default) is woken
/// back up.
fn usb_runtime_pm(usb: &UsbConfig, autosuspend: bool) {
    for device in sys_devices::usb::devices() {
        // Interfaces have no power/control; their parent device is handled.
        if device.is_interface() {
            continue;
        }

        let pm = if autosuspend
            && !usb_blacklisted(device.ids(), &device.interface_drivers(), usb)
        {
            RuntimePowerManagement::On
        } else {
            RuntimePowerManagement::Off
        };

        device.set_runtime_pm(pm);
    }
}

/// Whether a USB device must never be autosuspended: one of its interface
/// drivers or its `vid:pid` is blacklisted.
fn usb_blacklisted(
    ids: Option<(String, String)>,
    interface_drivers: &[String],
    usb: &UsbConfig,
) -> bool {
    if let Some(driver) =
        interface_drivers.iter().find(|driver| usb.blacklist_drivers.contains(driver))
    {
        log::debug!("USB device with '{}' interface excluded from autosuspend", driver);
        return true;
    }

    if let Some((vendor, product)) = ids {
        let id = format!("{}:{}", vendor, product);
        if usb.blacklist.contains(&id) {
            log::debug!("USB device {} excluded from autosuspend", id);
            return true;
        }
    }

    false
}

/// Sets the first supported link power-management policy on every SCSI/SATA
/// host, returning one error per host that could not be set.
fn scsi_host_link_time_pm_policy(policies: &'static [&'static str]) -> Vec<ScsiHostError> {
    let mut errors = Vec::new();

    for device in ScsiHost::iter() {
        match device {
            Ok(device) => {
                if let Err(why) = device.set_link_power_management_policy(policies) {
                    errors.push(ScsiHostError::LinkTimePolicy(
                        policies[0],
                        device.id().to_owned(),
                        why,
                    ));
                }
            }
            Err(why) => log::warn!("failed to iterate SCSI Host device: {}", why),
        }
    }

    errors
}

// ── Model-specific Intel limits ──────────────────────────────────────────────

/// MSR holding the TCC activation offset (`MSR_TEMPERATURE_TARGET`).
const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;

pub struct ModelProfile {
    pl1:        Option<u8>,
    pl2:        Option<u8>,
    tcc_offset: Option<u8>,
}

impl ModelProfile {
    pub fn set(&self) -> Result<(), ModelError> {
        // Thermald sets pl1 and pl2 on its own, conflicting with system76-power
        let _status = Command::new("systemctl")
            .arg("stop")
            .arg("thermald.service")
            .status()
            .map_err(ModelError::Thermald)?;
        // TODO: check status, allow thermald to be missing

        // Set PL1
        if let Some(pl1) = self.pl1 {
            fs::write(
                "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
                format!("{}", u64::from(pl1) * 1_000_000),
            )
            .map_err(ModelError::Pl1)?;
        }

        // Set PL2
        if let Some(pl2) = self.pl2 {
            fs::write(
                "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
                format!("{}", u64::from(pl2) * 1_000_000),
            )
            .map_err(ModelError::Pl2)?;
        }

        // Set TCC
        if let Some(tcc_offset) = self.tcc_offset {
            let path = Path::new("/dev/cpu/0/msr");
            if !path.exists() {
                let status =
                    Command::new("modprobe").arg("msr").status().map_err(ModelError::ModprobeIo)?;
                if !status.success() {
                    return Err(ModelError::ModprobeExitStatus(status));
                }
            }

            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(ModelError::MsrOpen)?;

            // Positional I/O: the file offset selects the MSR, so read and
            // write must both target it explicitly.
            let mut data = [0; 8];
            file.read_exact_at(&mut data, MSR_TEMPERATURE_TARGET).map_err(ModelError::MsrRead)?;
            data[3] = tcc_offset;
            file.write_all_at(&data, MSR_TEMPERATURE_TARGET).map_err(ModelError::MsrWrite)?;
        }

        Ok(())
    }
}

pub struct ModelProfiles {
    pub balanced:    ModelProfile,
    pub performance: ModelProfile,
    pub battery:     ModelProfile,
}

impl ModelProfiles {
    /// Profiles for the DMI `product_version` `model`, if it has any.
    #[must_use]
    pub fn new(model: &str) -> Option<Self> {
        match model.trim() {
            "galp5" => Some(Self {
                balanced:    ModelProfile {
                    pl1:        Some(28),
                    pl2:        None,     // galp5 doesn't like setting pl2
                    tcc_offset: Some(12), // 88 C
                },
                performance: ModelProfile {
                    pl1:        Some(40),
                    pl2:        None,    // galp5 doesn't like setting pl2
                    tcc_offset: Some(7), // 93 C
                },
                battery:     ModelProfile {
                    pl1:        Some(12),
                    pl2:        None,     // galp5 doesn't like setting pl2
                    tcc_offset: Some(32), // 68 C
                },
            }),
            "lemp9" => Some(Self {
                balanced:    ModelProfile {
                    pl1:        Some(20),
                    pl2:        Some(40), // Upped from 30
                    tcc_offset: Some(12), // 88 C
                },
                performance: ModelProfile {
                    pl1:        Some(30),
                    pl2:        Some(50),
                    tcc_offset: Some(2), // 98 C
                },
                battery:     ModelProfile {
                    pl1:        Some(10),
                    pl2:        Some(30),
                    tcc_offset: Some(32), // 68 C
                },
            }),
            _ => None,
        }
    }

    #[must_use]
    pub const fn get(&self, profile: Profile) -> &ModelProfile {
        match profile {
            Profile::Battery => &self.battery,
            Profile::Balanced => &self.balanced,
            Profile::Performance => &self.performance,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_config(drivers: &[&str], ids: &[&str]) -> UsbConfig {
        UsbConfig {
            autosuspend:       true,
            blacklist_drivers: drivers.iter().map(|s| (*s).to_owned()).collect(),
            blacklist:         ids.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn ids(vendor: &str, product: &str) -> Option<(String, String)> {
        Some((vendor.to_owned(), product.to_owned()))
    }

    #[test]
    fn usb_blacklist_matches_interface_driver() {
        let config = usb_config(&["usblp", "btusb"], &[]);
        assert!(usb_blacklisted(ids("0489", "e0d8"), &["btusb".to_owned()], &config));
        assert!(!usb_blacklisted(ids("04f2", "b7b6"), &["uvcvideo".to_owned()], &config));
    }

    #[test]
    fn usb_blacklist_matches_ids() {
        let config = usb_config(&[], &["048d:c103"]);
        assert!(usb_blacklisted(ids("048d", "c103"), &[], &config));
        assert!(!usb_blacklisted(ids("048d", "c104"), &[], &config));
        assert!(!usb_blacklisted(None, &[], &config));
    }

    #[test]
    fn performance_disables_runtime_pm_and_others_enable_it() {
        assert_eq!(settings(Profile::Performance).runtime_pm, RuntimePowerManagement::Off);
        assert_eq!(settings(Profile::Balanced).runtime_pm, RuntimePowerManagement::On);
        assert_eq!(settings(Profile::Battery).runtime_pm, RuntimePowerManagement::On);
    }

    #[test]
    fn battery_caps_agree() {
        let battery = settings(Profile::Battery);
        assert_eq!(battery.cpu_max_percent, battery.pstate_max_perf_pct);
        assert!(!battery.turbo);
        assert!(settings(Profile::Balanced).turbo);
        assert!(settings(Profile::Performance).turbo);
    }

    #[test]
    fn dirty_writeback_ordering() {
        assert_eq!(settings(Profile::Battery).dirty_secs, 30);
        assert_eq!(settings(Profile::Balanced).dirty_secs, 15);
        assert_eq!(settings(Profile::Performance).dirty_secs, 10);
    }

    #[test]
    fn model_profiles_only_for_known_models() {
        assert!(ModelProfiles::new("galp5").is_some());
        assert!(ModelProfiles::new("lemp9\n").is_some());
        assert!(ModelProfiles::new("Legion Pro 5 16ARX8").is_none());
    }
}
