//! Read-only graphics report: lists the graphics devices on the PCI bus and the
//! current mode. Changes nothing (it does not rescan the bus or switch modes),
//! so it does not need root.
//!
//! To switch modes, use `system76-power graphics <mode>` (boot-time) or
//! `system76-power graphics runtime <mode>` (live).

use log::LevelFilter;
use std::process;
use system76_power::{
    graphics::{Graphics, GraphicsDevice},
    logging,
};

fn list(kind: &str, devices: &[GraphicsDevice]) {
    for dev in devices {
        println!("  {:<7} {}", kind, dev.id());
    }
}

fn inner() -> anyhow::Result<()> {
    let graphics = Graphics::probe()?;

    println!("Graphics devices:");
    list("AMD", &graphics.amd);
    list("Intel", &graphics.intel);
    list("NVIDIA", &graphics.nvidia);
    list("Other", &graphics.other);

    if !graphics.can_switch() {
        println!("Switchable: no");
        return Ok(());
    }

    let mode: &'static str = graphics.get_vendor()?.into();
    let default: &'static str = graphics.get_default_graphics()?.into();
    println!("Switchable: yes");
    println!("Current mode: {}", mode);
    println!("Default mode: {}", default);
    println!("dGPU powered: {}", graphics.get_power()?);

    Ok(())
}

fn main() {
    if let Err(why) = logging::setup(LevelFilter::Warn) {
        eprintln!("failed to set up logging: {}", why);
        process::exit(1);
    }

    if let Err(err) = inner() {
        eprintln!("{:#}", err);
        process::exit(1);
    }
}
