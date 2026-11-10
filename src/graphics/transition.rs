// Copyright 2018-2022 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Orchestration, planning, and side-effect abstractions for live graphics mode switches.

use std::{fs, io, path, thread, time};

use crate::module::Module;

use super::config::ConfigSnapshot;
use super::device::GraphicsDevice;
use super::error::GraphicsDeviceError;
use super::mode::GraphicsMode;
use super::runtime::{self, ConsoleState};
use super::Graphics;

pub use crate::config::RuntimeRestartDm as RestartPolicy;

/// The execution stages of a runtime graphics switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SwitchStage {
    Initial,
    StopDm,
    Teardown,
    WriteConfig,
    Rescan,
    LoadModules,
    StartServices,
    StartDm,
    Done,
}

/// Snapshot of the live NVIDIA subsystem state before a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveState {
    pub nvidia_modules_loaded: bool,
    pub dgpu_driver_bound: bool,
    pub dgpu_display_connected: Option<String>,
}

impl LiveState {
    pub fn probe(graphics: &Graphics) -> Result<Self, GraphicsDeviceError> {
        let modules = Module::all().map_err(GraphicsDeviceError::ModulesFetch)?;
        let nvidia_modules_loaded = modules.iter().any(|m| {
            m.name == "nvidia"
                || m.name == "nvidia_drm"
                || m.name == "nvidia_modeset"
                || m.name == "nvidia_uvm"
                || m.name == "nouveau"
        });

        let dgpu_driver_bound = graphics.nvidia.iter().any(|d| d.is_driver_bound());
        let dgpu_display_connected = dgpu_display_connected(&graphics.nvidia);

        Ok(Self {
            nvidia_modules_loaded,
            dgpu_driver_bound,
            dgpu_display_connected,
        })
    }
}

/// Detect if any display connector driven by the discrete NVIDIA GPU is currently connected.
pub(crate) fn dgpu_display_connected(nvidia_devices: &[GraphicsDevice]) -> Option<String> {
    dgpu_display_connected_in(path::Path::new("/sys/class/drm"), nvidia_devices)
}

pub(crate) fn dgpu_display_connected_in(
    drm_dir: &path::Path,
    nvidia_devices: &[GraphicsDevice],
) -> Option<String> {
    if nvidia_devices.is_empty() || !drm_dir.exists() {
        return None;
    }

    let entries = fs::read_dir(drm_dir).ok()?;
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        // Connector directories are named like `card0-DP-1` or `card1-HDMI-A-1`
        if !file_name.contains('-') {
            continue;
        }

        let status_path = entry.path().join("status");
        if let Ok(status) = fs::read_to_string(&status_path) {
            if status.trim() == "connected" {
                // Determine whether this connector belongs to an NVIDIA device
                let dev_link = entry.path().join("device");
                let belongs_to_nvidia = if let Ok(canonical) = fs::canonicalize(&dev_link) {
                    let canon_str = canonical.to_string_lossy();
                    nvidia_devices.iter().any(|dev| canon_str.contains(dev.id()))
                } else {
                    // Try parent card directory: e.g. card1 from card1-DP-1
                    let card_name = file_name.split('-').next().unwrap_or_default();
                    let card_dev_link = drm_dir.join(card_name).join("device");
                    if let Ok(canonical) = fs::canonicalize(&card_dev_link) {
                        let canon_str = canonical.to_string_lossy();
                        nvidia_devices.iter().any(|dev| canon_str.contains(dev.id()))
                    } else {
                        false
                    }
                };

                if belongs_to_nvidia {
                    return Some(file_name);
                }
            }
        }
    }

    None
}

/// The planned actions for a runtime graphics transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransitionPlan {
    pub teardown: bool,
    pub restart_dm: bool,
    pub bring_up: bool,
}

/// The outcome of transition planning.
#[derive(Debug)]
pub(crate) enum PlanOutcome {
    NoChange,
    Rejected(GraphicsDeviceError),
    Plan(TransitionPlan),
}

impl PlanOutcome {
    #[allow(dead_code)]
    #[must_use]
    pub fn is_no_change(&self) -> bool {
        matches!(self, Self::NoChange)
    }
}

/// Report returned upon completion of a runtime graphics switch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SwitchReport {
    pub from: &'static str,
    pub to: &'static str,
    pub dm_restarted: Option<String>,
    /// Set when the switch completed but the display manager could not be
    /// started again (after one retry). `dm_restarted` is `None` in that case.
    pub dm_restart_error: Option<String>,
    pub stage_reached: SwitchStage,
    pub no_change: bool,
}

