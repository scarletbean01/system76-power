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

// ── Device users ──────────────────────────────────────────────────────────────

fn is_nvidia_node(path: &path::Path) -> bool {
    path.to_str().is_some_and(|p| p.starts_with("/dev/nvidia"))
}

/// DRM device nodes (`/dev/dri/card*`, `/dev/dri/renderD*`) of the given GPUs.
fn drm_nodes(devices: &[GraphicsDevice]) -> Vec<path::PathBuf> {
    let mut nodes = Vec::new();
    for dev in devices {
        let dev_drm = format!("/sys/bus/pci/devices/{}/drm", dev.id());
        if let Ok(entries) = fs::read_dir(dev_drm) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("card") || name.starts_with("renderD") {
                    nodes.push(path::Path::new("/dev/dri").join(name));
                }
            }
        }
    }
    nodes
}

/// True if the error means the process went away while we were inspecting it.
fn process_exited(why: &io::Error) -> bool {
    why.kind() == io::ErrorKind::NotFound || why.raw_os_error() == Some(libc::ESRCH)
}

/// PIDs, other than our own, that have a file descriptor open on, or a memory
/// mapping of, a path accepted by `matches`. `proc_root` is normally `/proc`.
///
/// Errors if `proc_root` cannot be listed or a process cannot be inspected for
/// any reason other than it exiting meanwhile, so callers can fail closed
/// rather than assume the devices are idle.
pub(crate) fn processes_using_in(
    proc_root: &path::Path,
    matches: &dyn Fn(&path::Path) -> bool,
) -> io::Result<Vec<u32>> {
    let self_pid = process::id();
    let mut pids = Vec::new();

    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        match process_uses(&entry.path(), matches) {
            Ok(true) => pids.push(pid),
            Ok(false) => (),
            Err(why) if process_exited(&why) => (),
            Err(why) => {
                return Err(io::Error::new(
                    why.kind(),
                    format!("{}: {}", entry.path().display(), why),
                ))
            }
        }
    }

    pids.sort_unstable();
    Ok(pids)
}

fn process_uses(proc_dir: &path::Path, matches: &dyn Fn(&path::Path) -> bool) -> io::Result<bool> {
    for fd in fs::read_dir(proc_dir.join("fd"))? {
        // An fd closed between listing and reading the link is simply gone.
        let Ok(fd) = fd else { continue };
        if let Ok(target) = fs::read_link(fd.path()) {
            if matches(&target) {
                return Ok(true);
            }
        }
    }

    // Mappings outlive the fd they were made from (CUDA maps /dev/nvidia*).
    let maps = fs::read_to_string(proc_dir.join("maps"))?;
    Ok(maps
        .lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .any(|mapped| matches(path::Path::new(mapped))))
}

/// Check that no process holds the NVIDIA devices, without killing anything.
/// Used when no display manager is managed, to avoid tearing the GPU out from
/// under a user's session.
///
/// Fails closed: if usage cannot be determined, returns `DeviceUsageUnknown`.
pub(crate) fn check_device_users_idle(
    nvidia_devices: &[GraphicsDevice],
) -> Result<(), GraphicsDeviceError> {
    let drm = drm_nodes(nvidia_devices);
    let pids = processes_using_in(path::Path::new("/proc"), &|p| {
        is_nvidia_node(p) || drm.iter().any(|node| node == p)
    })
    .map_err(GraphicsDeviceError::DeviceUsageUnknown)?;

    if pids.is_empty() {
        Ok(())
    } else {
        Err(GraphicsDeviceError::DeviceBusy { pids })
    }
}

/// Forcefully terminate any processes that still hold `/dev/nvidia*` open or
/// mapped.
///
/// Best effort: anything left holding the devices makes the following module
/// unload fail, which the teardown verifies and treats as an error.
pub(crate) fn kill_nvidia_device_users() {
    let pids = match processes_using_in(path::Path::new("/proc"), &is_nvidia_node) {
        Ok(pids) => pids,
        Err(why) => {
            log::warn!("Could not list processes using /dev/nvidia*: {}", why);
            return;
        }
    };

    if pids.is_empty() {
        log::info!("No processes hold /dev/nvidia*");
        return;
    }

    log::info!("Killing processes holding /dev/nvidia*: {:?}", pids);
    for &pid in &pids {
        let Ok(raw) = libc::pid_t::try_from(pid) else { continue };
        // SAFETY: kill(2) has no memory-safety preconditions.
        if unsafe { libc::kill(raw, libc::SIGKILL) } != 0 {
            let why = io::Error::last_os_error();
            if !process_exited(&why) {
                log::warn!("Could not kill {}: {}", pid, why);
            }
        }
    }

    thread::sleep(time::Duration::from_millis(500));
}

