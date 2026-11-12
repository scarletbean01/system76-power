// Copyright 2022 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use std::path::Path;
use sysfs_class::RuntimePowerManagement;

/// Returns the last path component of the `driver` symlink under `base`, if the
/// device is bound to a driver.
fn driver_name(base: &Path) -> Option<String> {
    std::fs::read_link(base.join("driver"))
        .ok()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Writes `"auto"` (On) or `"on"` (Off) to `<base>/power/control`.
///
/// Many sysfs entries (notably USB interface nodes) have no `power/control`;
/// those are skipped at debug level so a profile switch does not emit a warning
/// per node. Real write failures are warned about.
fn set_power_control(base: &Path, pm: RuntimePowerManagement) {
    let value = match pm {
        RuntimePowerManagement::Off => "on",
        RuntimePowerManagement::On => "auto",
    };
    let path = base.join("power/control");

    if !path.exists() {
        log::debug!("{}: no power/control, skipping", path.display());
        return;
    }

    if let Err(why) = std::fs::write(&path, value) {
        log::warn!("{}: failed to set runtime PM to {}: {}", path.display(), value, why);
    }
}

pub mod i2c {
    use super::set_power_control;
    use std::path::PathBuf;
    use sysfs_class::RuntimePowerManagement;

    pub struct I2cDevice {
        path: PathBuf,
    }

    impl I2cDevice {
        /// Sets runtime PM on the I2C client itself (e.g. `i2c-ELAN06FA:00`).
        /// Adapter nodes (`i2c-N`) have no `power/control` and are skipped.
        pub fn set_runtime_pm(&self, pm: RuntimePowerManagement) {
            set_power_control(&self.path, pm);
        }
    }

    pub fn devices() -> impl Iterator<Item = I2cDevice> {
        std::fs::read_dir("/sys/bus/i2c/devices/")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| I2cDevice { path: entry.path() })
    }
}

pub mod pci {
    use super::{driver_name, set_power_control};
    use std::path::PathBuf;
    use sysfs_class::RuntimePowerManagement;

    pub struct PciDevice {
        path: PathBuf,
    }

    impl PciDevice {
        pub fn set_runtime_pm(&self, pm: RuntimePowerManagement) {
            set_power_control(&self.path, pm);
        }

        /// Name of the bound driver, or `None` for a driverless device.
        #[must_use]
        pub fn driver_name(&self) -> Option<String> { driver_name(&self.path) }

        /// PCI address, e.g. `0000:01:00.1`.
        #[must_use]
        pub fn name(&self) -> std::borrow::Cow<'_, str> {
            self.path.file_name().map_or_else(|| "?".into(), |name| name.to_string_lossy())
        }

        /// Whether this device is a GPU (display-controller class `0x03xxxx`)
        /// or another function of a GPU's slot (its HDA audio, USB-C, UCSI…).
        ///
        /// A GPU can only reach D3cold once *all* functions of its slot are
        /// suspended, so the companion functions belong to it as well.
        #[must_use]
        pub fn belongs_to_gpu(&self) -> bool {
            if is_display_class(&self.path) {
                return true;
            }

            // `0000:01:00.1` → function 0 of the same slot, `0000:01:00.0`.
            let name = self.name();
            match name.rsplit_once('.') {
                Some((slot, function)) if function != "0" => {
                    is_display_class(&self.path.with_file_name(format!("{}.0", slot)))
                }
                _ => false,
            }
        }
    }

    fn is_display_class(path: &std::path::Path) -> bool {
        std::fs::read_to_string(path.join("class"))
            .is_ok_and(|class| class.trim().starts_with("0x03"))
    }

    pub fn devices() -> impl Iterator<Item = PciDevice> {
        std::fs::read_dir("/sys/bus/pci/devices/")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| PciDevice { path: entry.path() })
    }
}

pub mod usb {
    use super::{driver_name, set_power_control};
    use std::path::PathBuf;
    use sysfs_class::RuntimePowerManagement;

    pub struct UsbDevice {
        path: PathBuf,
    }

