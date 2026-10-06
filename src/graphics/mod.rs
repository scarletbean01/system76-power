// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

mod config;
mod device;
mod error;
mod mode;
mod nvidia;
mod runtime;
mod systemd;
pub(crate) mod transition;

pub(crate) use config::update_initramfs_cmd;
pub use device::GraphicsDevice;
pub use error::GraphicsDeviceError;
pub use mode::GraphicsMode;
pub use transition::{RestartPolicy, SwitchReport, SwitchStage};

use crate::{module::Module, pci::PciBus};
use std::{fs, io};
use sysfs_class::{PciDevice, SysClass};

use config::EXTERNAL_DISPLAY_REQUIRES_NVIDIA;

pub struct Graphics {
    pub bus: PciBus,
    pub amd: Vec<GraphicsDevice>,
    pub intel: Vec<GraphicsDevice>,
    pub nvidia: Vec<GraphicsDevice>,
    pub other: Vec<GraphicsDevice>,
}

type ProbedDevices = (
    Vec<GraphicsDevice>,
    Vec<GraphicsDevice>,
    Vec<GraphicsDevice>,
    Vec<GraphicsDevice>,
);

fn probe_devices() -> io::Result<ProbedDevices> {
    let devs = PciDevice::all()?;

    let functions = |parent: &PciDevice| -> Vec<PciDevice> {
        let mut functions = Vec::new();
        if let Some(parent_slot) = parent.id().split('.').next() {
            for func in &devs {
                if let Some(func_slot) = func.id().split('.').next() {
                    if func_slot == parent_slot {
                        log::info!("{}: Function for {}", func.id(), parent.id());
                        functions.push(func.clone());
                    }
                }
            }
        }
        functions
    };

    let mut amd = Vec::new();
    let mut intel = Vec::new();
    let mut nvidia = Vec::new();
    let mut other = Vec::new();
    for dev in &devs {
        let c = dev.class()?;
        if (c >> 16) & 0xFF == 0x03 {
            match dev.vendor()? {
                0x1002 => {
                    log::info!("{}: AMD graphics", dev.id());
                    amd.push(GraphicsDevice::new(
                        dev.id().to_owned(),
                        dev.device()?,
                        functions(dev),
                    ));
                }
                0x10DE => {
                    log::info!("{}: NVIDIA graphics", dev.id());
                    nvidia.push(GraphicsDevice::new(
                        dev.id().to_owned(),
                        dev.device()?,
                        functions(dev),
                    ));
                }
                0x8086 => {
                    log::info!("{}: Intel graphics", dev.id());
                    intel.push(GraphicsDevice::new(
                        dev.id().to_owned(),
                        dev.device()?,
                        functions(dev),
                    ));
                }
                vendor => {
                    log::info!("{}: Other({:X}) graphics", dev.id(), vendor);
                    other.push(GraphicsDevice::new(
                        dev.id().to_owned(),
                        dev.device()?,
                        functions(dev),
                    ));
                }
            }
        }
    }

    Ok((amd, intel, nvidia, other))
}

impl Graphics {
    pub fn new() -> io::Result<Self> {
        let bus = PciBus::new()?;

        log::info!("Rescanning PCI bus");
        bus.rescan()?;

        let (amd, intel, nvidia, other) = probe_devices()?;
        Ok(Self { bus, amd, intel, nvidia, other })
    }

    pub fn refresh_devices(&mut self) -> io::Result<()> {
        let (amd, intel, nvidia, other) = probe_devices()?;
        self.amd = amd;
        self.intel = intel;
        self.nvidia = nvidia;
        self.other = other;
        Ok(())
    }

    pub fn is_desktop(&self) -> bool {
        let chassis = fs::read_to_string("/sys/class/dmi/id/chassis_type")
            .map_err(GraphicsDeviceError::SysFs)
            .unwrap_or_default();
        chassis.trim() == "3"
    }

    #[must_use]
    pub fn can_switch(&self) -> bool {
        !self.is_desktop()
            && (!self.nvidia.is_empty() && (!self.intel.is_empty() || !self.amd.is_empty()))
    }

    pub fn get_external_displays_require_dgpu(&self) -> Result<bool, GraphicsDeviceError> {
        self.switchable_or_fail()?;
        let model = match fs::read_to_string("/sys/class/dmi/id/product_version") {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(GraphicsDeviceError::SysFs(e)),
        };
        Ok(EXTERNAL_DISPLAY_REQUIRES_NVIDIA.contains(&model.trim()))
    }