/// Pure transition planner.
///
/// Determines whether teardown, config write, bring-up, and display manager
/// restart are needed for the requested transition given the live system state.
pub(crate) fn plan_transition(
    from: GraphicsMode,
    to: GraphicsMode,
    live: &LiveState,
    policy: RestartPolicy,
    external_display_requires_nvidia: bool,
) -> PlanOutcome {
    if from == to {
        let state_matches = match to {
            GraphicsMode::Integrated => !live.nvidia_modules_loaded && !live.dgpu_driver_bound,
            _ => live.nvidia_modules_loaded || live.dgpu_driver_bound,
        };
        if state_matches {
            return PlanOutcome::NoChange;
        }
    }

    if to == GraphicsMode::Discrete {
        return PlanOutcome::Rejected(GraphicsDeviceError::RuntimeDiscreteUnsupported);
    }

    let teardown = live.nvidia_modules_loaded || live.dgpu_driver_bound;

    if teardown {
        if let Some(ref conn) = live.dgpu_display_connected {
            return PlanOutcome::Rejected(GraphicsDeviceError::DgpuDisplayActive {
                connector: conn.clone(),
            });
        }
        if external_display_requires_nvidia {
            log::warn!("teardown requested but external displays on this model route through dGPU");
            return PlanOutcome::Rejected(GraphicsDeviceError::NotSwitchable);
        }
    }

    let bring_up = to != GraphicsMode::Integrated;

    let restart_dm = match policy {
        RestartPolicy::Never => false,
        RestartPolicy::Always => true,
        RestartPolicy::Auto => teardown,
    };

    PlanOutcome::Plan(TransitionPlan { teardown, restart_dm, bring_up })
}

// ── Switch operations abstraction ─────────────────────────────────────────────

/// NVIDIA driver modules unloaded on teardown, in unload order (dependents
/// first), as named in `/proc/modules`.
pub(crate) const NVIDIA_MODULES: &[&str] =
    &["nvidia_drm", "nvidia_modeset", "nvidia_uvm", "nvidia"];

