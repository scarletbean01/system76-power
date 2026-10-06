// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

use anyhow::Context;
use std::{
    collections::HashMap,
    fmt::Display,
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::Mutex,
    time::sleep,
};
use zbus::Interface;

use crate::{
    charge_thresholds::{get_charge_profiles, get_charge_thresholds, set_charge_thresholds},
    errors::ProfileError,
    fan::FanDaemon,
    graphics::{update_initramfs_cmd, Graphics, GraphicsMode, SwitchStage},
    hid_backlight,
    hotplug::{mux, Detect, HotPlugDetect},
    kernel_parameters::{KernelParameter, NmiWatchdog},
    runtime_pm::{runtime_pm_quirks, thunderbolt_hotplug_wakeup},
    DBUS_NAME, DBUS_PATH,
};

mod profiles;
use self::profiles::{balanced, battery, performance};

use system76_power_zbus::ChargeProfile;

const THRESHOLD_POLICY: &str = "com.system76.powerdaemon.set-charge-thresholds";
const NET_HADESS_POWER_PROFILES_DBUS_NAME: &str = "net.hadess.PowerProfiles";
const NET_HADESS_POWER_PROFILES_DBUS_PATH: &str = "/net/hadess/PowerProfiles";
const POWER_PROFILES_DBUS_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const POWER_PROFILES_DBUS_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

static CONTINUE: AtomicBool = AtomicBool::new(true);
/// Set by SIGHUP handling; consumed by the main loop to re-apply the profile.
static RELOAD: AtomicBool = AtomicBool::new(false);

async fn signal_handling(
    mut int: tokio::signal::unix::Signal,
    mut hup: tokio::signal::unix::Signal,
    mut term: tokio::signal::unix::Signal,
) {
    loop {
        let sig = tokio::select! {
            _ = int.recv() => "SIGINT",
            _ = hup.recv() => "SIGHUP",
            _ = term.recv() => "SIGTERM",
        };

        log::info!("caught signal: {}", sig);

        match sig {
            "SIGHUP" => {
                crate::config::reload();
                log::info!("configuration reloaded");
                RELOAD.store(true, Ordering::SeqCst);
            }
            _ => {
                CONTINUE.store(false, Ordering::SeqCst);
                break;
            }
        }
    }
}

pub const GRAPHICS_SWITCH_STATUS_PATH: &str = "/run/system76-power/graphics-switch.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphicsSwitchStatus {
    pub timestamp: u64,
    pub from: String,
    pub to: String,
    pub in_progress: bool,
    pub success: bool,
    pub stage: Option<SwitchStage>,
    pub error: Option<String>,
    pub rolled_back: bool,
    pub dm_restarted: Option<String>,
    pub initramfs_success: Option<bool>,
}

fn record_switch_status(status: &GraphicsSwitchStatus) {
    if let Err(e) = (|| -> std::io::Result<()> {
        let dir = std::path::Path::new("/run/system76-power");
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(status)?;
        let tmp_path = dir.join("graphics-switch.json.tmp");
        let target_path = dir.join("graphics-switch.json");
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &target_path)?;
        Ok(())
    })() {
        log::warn!("Could not record switch status: {}", e);
    }
}

struct PowerDaemon {
    initial_set:                 bool,
    graphics:                    Arc<std::sync::Mutex<Graphics>>,
    switch_in_progress:          Arc<AtomicBool>,
    power_profile:               String,
    profile_errors:              Vec<ProfileError>,
    held_profiles:               Vec<(u32, &'static str, String, String)>,
    profile_ids:                 u32,
    connections:                 Option<(zbus::Connection, zbus::Connection, zbus::Connection)>,
    auto_switch_manual_override: bool,
    /// True once a high-refresh display mode/rate was applied on AC power.
    /// Cleared only when the charger is unplugged (PowerSource::Battery event).
    /// While set, profile switches will not override the display refresh rate.
    ac_display_locked:           bool,
}

impl PowerDaemon {
    fn new() -> anyhow::Result<Self> {
        let graphics = Arc::new(std::sync::Mutex::new(Graphics::new()?));
        let switch_in_progress = Arc::new(AtomicBool::new(false));
        let _ = crate::config::reload();

        Ok(Self {
            initial_set:                 false,
            graphics,
            switch_in_progress,
            power_profile:               String::new(),
            profile_errors:              Vec::new(),
            held_profiles:               Vec::new(),
            profile_ids:                 0,
            connections:                 None,
            auto_switch_manual_override: false,
            ac_display_locked:           false,
        })
    }