    impl UsbDevice {
        pub fn set_runtime_pm(&self, pm: RuntimePowerManagement) {
            set_power_control(&self.path, pm);
        }

        /// Whether this is an interface node (`1-1:1.0`). Interfaces have no
        /// `power/control`; runtime PM is set on their parent device.
        #[must_use]
        pub fn is_interface(&self) -> bool {
            self.path.file_name().is_some_and(|name| name.to_string_lossy().contains(':'))
        }

        /// Drivers bound to this device's interfaces (`usbhid`, `btusb`, …).
        ///
        /// The device node itself is always bound to the generic `usb` driver,
        /// so the function-specific drivers have to be read from its
        /// `<name>:<config>.<interface>` child directories.
        #[must_use]
        pub fn interface_drivers(&self) -> Vec<String> {
            let Ok(dir) = std::fs::read_dir(&self.path) else {
                return Vec::new();
            };

            dir.filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(':'))
                .filter_map(|entry| driver_name(&entry.path()))
                .collect()
        }

        /// `(idVendor, idProduct)` as lowercase hex, or `None` for root hubs.
        #[must_use]
        pub fn ids(&self) -> Option<(String, String)> {
            let vendor = std::fs::read_to_string(self.path.join("idVendor")).ok()?;
            let product = std::fs::read_to_string(self.path.join("idProduct")).ok()?;
            Some((
                vendor.trim().to_ascii_lowercase(),
                product.trim().to_ascii_lowercase(),
            ))
        }
    }

    pub fn devices() -> impl Iterator<Item = UsbDevice> {
        std::fs::read_dir("/sys/bus/usb/devices/")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| UsbDevice { path: entry.path() })
    }
}

pub mod rfkill {
    use std::path::PathBuf;

    pub struct RfkillDevice {
        path: PathBuf,
    }

    impl RfkillDevice {
        /// Contents of the `type` attribute (e.g. `bluetooth`, `wlan`).
        #[must_use]
        pub fn kind(&self) -> Option<String> {
            std::fs::read_to_string(self.path.join("type")).ok().map(|kind| kind.trim().to_owned())
        }

        /// Writes the `soft` rfkill state (`true` = blocked, `false` = unblocked).
        pub fn set_soft(&self, blocked: bool) {
            let path = self.path.join("soft");
            let value = if blocked { "1" } else { "0" };

            if let Err(why) = std::fs::write(&path, value) {
                log::warn!("{}: failed to set soft block to {}: {}", path.display(), value, why);
            }
        }
    }

    pub fn devices() -> impl Iterator<Item = RfkillDevice> {
        std::fs::read_dir("/sys/class/rfkill/")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| RfkillDevice { path: entry.path() })
    }
}

pub mod drm {
    use std::path::PathBuf;

    /// amdgpu `panel_power_savings` attributes of all built-in panels
    /// (`/sys/class/drm/cardN-eDP-M/amdgpu/panel_power_savings`).
    fn panel_power_savings_paths() -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return Vec::new();
        };

        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("-eDP-"))
            .map(|entry| entry.path().join("amdgpu/panel_power_savings"))
            .filter(|path| path.exists())
            .collect()
    }

    /// Sets the amdgpu panel power-savings (ABM) level on every built-in panel.
    ///
    /// Panels without the attribute (non-AMD GPUs, kernels before 6.9) are
    /// skipped. A write fails with `EBUSY` when the `amdgpu.abmlevel` module
    /// parameter pins the level; that is logged, not treated as an error.
    pub fn set_panel_power_savings(level: u8) {
        let paths = panel_power_savings_paths();
        if paths.is_empty() {
            log::debug!("no amdgpu panel_power_savings attribute found");
        }

        for path in paths {
            match std::fs::write(&path, level.to_string()) {
                Ok(()) => log::info!("{}: panel power savings set to {}", path.display(), level),
                Err(why) => log::warn!(
                    "{}: failed to set panel power savings to {}: {}",
                    path.display(),
                    level,
                    why
                ),
            }
        }
    }
}