    pub fn get_default_graphics(&self) -> Result<GraphicsMode, GraphicsDeviceError> {
        // Models that should default to discrete graphics only
        const DEFAULT_DISCRETE: &[&str] = &["bonw16"];

        self.switchable_or_fail()?;

        let vendor = fs::read_to_string("/sys/class/dmi/id/sys_vendor")
            .map_err(GraphicsDeviceError::SysFs)
            .map(|s| s.trim().to_string())?;

        let product = fs::read_to_string("/sys/class/dmi/id/product_version")
            .map_err(GraphicsDeviceError::SysFs)
            .map(|s| s.trim().to_string())?;

        let runtimepm = match nvidia::gpu_supports_runtimepm(&self.nvidia) {
            Ok(ok) => ok,
            Err(err) => {
                log::warn!("could not determine GPU runtimepm support: {}", err);
                false
            }
        };

        // Only default to hybrid on System76 models
        if vendor != "System76" || DEFAULT_DISCRETE.contains(&product.as_str()) {
            Ok(GraphicsMode::Discrete)
        } else if runtimepm {
            Ok(GraphicsMode::Hybrid)
        } else {
            Ok(GraphicsMode::Integrated)
        }
    }

    pub fn get_vendor(&self) -> Result<GraphicsMode, GraphicsDeviceError> {
        let modules = Module::all().map_err(GraphicsDeviceError::ModulesFetch)?;
        let vendor =
            if modules.iter().any(|module| module.name == "nouveau" || module.name == "nvidia") {
                let mode = match config::get_prime_discrete() {
                    Ok(m) => m,
                    Err(_) => "nvidia".to_string(),
                };

                if mode == "on-demand" {
                    GraphicsMode::Hybrid
                } else if mode == "off" {
                    GraphicsMode::Compute
                } else {
                    GraphicsMode::Discrete
                }
            } else {
                GraphicsMode::Integrated
            };
        Ok(vendor)
    }

    /// Set the graphics mode (boot-time path).
    ///
    /// Writes all configuration files and rebuilds the initramfs. A reboot is
    /// required for the change to take effect. The existing behaviour is fully
    /// preserved.
    pub fn set_vendor(&self, vendor: GraphicsMode) -> Result<(), GraphicsDeviceError> {
        self.switchable_or_fail()?;
        config::write_vendor_config(vendor)?;

        log::info!("Updating initramfs");
        let (cmd, arg) = config::update_initramfs_cmd();
        let status = std::process::Command::new(cmd)
            .arg(arg)
            .status()
            .map_err(|why| GraphicsDeviceError::Command { cmd, why })?;

        if !status.success() {
            return Err(GraphicsDeviceError::UpdateInitramfs(status));
        }

        Ok(())
    }

    /// Switch graphics mode at runtime without a reboot.
    ///
    /// The switch is planned dynamically using live system state:
    /// - From integrated: skips unnecessary driver teardown and display manager restart (when auto).
    /// - Checks whether external displays or internal panels are currently active on the discrete GPU.
    /// - Guarantees the display manager is always restored even on error or panic.
    /// - Automatically rolls back configuration and device state if an intermediate step fails.
    pub fn switch_runtime(&mut self, vendor: GraphicsMode) -> Result<SwitchReport, GraphicsDeviceError> {
        let policy = crate::config::current().graphics.runtime_restart_dm;
        self.switch_runtime_with_ops(&transition::RealOps, vendor, policy)
    }

