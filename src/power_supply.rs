// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! AC power detection and monitoring
//!
//! The power source is read from `/sys/class/power_supply`, the same way upower does it: the
//! system is on AC when any external supply (`Mains`, or `USB` for USB-C PD chargers) is online.
//! Changes are picked up from kernel uevents on a Netlink socket and passed to the daemon over a
//! channel, which drives automatic profile switching.

use std::{
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    sync::mpsc::UnboundedSender,
    time::{Instant, timeout_at},
};

const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

/// The kernel caps a uevent's environment at 2 KiB, so this never truncates one.
const UEVENT_BUFFER_SIZE: usize = 4096;

/// Requested Netlink receive buffer, to ride out bursts of uevents without ENOBUFS.
const RECEIVE_BUFFER_SIZE: libc::c_int = 256 * 1024;

/// After a relevant uevent, wait until no relevant uevent has arrived for this long before
/// reading the state. One plug event fires several uevents, and USB-C PD negotiation can make
/// a supply flap online/offline before it settles.
const SETTLE_QUIET: Duration = Duration::from_millis(300);

/// Upper bound on the settle wait, so a stream of uevents cannot postpone the update forever.
const SETTLE_MAX: Duration = Duration::from_secs(2);

/// Current power source
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    /// An external power supply is connected
    AC,
    /// Running on battery power
    Battery,
}

/// Why [`monitor_channel`] stopped.
#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    /// The system has no battery, so there is nothing to switch between. Not worth retrying.
    #[error("no system battery found, AC/battery auto-switching does not apply")]
    Unavailable,
    /// The uevent socket failed. Retrying with a fresh socket may succeed.
    #[error("netlink uevent socket: {0}")]
    Socket(#[from] io::Error),
}

/// The power-related state of every power supply in sysfs.
#[derive(Debug, Default)]
struct Snapshot {
    /// An external supply with a readable `online` attribute exists.
    adapter_present: bool,
    /// At least one external supply is online.
    adapter_online: bool,
    /// A system battery (not a peripheral's) is present.
    battery_present: bool,
    /// A system battery reports `Discharging`.
    battery_discharging: bool,
    /// A system battery reports a status that implies external power.
    battery_on_external_power: bool,
}