    async fn apply_profile(
        &mut self,
        context: &zbus::SignalContext<'_>,
        func: fn(&mut Vec<ProfileError>, bool),
        name: &str,
        force: bool,
    ) -> Result<(), String> {
        if !force && self.power_profile == name {
            log::info!("profile was already set");
            return Ok(());
        }

        let _res = System76Power::power_profile_switch(context, name).await;

        let config = crate::config::current();
        // Backlight/keyboard dimming is opt-in and only applies to the Battery
        // profile.  Other profiles never touch the user's brightness.
        let set_brightness = config.profile.dim_on_battery && name == "Battery";
        func(&mut self.profile_errors, set_brightness);

        self.power_profile = name.into();

        // Apply refresh rate if enabled
        let refresh = &config.refresh_rate;
        if refresh.enabled {
            let hz = match name {
                "Battery" => refresh.battery,
                "Balanced" => refresh.balanced,
                "Performance" => refresh.performance,
                _ => {
                    log::warn!("Unknown profile '{}', skipping refresh rate change", name);
                    0 // Will be skipped
                }
            };

            if hz > 0 {
                if self.ac_display_locked {
                    log::info!(
                        "AC display lock active — skipping refresh rate change to {}Hz for {} profile",
                        hz, name
                    );
                } else {
                    log::info!("Setting display refresh rate to {}Hz for {} profile", hz, name);
                    if let Err(e) = crate::display::set_refresh_rate(hz) {
                        log::warn!("Failed to set display refresh rate to {}Hz: {}", hz, e);
                    }
                }
            }
        }

        if self.profile_errors.is_empty() {
            Ok(())
        } else {
            let mut error_message = String::from("Errors found when setting profile:");
            for error in self.profile_errors.drain(..) {
                error_message = format!("{}\n    - {}", error_message, error);
            }

            Err(error_message)
        }
    }
}

#[derive(Clone)]
struct System76Power {
    daemon: Arc<Mutex<PowerDaemon>>,
    graphics: Arc<std::sync::Mutex<Graphics>>,
    switch_in_progress: Arc<AtomicBool>,
}

impl System76Power {
    fn lock_graphics(&self) -> zbus::fdo::Result<std::sync::MutexGuard<'_, Graphics>> {
        if self.switch_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("Graphics switch in progress".into()));
        }
        match self.graphics.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::WouldBlock) => {
                Err(zbus::fdo::Error::Failed("Graphics switch in progress".into()))
            }
            Err(std::sync::TryLockError::Poisoned(p)) => {
                log::warn!("Graphics mutex was poisoned; recovering");
                Ok(p.into_inner())
            }
        }
    }

    pub async fn emit_active_profile_changed(&self) {
        let (upp_connection, hadess_connection, profile) = {
            let this = self.daemon.lock().await;
            let Some((_, upp, hadess)) = this.connections.clone() else { return };

            let profile = system76_profile_to_upp_str(&this.power_profile);
            (upp, hadess, profile)
        };

        let value = zvariant::Value::Str(zvariant::Str::from(profile));
        let changed = HashMap::from_iter(std::iter::once(("ActiveProfile", &value)));
        let invalidated = &[];

        if let Ok(context) = zbus::SignalContext::new(&upp_connection, POWER_PROFILES_DBUS_PATH) {
            let _res = zbus::fdo::Properties::properties_changed(
                &context,
                UPowerPowerProfiles::name(),
                &changed,
                invalidated,
            )
            .await;
        }

        if let Ok(context) =
            zbus::SignalContext::new(&hadess_connection, NET_HADESS_POWER_PROFILES_DBUS_PATH)
        {
            let _res = zbus::fdo::Properties::properties_changed(
                &context,
                NetHadessPowerProfiles::name(),
                &changed,
                invalidated,
            )
            .await;
        }
    }
}

#[zbus::dbus_interface(name = "com.system76.PowerDaemon")]
impl System76Power {
    async fn battery(
        &mut self,
        #[zbus(signal_context)] context: zbus::SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        // Detect manual profile change - set override flag (but not during initial startup)
        {
            let mut daemon = self.daemon.lock().await;
            if crate::config::current().auto_switch
                && daemon.initial_set 
                && daemon.power_profile != "Battery" 
            {
                log::info!("Manual profile change to Battery detected, setting override flag");
                daemon.auto_switch_manual_override = true;
            }
        }
        
        let result = self
            .daemon
            .lock()
            .await
            .apply_profile(&context, battery, "Battery", false)
            .await
            .map_err(zbus_error_from_display);

        if result.is_ok() {
            self.emit_active_profile_changed().await
        }

        result
    }

    async fn balanced(
        &mut self,
        #[zbus(signal_context)] context: zbus::SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        // Detect manual profile change - set override flag (but not during initial startup)
        {
            let mut daemon = self.daemon.lock().await;
            if crate::config::current().auto_switch
                && daemon.initial_set 
                && daemon.power_profile != "Balanced" 
            {
                log::info!("Manual profile change to Balanced detected, setting override flag");
                daemon.auto_switch_manual_override = true;
            }
        }
        
        let result = self
            .daemon
            .lock()
            .await
            .apply_profile(&context, balanced, "Balanced", false)
            .await
            .map_err(zbus_error_from_display);

        if result.is_ok() {
            self.emit_active_profile_changed().await
        }

        result
    }

    async fn performance(
        &mut self,
        #[zbus(signal_context)] context: zbus::SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        // Detect manual profile change - set override flag (but not during initial startup)
        {
            let mut daemon = self.daemon.lock().await;
            if crate::config::current().auto_switch
                && daemon.initial_set 
                && daemon.power_profile != "Performance" 
            {
                log::info!("Manual profile change to Performance detected, setting override flag");
                daemon.auto_switch_manual_override = true;
            }
        }
        
        let result = self
            .daemon
            .lock()
            .await
            .apply_profile(&context, performance, "Performance", false)
            .await
            .map_err(zbus_error_from_display);

        if result.is_ok() {
            self.emit_active_profile_changed().await
        }

        result
    }