    pub(crate) fn switch_runtime_with_ops<O: transition::SwitchOps>(
        &mut self,
        ops: &O,
        target: GraphicsMode,
        policy: RestartPolicy,
    ) -> Result<SwitchReport, GraphicsDeviceError> {
        self.switchable_or_fail()?;

        let from = ops.get_vendor(self)?;
        let live = ops.probe_live_state(self)?;
        let external_displays_require_dgpu = self.get_external_displays_require_dgpu()?;

        let plan = match transition::plan_transition(
            from,
            target,
            &live,
            policy,
            external_displays_require_dgpu,
        ) {
            transition::PlanOutcome::NoChange => {
                log::info!("Already in {:?} graphics mode with matching state", target);
                return Ok(SwitchReport {
                    from: from.into(),
                    to: target.into(),
                    dm_restarted: None,
                    stage_reached: SwitchStage::Done,
                    no_change: true,
                });
            }
            transition::PlanOutcome::Rejected(err) => return Err(err),
            transition::PlanOutcome::Plan(p) => p,
        };

        let dm_to_manage = if plan.restart_dm {
            ops.detect_display_manager()
        } else {
            None
        };

        if dm_to_manage.is_none() && plan.teardown {
            ops.stop_nvidia_services();
            if let Err(why) = ops.check_device_users_idle(&self.nvidia) {
                ops.start_enabled_nvidia_services();
                return Err(why);
            }
        }

        let mut dm_guard = runtime::DmGuard::new(dm_to_manage.clone());
        let mut console_state = runtime::ConsoleState::default();
        let mut current_stage = SwitchStage::Initial;

        if let Some(ref dm) = dm_to_manage {
            current_stage = SwitchStage::StopDm;
            if let Err(why) = ops.stop_display_manager(dm) {
                if let Some(dm_name) = dm_guard.unit() {
                    let _ = ops.start_display_manager(dm_name);
                    dm_guard.mark_restored();
                }
                return Err(GraphicsDeviceError::RuntimeSwitch {
                    target: target.into(),
                    stage: SwitchStage::StopDm,
                    why: Box::new(why),
                    rolled_back: false,
                });
            }
            ops.sleep_ms(2000);
        }

        let result = self.apply_plan(
            ops,
            plan,
            target,
            &mut current_stage,
            &mut console_state,
        );

        match result {
            Ok(()) => {
                ops.rebind_framebuffers(&console_state);
                let dm_restarted = if let Some(dm) = dm_guard.unit().map(|s| s.to_string()) {
                    current_stage = SwitchStage::StartDm;
                    let res = ops.start_display_manager(&dm);
                    dm_guard.mark_restored();
                    if res.is_ok() {
                        Some(dm)
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok(SwitchReport {
                    from: from.into(),
                    to: target.into(),
                    dm_restarted,
                    stage_reached: SwitchStage::Done,
                    no_change: false,
                })
            }
            Err(err) => {
                log::error!("Runtime graphics switch failed at {:?}: {}", current_stage, err);
                let rolled_back = self.rollback_with_ops(ops, from).is_ok();
                ops.rebind_framebuffers(&console_state);
                if let Some(dm) = dm_guard.unit().map(|s| s.to_string()) {
                    let _ = ops.start_display_manager(&dm);
                    dm_guard.mark_restored();
                }
                Err(GraphicsDeviceError::RuntimeSwitch {
                    target: target.into(),
                    stage: current_stage,
                    why: Box::new(err),
                    rolled_back,
                })
            }
        }
    }

    fn apply_plan<O: transition::SwitchOps>(
        &mut self,
        ops: &O,
        plan: transition::TransitionPlan,
        target: GraphicsMode,
        stage: &mut SwitchStage,
        console_state: &mut runtime::ConsoleState,
    ) -> Result<(), GraphicsDeviceError> {
        // Phase 1: Teardown
        if plan.teardown {
            *stage = SwitchStage::Teardown;
            ops.stop_nvidia_services();
            *console_state = ops.unbind_framebuffers(&self.nvidia)?;
            ops.kill_nvidia_device_users();

            for module in &["nvidia-drm", "nvidia-modeset", "nvidia_uvm", "nvidia"] {
                if let Err(why) = ops.unload_module(module) {
                    log::warn!("Could not unload {}: {} (continuing)", module, why);
                }
            }

            for dev in &self.nvidia {
                ops.unbind_device(dev)?;
            }

            for dev in &self.nvidia {
                ops.remove_device(dev)?;
            }
        }

        // Phase 2: Configuration
        *stage = SwitchStage::WriteConfig;
        ops.write_vendor_config(target)?;

        // Phase 3: Bring-up
        if plan.bring_up {
            *stage = SwitchStage::Rescan;
            let target_pci_id = self.nvidia.first().map(|d| d.id.clone());
            ops.rescan_pci()?;
            let _ = ops.udev_settle();

            if let Some(ref pci_id) = target_pci_id {
                let found = ops.wait_for_device(pci_id, 5000)?;
                if !found {
                    return Err(GraphicsDeviceError::DeviceNotFound { id: pci_id.clone() });
                }
            }

            // Refresh device inventory
            let old_nvidia = self.nvidia.clone();
            ops.refresh_inventory(self)?;

            if self.nvidia.is_empty() {
                self.nvidia = old_nvidia;
                let missing_id = target_pci_id.unwrap_or_else(|| "0000:01:00.0".to_string());
                return Err(GraphicsDeviceError::DeviceNotFound { id: missing_id });
            }

            let first_pci = self.nvidia.first().map(|d| d.id.clone());

            *stage = SwitchStage::LoadModules;
            ops.load_modules_for_mode(target)?;

            if target == GraphicsMode::Hybrid || target == GraphicsMode::Discrete {
                let modeset_ok = ops.verify_modeset()?;
                if !modeset_ok {
                    log::warn!("nvidia-drm modeset not active, attempting reload");
                    let _ = ops.unload_module("nvidia-drm");
                    ops.load_module("nvidia-drm", &["modeset=1"])?;
                    if !ops.verify_modeset()? {
                        return Err(GraphicsDeviceError::ModesetDisabled);
                    }
                }

                if let Some(ref pci_id) = first_pci {
                    let _ = ops.wait_for_drm_card(pci_id, 5000);
                }
            }

            *stage = SwitchStage::StartServices;
            ops.start_enabled_nvidia_services();

            if let Some(pci_id) = first_pci {
                ops.sysfs_power_control(pci_id, target);
            }
        }

        Ok(())
    }

    fn rollback_with_ops<O: transition::SwitchOps>(
        &mut self,
        ops: &O,
        prev: GraphicsMode,
    ) -> Result<(), GraphicsDeviceError> {
        log::warn!("Rolling back graphics state to {:?}", prev);
        let mut success = true;

        if let Err(e) = ops.write_vendor_config(prev) {
            log::error!("Failed to restore config for {:?}: {}", prev, e);
            success = false;
        }

        if prev == GraphicsMode::Integrated {
            ops.stop_nvidia_services();
            ops.kill_nvidia_device_users();
            for module in &["nvidia-drm", "nvidia-modeset", "nvidia_uvm", "nvidia"] {
                if let Err(why) = ops.unload_module(module) {
                    log::warn!("Rollback failed to unload {}: {}", module, why);
                }
            }
            let runtimepm = nvidia::gpu_supports_runtimepm(&self.nvidia).unwrap_or(false);
            if !runtimepm {
                for dev in &self.nvidia {
                    if ops.unbind_device(dev).is_err() {
                        success = false;
                    }
                    if ops.remove_device(dev).is_err() {
                        success = false;
                    }
                }
            }
        } else {
            if ops.rescan_pci().is_err() {
                success = false;
            }
            let _ = ops.udev_settle();
            let _ = ops.refresh_inventory(self);
            if let Err(e) = ops.load_modules_for_mode(prev) {
                log::warn!("Rollback failed to load modules for {:?}: {}", prev, e);
                success = false;
            }
            ops.start_enabled_nvidia_services();
        }

        if success {
            Ok(())
        } else {
            Err(GraphicsDeviceError::Command {
                cmd: "rollback",
                why: io::Error::other("rollback steps failed"),
            })
        }
    }

    pub fn get_power(&self) -> Result<bool, GraphicsDeviceError> {
        self.switchable_or_fail()?;
        Ok(self.nvidia.iter().any(GraphicsDevice::exists))
    }

    pub fn set_power(&self, power: bool) -> Result<(), GraphicsDeviceError> {
        self.switchable_or_fail()?;

        if power {
            log::info!("Enabling graphics power");
            self.bus.rescan().map_err(GraphicsDeviceError::Rescan)?;
            runtime::sysfs_power_control(self.nvidia[0].id.clone(), self.get_vendor()?);
        } else {
            log::info!("Disabling graphics power");

            // TODO: Don't allow turning off power if nvidia_drm modeset is enabled

            unsafe {
                // Unbind NVIDIA graphics devices and their functions
                let unbinds = self.nvidia.iter().map(|dev| dev.unbind());

                // Remove NVIDIA graphics devices and their functions
                let removes = self.nvidia.iter().map(|dev| dev.remove());

                unbinds.chain(removes).collect::<Result<(), _>>()?;
            }
        }

        Ok(())
    }

    pub fn auto_power(&self) -> Result<(), GraphicsDeviceError> {
        // Only disable power if in integrated mode and the device does not
        // support runtime power management.
        let vendor = self.get_vendor()?;
        let power =
            vendor != GraphicsMode::Integrated || nvidia::gpu_supports_runtimepm(&self.nvidia)?;
        self.set_power(power)
    }

    fn switchable_or_fail(&self) -> Result<(), GraphicsDeviceError> {
        if self.can_switch() { Ok(()) } else { Err(GraphicsDeviceError::NotSwitchable) }
    }
}