impl Snapshot {
    fn read(root: &Path) -> Self {
        let mut snapshot = Self::default();
        let Ok(entries) = fs::read_dir(root) else {
            log::debug!("{} is not readable", root.display());
            return snapshot;
        };

        for entry in entries.flatten() {
            let supply = entry.path();
            // Batteries of mice, keyboards and the like are not the system's power source.
            if read_attr(&supply, "scope").as_deref() == Some("Device") {
                continue;
            }

            match read_attr(&supply, "type").as_deref() {
                Some("Mains" | "USB") => {
                    // `online` is 0 when offline, 1 or 2 when online (fixed / programmable).
                    if let Some(online) =
                        read_attr(&supply, "online").and_then(|v| v.parse::<u8>().ok())
                    {
                        snapshot.adapter_present = true;
                        snapshot.adapter_online |= online != 0;
                    }
                }
                Some("Battery") => {
                    if read_attr(&supply, "present").as_deref() == Some("0") {
                        continue;
                    }
                    snapshot.battery_present = true;
                    match read_attr(&supply, "status").as_deref() {
                        Some("Discharging") => snapshot.battery_discharging = true,
                        Some("Charging" | "Full" | "Not charging") => {
                            snapshot.battery_on_external_power = true;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        snapshot
    }

    fn power_source(&self) -> Option<PowerSource> {
        if self.adapter_online {
            return Some(PowerSource::AC);
        }
        // Without a battery the machine cannot be running off one, whatever the adapters say.
        // This keeps desktops with idle USB-C supplies from being reported as on battery.
        if !self.battery_present {
            return None;
        }
        if self.adapter_present {
            return Some(PowerSource::Battery);
        }
        // No adapter is exposed in sysfs: fall back to what the batteries report.
        if self.battery_discharging {
            Some(PowerSource::Battery)
        } else if self.battery_on_external_power {
            Some(PowerSource::AC)
        } else {
            None
        }
    }
}

fn read_attr(supply: &Path, attr: &str) -> Option<String> {
    fs::read_to_string(supply.join(attr)).ok().map(|value| value.trim().to_owned())
}

/// Detect the current power source from sysfs.
///
/// Returns `None` when it cannot be determined, for example on a desktop without a battery.
pub fn get_power_source() -> Option<PowerSource> {
    let snapshot = Snapshot::read(Path::new(POWER_SUPPLY_DIR));
    let source = snapshot.power_source();
    log::debug!("Power supply snapshot: {:?} -> {:?}", snapshot, source);
    source
}

/// Monitor the power source and send each change to `tx`.
///
/// `last_sent` is the last state the receiver was given. It belongs to the caller so that it
/// survives restarts after a socket error: a restart then only sends a state that differs from
/// what the receiver already has, instead of repeating the current one. Pass `None` on the
/// first call to have the current state sent right away.
///
/// Returns `Ok(())` once the receiver is dropped.
///
/// # Errors
///
/// [`MonitorError::Unavailable`] when the system has no battery (do not retry), or
/// [`MonitorError::Socket`] when the Netlink socket fails (a retry may succeed).
pub async fn monitor_channel(
    tx: &UnboundedSender<PowerSource>,
    last_sent: &mut Option<PowerSource>,
) -> Result<(), MonitorError> {
    let snapshot = Snapshot::read(Path::new(POWER_SUPPLY_DIR));
    if !snapshot.battery_present {
        return Err(MonitorError::Unavailable);
    }
    // Battery uevents only matter when no adapter is exposed and the battery status is the
    // only signal. Otherwise they are skipped: batteries emit one on every capacity change.
    let watch_batteries = !snapshot.adapter_present;

    // Bind before reading the state, so a change in between is still seen as a uevent.
    let socket = AsyncFd::new(open_uevent_socket()?)?;
    log::info!("Monitoring power supply uevents (watching batteries: {})", watch_batteries);

    if !publish(tx, last_sent, get_power_source()) {
        return Ok(());
    }

    let mut buffer = [0u8; UEVENT_BUFFER_SIZE];
    loop {
        if !drain(&socket, &mut buffer, watch_batteries).await? {
            continue;
        }
        settle(&socket, &mut buffer, watch_batteries).await?;
        if !publish(tx, last_sent, get_power_source()) {
            return Ok(());
        }
    }
}

/// Send `current` if it differs from `last_sent`.
///
/// Returns `false` once the receiver has been dropped.
fn publish(
    tx: &UnboundedSender<PowerSource>,
    last_sent: &mut Option<PowerSource>,
    current: Option<PowerSource>,
) -> bool {
    let Some(current) = current else {
        log::warn!("Could not determine the power source from sysfs");
        return true;
    };
    if *last_sent == Some(current) {
        log::debug!("Power source unchanged: {:?}", current);
        return true;
    }

    match last_sent {
        Some(previous) => log::info!("Power source changed: {:?} -> {:?}", previous, current),
        None => log::info!("Power source: {:?}", current),
    }
    if tx.send(current).is_err() {
        log::info!("Power source receiver dropped, stopping monitoring");
        return false;
    }
    *last_sent = Some(current);
    true
}

/// Keep reading uevents until no relevant one has arrived for [`SETTLE_QUIET`], or
/// [`SETTLE_MAX`] has passed.
async fn settle(
    socket: &AsyncFd<OwnedFd>,
    buffer: &mut [u8],
    watch_batteries: bool,
) -> io::Result<()> {
    let deadline = Instant::now() + SETTLE_MAX;
    let mut quiet_until = Instant::now() + SETTLE_QUIET;
    loop {
        // Cancelling `drain` is safe: it only awaits readiness, never a partial read.
        match timeout_at(quiet_until.min(deadline), drain(socket, buffer, watch_batteries)).await {
            Err(_elapsed) => return Ok(()),
            Ok(relevant) => {
                if relevant? {
                    quiet_until = Instant::now() + SETTLE_QUIET;
                }
            }
        }
    }
}

/// Wait until the socket is readable, then read every queued uevent.
///
/// Returns whether any of them may have changed the power source.
async fn drain(
    socket: &AsyncFd<OwnedFd>,
    buffer: &mut [u8],
    watch_batteries: bool,
) -> io::Result<bool> {
    let mut guard = socket.readable().await?;
    let mut relevant = false;
    loop {
        match guard.try_io(|fd| recv(fd.as_raw_fd(), &mut *buffer)) {
            Ok(Ok(len)) => relevant |= is_relevant_uevent(&buffer[..len], watch_batteries),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(e)) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                // The kernel dropped uevents because the receive buffer was full. The socket
                // still works; treat it as a possible change so the state is re-read.
                log::warn!("Netlink receive buffer overflowed, re-reading the power source");
                relevant = true;
            }
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => return Ok(relevant),
        }
    }
}

fn recv(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buffer` is valid for writes of `buffer.len()` bytes.
    let len = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    if len < 0 { Err(io::Error::last_os_error()) } else { Ok(len as usize) }
}

/// Whether a uevent may change the power source.
///
/// A uevent is a header followed by NUL-separated `KEY=value` fields.
fn is_relevant_uevent(payload: &[u8], watch_batteries: bool) -> bool {
    let mut power_supply = false;
    let mut battery = false;
    for field in payload.split(|&b| b == 0) {
        match field {
            b"SUBSYSTEM=power_supply" => power_supply = true,
            b"POWER_SUPPLY_TYPE=Battery" => battery = true,
            _ => {}
        }
    }
    power_supply && (watch_batteries || !battery)
}

/// Open a non-blocking Netlink socket subscribed to kernel uevents.
fn open_uevent_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall, the result is checked below.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a newly created descriptor that nothing else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };

    set_receive_buffer(&socket);

    // SAFETY: `sockaddr_nl` is plain data, all-zero is a valid value.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    address.nl_groups = 1; // kernel uevents

    // SAFETY: `address` is a valid `sockaddr_nl` and the length matches it.
    let result = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const address).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(socket)
}

/// Enlarge the receive buffer. `SO_RCVBUFFORCE` ignores `net.core.rmem_max` but needs
/// `CAP_NET_ADMIN`; `SO_RCVBUF` is capped by it. Failure only means a higher chance of ENOBUFS.
fn set_receive_buffer(socket: &OwnedFd) {
    let size = RECEIVE_BUFFER_SIZE;
    for (option, name) in
        [(libc::SO_RCVBUFFORCE, "SO_RCVBUFFORCE"), (libc::SO_RCVBUF, "SO_RCVBUF")]
    {
        // SAFETY: the value pointer and length describe a live `c_int`.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&raw const size).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if result == 0 {
            return;
        }
        log::debug!("{} on the uevent socket failed: {}", name, io::Error::last_os_error());
    }
    log::warn!("Could not enlarge the uevent receive buffer, keeping the kernel default");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fake `/sys/class/power_supply`, removed on drop.
    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new(name: &str, supplies: &[(&str, &[(&str, &str)])]) -> Self {
            let root = std::env::temp_dir()
                .join(format!("system76-power-test-{}-{}", std::process::id(), name));
            let _ = fs::remove_dir_all(&root);
            for (supply, attrs) in supplies {
                let dir = root.join(supply);
                fs::create_dir_all(&dir).unwrap();
                for (attr, value) in *attrs {
                    fs::write(dir.join(attr), format!("{}\n", value)).unwrap();
                }
            }
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn power_source(&self) -> Option<PowerSource> { Snapshot::read(&self.0).power_source() }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    const BATTERY_DISCHARGING: &[(&str, &str)] =
        &[("type", "Battery"), ("present", "1"), ("status", "Discharging")];
    const BATTERY_CHARGING: &[(&str, &str)] = &[("type", "Battery"), ("status", "Charging")];
    const MAINS_OFFLINE: &[(&str, &str)] = &[("type", "Mains"), ("online", "0")];
    const MAINS_ONLINE: &[(&str, &str)] = &[("type", "Mains"), ("online", "1")];
    const USB_OFFLINE: &[(&str, &str)] = &[("type", "USB"), ("online", "0")];
    const USB_ONLINE: &[(&str, &str)] = &[("type", "USB"), ("online", "1")];

    #[test]
    fn mains_online_is_ac() {
        let sysfs = FakeSysfs::new("mains-online", &[("ADP0", MAINS_ONLINE), ("BAT0", BATTERY_CHARGING)]);
        assert_eq!(sysfs.power_source(), Some(PowerSource::AC));
    }

    #[test]
    fn usb_c_charger_is_ac_even_if_mains_is_offline() {
        let sysfs = FakeSysfs::new(
            "usb-c",
            &[("ADP0", MAINS_OFFLINE), ("ucsi-source-psy-1", USB_ONLINE), ("BAT0", BATTERY_CHARGING)],
        );
        assert_eq!(sysfs.power_source(), Some(PowerSource::AC));
    }

    #[test]
    fn all_adapters_offline_is_battery() {
        let sysfs = FakeSysfs::new(
            "offline",
            &[("ADP0", MAINS_OFFLINE), ("ucsi-source-psy-1", USB_OFFLINE), ("BAT1", BATTERY_DISCHARGING)],
        );
        assert_eq!(sysfs.power_source(), Some(PowerSource::Battery));
    }

    #[test]
    fn falls_back_to_battery_status_without_adapters() {
        let discharging = FakeSysfs::new("fallback-discharging", &[("BAT1", BATTERY_DISCHARGING)]);
        assert_eq!(discharging.power_source(), Some(PowerSource::Battery));

        let charging = FakeSysfs::new("fallback-charging", &[("BAT0", BATTERY_CHARGING)]);
        assert_eq!(charging.power_source(), Some(PowerSource::AC));
    }

    #[test]
    fn desktop_without_battery_is_undetermined() {
        let sysfs = FakeSysfs::new("desktop", &[("ucsi-source-psy-1", USB_OFFLINE)]);
        assert_eq!(sysfs.power_source(), None);
    }

    #[test]
    fn ignores_peripheral_and_absent_batteries() {
        let sysfs = FakeSysfs::new(
            "peripheral",
            &[
                ("hidpp_battery_0", &[("type", "Battery"), ("scope", "Device"), ("status", "Discharging")]),
                ("BAT0", &[("type", "Battery"), ("present", "0"), ("status", "Discharging")]),
                ("ADP0", MAINS_OFFLINE),
            ],
        );
        assert_eq!(sysfs.power_source(), None);
    }

    #[test]
    fn uevent_filter() {
        let adapter = b"change@/devices/ADP0\0ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_TYPE=Mains\0";
        let battery = b"change@/devices/BAT0\0ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_TYPE=Battery\0";
        let other = b"change@/devices/foo\0ACTION=change\0SUBSYSTEM=power_supply_not\0";

        assert!(is_relevant_uevent(adapter, false));
        assert!(!is_relevant_uevent(battery, false));
        assert!(is_relevant_uevent(battery, true));
        assert!(!is_relevant_uevent(other, true));
    }

    #[test]
    fn publish_sends_only_changes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut last_sent = None;

        assert!(publish(&tx, &mut last_sent, Some(PowerSource::AC)));
        assert!(publish(&tx, &mut last_sent, Some(PowerSource::AC)));
        assert!(publish(&tx, &mut last_sent, None));
        assert!(publish(&tx, &mut last_sent, Some(PowerSource::Battery)));

        assert_eq!(rx.try_recv(), Ok(PowerSource::AC));
        assert_eq!(rx.try_recv(), Ok(PowerSource::Battery));
        assert!(rx.try_recv().is_err());
        assert_eq!(last_sent, Some(PowerSource::Battery));

        drop(rx);
        assert!(!publish(&tx, &mut last_sent, Some(PowerSource::AC)));
        assert_eq!(last_sent, Some(PowerSource::Battery));
    }
}