    #[dbus_interface(out_args("profile"))]
    async fn get_profile(&self) -> zbus::fdo::Result<String> {
        Ok(self.daemon.lock().await.power_profile.clone())
    }

    #[dbus_interface(out_args("required"))]
    async fn get_external_displays_require_dgpu(&mut self) -> zbus::fdo::Result<bool> {
        let g = self.lock_graphics()?;
        g.get_external_displays_require_dgpu().map_err(zbus_error_from_display)
    }

    #[dbus_interface(out_args("vendor"))]
    async fn get_default_graphics(&self) -> zbus::fdo::Result<String> {
        let g = self.lock_graphics()?;
        g.get_default_graphics()
            .map_err(zbus_error_from_display)
            .map(|mode| <&'static str>::from(mode).to_owned())
    }

    #[dbus_interface(out_args("vendor"))]
    async fn get_graphics(&self) -> zbus::fdo::Result<String> {
        let g = self.lock_graphics()?;
        g.get_vendor()
            .map_err(zbus_error_from_display)
            .map(|mode| <&'static str>::from(mode).to_owned())
    }

    async fn set_graphics(&mut self, vendor: &str) -> zbus::fdo::Result<()> {
        if self.switch_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("Graphics switch in progress".into()));
        }
        let mode = GraphicsMode::from(vendor);
        let graphics = self.graphics.clone();
        tokio::task::spawn_blocking(move || {
            let g = match graphics.lock() {
                Ok(g) => g,
                Err(p) => {
                    log::warn!("Graphics mutex was poisoned; recovering");
                    p.into_inner()
                }
            };
            g.set_vendor(mode)
        })
        .await
        .map_err(zbus_error_from_display)?
        .map_err(zbus_error_from_display)
    }

