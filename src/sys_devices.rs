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
        pub fn set_runtime_pm(&self, pm: RuntimePowerManagement) {
            set_power_control(&self.path.join("device"), pm);
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

        /// Interface driver bound to this device (present on interface nodes such
        /// as `1-1:1.0`; root hubs and plain device nodes have none).
        #[must_use]
        pub fn driver_name(&self) -> Option<String> { driver_name(&self.path) }

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
