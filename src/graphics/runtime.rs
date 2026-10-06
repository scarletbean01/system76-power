// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Helpers for the runtime (no-reboot) graphics mode switch.
//!
//! The switch is performed in three phases:
//!   1. **Teardown** — stop services, unbind framebuffers, kill device users,
//!      unload modules, unbind and remove PCI devices.
//!   2. **Configuration** — write config files (handled by `config::write_vendor_config`).
//!   3. **Bring-up** — rescan PCI bus, load modules, start services, restart DM.

use std::{fs, io, path, process, thread, time};

use crate::modprobe;

use super::device::GraphicsDevice;
use super::error::GraphicsDeviceError;
use super::mode::GraphicsMode;
use super::systemd;

// ── Display manager ───────────────────────────────────────────────────────────

/// Detect the active display manager.
///
/// First queries the systemd `display-manager.service` standard alias. If that
/// fails or is inactive, falls back to checking `gdm`, `gdm3`, `sddm`, `lightdm`,
/// and `greetd` in that order. Returns `None` if no display manager is active.
pub(crate) fn detect_display_manager() -> Option<String> {
    // 1. Check standard display-manager.service alias via systemctl show
    if let Ok(output) = process::Command::new(systemd::SYSTEMCTL_CMD)
        .args(["show", "-p", "Id", "--value", "display-manager.service"])
        .output()
    {
        if output.status.success() {
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let unit = id.strip_suffix(".service").unwrap_or(&id);
            if !unit.is_empty() && unit != "display-manager" && systemd::is_active(unit) {
                log::info!("Detected active display manager via alias: {}", unit);
                return Some(unit.to_string());
            }
        }
    }

    // 2. Known list fallback
    const DMS: &[&str] = &["gdm", "gdm3", "sddm", "lightdm", "greetd"];
    for &dm in DMS {
        if systemd::is_active(dm) {
            log::info!("Detected active display manager: {}", dm);
            return Some(dm.to_string());
        }
    }
    log::warn!("No active display manager detected");
    None
}

/// RAII guard ensuring the display manager is always restarted even on early return or panic.
pub(crate) struct DmGuard {
    unit: Option<String>,
    restored: bool,
}

impl DmGuard {
    pub fn new(unit: Option<String>) -> Self {
        Self { unit, restored: false }
    }

    #[must_use]
    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }

    pub fn mark_restored(&mut self) {
        self.restored = true;
    }
}

impl Drop for DmGuard {
    fn drop(&mut self) {
        if !self.restored {
            if let Some(ref dm) = self.unit {
                log::error!(
                    "DmGuard dropped without explicit restart; emergency restarting display manager: {}",
                    dm
                );
                if let Err(e) = systemd::start(dm) {
                    log::error!("Failed to emergency-restart {}: {}", dm, e);
                }
            }
        }
    }
}

// ── Framebuffer management ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConsoleState {
    pub unbound_vtcons: Vec<path::PathBuf>,
}

/// Unbind the kernel framebuffer console from the GPU.
///
/// Only unbinds `vtcon` consoles if their underlying framebuffer device belongs
/// to an NVIDIA GPU. This ensures iGPU text consoles are preserved during integrated
/// mode transitions.
pub(crate) fn unbind_framebuffers(nvidia_devices: &[GraphicsDevice]) -> ConsoleState {
    unbind_framebuffers_in(path::Path::new("/sys"), nvidia_devices)
}