/// Trait abstracting hardware, systemd, kernel module, and filesystem side-effects.
pub(crate) trait SwitchOps {
    fn detect_display_manager(&self) -> Option<String>;
    fn stop_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError>;
    fn start_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError>;
    fn stop_nvidia_services(&self);
    fn start_enabled_nvidia_services(&self);
    fn unbind_framebuffers(
        &self,
        nvidia: &[GraphicsDevice],
    ) -> Result<ConsoleState, GraphicsDeviceError>;
    fn rebind_framebuffers(&self, state: &ConsoleState);
    fn check_device_users_idle(
        &self,
        nvidia: &[GraphicsDevice],
    ) -> Result<(), GraphicsDeviceError>;
    fn kill_nvidia_device_users(&self);
    fn unload_module(&self, module: &'static str) -> Result<(), GraphicsDeviceError>;
    /// Names of the NVIDIA driver modules currently loaded (as in `/proc/modules`).
    fn loaded_nvidia_modules(&self) -> Result<Vec<String>, GraphicsDeviceError>;
    fn load_module(&self, module: &'static str, options: &[&str]) -> Result<(), GraphicsDeviceError>;
    fn load_modules_for_mode(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError>;
    fn unbind_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError>;
    fn remove_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError>;
    fn write_vendor_config(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError>;
    fn snapshot_config(&self) -> Result<ConfigSnapshot, GraphicsDeviceError>;
    fn restore_config(&self, snapshot: &ConfigSnapshot) -> Result<(), GraphicsDeviceError>;
    fn rescan_pci(&self) -> Result<(), GraphicsDeviceError>;
    fn udev_settle(&self) -> Result<(), GraphicsDeviceError>;
    fn wait_for_device(&self, dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError>;
    fn wait_for_drm_card(&self, dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError>;
    fn verify_modeset(&self) -> Result<bool, GraphicsDeviceError>;
    fn sysfs_power_control(&self, pciid: String, mode: GraphicsMode);
    fn sleep_ms(&self, ms: u64);
    fn get_vendor(&self, graphics: &Graphics) -> Result<GraphicsMode, GraphicsDeviceError>;
    fn probe_live_state(&self, graphics: &Graphics) -> Result<LiveState, GraphicsDeviceError>;
    fn refresh_inventory(&self, graphics: &mut Graphics) -> Result<(), GraphicsDeviceError>;
}

/// The production implementation of `SwitchOps`.
pub(crate) struct RealOps;

impl SwitchOps for RealOps {
    fn detect_display_manager(&self) -> Option<String> {
        runtime::detect_display_manager()
    }

    fn stop_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError> {
        let status = super::systemd::stop(dm).map_err(|why| {
            GraphicsDeviceError::DisplayManagerStop { dm: dm.to_owned(), why }
        })?;
        if !status.success() {
            return Err(GraphicsDeviceError::DisplayManagerStop {
                dm: dm.to_owned(),
                why: io::Error::other(format!("systemctl stop {} exited with {}", dm, status)),
            });
        }
        Ok(())
    }

    fn start_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError> {
        let status = super::systemd::start(dm).map_err(|why| {
            GraphicsDeviceError::DisplayManagerStart { dm: dm.to_owned(), why }
        })?;
        if !status.success() {
            log::warn!(
                "systemctl start {} exited with {} — display may need manual restart",
                dm,
                status
            );
            return Err(GraphicsDeviceError::DisplayManagerStart {
                dm: dm.to_owned(),
                why: io::Error::other(format!("systemctl start {} exited with {}", dm, status)),
            });
        }
        Ok(())
    }

    fn stop_nvidia_services(&self) {
        runtime::stop_nvidia_services();
    }

    fn start_enabled_nvidia_services(&self) {
        runtime::start_enabled_nvidia_services();
    }

    fn unbind_framebuffers(
        &self,
        nvidia: &[GraphicsDevice],
    ) -> Result<ConsoleState, GraphicsDeviceError> {
        Ok(runtime::unbind_framebuffers(nvidia))
    }

    fn rebind_framebuffers(&self, state: &ConsoleState) {
        runtime::rebind_framebuffers(state);
    }

    fn check_device_users_idle(
        &self,
        nvidia: &[GraphicsDevice],
    ) -> Result<(), GraphicsDeviceError> {
        runtime::check_device_users_idle(nvidia)
    }

    fn kill_nvidia_device_users(&self) {
        runtime::kill_nvidia_device_users();
    }

    fn unload_module(&self, module: &'static str) -> Result<(), GraphicsDeviceError> {
        crate::modprobe::unload(module)
            .map_err(|why| GraphicsDeviceError::ModuleUnload { module, why })
    }

    fn loaded_nvidia_modules(&self) -> Result<Vec<String>, GraphicsDeviceError> {
        let modules = Module::all().map_err(GraphicsDeviceError::ModulesFetch)?;
        Ok(modules
            .into_iter()
            .map(|m| m.name)
            .filter(|name| NVIDIA_MODULES.contains(&name.as_str()))
            .collect())
    }

    fn load_module(&self, module: &'static str, options: &[&str]) -> Result<(), GraphicsDeviceError> {
        crate::modprobe::load(module, options)
            .map_err(|why| GraphicsDeviceError::ModuleLoad { module, why })
    }

    fn load_modules_for_mode(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError> {
        runtime::load_modules_for_mode(mode)
    }

    fn unbind_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError> {
        unsafe { dev.unbind() }
    }

    fn remove_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError> {
        unsafe { dev.remove() }
    }

    fn write_vendor_config(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError> {
        super::config::write_vendor_config(mode)
    }

    fn snapshot_config(&self) -> Result<ConfigSnapshot, GraphicsDeviceError> {
        ConfigSnapshot::capture()
    }

    fn restore_config(&self, snapshot: &ConfigSnapshot) -> Result<(), GraphicsDeviceError> {
        snapshot.restore()
    }

    fn rescan_pci(&self) -> Result<(), GraphicsDeviceError> {
        let bus = crate::pci::PciBus::new().map_err(GraphicsDeviceError::Rescan)?;
        bus.rescan().map_err(GraphicsDeviceError::Rescan)
    }

    fn udev_settle(&self) -> Result<(), GraphicsDeviceError> {
        runtime::udev_settle()
    }

    fn wait_for_device(&self, dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
        runtime::wait_for_device(dev_id, timeout_ms)
    }

    fn wait_for_drm_card(&self, dev_id: &str, timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
        runtime::wait_for_drm_card(dev_id, timeout_ms)
    }

    fn verify_modeset(&self) -> Result<bool, GraphicsDeviceError> {
        runtime::verify_modeset()
    }

    fn sysfs_power_control(&self, pciid: String, mode: GraphicsMode) {
        runtime::sysfs_power_control(pciid, mode);
    }

    fn sleep_ms(&self, ms: u64) {
        thread::sleep(time::Duration::from_millis(ms));
    }

    fn get_vendor(&self, graphics: &Graphics) -> Result<GraphicsMode, GraphicsDeviceError> {
        graphics.get_vendor()
    }

    fn probe_live_state(&self, graphics: &Graphics) -> Result<LiveState, GraphicsDeviceError> {
        LiveState::probe(graphics)
    }

    fn refresh_inventory(&self, graphics: &mut Graphics) -> Result<(), GraphicsDeviceError> {
        graphics.refresh_devices().map_err(GraphicsDeviceError::Rescan)?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct RecordingOps {
        pub calls: std::sync::Mutex<Vec<String>>,
        pub dm: std::sync::Mutex<Option<String>>,
        pub fail_on_op: std::sync::Mutex<Option<&'static str>>,
        pub vendor: std::sync::Mutex<GraphicsMode>,
        pub live_state: std::sync::Mutex<LiveState>,
        /// Modules reported loaded after a teardown unload.
        pub loaded_modules: std::sync::Mutex<Vec<String>>,
        /// Number of `start_display_manager` calls that fail before one succeeds.
        pub dm_start_failures: std::sync::Mutex<usize>,
    }

    impl RecordingOps {
        pub fn new(vendor: GraphicsMode, live_state: LiveState, dm: Option<String>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                dm: std::sync::Mutex::new(dm),
                fail_on_op: std::sync::Mutex::new(None),
                vendor: std::sync::Mutex::new(vendor),
                live_state: std::sync::Mutex::new(live_state),
                loaded_modules: std::sync::Mutex::new(Vec::new()),
                dm_start_failures: std::sync::Mutex::new(0),
            }
        }

        pub fn record(&self, msg: &str) {
            self.calls.lock().unwrap().push(msg.to_string());
        }

        pub fn check_fail(&self, op: &str) -> Result<(), GraphicsDeviceError> {
            if let Some(fail) = *self.fail_on_op.lock().unwrap() {
                if fail == op {
                    return Err(GraphicsDeviceError::Command {
                        cmd: "mock",
                        why: io::Error::other(format!("injected failure on {}", op)),
                    });
                }
            }
            Ok(())
        }
    }

    impl SwitchOps for RecordingOps {
        fn detect_display_manager(&self) -> Option<String> {
            self.record("detect_display_manager");
            self.dm.lock().unwrap().clone()
        }

        fn stop_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("stop_display_manager:{}", dm));
            self.check_fail("stop_display_manager")
        }

        fn start_display_manager(&self, dm: &str) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("start_display_manager:{}", dm));
            let mut failures = self.dm_start_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(GraphicsDeviceError::DisplayManagerStart {
                    dm: dm.to_owned(),
                    why: io::Error::other("injected start failure"),
                });
            }
            Ok(())
        }

        fn stop_nvidia_services(&self) {
            self.record("stop_nvidia_services");
        }

        fn start_enabled_nvidia_services(&self) {
            self.record("start_enabled_nvidia_services");
        }

        fn unbind_framebuffers(
            &self,
            _nvidia: &[GraphicsDevice],
        ) -> Result<ConsoleState, GraphicsDeviceError> {
            self.record("unbind_framebuffers");
            self.check_fail("unbind_framebuffers")?;
            Ok(ConsoleState { unbound_vtcons: vec![path::PathBuf::from("/mock/vtcon1/bind")] })
        }

        fn rebind_framebuffers(&self, _state: &ConsoleState) {
            self.record("rebind_framebuffers");
        }

        fn check_device_users_idle(
            &self,
            _nvidia: &[GraphicsDevice],
        ) -> Result<(), GraphicsDeviceError> {
            self.record("check_device_users_idle");
            self.check_fail("check_device_users_idle")
        }

        fn kill_nvidia_device_users(&self) {
            self.record("kill_nvidia_device_users");
        }

        fn unload_module(&self, module: &'static str) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("unload_module:{}", module));
            self.check_fail(&format!("unload_module:{}", module))
        }

        fn loaded_nvidia_modules(&self) -> Result<Vec<String>, GraphicsDeviceError> {
            self.record("loaded_nvidia_modules");
            Ok(self.loaded_modules.lock().unwrap().clone())
        }

        fn load_module(&self, module: &'static str, options: &[&str]) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("load_module:{}:{:?}", module, options));
            self.check_fail(&format!("load_module:{}", module))
        }

        fn load_modules_for_mode(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("load_modules_for_mode:{:?}", mode));
            self.check_fail(&format!("load_modules_for_mode:{:?}", mode))
        }

        fn unbind_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("unbind_device:{}", dev.id()));
            self.check_fail("unbind_device")
        }

        fn remove_device(&self, dev: &GraphicsDevice) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("remove_device:{}", dev.id()));
            self.check_fail("remove_device")
        }

        fn write_vendor_config(&self, mode: GraphicsMode) -> Result<(), GraphicsDeviceError> {
            self.record(&format!("write_vendor_config:{:?}", mode));
            self.check_fail("write_vendor_config")
        }

        fn snapshot_config(&self) -> Result<ConfigSnapshot, GraphicsDeviceError> {
            self.record("snapshot_config");
            self.check_fail("snapshot_config")?;
            Ok(ConfigSnapshot::default())
        }

        fn restore_config(&self, _snapshot: &ConfigSnapshot) -> Result<(), GraphicsDeviceError> {
            self.record("restore_config");
            self.check_fail("restore_config")
        }

        fn rescan_pci(&self) -> Result<(), GraphicsDeviceError> {
            self.record("rescan_pci");
            self.check_fail("rescan_pci")
        }

        fn udev_settle(&self) -> Result<(), GraphicsDeviceError> {
            self.record("udev_settle");
            self.check_fail("udev_settle")
        }

        fn wait_for_device(&self, dev_id: &str, _timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
            self.record(&format!("wait_for_device:{}", dev_id));
            if let Some(fail) = *self.fail_on_op.lock().unwrap() {
                if fail == "wait_for_device" {
                    return Ok(false);
                }
            }
            Ok(true)
        }

        fn wait_for_drm_card(&self, dev_id: &str, _timeout_ms: u64) -> Result<bool, GraphicsDeviceError> {
            self.record(&format!("wait_for_drm_card:{}", dev_id));
            Ok(true)
        }

        fn verify_modeset(&self) -> Result<bool, GraphicsDeviceError> {
            self.record("verify_modeset");
            if let Some(fail) = *self.fail_on_op.lock().unwrap() {
                if fail == "verify_modeset" {
                    return Ok(false);
                }
            }
            Ok(true)
        }

        fn sysfs_power_control(&self, pciid: String, mode: GraphicsMode) {
            self.record(&format!("sysfs_power_control:{}:{:?}", pciid, mode));
        }

        fn sleep_ms(&self, ms: u64) {
            self.record(&format!("sleep_ms:{}", ms));
        }

        fn get_vendor(&self, _graphics: &Graphics) -> Result<GraphicsMode, GraphicsDeviceError> {
            Ok(*self.vendor.lock().unwrap())
        }

        fn probe_live_state(&self, _graphics: &Graphics) -> Result<LiveState, GraphicsDeviceError> {
            Ok(self.live_state.lock().unwrap().clone())
        }

        fn refresh_inventory(&self, _graphics: &mut Graphics) -> Result<(), GraphicsDeviceError> {
            self.record("refresh_inventory");
            self.check_fail("refresh_inventory")
        }
    }

    #[test]
    fn test_planner_from_integrated_to_hybrid_skips_teardown() {
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };

        // Under Auto policy, teardown is false and target is Hybrid, so restart_dm is false!
        let outcome = plan_transition(
            GraphicsMode::Integrated,
            GraphicsMode::Hybrid,
            &live,
            RestartPolicy::Auto,
            true, // external_display_requires_nvidia is ignored because teardown is false
        );

        match outcome {
            PlanOutcome::Plan(plan) => {
                assert!(!plan.teardown, "teardown must be skipped when coming from clean integrated");
                assert!(plan.bring_up, "bring_up must be true for hybrid");
                assert!(!plan.restart_dm, "auto policy must not restart DM when teardown is false and target is hybrid");
            }
            other => panic!("expected Plan, got {:?}", other),
        }
    }

    #[test]
    fn test_planner_always_policy_restarts_dm() {
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Integrated,
            GraphicsMode::Hybrid,
            &live,
            RestartPolicy::Always,
            false,
        );

        match outcome {
            PlanOutcome::Plan(plan) => {
                assert!(plan.restart_dm, "Always policy must restart DM");
                assert!(!plan.teardown);
                assert!(plan.bring_up);
            }
            other => panic!("expected Plan, got {:?}", other),
        }
    }

    #[test]
    fn test_planner_never_policy_never_restarts_dm() {
        let live = LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Hybrid,
            GraphicsMode::Integrated,
            &live,
            RestartPolicy::Never,
            false,
        );

        match outcome {
            PlanOutcome::Plan(plan) => {
                assert!(!plan.restart_dm, "Never policy must not restart DM");
                assert!(plan.teardown);
                assert!(!plan.bring_up);
            }
            other => panic!("expected Plan, got {:?}", other),
        }
    }

    #[test]
    fn test_planner_rejects_teardown_when_dgpu_display_connected() {
        let live = LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: Some("card1-DP-1".to_string()),
        };

        let outcome = plan_transition(
            GraphicsMode::Hybrid,
            GraphicsMode::Integrated,
            &live,
            RestartPolicy::Auto,
            false,
        );

        match outcome {
            PlanOutcome::Rejected(GraphicsDeviceError::DgpuDisplayActive { connector }) => {
                assert_eq!(connector, "card1-DP-1");
            }
            other => panic!("expected Rejected(DgpuDisplayActive), got {:?}", other),
        }
    }

    #[test]
    fn test_planner_rejects_teardown_when_static_model_requires_nvidia() {
        let live = LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Hybrid,
            GraphicsMode::Integrated,
            &live,
            RestartPolicy::Auto,
            true, // static model requires nvidia
        );

        match outcome {
            PlanOutcome::Rejected(GraphicsDeviceError::NotSwitchable) => {}
            other => panic!("expected Rejected(NotSwitchable), got {:?}", other),
        }
    }

    #[test]
    fn test_planner_no_change_when_already_in_state() {
        let live_integrated = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Integrated,
            GraphicsMode::Integrated,
            &live_integrated,
            RestartPolicy::Auto,
            false,
        );
        assert!(outcome.is_no_change());

        let live_hybrid = LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Hybrid,
            GraphicsMode::Hybrid,
            &live_hybrid,
            RestartPolicy::Auto,
            false,
        );
        assert!(outcome.is_no_change());
    }

    #[test]
    fn test_dgpu_display_detection_in_mock_sysfs() {
        let temp_dir = std::env::temp_dir().join(format!("s76_test_drm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let drm = temp_dir.join("class/drm");
        fs::create_dir_all(&drm).unwrap();

        // Create card0 (iGPU) connector
        let card0_edp = drm.join("card0-eDP-1");
        fs::create_dir_all(&card0_edp).unwrap();
        fs::write(card0_edp.join("status"), "connected\n").unwrap();

        // Create card1 (dGPU) connector disconnected
        let card1_dp = drm.join("card1-DP-1");
        fs::create_dir_all(&card1_dp).unwrap();
        fs::write(card1_dp.join("status"), "disconnected\n").unwrap();

        let dummy_nvidia = vec![GraphicsDevice::new("0000:01:00.0".to_string(), 0x10de, vec![])];

        // Neither connector is connected on NVIDIA
        let detected = dgpu_display_connected_in(&drm, &dummy_nvidia);
        assert!(detected.is_none());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    fn mock_graphics() -> Graphics {
        Graphics {
            bus: crate::pci::PciBus::new().expect("pci bus"),
            amd: vec![],
            intel: vec![GraphicsDevice::new("0000:00:02.0".to_string(), 0x8086, vec![])],
            nvidia: vec![GraphicsDevice::new("0000:01:00.0".to_string(), 0x10de, vec![])],
            other: vec![],
        }
    }

    #[test]
    fn test_switch_runtime_integrated_to_hybrid_success() {
        let mut graphics = mock_graphics();
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };
        let ops = RecordingOps::new(GraphicsMode::Integrated, live, Some("gdm".to_string()));

        let report = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect("switch must succeed");

        assert_eq!(report.from, "integrated");
        assert_eq!(report.to, "hybrid");
        assert_eq!(report.dm_restarted, Some("gdm".to_string()));
        assert!(!report.no_change);

        let calls = ops.calls.lock().unwrap();
        assert!(calls.contains(&"stop_display_manager:gdm".to_string()));
        assert!(calls.contains(&"start_display_manager:gdm".to_string()));
        assert!(calls.contains(&"write_vendor_config:Hybrid".to_string()));
        assert!(calls.contains(&"load_modules_for_mode:Hybrid".to_string()));
    }

    #[test]
    fn test_switch_runtime_failure_triggers_rollback_and_dm_start() {
        let mut graphics = mock_graphics();
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };
        let ops = RecordingOps::new(GraphicsMode::Integrated, live, Some("gdm".to_string()));
        *ops.fail_on_op.lock().unwrap() = Some("load_modules_for_mode:Hybrid");

        let err = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect_err("switch must fail");

        match err {
            GraphicsDeviceError::RuntimeSwitch { stage, rolled_back, .. } => {
                assert_eq!(stage, SwitchStage::LoadModules);
                assert!(rolled_back, "rollback must be marked true");
            }
            other => panic!("expected RuntimeSwitch, got {:?}", other),
        }

        let calls = ops.calls.lock().unwrap();
        assert!(calls.contains(&"start_display_manager:gdm".to_string()));
        assert!(calls.contains(&"write_vendor_config:Integrated".to_string()));
    }

    #[test]
    fn test_switch_runtime_auto_policy_skips_dm_restart_for_clean_integrated() {
        let mut graphics = mock_graphics();
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };
        let ops = RecordingOps::new(GraphicsMode::Integrated, live, Some("gdm".to_string()));

        let report = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Auto)
            .expect("switch must succeed");

        assert_eq!(report.dm_restarted, None);

        let calls = ops.calls.lock().unwrap();
        assert!(!calls.iter().any(|c| c.starts_with("stop_display_manager")));
        assert!(!calls.iter().any(|c| c.starts_with("start_display_manager")));
        assert!(calls.contains(&"write_vendor_config:Hybrid".to_string()));
    }

    #[test]
    fn test_planner_discrete_rejected() {
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };

        let outcome = plan_transition(
            GraphicsMode::Integrated,
            GraphicsMode::Discrete,
            &live,
            RestartPolicy::Always,
            false,
        );

        match outcome {
            PlanOutcome::Rejected(GraphicsDeviceError::RuntimeDiscreteUnsupported) => {}
            other => panic!("expected Rejected(RuntimeDiscreteUnsupported), got {:?}", other),
        }
    }

    #[test]
    fn test_dgpu_display_detection_in_mock_sysfs_connected() {
        let temp_dir = std::env::temp_dir().join(format!("s76_test_drm_conn_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let drm = temp_dir.join("class/drm");
        let pci = temp_dir.join("bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&drm).unwrap();
        fs::create_dir_all(&pci).unwrap();

        // Create card1 directory and device symlink to NVIDIA PCI device
        let card1 = drm.join("card1");
        fs::create_dir_all(&card1).unwrap();
        std::os::unix::fs::symlink(&pci, card1.join("device")).unwrap();

        // Create card1-DP-1 connector with connected status
        let card1_dp = drm.join("card1-DP-1");
        fs::create_dir_all(&card1_dp).unwrap();
        fs::write(card1_dp.join("status"), "connected\n").unwrap();

        let dummy_nvidia = vec![GraphicsDevice::new("0000:01:00.0".to_string(), 0x10de, vec![])];

        let detected = dgpu_display_connected_in(&drm, &dummy_nvidia);
        assert_eq!(detected, Some("card1-DP-1".to_string()));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_switch_runtime_failure_at_rescan_triggers_rollback() {
        let mut graphics = mock_graphics();
        let live = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };
        let ops = RecordingOps::new(GraphicsMode::Integrated, live, Some("gdm".to_string()));
        *ops.fail_on_op.lock().unwrap() = Some("rescan_pci");

        let err = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect_err("switch must fail");

        match err {
            GraphicsDeviceError::RuntimeSwitch { stage, rolled_back, .. } => {
                assert_eq!(stage, SwitchStage::Rescan);
                assert!(rolled_back, "rollback must be marked true");
            }
            other => panic!("expected RuntimeSwitch, got {:?}", other),
        }

        let calls = ops.calls.lock().unwrap();
        assert!(calls.contains(&"start_display_manager:gdm".to_string()));
        assert!(calls.contains(&"write_vendor_config:Integrated".to_string()));
        assert!(calls.contains(&"rebind_framebuffers".to_string()));
    }

    #[test]
    fn test_planner_full_matrix() {
        let live_clean = LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        };
        let live_loaded = LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: None,
        };

        let modes = [GraphicsMode::Integrated, GraphicsMode::Hybrid, GraphicsMode::Compute];

        for &from in &modes {
            for &to in &modes {
                let live = if from == GraphicsMode::Integrated { &live_clean } else { &live_loaded };
                let outcome = plan_transition(from, to, live, RestartPolicy::Auto, false);
                if from == to {
                    assert!(outcome.is_no_change(), "from == to should be NoChange");
                } else {
                    match outcome {
                        PlanOutcome::Plan(plan) => {
                            if from == GraphicsMode::Integrated {
                                assert!(!plan.teardown);
                            } else {
                                assert!(plan.teardown);
                            }
                            if to == GraphicsMode::Integrated {
                                assert!(!plan.bring_up);
                            } else {
                                assert!(plan.bring_up);
                            }
                        }
                        other => panic!("unexpected outcome: {:?}", other),
                    }
                }
            }
        }
    }

    fn live_loaded() -> LiveState {
        LiveState {
            nvidia_modules_loaded: true,
            dgpu_driver_bound: true,
            dgpu_display_connected: None,
        }
    }

    fn live_clean() -> LiveState {
        LiveState {
            nvidia_modules_loaded: false,
            dgpu_driver_bound: false,
            dgpu_display_connected: None,
        }
    }

    fn gdm() -> Option<String> { Some("gdm".to_string()) }

    fn position(calls: &[String], call: &str) -> usize {
        calls
            .iter()
            .position(|c| c == call)
            .unwrap_or_else(|| panic!("{} not called: {:?}", call, calls))
    }

    #[test]
    fn test_dm_restart_failure_is_retried_then_reported() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Integrated, live_clean(), gdm());
        *ops.dm_start_failures.lock().unwrap() = 2;

        let report = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect("the switch itself succeeded");

        assert_eq!(report.dm_restarted, None);
        assert!(report.dm_restart_error.is_some(), "restart failure must be reported");

        let calls = ops.calls.lock().unwrap();
        let starts = calls.iter().filter(|c| *c == "start_display_manager:gdm").count();
        assert_eq!(starts, 2, "start must be retried exactly once");
    }

    #[test]
    fn test_dm_restart_succeeds_on_retry() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Integrated, live_clean(), gdm());
        *ops.dm_start_failures.lock().unwrap() = 1;

        let report = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect("switch must succeed");

        assert_eq!(report.dm_restarted, Some("gdm".to_string()));
        assert_eq!(report.dm_restart_error, None);
    }

    #[test]
    fn test_dm_restart_failure_after_failed_switch_is_reported() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Integrated, live_clean(), gdm());
        *ops.fail_on_op.lock().unwrap() = Some("rescan_pci");
        *ops.dm_start_failures.lock().unwrap() = 2;

        match graphics.switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always) {
            Err(GraphicsDeviceError::RuntimeSwitch { dm_restart_error, .. }) => {
                assert!(dm_restart_error.is_some());
            }
            other => panic!("expected RuntimeSwitch, got {:?}", other),
        }
    }

    #[test]
    fn test_modules_still_loaded_aborts_teardown_and_rolls_back() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Hybrid, live_loaded(), gdm());
        *ops.loaded_modules.lock().unwrap() = vec!["nvidia".to_string()];

        let err = graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Integrated, RestartPolicy::Always)
            .expect_err("switch must fail while nvidia is still loaded");

        match err {
            GraphicsDeviceError::RuntimeSwitch { stage, why, .. } => {
                assert_eq!(stage, SwitchStage::Teardown);
                let still_loaded = match *why {
                    GraphicsDeviceError::ModulesStillLoaded { ref modules } => modules.clone(),
                    ref other => panic!("unexpected cause: {:?}", other),
                };
                assert_eq!(still_loaded, ["nvidia"]);
            }
            other => panic!("expected RuntimeSwitch, got {:?}", other),
        }

        let calls = ops.calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.starts_with("unbind_device")),
            "must not unbind with modules loaded"
        );
        assert!(!calls.contains(&"write_vendor_config:Integrated".to_string()));
        assert!(
            calls.contains(&"load_modules_for_mode:Hybrid".to_string()),
            "rollback must reload the driver"
        );
        assert!(calls.contains(&"start_display_manager:gdm".to_string()));
    }

    #[test]
    fn test_individual_unload_failures_are_tolerated_when_modules_are_gone() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Hybrid, live_loaded(), gdm());
        // `modprobe -r nvidia_drm` already took nvidia_modeset with it.
        *ops.fail_on_op.lock().unwrap() = Some("unload_module:nvidia_modeset");

        graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Integrated, RestartPolicy::Always)
            .expect("switch must succeed once no module remains");

        let calls = ops.calls.lock().unwrap();
        let verified = position(&calls, "loaded_nvidia_modules");
        assert!(verified < position(&calls, "unbind_device:0000:01:00.0"));
    }

    #[test]
    fn test_rollback_restores_config_snapshot_after_hardware() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Integrated, live_clean(), gdm());
        *ops.fail_on_op.lock().unwrap() = Some("load_modules_for_mode:Hybrid");

        let _ = graphics.switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always);

        let calls = ops.calls.lock().unwrap();
        // Snapshot precedes every side effect.
        assert!(position(&calls, "snapshot_config") < position(&calls, "stop_display_manager:gdm"));
        // Live-mode config is written so the hardware rollback can load modules,
        // then the exact snapshot is put back last.
        let live_config =
            calls.iter().rposition(|c| c == "write_vendor_config:Integrated").unwrap();
        assert!(live_config < position(&calls, "restore_config"));
        assert!(
            calls.iter().rposition(|c| c.starts_with("unload_module")).unwrap()
                < position(&calls, "restore_config")
        );
    }

    #[test]
    fn test_snapshot_failure_aborts_before_side_effects() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Integrated, live_clean(), gdm());
        *ops.fail_on_op.lock().unwrap() = Some("snapshot_config");

        graphics
            .switch_runtime_with_ops(&ops, GraphicsMode::Hybrid, RestartPolicy::Always)
            .expect_err("switch must fail");

        let calls = ops.calls.lock().unwrap();
        assert_eq!(*calls, vec!["snapshot_config".to_string()]);
    }

    #[test]
    fn test_failed_restore_marks_rollback_incomplete() {
        let mut graphics = mock_graphics();
        let ops = RecordingOps::new(GraphicsMode::Hybrid, live_loaded(), None);
        *ops.fail_on_op.lock().unwrap() = Some("restore_config");

        let snapshot = ConfigSnapshot::default();
        let result = graphics.rollback_with_ops(&ops, GraphicsMode::Hybrid, &snapshot);

        assert!(result.is_err(), "a failed config restore must not count as rolled back");
        assert!(ops.calls.lock().unwrap().contains(&"load_modules_for_mode:Hybrid".to_string()));
    }
}