// ── Module management ─────────────────────────────────────────────────────────

/// Load the kernel modules required for the given graphics mode.
///
/// Module load order matters: `nvidia` core must be loaded before
/// `nvidia-modeset`, which must be loaded before `nvidia-drm`.
/// For PRIME/DRM modes `nvidia-drm` must be loaded with `modeset=1`.
pub(crate) fn load_modules_for_mode(vendor: GraphicsMode) -> Result<(), GraphicsDeviceError> {
    match vendor {
        GraphicsMode::Integrated => {
            // Nothing to load; the NVIDIA driver stack stays absent.
            log::info!("Integrated mode: no NVIDIA modules to load");
            Ok(())
        }
        GraphicsMode::Compute => {
            // nvidia core only — drm/modeset remain blacklisted so the GPU is
            // accessible for CUDA but does not participate in display.
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
            // modeset=1 is required for PRIME offload (Hybrid) and for DRM
            // master hand-off (Discrete).
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

/// Stop the NVIDIA daemon services that hold open references to the driver stack.
///
/// Only the two long-running daemon services are considered:
/// - `nvidia-powerd`       — NVIDIA power management daemon
/// - `nvidia-persistenced` — keeps NVIDIA contexts persistent
///
/// The oneshot hook services (nvidia-suspend, nvidia-hibernate, nvidia-resume,
/// nvidia-suspend-then-hibernate) run and exit immediately; they are never
/// "active" at query time and hold no file-descriptor references, so they are
/// intentionally skipped.
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

/// Start NVIDIA daemon services that are enabled in systemd.
///
/// Called during bring-up after a runtime switch to a GPU-active mode
/// (hybrid, compute, nvidia). We check `systemctl is-enabled` rather than
/// relying on whether the service was running before teardown, because the
/// prior running state may not reflect the user's intended configuration
/// (e.g., after a transition from integrated mode where the service was
/// correctly not running).
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

// HACK: Normally, power/control would be set to "auto" by a udev rule in
// nvidia-drivers, but because of a bug we cannot enable automatic power
// management too early after turning on the GPU. Otherwise, it will turn off
// before the NVIDIA driver finishes initializing, leaving the system in an
// invalid state that will eventually lock up. So defer setting power
// management using a thread.
//
// Ref: pop-os/nvidia-graphics-drivers@f9815ed603bd
// Ref: system76/firmware-open#160
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fake_proc(name: &str) -> path::PathBuf {
        let root = std::env::temp_dir().join(format!("s76_test_proc_{}_{}", name, process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn fake_process(root: &path::Path, pid: u32, fds: &[&str], maps: &str) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(dir.join("fd")).unwrap();
        for (n, target) in fds.iter().enumerate() {
            symlink(target, dir.join("fd").join(n.to_string())).unwrap();
        }
        fs::write(dir.join("maps"), maps).unwrap();
    }

    #[test]
    fn finds_open_fds_and_mappings_but_not_unrelated_processes() {
        let root = fake_proc("users");
        fake_process(&root, 100, &["/dev/null", "/dev/nvidia0"], "");
        fake_process(
            &root,
            200,
            &["/dev/null"],
            "7f00-7f10 rw-s 00000000 00:05 42   /dev/nvidiactl\n",
        );
        fake_process(&root, 300, &["/dev/dri/card0", "socket:[123]"], "");
        fs::create_dir_all(root.join("self")).unwrap(); // non-PID entries are skipped

        let pids = processes_using_in(&root, &is_nvidia_node).unwrap();
        assert_eq!(pids, vec![100, 200]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn idle_when_nothing_matches() {
        let root = fake_proc("idle");
        fake_process(&root, 100, &["/dev/null"], "");
        assert_eq!(processes_using_in(&root, &is_nvidia_node).unwrap(), Vec::<u32>::new());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn exited_process_is_skipped() {
        let root = fake_proc("exited");
        // A PID directory without fd/ looks like a process that exited mid-scan.
        fs::create_dir_all(root.join("100")).unwrap();
        fake_process(&root, 200, &["/dev/nvidia-uvm"], "");
        assert_eq!(processes_using_in(&root, &is_nvidia_node).unwrap(), vec![200]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_proc_fails_closed() {
        let root = fake_proc("missing").join("does-not-exist");
        assert!(processes_using_in(&root, &is_nvidia_node).is_err());
    }
}