pub(crate) fn unbind_framebuffers_in(
    sysfs: &path::Path,
    nvidia_devices: &[GraphicsDevice],
) -> ConsoleState {
    let mut unbound = Vec::new();
    let vtcon_dir = sysfs.join("class/vtconsole");
    if vtcon_dir.exists() {
        if let Ok(entries) = fs::read_dir(&vtcon_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name_path = path.join("name");
                let bind_path = path.join("bind");
                if let Ok(name) = fs::read_to_string(&name_path) {
                    if name.contains("frame buffer device") {
                        if let Ok(bind_val) = fs::read_to_string(&bind_path) {
                            if bind_val.trim() == "1" {
                                let is_nvidia = fb_belongs_to_nvidia(sysfs, nvidia_devices);
                                if is_nvidia {
                                    log::info!("Unbinding framebuffer console: {:?}", path);
                                    if let Err(e) = fs::write(&bind_path, "0") {
                                        log::warn!("Failed to unbind {:?}: {}", bind_path, e);
                                    } else {
                                        unbound.push(bind_path);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let efifb_device = sysfs.join("bus/platform/drivers/efi-framebuffer/efi-framebuffer.0");
    let efifb_unbind = sysfs.join("bus/platform/drivers/efi-framebuffer/unbind");
    if efifb_device.exists() && efifb_unbind.exists() {
        log::info!("Unbinding EFI framebuffer (irreversible; memory reclaimed by DRM)");
        if let Err(e) = fs::write(&efifb_unbind, "efi-framebuffer.0") {
            log::warn!("Failed to unbind efi-framebuffer: {}", e);
        }
    }

    ConsoleState { unbound_vtcons: unbound }
}

/// Rebind any framebuffer consoles that were unbound during teardown.
pub(crate) fn rebind_framebuffers(state: &ConsoleState) {
    for path in &state.unbound_vtcons {
        log::info!("Rebinding framebuffer console: {:?}", path);
        if let Err(e) = fs::write(path, "1") {
            log::warn!("Failed to rebind {:?}: {}", path, e);
        }
    }
}

/// Checks if any framebuffer in `/sys/class/graphics/fb*` belongs to an NVIDIA PCI device.
fn fb_belongs_to_nvidia(sysfs: &path::Path, nvidia_devices: &[GraphicsDevice]) -> bool {
    if nvidia_devices.is_empty() {
        return false;
    }

    let fb_dir = sysfs.join("class/graphics");
    if !fb_dir.exists() {
        return false;
    }

    if let Ok(entries) = fs::read_dir(&fb_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with("fb") {
                let dev_link = entry.path().join("device");
                if let Ok(canonical) = fs::canonicalize(&dev_link) {
                    let canon_str = canonical.to_string_lossy();
                    for dev in nvidia_devices {
                        if canon_str.contains(dev.id()) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

// ── Kill device users ─────────────────────────────────────────────────────────

/// Check if any processes are holding NVIDIA devices without killing them.
/// Used when no display manager is active to avoid aborting a user's direct session.
pub(crate) fn check_device_users_idle(
    nvidia_devices: &[GraphicsDevice],
) -> Result<(), GraphicsDeviceError> {
    let mut targets: Vec<String> = vec![
        "/dev/nvidia0".into(),
        "/dev/nvidiactl".into(),
        "/dev/nvidia-modeset".into(),
        "/dev/nvidia-uvm".into(),
    ];

    for dev in nvidia_devices {
        let dev_drm = format!("/sys/bus/pci/devices/{}/drm", dev.id());
        if let Ok(entries) = fs::read_dir(dev_drm) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("card") || name.starts_with("renderD") {
                    let dev_node = format!("/dev/dri/{}", name);
                    if path::Path::new(&dev_node).exists() {
                        targets.push(dev_node);
                    }
                }
            }
        }
    }

    let existing: Vec<String> =
        targets.into_iter().filter(|d| path::Path::new(d).exists()).collect();

    if existing.is_empty() {
        return Ok(());
    }

    let output = match process::Command::new("fuser").args(&existing).output() {
        Ok(out) => out,
        Err(e) => {
            log::warn!("Could not execute fuser: {} (psmisc installed?)", e);
            return Ok(());
        }
    };

    let mut pids = Vec::new();
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    for token in stdout_str.split_whitespace() {
        let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Ok(pid) = digits.parse::<u32>() {
            pids.push(pid);
        }
    }

    if !pids.is_empty() {
        return Err(GraphicsDeviceError::DeviceBusy { pids });
    }

    Ok(())
}

/// Forcefully terminate any processes that still hold open file descriptors on
/// `/dev/nvidia*` device nodes.
pub(crate) fn kill_nvidia_device_users() {
    const NVIDIA_DEVICES: &[&str] =
        &["/dev/nvidia0", "/dev/nvidiactl", "/dev/nvidia-modeset", "/dev/nvidia-uvm"];

    let existing: Vec<&str> =
        NVIDIA_DEVICES.iter().copied().filter(|d| path::Path::new(d).exists()).collect();

    if existing.is_empty() {
        log::info!("No /dev/nvidia* devices found; skipping fuser");
        return;
    }

    log::info!("Killing processes holding /dev/nvidia* fds: {:?}", existing);
    let result = process::Command::new("fuser").arg("-k").args(&existing).status();
    match result {
        Ok(s) => log::info!("fuser exited with {}", s),
        Err(e) => log::warn!("fuser failed to execute: {} (psmisc installed?)", e),
    }

    thread::sleep(time::Duration::from_millis(500));
}

// ── Module management ─────────────────────────────────────────────────────────

/// Load the kernel modules required for the given graphics mode.
pub(crate) fn load_modules_for_mode(vendor: GraphicsMode) -> Result<(), GraphicsDeviceError> {
    match vendor {
        GraphicsMode::Integrated => {
            log::info!("Integrated mode: no NVIDIA modules to load");
            Ok(())
        }
        GraphicsMode::Compute => {
            modprobe::load("nvidia", &[])
                .map_err(|why| GraphicsDeviceError::ModuleLoad { module: "nvidia", why })?;
            if let Err(why) = modprobe::load("nvidia_uvm", &[]) {
                log::warn!("Could not load nvidia_uvm: {} (continuing)", why);
            }
            Ok(())
        }
        GraphicsMode::Hybrid | GraphicsMode::Discrete => {
            modprobe::load("nvidia", &[])
                .map_err(|why| GraphicsDeviceError::ModuleLoad { module: "nvidia", why })?;
            modprobe::load("nvidia-modeset", &[])
                .map_err(|why| GraphicsDeviceError::ModuleLoad { module: "nvidia-modeset", why })?;
            modprobe::load("nvidia-drm", &["modeset=1"])
                .map_err(|why| GraphicsDeviceError::ModuleLoad { module: "nvidia-drm", why })?;
            if let Err(why) = modprobe::load("nvidia_uvm", &[]) {
                log::warn!("Could not load nvidia_uvm: {} (continuing)", why);
            }
            Ok(())
        }
    }
}

/// Verify that `nvidia_drm` loaded with `modeset=1` (`Y`).
pub(crate) fn verify_modeset() -> Result<bool, GraphicsDeviceError> {
    let param_path = "/sys/module/nvidia_drm/parameters/modeset";
    if !path::Path::new(param_path).exists() {
        return Ok(false);
    }
    match fs::read_to_string(param_path) {
        Ok(val) => Ok(val.trim() == "Y"),
        Err(e) => {
            log::warn!("Could not read {}: {}", param_path, e);
            Ok(false)
        }
    }
}

// ── Settle and wait helpers ───────────────────────────────────────────────────

/// Run `udevadm settle --timeout=10` non-fatally.
pub(crate) fn udev_settle() -> Result<(), GraphicsDeviceError> {
    let result = process::Command::new("udevadm")
        .args(["settle", "--timeout=10"])
        .status();
    match result {
        Ok(status) => {
            if !status.success() {
                log::warn!("udevadm settle exited with {}", status);
            }
            Ok(())
        }
        Err(e) => {
            log::warn!("Could not execute udevadm settle: {} (continuing)", e);
            Ok(())
        }
    }
}

/// Poll for up to `timeout_ms` until `/sys/bus/pci/devices/<dev_id>` exists.
pub(crate) fn wait_for_device(dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
    let dev_path = format!("/sys/bus/pci/devices/{}", dev_id);
    let start = time::Instant::now();
    let poll_interval = time::Duration::from_millis(100);
    while start.elapsed() < time::Duration::from_millis(timeout_ms) {
        if path::Path::new(&dev_path).exists() {
            log::info!("Device {} appeared in sysfs after {:?}", dev_id, start.elapsed());
            return Ok(true);
        }
        thread::sleep(poll_interval);
    }
    log::warn!("Timeout waiting for {} in sysfs", dev_path);
    Ok(false)
}

/// Poll for up to `timeout_ms` until a DRM card directory appears under `/sys/bus/pci/devices/<dev_id>/drm`.
pub(crate) fn wait_for_drm_card(dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
    let dev_drm = format!("/sys/bus/pci/devices/{}/drm", dev_id);
    let start = time::Instant::now();
    let poll_interval = time::Duration::from_millis(100);
    while start.elapsed() < time::Duration::from_millis(timeout_ms) {
        if let Ok(entries) = fs::read_dir(&dev_drm) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("card") {
                    log::info!("DRM card for {} appeared: {:?}", dev_id, entry.file_name());
                    return Ok(true);
                }
            }
        }
        thread::sleep(poll_interval);
    }
    log::warn!("Timeout waiting for DRM card on {}", dev_id);
    Ok(false)
}

// ── NVIDIA service management ─────────────────────────────────────────────────

pub(crate) fn stop_nvidia_services() {
    const NVIDIA_DAEMONS: &[&str] = &["nvidia-powerd", "nvidia-persistenced"];

    for svc in NVIDIA_DAEMONS {
        if systemd::is_active(svc) {
            log::info!("Stopping NVIDIA service: {}", svc);
            match systemd::stop(svc) {
                Ok(s) if s.success() => {}
                Ok(s) => log::warn!("systemctl stop {} exited with {} (continuing)", svc, s),
                Err(e) => log::warn!("Failed to stop {}: {} (continuing)", svc, e),
            }
        }
    }
}

pub(crate) fn start_enabled_nvidia_services() {
    const NVIDIA_DAEMONS: &[&str] = &["nvidia-powerd", "nvidia-persistenced"];

    for svc in NVIDIA_DAEMONS {
        if systemd::is_enabled(svc) {
            log::info!("Starting enabled NVIDIA service: {}", svc);
            match systemd::start(svc) {
                Ok(s) if s.success() => log::info!("Started {}", svc),
                Ok(s) => {
                    log::warn!(
                        "systemctl start {} exited with {} — may need manual restart",
                        svc,
                        s
                    )
                }
                Err(e) => log::warn!("Failed to start {}: {}", svc, e),
            }
        }
    }
}

// ── PCI power control ─────────────────────────────────────────────────────────

pub(crate) fn sysfs_power_control(pciid: String, mode: GraphicsMode) {
    thread::spawn(move || {
        thread::sleep(time::Duration::from_millis(5000));

        let control = format!("/sys/bus/pci/devices/{}/power/control", pciid);
        if !path::Path::new(&control).exists() {
            log::info!("Device {} no longer present; skipping power control", pciid);
            return;
        }

        let pm = if mode == GraphicsMode::Discrete { "on\n" } else { "auto\n" };
        log::info!("Setting power management to {}", pm);

        let file = fs::OpenOptions::new().create(false).truncate(false).write(true).open(control);

        #[allow(unused_must_use)]
        if let Ok(mut file) = file {
            use std::io::Write;
            file.write_all(pm.as_bytes()).and_then(|()| file.sync_all());
        }
    });
}