    /// Switch the graphics mode at runtime without a reboot.
    ///
    /// Validates request compatibility fast, then spawns background orchestration.
    /// Emits `GraphicsModeChanged` on success or `GraphicsSwitchFailed` on error.
    async fn set_graphics_runtime(
        &mut self,
        #[zbus(signal_context)] ctx: zbus::SignalContext<'_>,
        vendor: &str,
    ) -> zbus::fdo::Result<()> {
        if self
            .switch_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed("Graphics switch in progress".into()));
        }

        let mode = GraphicsMode::from(vendor);
        let from_mode = {
            let g = match self.graphics.lock() {
                Ok(g) => g,
                Err(p) => {
                    log::warn!("Graphics mutex was poisoned; recovering");
                    p.into_inner()
                }
            };
            if !g.can_switch() {
                self.switch_in_progress.store(false, Ordering::SeqCst);
                return Err(zbus::fdo::Error::Failed(
                    "Graphics switching is not supported on this device".into(),
                ));
            }
            let from = match g.get_vendor() {
                Ok(m) => m,
                Err(e) => {
                    self.switch_in_progress.store(false, Ordering::SeqCst);
                    return Err(zbus_error_from_display(e));
                }
            };

            let live = match crate::graphics::transition::LiveState::probe(&g) {
                Ok(l) => l,
                Err(e) => {
                    self.switch_in_progress.store(false, Ordering::SeqCst);
                    return Err(zbus_error_from_display(e));
                }
            };
            let ext_req = match g.get_external_displays_require_dgpu() {
                Ok(r) => r,
                Err(e) => {
                    self.switch_in_progress.store(false, Ordering::SeqCst);
                    return Err(zbus_error_from_display(e));
                }
            };
            let policy = crate::config::current().graphics.runtime_restart_dm;
            match crate::graphics::transition::plan_transition(from, mode, &live, policy, ext_req) {
                crate::graphics::transition::PlanOutcome::NoChange => {
                    self.switch_in_progress.store(false, Ordering::SeqCst);
                    log::info!("Already in {:?} graphics mode with matching state", mode);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    record_switch_status(&GraphicsSwitchStatus {
                        timestamp: now,
                        from: <&'static str>::from(from).to_owned(),
                        to: vendor.to_owned(),
                        in_progress: false,
                        success: true,
                        stage: Some(SwitchStage::Done),
                        error: None,
                        rolled_back: false,
                        dm_restarted: None,
                        initramfs_success: Some(true),
                    });
                    let v = vendor.to_owned();
                    let ctx_c1 = ctx.to_owned();
                    let ctx_c2 = ctx.to_owned();
                    tokio::spawn(async move {
                        let _ = System76Power::graphics_mode_changed(&ctx_c1, &v).await;
                        let _ = System76Power::graphics_initramfs_done(&ctx_c2, &v, true).await;
                    });
                    return Ok(());
                }
                crate::graphics::transition::PlanOutcome::Rejected(err) => {
                    self.switch_in_progress.store(false, Ordering::SeqCst);
                    return Err(zbus_error_from_display(err));
                }
                crate::graphics::transition::PlanOutcome::Plan(_) => (),
            }

            from
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let initial_status = GraphicsSwitchStatus {
            timestamp: now,
            from: <&'static str>::from(from_mode).to_owned(),
            to: vendor.to_owned(),
            in_progress: true,
            success: false,
            stage: Some(SwitchStage::Initial),
            error: None,
            rolled_back: false,
            dm_restarted: None,
            initramfs_success: None,
        };
        record_switch_status(&initial_status);

        let graphics = self.graphics.clone();
        let switch_in_progress = self.switch_in_progress.clone();
        let vendor_owned = vendor.to_owned();
        let from_str = <&'static str>::from(from_mode).to_owned();
        let ctx_owned = ctx.to_owned();

        struct SwitchInProgressGuard(Arc<AtomicBool>);
        impl Drop for SwitchInProgressGuard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }

        tokio::task::spawn_blocking(move || {
            let _guard = SwitchInProgressGuard(switch_in_progress);

            let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut g = match graphics.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        log::warn!("Graphics mutex was poisoned; recovering");
                        poisoned.into_inner()
                    }
                };
                g.switch_runtime(mode)
            }));

            let finish_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            match panic_res {
                Ok(Ok(report)) => {
                    log::info!("Runtime graphics switch to {} succeeded", vendor_owned);
                    record_switch_status(&GraphicsSwitchStatus {
                        timestamp: finish_time,
                        from: from_str,
                        to: vendor_owned.clone(),
                        in_progress: false,
                        success: true,
                        stage: Some(report.stage_reached),
                        error: None,
                        rolled_back: false,
                        dm_restarted: report.dm_restarted.clone(),
                        initramfs_success: None,
                    });

                    let v_clone = vendor_owned.clone();
                    let ctx_c1 = ctx_owned.clone();
                    tokio::runtime::Handle::current().block_on(async move {
                        System76Power::graphics_mode_changed(&ctx_c1, &v_clone)
                            .await
                            .unwrap_or_else(|e| log::warn!("Failed to emit GraphicsModeChanged: {}", e));
                    });

                    log::info!("Background initramfs rebuild starting");
                    let (cmd, arg) = update_initramfs_cmd();
                    let initramfs_success = std::process::Command::new(cmd)
                        .arg(arg)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    log::info!(
                        "Background initramfs rebuild {}",
                        if initramfs_success { "succeeded" } else { "failed" }
                    );

                    record_switch_status(&GraphicsSwitchStatus {
                        timestamp: finish_time,
                        from: <&'static str>::from(from_mode).to_owned(),
                        to: vendor_owned.clone(),
                        in_progress: false,
                        success: true,
                        stage: Some(report.stage_reached),
                        error: None,
                        rolled_back: false,
                        dm_restarted: report.dm_restarted,
                        initramfs_success: Some(initramfs_success),
                    });

                    let v_clone2 = vendor_owned;
                    let ctx_c2 = ctx_owned;
                    tokio::runtime::Handle::current().block_on(async move {
                        System76Power::graphics_initramfs_done(&ctx_c2, &v_clone2, initramfs_success)
                            .await
                            .unwrap_or_else(|e| log::warn!("Failed to emit GraphicsInitramfsDone: {}", e));
                    });
                }
                Ok(Err(err)) => {
                    log::error!("Runtime graphics switch to {} failed: {}", vendor_owned, err);
                    let (stage_name, rolled_back) = match &err {
                        crate::graphics::GraphicsDeviceError::RuntimeSwitch {
                            stage,
                            rolled_back,
                            ..
                        } => (format!("{:?}", stage), *rolled_back),
                        _ => (String::from("Unknown"), false),
                    };

                    record_switch_status(&GraphicsSwitchStatus {
                        timestamp: finish_time,
                        from: from_str,
                        to: vendor_owned.clone(),
                        in_progress: false,
                        success: false,
                        stage: match &err {
                            crate::graphics::GraphicsDeviceError::RuntimeSwitch { stage, .. } => {
                                Some(*stage)
                            }
                            _ => None,
                        },
                        error: Some(err.to_string()),
                        rolled_back,
                        dm_restarted: None,
                        initramfs_success: None,
                    });

                    let v_clone = vendor_owned;
                    let err_str = err.to_string();
                    let ctx_c = ctx_owned;
                    tokio::runtime::Handle::current().block_on(async move {
                        System76Power::graphics_switch_failed(
                            &ctx_c,
                            &v_clone,
                            &stage_name,
                            &err_str,
                            rolled_back,
                        )
                        .await
                        .unwrap_or_else(|e| log::warn!("Failed to emit GraphicsSwitchFailed: {}", e));
                    });
                }
                Err(panic_err) => {
                    log::error!("Runtime graphics switch panicked: {:?}", panic_err);
                    record_switch_status(&GraphicsSwitchStatus {
                        timestamp: finish_time,
                        from: from_str,
                        to: vendor_owned.clone(),
                        in_progress: false,
                        success: false,
                        stage: None,
                        error: Some("panic during switch".into()),
                        rolled_back: false,
                        dm_restarted: None,
                        initramfs_success: None,
                    });

                    let v_clone = vendor_owned;
                    let ctx_c = ctx_owned;
                    tokio::runtime::Handle::current().block_on(async move {
                        System76Power::graphics_switch_failed(
                            &ctx_c,
                            &v_clone,
                            "Panic",
                            "internal panic during switch",
                            false,
                        )
                        .await
                        .unwrap_or_else(|e| log::warn!("Failed to emit GraphicsSwitchFailed: {}", e));
                    });
                }
            }
        });

        Ok(())
    }

    #[dbus_interface(out_args("status"))]
    async fn get_graphics_switch_status(&self) -> zbus::fdo::Result<String> {
        let path = std::path::Path::new(GRAPHICS_SWITCH_STATUS_PATH);
        if path.exists() {
            std::fs::read_to_string(path).map_err(zbus_error_from_display)
        } else {
            Ok(String::from("{}"))
        }
    }

    #[dbus_interface(out_args("desktop"))]
    async fn get_desktop(&mut self) -> zbus::fdo::Result<bool> {
        let g = self.lock_graphics()?;
        Ok(g.is_desktop())
    }

    #[dbus_interface(out_args("switchable"))]
    async fn get_switchable(&mut self) -> zbus::fdo::Result<bool> {
        let g = self.lock_graphics()?;
        Ok(g.can_switch())
    }

    #[dbus_interface(out_args("power"))]
    async fn get_graphics_power(&mut self) -> zbus::fdo::Result<bool> {
        let g = self.lock_graphics()?;
        g.get_power().map_err(zbus_error_from_display)
    }

    async fn set_graphics_power(&mut self, power: bool) -> zbus::fdo::Result<()> {
        let g = self.lock_graphics()?;
        g.set_power(power).map_err(zbus_error_from_display)
    }

    async fn auto_graphics_power(&mut self) -> zbus::fdo::Result<()> {
        let g = self.lock_graphics()?;
        g.auto_power().map_err(zbus_error_from_display)
    }

    #[dbus_interface(out_args("start", "end"))]
    async fn get_charge_thresholds(&mut self) -> zbus::fdo::Result<(u8, u8)> {
        get_charge_thresholds().map_err(zbus_error_from_display)
    }

    async fn set_charge_thresholds(&mut self, thresholds: (u8, u8)) -> zbus::fdo::Result<()> {
        let connection = zbus::Connection::system().await?;
        let polkit = zbus_polkit::policykit1::AuthorityProxy::new(&connection)
            .await
            .context("could not connect to polkit authority daemon")
            .map_err(zbus_error_from_display)?;

        let pid = std::process::id();

        let permitted = if pid == 0 {
            true
        } else {
            let subject = zbus_polkit::policykit1::Subject::new_for_owner(pid, None, None)
                .context("could not create policykit1 subject")
                .map_err(zbus_error_from_display)?;

            polkit
                .check_authorization(
                    &subject,
                    THRESHOLD_POLICY,
                    &std::collections::HashMap::new(),
                    Default::default(),
                    "",
                )
                .await
                .context("could not check policykit authorization")
                .map_err(zbus_error_from_display)?
                .is_authorized
        };

        if permitted {
            set_charge_thresholds(thresholds).map_err(zbus_error_from_display)
        } else {
            Err(zbus_error_from_display("Operation not permitted by Polkit"))
        }
    }

    #[dbus_interface(out_args("profiles"))]
    async fn get_charge_profiles(&mut self) -> zbus::fdo::Result<Vec<ChargeProfile>> {
        Ok(get_charge_profiles())
    }

    #[dbus_interface(signal)]
    async fn hot_plug_detect(context: &zbus::SignalContext<'_>, port: u64) -> zbus::Result<()>;

    #[dbus_interface(signal)]
    async fn power_profile_switch(
        context: &zbus::SignalContext<'_>,
        profile: &str,
    ) -> zbus::Result<()>;

    /// Emitted immediately after a runtime graphics mode switch completes.
    /// The `mode` argument is the new graphics mode string (e.g. `"hybrid"`).
    #[dbus_interface(signal)]
    async fn graphics_mode_changed(
        context: &zbus::SignalContext<'_>,
        mode: &str,
    ) -> zbus::Result<()>;

    /// Emitted when a runtime graphics mode switch fails.
    #[dbus_interface(signal)]
    async fn graphics_switch_failed(
        context: &zbus::SignalContext<'_>,
        target: &str,
        stage: &str,
        error: &str,
        rolled_back: bool,
    ) -> zbus::Result<()>;

    /// Emitted when the background initramfs rebuild triggered by
    /// `SetGraphicsRuntime` finishes. `success` is `true` if the rebuild
    /// command exited with status 0.
    #[dbus_interface(signal)]
    async fn graphics_initramfs_done(
        context: &zbus::SignalContext<'_>,
        mode: &str,
        success: bool,
    ) -> zbus::Result<()>;
}

