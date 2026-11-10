// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

use crate::args::{Args, GraphicsArgs};
use anyhow::Context;
use intel_pstate::PState;
use std::io;
use sysfs_class::{Backlight, Brightness, Leds, SysClass};
use system76_power_zbus::PowerDaemonProxy;

async fn profile(client: &mut PowerDaemonProxy<'_>) -> io::Result<()> {
    let profile = client.get_profile().await.ok();
    let profile = profile.as_ref().map_or("?", |s| s.as_str());
    println!("Power Profile: {}", profile);

    if let Ok(values) = PState::new().and_then(|pstate| pstate.values()) {
        println!(
            "CPU: {}% - {}%, {}",
            values.min_perf_pct,
            values.max_perf_pct,
            if values.no_turbo { "No Turbo" } else { "Turbo" }
        );
    }

    for backlight in Backlight::iter() {
        let backlight = backlight?;
        let brightness = backlight.actual_brightness()?;
        let max_brightness = backlight.max_brightness()?;
        let ratio = (brightness as f64) / (max_brightness as f64);
        let percent = (ratio * 100.0) as u64;
        println!("Backlight {}: {}/{} = {}%", backlight.id(), brightness, max_brightness, percent);
    }

    for backlight in Leds::iter_keyboards() {
        let backlight = backlight?;
        let brightness = backlight.brightness()?;
        let max_brightness = backlight.max_brightness()?;
        let ratio = (brightness as f64) / (max_brightness as f64);
        let percent = (ratio * 100.0) as u64;
        println!(
            "Keyboard Backlight {}: {}/{} = {}%",
            backlight.id(),
            brightness,
            max_brightness,
            percent
        );
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn client(args: &Args) -> anyhow::Result<()> {
    // The doctor audit is read-only and works with the daemon stopped, so it
    // must run before any D-Bus connection is attempted.
    if let Args::Doctor = args {
        let clean = crate::doctor::run()?;
        if !clean {
            std::process::exit(1);
        }
        return Ok(());
    }

    let connection =
        zbus::Connection::system().await.context("failed to create zbus system connection")?;

    let mut client = PowerDaemonProxy::new(&connection)
        .await
        .context("failed to connect to system76-power daemon")?;

    match args {
        Args::Profile { profile: name } => match name.as_deref() {
            Some("balanced") => client.balanced().await.map_err(zbus_error),
            Some("battery") => {
                if client.get_desktop().await.map_err(zbus_error)? {
                    return Err(anyhow::anyhow!(
                        r#"
Battery power profile is not supported on desktop computers.
"#,
                    ));
                }
                client.battery().await.map_err(zbus_error)
            }
            Some("performance") => client.performance().await.map_err(zbus_error),
            _ => profile(&mut client).await.context("failed to get power profile"),
        },
        Args::Graphics { cmd } => {
            if !client.get_switchable().await? {
                return Err(anyhow::anyhow!(
                    r#"
Graphics switching is not supported on this device, because
this device is either a desktop or doesn't have both an iGPU and dGPU.
"#,
                ));
            }

            match cmd.as_ref() {
                Some(GraphicsArgs::Compute) => {
                    client.set_graphics("compute").await.map_err(zbus_error)
                }
                Some(GraphicsArgs::Hybrid) => {
                    client.set_graphics("hybrid").await.map_err(zbus_error)
                }
                Some(GraphicsArgs::Integrated) => {
                    client.set_graphics("integrated").await.map_err(zbus_error)
                }
                Some(GraphicsArgs::Nvidia) => {
                    client.set_graphics("nvidia").await.map_err(zbus_error)
                }
                Some(GraphicsArgs::Runtime { mode, wait, status }) => {
                    use futures_lite::StreamExt;

                    if *status {
                        let status_json =
                            client.get_graphics_switch_status().await.map_err(zbus_error)?;
                        if status_json.trim().is_empty() || status_json.trim() == "{}" {
                            println!("No runtime graphics switch recorded.");
                        } else {
                            println!("{}", status_json);
                        }
                        return Ok(());
                    }

                    let Some(mode) = mode.as_deref() else {
                        eprintln!(
                            "Error: A graphics mode is required (or use --status to view last switch)"
                        );
                        std::process::exit(1);
                    };

                    if mode == "nvidia" {
                        eprintln!(
                            "Error: runtime switch to nvidia is not supported (Wayland compositors keep the boot GPU primary); run 'system76-power graphics nvidia' and reboot"
                        );
                        std::process::exit(1);
                    }

                    if *wait {
                        println!(
                            "Switching to {} graphics at runtime (display manager may restart)...",
                            mode
                        );
                        let mut mode_changed =
                            client.receive_graphics_mode_changed().await.map_err(zbus_error)?;
                        let mut switch_failed =
                            client.receive_graphics_switch_failed().await.map_err(zbus_error)?;
                        let mut initramfs_done =
                            client.receive_graphics_initramfs_done().await.map_err(zbus_error)?;

                        client.set_graphics_runtime(mode).await.map_err(zbus_error)?;

                        println!("Switch initiated. Waiting for completion...");

                        tokio::select! {
                            sig = mode_changed.next() => {
                                if let Some(signal) = sig {
                                    let new_mode = signal.args().map(|a| a.mode).unwrap_or(mode);
                                    println!("Done. Live graphics switch to {} completed successfully.", new_mode);
                                }
                            }
                            sig = switch_failed.next() => {
                                if let Some(signal) = sig {
                                    if let Ok(args) = signal.args() {
                                        return Err(anyhow::anyhow!(
                                            "Graphics switch to {} failed at stage '{}': {} (rolled back: {})",
                                            args.target, args.stage, args.error, args.rolled_back
                                        ));
                                    } else {
                                        return Err(anyhow::anyhow!("Graphics switch failed"));
                                    }
                                }
                            }
                        }

                        // Only a real switch is followed by an initramfs rebuild
                        // and `GraphicsInitramfsDone`. The daemon records the status
                        // before emitting `GraphicsModeChanged`, keeping
                        // `in_progress` set while a rebuild is pending.
                        let status = client
                            .get_graphics_switch_status()
                            .await
                            .ok()
                            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
                            .unwrap_or_default();

                        if let Some(why) = status["dm_restart_error"].as_str() {
                            eprintln!(
                                "Warning: the display manager could not be restarted: {}\n\
                                 Start it manually, e.g. 'systemctl start display-manager'.",
                                why
                            );
                        }

                        // An unreadable status keeps the old behaviour: wait.
                        if status["in_progress"].as_bool() == Some(false) {
                            match status["initramfs_success"].as_bool() {
                                Some(true) => println!("Initramfs rebuild completed successfully."),
                                Some(false) => {
                                    eprintln!("Warning: Background initramfs rebuild failed.")
                                }
                                None => println!("No initramfs rebuild needed."),
                            }
                            return Ok(());
                        }

                        println!("Waiting for background initramfs rebuild...");
                        if let Some(signal) = initramfs_done.next().await {
                            if let Ok(args) = signal.args() {
                                if args.success {
                                    println!("Initramfs rebuild completed successfully.");
                                } else {
                                    eprintln!("Warning: Background initramfs rebuild failed.");
                                }
                            }
                        }
                        Ok(())
                    } else {
                        println!(
                            "Switching to {} graphics at runtime (display manager may restart)...",
                            mode
                        );
                        client.set_graphics_runtime(mode).await.map_err(zbus_error)?;
                        println!("Runtime graphics switch initiated in the background.");
                        println!("Use 'system76-power graphics runtime --status' to monitor progress.");
                        Ok(())
                    }
                }
                Some(GraphicsArgs::Switchable) => client
                    .get_switchable()
                    .await
                    .map_err(zbus_error)
                    .map(|b| println!("{}", if b { "switchable" } else { "not switchable" })),
                Some(GraphicsArgs::Power { state }) => match state.as_deref() {
                    Some("auto") => client.auto_graphics_power().await.map_err(zbus_error),
                    Some("off") => client.set_graphics_power(false).await.map_err(zbus_error),
                    Some("on") => client.set_graphics_power(true).await.map_err(zbus_error),
                    _ => {
                        if client.get_graphics_power().await.map_err(zbus_error)? {
                            println!("on (discrete)");
                        } else {
                            println!("off (discrete)");
                        }
                        Ok(())
                    }
                },
                None => {
                    println!("{}", client.get_graphics().await.map_err(zbus_error)?);
                    Ok(())
                }
            }
        }
        Args::ChargeThresholds { profile, list_profiles, thresholds } => {
            if client.get_desktop().await.map_err(zbus_error)? {
                return Err(anyhow::anyhow!(
                    r#"
Charge thresholds are not supported on desktop computers.
"#,
                ));
            }

            let profiles = client.get_charge_profiles().await.map_err(zbus_error)?;

            if !thresholds.is_empty() {
                let start = thresholds[0];
                let end = thresholds[1];
                client.set_charge_thresholds(&(start, end)).await.map_err(zbus_error)?;
            } else if let Some(name) = profile {
                if let Some(profile) = profiles.iter().find(|p| &p.id == name) {
                    client
                        .set_charge_thresholds(&(profile.start, profile.end))
                        .await
                        .map_err(zbus_error)?;
                } else {
                    return Err(anyhow::anyhow!("No such profile '{}'", name));
                }
            } else if *list_profiles {
                for profile in &profiles {
                    println!("{}", profile.id);
                    println!("  Title: {}", profile.title);
                    println!("  Description: {}", profile.description);
                    println!("  Start: {}", profile.start);
                    println!("  End: {}", profile.end);
                }
                return Ok(());
            }

            let (start, end) = client.get_charge_thresholds().await.map_err(zbus_error)?;
            if let Some(profile) = profiles.iter().find(|p| p.start == start && p.end == end) {
                println!("Profile: {} ({})", profile.title, profile.id);
            } else {
                println!("Profile: Custom");
            }
            println!("Start: {}", start);
            println!("End: {}", end);

            Ok(())
        }
        Args::Doctor => unreachable!(),
        Args::Daemon { .. } => unreachable!(),
    }
}

fn zbus_error(why: zbus::Error) -> anyhow::Error { anyhow::anyhow!("{}", why) }