struct UPowerPowerProfiles(Arc<Mutex<PowerDaemon>>);

impl UPowerPowerProfiles {
    pub async fn apply_held_profile(&mut self) {
        let mut set_profile = "balanced";

        for (_, profile, ..) in &self.0.lock().await.held_profiles {
            match *profile {
                "power-saver" => {
                    set_profile = "power-saver";
                    break;
                }
                "performance" => set_profile = "performance",
                _ => (),
            }
        }

        self.set_active_profile(set_profile).await;
    }
}

#[zbus::dbus_interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl UPowerPowerProfiles {
    #[dbus_interface(out_args("cookie"))]
    async fn hold_profile(
        &mut self,
        profile: &str,
        reason: &str,
        application_id: &str,
    ) -> zbus::fdo::Result<u32> {
        let mut this = self.0.lock().await;
        let id = this.profile_ids;

        let profile_static = match profile {
            "power-saver" => "power-saver",
            "balanced" => "balanced",
            "performance" => "performance",
            _ => return Err(zbus::fdo::Error::Failed(String::from("unknown power profile"))),
        };

        this.profile_ids += 1;
        this.held_profiles.push((id, profile_static, reason.into(), application_id.into()));
        drop(this);

        self.apply_held_profile().await;

        Ok(id)
    }

    async fn release_profile(&mut self, cookie: u32) {
        let mut this = self.0.lock().await;

        if let Some(pos) = this.held_profiles.iter().position(|(id, ..)| *id == cookie) {
            this.held_profiles.swap_remove(pos);
            drop(this);

            self.apply_held_profile().await;

            let this = self.0.lock().await;
            let Some((_, ref connection, _)) = this.connections else {
                return;
            };

            if let Ok(context) = zbus::SignalContext::new(connection, POWER_PROFILES_DBUS_PATH) {
                let _res = Self::profile_released(&context, cookie);
            }
        }
    }

    #[dbus_interface(signal)]
    async fn profile_released(context: &zbus::SignalContext<'_>, cookie: u32) -> zbus::Result<()>;

    #[dbus_interface(property)]
    async fn active_profile(&self) -> &str {
        system76_profile_to_upp_str(self.0.lock().await.power_profile.as_str())
    }

    #[dbus_interface(property)]
    async fn set_active_profile(&mut self, profile: &str) {
        let (func, profile): (fn(&mut Vec<ProfileError>, bool), &'static str) = match profile {
            "power-saver" => (battery, "Battery"),
            "balanced" => (balanced, "Balanced"),
            "performance" => (performance, "Performance"),
            _ => return,
        };

        let mut this = self.0.lock().await;

        // A manual profile change arms the override so the next automatic
        // switch is suppressed (only after startup finished).
        if crate::config::current().auto_switch
            && this.initial_set
            && this.power_profile != profile
        {
            log::info!("manual profile change to {} detected, setting override flag", profile);
            this.auto_switch_manual_override = true;
        }

        let Some((ref connection, ..)) = this.connections else {
            return;
        };

        if let Ok(context) = zbus::SignalContext::new(connection, DBUS_PATH) {
            let _res =
                this.apply_profile(&context, func, profile, false).await.map_err(zbus_error_from_display);
        }
    }

    #[dbus_interface(property)]
    async fn profiles(&self) -> Vec<HashMap<&'static str, zvariant::Value>> {
        vec![
            {
                let mut map = HashMap::new();
                map.insert("Profile", zvariant::Value::Str(zvariant::Str::from("balanced")));
                map
            },
            {
                let mut map = HashMap::new();
                map.insert("Profile", zvariant::Value::Str(zvariant::Str::from("performance")));
                map
            },
            {
                let mut map = HashMap::new();
                map.insert("Profile", zvariant::Value::Str(zvariant::Str::from("power-saver")));
                map
            },
        ]
    }

    #[dbus_interface(property)]
    async fn performance_degraded(&self) -> &str { "" }

    #[dbus_interface(property)]
    async fn performance_inhibited(&self) -> &str { "" }

    #[dbus_interface(property)]
    async fn active_profile_holds(&self) -> Vec<HashMap<String, zvariant::Value>> { Vec::new() }

    #[dbus_interface(property)]
    async fn actions(&self) -> Vec<String> { vec![] }

    #[dbus_interface(property)]
    async fn version(&self) -> &str { concat!("system76-power ", env!("CARGO_PKG_VERSION")) }
}

pub struct NetHadessPowerProfiles(UPowerPowerProfiles);

#[zbus::dbus_interface(name = "net.hadess.PowerProfiles")]
impl NetHadessPowerProfiles {
    #[dbus_interface(property)]
    async fn active_profile(&self) -> &str { self.0.active_profile().await }

    #[dbus_interface(property)]
    async fn set_active_profile(&mut self, profile: &str) {
        self.0.set_active_profile(profile).await
    }

    #[dbus_interface(property)]
    async fn performance_inhibited(&self) -> &str { self.0.performance_inhibited().await }

    #[dbus_interface(property)]
    async fn profiles(&self) -> Vec<HashMap<&'static str, zvariant::Value>> {
        self.0.profiles().await
    }

    #[dbus_interface(property)]
    async fn actions(&self) -> Vec<String> { self.0.actions().await }
}

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::too_many_lines)]
pub async fn daemon() -> anyhow::Result<()> {
    // Install the signal handlers immediately: `signal()` sets the handler
    // synchronously, so SIGHUP is caught even during the window before the
    // D-Bus name is acquired (systemd's Type=dbus readiness signal).
    let int = signal(SignalKind::interrupt()).unwrap();
    let hup = signal(SignalKind::hangup()).unwrap();
    let term = signal(SignalKind::terminate()).unwrap();
    let signal_handling_fut = signal_handling(int, hup, term);

    let daemon = PowerDaemon::new()?;
    let graphics = daemon.graphics.clone();
    let switch_in_progress = daemon.switch_in_progress.clone();

    let nvidia_exists = !graphics.lock().unwrap().nvidia.is_empty();

    NmiWatchdog.set(b"0");

    // Get the NVIDIA device ID before potentially removing it.
    let nvidia_device_id = if nvidia_exists {
        fs::read_to_string("/sys/bus/pci/devices/0000:01:00.0/device").ok()
    } else {
        None
    };

    let daemon = Arc::new(Mutex::new(daemon));
    let mut system76_daemon = System76Power {
        daemon: daemon.clone(),
        graphics,
        switch_in_progress,
    };

    match system76_daemon.auto_graphics_power().await {
        Ok(()) => (),
        Err(err) => {
            log::warn!("Failed to set automatic graphics power: {}", err);
        }
    }

    let vendor = fs::read_to_string("/sys/class/dmi/id/sys_vendor")?;
    let model = fs::read_to_string("/sys/class/dmi/id/product_version")?;
    match runtime_pm_quirks(&vendor, &model) {
        Ok(()) => (),
        Err(err) => {
            log::warn!("Failed to set runtime power management quirks: {}", err);
        }
    }

    // Register DBus interface for org.freedesktop.UPower.PowerProfiles.
    // This is used by powerprofilesctl
    let upp_connection = zbus::ConnectionBuilder::system()
        .context("failed to create zbus connection builder")?
        .name(POWER_PROFILES_DBUS_NAME)
        .context("unable to register name")?
        .serve_at(POWER_PROFILES_DBUS_PATH, UPowerPowerProfiles(daemon.clone()))
        .context("unable to serve")?
        .build()
        .await
        .context("unable to create system service for org.freedesktop.UPower.PowerProfiles")?;

    // Register DBus interface for net.hadess.PowerProfiles.
    // This is used by gnome-shell
    let hadess_connection = zbus::ConnectionBuilder::system()
        .context("failed to create zbus connection builder")?
        .name(NET_HADESS_POWER_PROFILES_DBUS_NAME)
        .context("unable to register name")?
        .serve_at(
            NET_HADESS_POWER_PROFILES_DBUS_PATH,
            NetHadessPowerProfiles(UPowerPowerProfiles(daemon)),
        )
        .context("unable to serve")?
        .build()
        .await
        .context("unable to create system service for net.hadess.PowerProfiles")?;

    // Register DBus interface for com.system76.PowerDaemon.
    let connection = zbus::ConnectionBuilder::system()
        .context("failed to create zbus connection builder")?
        .name(DBUS_NAME)
        .context("unable to register name")?
        .serve_at(DBUS_PATH, system76_daemon.clone())
        .context("unable to serve")?
        .build()
        .await
        .context("unable to create system service for com.system76.PowerDaemon")?;

    system76_daemon.daemon.lock().await.connections =
        Some((connection.clone(), upp_connection, hadess_connection));

    let context = zbus::SignalContext::new(&connection, DBUS_PATH)
        .context("unable to create signal context")?;

    if let Err(why) = system76_daemon.balanced(context.clone()).await {
        log::warn!("Failed to set initial profile: {}", why);
    }

    system76_daemon.daemon.lock().await.initial_set = true;

    // Create channel for AC power monitoring
    let (power_tx, mut power_rx) = tokio::sync::mpsc::unbounded_channel();
    
    // Spawn async AC power monitoring task - uses Netlink uevents for instant hardware-driven detection.
    // Retries with exponential backoff on fatal socket errors so auto-switching survives transient
    // kernel-level failures without requiring a daemon restart.
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(1);
        loop {
            match crate::power_supply::monitor_channel(power_tx.clone()).await {
                Ok(()) => {
                    // Channel closed cleanly (receiver dropped) — no point retrying.
                    log::info!("AC power monitoring stopped (channel closed)");
                    break;
                }
                Err(e) => {
                    log::warn!(
                        "AC power monitoring failed: {}. Retrying in {:?}...",
                        e,
                        delay
                    );
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
    });

    // Handle power source changes in the async runtime
    let daemon_clone = system76_daemon.daemon.clone();
    let system76_clone = system76_daemon.clone();
    let context_clone = context.clone();
    
    tokio::spawn(async move {
        log::info!("Power source change handler started, waiting for events...");
        while let Some(source) = power_rx.recv().await {
            log::info!("Received power source event: {:?}", source);
            let mut daemon = daemon_clone.lock().await;
            let config = crate::config::current();
            
            // Check if auto-switching is enabled
            if !config.auto_switch {
                log::debug!("Auto-switching disabled, ignoring power source change to {:?}", source);
                continue;
            }
            
            log::info!("Auto-switching is enabled, processing power source change to {:?}", source);
            
            // (a) Display-mode switching is independent of profile switching.
            if config.display_modes.enabled {
                log::info!("Display mode switching is enabled for AC power changes");

                match source {
                    crate::power_supply::PowerSource::AC => {
                        log::info!("AC adapter connected, checking for AC display mode");
                        if let Some(mode_spec) = &config.display_modes.ac_mode.clone() {
                            if let Err(e) = crate::display::set_display_mode(mode_spec) {
                                log::error!("Failed to set AC display mode: {}", e);
                            } else {
                                daemon.ac_display_locked = true;
                                log::info!("AC display lock set — refresh rate will not change until charger is unplugged");
                            }
                        } else {
                            log::warn!("Display mode switching enabled but no AC mode configured");
                        }
                    }
                    crate::power_supply::PowerSource::Battery => {
                        log::info!("On battery power, clearing AC display lock");
                        daemon.ac_display_locked = false;
                        log::info!("AC display lock cleared — charger unplugged");
                        if let Some(mode_spec) = &config.display_modes.battery_mode.clone() {
                            if let Err(e) = crate::display::set_display_mode(mode_spec) {
                                log::error!("Failed to set battery display mode: {}", e);
                            } else {
                                log::info!("Battery display mode successfully applied");
                            }
                        } else {
                            log::warn!("Display mode switching enabled but no battery mode configured");
                        }
                    }
                }
            } else {
                log::info!("Display mode switching disabled, using profile-based auto-switching");
            }

            // (b) Profile switching — unless a manual override consumed this event.
            // The display-mode handling above still runs, so a manual override only
            // suppresses the automatic profile change for this one event.
            if daemon.auto_switch_manual_override {
                daemon.auto_switch_manual_override = false;
                log::info!("manual override active — skipping this auto-switch");
                drop(daemon);
                continue;
            }

            let (target_profile, profile_func) = match source {
                crate::power_supply::PowerSource::AC => {
                    log::info!("AC adapter connected, auto-switching to Balanced profile");
                    ("Balanced", balanced as fn(&mut Vec<ProfileError>, bool))
                }
                crate::power_supply::PowerSource::Battery => {
                    log::info!("On battery power, auto-switching to Battery profile");
                    daemon.ac_display_locked = false;
                    log::info!("AC display lock cleared — charger unplugged");
                    ("Battery", battery as fn(&mut Vec<ProfileError>, bool))
                }
            };

            match daemon.apply_profile(&context_clone, profile_func, target_profile, false).await {
                Ok(()) => {
                    // Set the lock after a successful AC profile apply.
                    if target_profile == "Balanced" {
                        daemon.ac_display_locked = true;
                        log::info!("AC display lock set — refresh rate will not change until charger is unplugged");
                    }
                    log::info!("Auto-switch to {} profile successful", target_profile);
                    // Drop the lock before emitting signals
                    drop(daemon);
                    // Emit D-Bus signals to notify clients of the profile change
                    system76_clone.emit_active_profile_changed().await;
                }
                Err(e) => {
                    log::error!("Auto-switch to {} profile failed: {}", target_profile, e);
                }
            }
        }
        
        log::warn!("Power monitoring channel closed, auto-switching stopped");
    });

    // Spawn hid backlight daemon
    let _hid_backlight = thread::spawn(hid_backlight::daemon);
    let mut fan_daemon = FanDaemon::new(nvidia_exists);
    let mut hpd_res = unsafe { HotPlugDetect::new(nvidia_device_id) };
    let mux_res = unsafe { mux::DisplayPortMux::new() };
    let mut hpd = || -> [bool; 4] {
        if let Ok(ref mut hpd) = hpd_res {
            unsafe { hpd.detect() }
        } else {
            [false; 4]
        }
    };

    let main_loop = async move {
        let mut last = hpd();

        while CONTINUE.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(1000)).await;

            // A SIGHUP reloaded the configuration; re-apply the active profile
            // so the new settings take effect immediately.
            if RELOAD.swap(false, Ordering::SeqCst) {
                let mut daemon = system76_daemon.daemon.lock().await;
                let name = if daemon.power_profile.is_empty() {
                    "Balanced".to_string()
                } else {
                    daemon.power_profile.clone()
                };

                let func = match name.as_str() {
                    "Battery" => battery as fn(&mut Vec<ProfileError>, bool),
                    "Performance" => performance as fn(&mut Vec<ProfileError>, bool),
                    _ => balanced as fn(&mut Vec<ProfileError>, bool),
                };

                match daemon.apply_profile(&context, func, &name, true).await {
                    Ok(()) => log::info!("re-applied {} profile after config reload", name),
                    Err(e) => {
                        log::warn!("failed to re-apply {} profile after config reload: {}", name, e)
                    }
                }

                drop(daemon);
                system76_daemon.emit_active_profile_changed().await;
            }

            fan_daemon.step();

            // HACK: As of Linux 6.9.3, TBT5 controller must be active for HPD
            // to work on USB-C ports.
            match thunderbolt_hotplug_wakeup(&vendor, &model) {
                Ok(()) => (),
                Err(err) => {
                    log::warn!("Failed to wakeup thunderbolt on hotplug: {}", err);
                }
            }

            let hpd = hpd();
            for i in 0..hpd.len() {
                if hpd[i] != last[i] && hpd[i] {
                    log::info!("HotPlugDetect {}", i);
                    let _res = System76Power::hot_plug_detect(&context, i as u64).await;
                }
            }

            last = hpd;

            if let Ok(ref mux) = mux_res {
                unsafe {
                    mux.step();
                }
            }
        }
    };

    log::info!("Handling dbus requests");
    futures_lite::future::zip(signal_handling_fut, main_loop).await;

    log::info!("daemon exited from loop");
    Ok(())
}

fn system76_profile_to_upp_str(system76_profile: &str) -> &'static str {
    match system76_profile {
        "Battery" => "power-saver",
        "Balanced" => "balanced",
        "Performance" => "performance",
        _ => "unknown",
    }
}

fn zbus_error_from_display<E: Display>(why: E) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!("{}", why))
}
