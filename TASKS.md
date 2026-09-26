# system76-power fork → sophisticated TLP replacement — execution tracker

Source plan: `local://s76-power-tlp-replacement-plan.md` (durable copy).
Repo: `/home/deplague/Projects/system76-power` (HEAD `b57b871`, v1.2.8).
Upstream reference tree: `/home/deplague/Projects/system76-power-original` (HEAD `9a33472`).
Toolchain: `devenv shell -- cargo` (cargo 1.98.1).

Legend: `[ ]` pending · `[~]` in progress · `[x]` done · `[!]` blocked.

---

## Step 1 — Port two upstream fixes

- [x] 1.1 `src/graphics/nvidia.rs`: replace `supported_gpus` collection with upstream `nvidia-kernel-common`-aware filter_map; keep `len() != 1` error.
- [x] 1.2 `src/hotplug/mod.rs`: re-add `addp6` and `oryp14` arms (data-only), before final `other => Err(...)`.
- [x] 1.3 Do NOT port upstream `1c8a14f` coreboot gate on deferred `power/control` write.
- [x] 1.4 `cargo build && cargo test` green.

## Step 2 — One real config subsystem (`src/config.rs`)

- [x] 2.1 New `src/config.rs`: `CONFIG_PATH`, `Config` struct with `auto_switch`, `refresh_rate`, `display_modes`, `profile`, `usb`, `audio`, `wifi`, `pci`, `radio`, `cpu`.
- [x] 2.2 Move `RefreshRateConfig` + `DisplayModeConfig` into `config.rs` (not copy); field semantics unchanged.
- [x] 2.3 `load()`/`parse(&str)` section-aware INI parser; unknown section/key → `warn!`; missing file → defaults; malformed value → `warn!` + default.
- [x] 2.4 `static CONFIG: RwLock<Config>`; `current() -> Config`, `reload() -> Config`.
- [x] 2.5 Exact defaults per plan (auto_switch true, refresh 60/60/165, display_modes false, dim_on_battery false, usb autosuspend true/blacklist_drivers "usblp", audio true, wifi true, pci runtime_pm true/blacklist_drivers "amdgpu nvidia nouveau radeon", radio false, cpu battery_epp "power" + SMU/tctl values).
- [x] 2.6 `#[cfg(test)] mod tests`: defaults-on-missing, full parse of repo conf, section-unawareness regression, malformed→default+no panic, unknown section ignored.
- [x] 2.7 `src/lib.rs`: add `pub mod config;`. Delete three `load_*_config` fns + call sites; `PowerDaemon` fields removed; use `config::current()` at use sites.
- [x] 2.8 `cargo build && cargo test` green.

## Step 3 — SIGHUP reload + apply-after-reload

- [x] 3.1 `signal_handling()`: `SIGINT|SIGTERM → CONTINUE=false, break`; `SIGHUP → config::reload(); log; re-apply current profile` (reset `power_profile` first).
- [x] 3.2 README documents `sudo kill -HUP $(pidof system76-power)`.
- [x] 3.3 `cargo build && cargo test` green.

## Step 4 — Fix auto-switch bugs

- [x] 4.1 Compositing: remove display-mode/profile either-or split; per event: (a) display mode if enabled, (b) then profile unless override consumed event.
- [x] 4.2 Override flag: set on manual profile set (D-Bus battery/balanced/performance + UPower ActiveProfile setter) when `auto_switch_enabled && !initial_set`; AC handler consumes it and logs skip.
- [x] 4.3 Brightness gating: `set_brightness = initial_set || config::current().profile.dim_on_battery`.
- [x] 4.4 `cargo build && cargo test` green.

## Step 5 — Device-level knobs in profiles

- [x] 5.1 USB: `sys_devices::usb::{driver_name(), ids()}`, warn-on-write-error; profiles auto/on + blacklists.
- [x] 5.2 Audio: `SoundDevice::get_devices().set_power_save(...)` gated on `audio.power_save`.
- [x] 5.3 Wi-Fi: rewrite `src/wifi.rs` `set_power_save(bool)` via `iw`; battery on / balanced+perf off.
- [x] 5.4 PCI: delete `S76_POWER_PCI_RUNTIME_PM`/`PCI_RUNTIME_PM`/`pci_runtime_pm_support()`; gate on `pci.runtime_pm`; `pci::driver_name()` + blacklist skip (driverless NOT skipped).
- [x] 5.5 Radio: `sys_devices::rfkill` module; battery soft=1 / others soft=0, gated on `radio.bluetooth_off_on_battery`.
- [x] 5.6 CPU: configurable battery EPP with available-preferences validation; extract `governor_for`/`epp_for`.
- [x] 5.7 SMU limits configurable via `config::current().cpu`.
- [x] 5.8 `cargo build && cargo test` green.

## Step 6 — Charge thresholds off System76 firmware

- [x] 6.1 `Mechanism` enum + `detect()` (Thresholds generic / Conservation `conservation_mode` / None).
- [x] 6.2 `set_charge_thresholds` handles optional start file; Conservation write `1` if `end<=70` else `0`.
- [x] 6.3 `get_charge_thresholds` reads pair or maps conservation `(50,60)`/`(90,100)`.
- [x] 6.4 unit tests for the two pure mappings.
- [x] 6.5 `cargo build && cargo test` green.

## Step 7 — dGPU runtime D3

- [x] 7.1 `MODPROBE_HYBRID` template with `NVreg_DynamicPowerManagement=0x02`.
- [x] 7.2 `write_vendor_config` maps Hybrid|Compute → the D3 options (Hybrid → `MODPROBE_HYBRID`, Compute → `MODPROBE_COMPUTE` keeping its nvidia-drm/modeset blacklists + the option line).
- [x] 7.3 `cargo build && cargo test` green.

## Step 8 — `doctor` subcommand

- [x] 8.1 `args.rs`: `Doctor` variant.
- [x] 8.2 `client.rs`: dispatch `Doctor` before zbus connection.
- [x] 8.3 New `src/doctor.rs` `run()`: per-knob `OK`/`DRIFT`/`SKIP`; exit 0 if zero DRIFT else 1.
- [x] 8.4 `lib.rs` + `main.rs` wiring.
- [x] 8.5 `cargo build && cargo test` green.

## Step 9 — Hygiene

- [x] 9.1 `version()` returns `concat!("system76-power ", env!("CARGO_PKG_VERSION"))`.
- [x] 9.2 `data/com.system76.PowerDaemon.xml`: add `AutoGraphicsPower`, `SetGraphicsRuntime`, `GraphicsModeChanged`, `GraphicsInitramfsDone`.
- [x] 9.3 `fan.rs:53` + `hid_backlight.rs:98`: restore commented logs at `debug!`.
- [x] 9.4 repo `system76-power.conf`: `[display_modes] enabled = false`; keep `performance = 240`.
- [x] 9.5 `Makefile`: `sysconfdir ?= /etc` + separate `install-config` target (not in `install`).
- [x] 9.6 README: config path fix, SIGHUP, all sections+defaults, override semantics, `dim_on_battery`, `install-config`, `doctor`.
- [x] 9.7 `cargo build && cargo test` green.

## Verification

- [x] V1 Build + unit tests after each step (32 unit tests pass).
- [ ] V2 Install + daemon active (`install-auto-switch.sh`, `systemctl restart`, `is-active`).
- [ ] V3 SIGHUP reload logs `configuration reloaded` + re-apply.
- [ ] V4 Battery profile knobs (boost/EPP/ASPM/iw/snd/radeon-dpm/USB tally/ryzenadj).
- [ ] V5 Balanced profile knobs revert.
- [ ] V6 Charge thresholds on Legion (`conservation_mode` 1/0, query mapping).
- [ ] V7 dGPU D3 (`NVreg` line + post-reboot `runtime_status=suspended`).
- [ ] V8 `doctor` exit 0 clean, exit 1 on induced boost drift.
- [ ] V9 Auto-switch composite path + manual override log.

## Progress log

- 2026-09-26: tracker created; baseline `cargo build` OK, `cargo test` 13 passed.
- Step 1 done (nvidia-kernel-common discovery, addp6/oryp14 arms). Step 2 done (new src/config.rs, loaders deleted, display structs consolidated).
- ReviewStep12 (reviewer): overall_correctness=correct; 1 low-impact finding (quoted-value comment stripping) -> fixed + regression test. `cargo test` 21 passed.
- Step 3 done (SIGHUP reloads config + re-applies the active profile; signal streams now created before the D-Bus name is acquired). ReviewStep3 (reviewer): found the handler-install timing defect (default SIGHUP disposition before first poll) -> fixed by creating `signal()` streams at the top of `daemon()`; README config path fixed + SIGHUP documented.
- Step 4 done (AC handler composites display-mode + profile switching; manual override armed by D-Bus battery/balanced/performance and the UPower setter, consuming exactly one event; brightness gated on `initial_set || dim_on_battery`). ReviewStep4 (reviewer): overall_correctness=correct; 1 informational finding (extra same-profile guard on override arming — behaviorally equivalent, no fix required).
- Step 5 done (USB autosuspend w/ driver+vid:pid blacklists + warn-on-failure writes; HDA power-save; Wi-Fi `iw` power-save; PCI runtime PM via config with blacklisted-driver skip; rfkill bluetooth soft-block; configurable battery EPP with availability fallback; SMU limits from `[cpu]`). Ready 5.4 removed `S76_POWER_PCI_RUNTIME_PM`/`PCI_RUNTIME_PM`/`pci_runtime_pm_support`; dead `PowerLevel` removed.
- ReviewStep5 (reviewer): overall_correctness=correct. 3 dead-code findings -> fixed: removed orphaned `modprobe::reload`, deleted `PciDeviceError` + `ProfileError::PciDevice`, dropped unused `I2cDevice::driver_name`.
- External review round (report supplied by user): fixed all 3 blocking items + strongly-recommended ones:
  - (3.1) one `config::current()` snapshot per profile fn + `&Config` threaded through pci/device-policy helpers; single snapshot in `apply_profile`.
  - (3.2) `rfkill::set_soft` now takes `bool` (kernel rfkill ABI only accepts 0/1).
  - (3.6) `set_power_control` skips absent `power/control` at debug level (USB interface nodes no longer spam warnings); real write errors still `warn!`.
  - (3.8) shipped `system76-power.conf` now documents `[profile] [usb] [audio] [wifi] [pci] [radio] [cpu]` with defaults.
  - (3.10) dropped redundant `Path::new` + unused import in `wifi.rs`.
  - Rejected as invalid: (3.3) `signal_handling` DOES `break` on SIGINT/SIGTERM (verified daemon/mod.rs:69-71). Intentional per plan: (3.4) `power_profile` blanking before re-apply; (3.7) `battery_epp = "power"` default.
- FinalReview (subagent): found P2 real defect — `set_brightness = initial_set || dim_on_battery` made the new `dim_on_battery` knob dead (`initial_set` is permanently true post-startup), so dimming stayed always-on. Fixed to `set_brightness = config.profile.dim_on_battery` (implements the plan's stated 'unconditional-true becomes opt-in' intent). Reviewer confirmed: config-snapshot refactor complete, `set_soft(bool)`/`set_power_control` correct, parser/AC-handler/reload/cpufreq/wifi clean, no residual dead code, and the 3.3 claim rejected.
- Step 6 done (charge thresholds no longer gated on System76/Huawei/ThinkPad firmware: `Mechanism::{Thresholds{start,end},Conservation,None}` + `detect()` scanning `/sys/class/power_supply/BAT*/charge_control_end_threshold` then the ideapad driver's `conservation_mode`; Thresholds path tolerates a missing start attribute (reports 0); Conservation writes `1` for `end<=70` else `0` and maps `1 -> (50,60)`, `0 -> (90,100)`; the old vendor-ACPI `is_supported()` gate and `supports_thresholds()` are gone). Smoke on the Legion (non-root): mechanism detected as `Conservation(/sys/bus/platform/drivers/ideapad_acpi/VPC2004:00/conservation_mode)`, `get` returned `Ok((50,60))` with `conservation_mode=1`, set attempted the conservation write (EACCES without root), validation errors intact. `cargo test` 24 passed.
- Step 7 done (dGPU runtime D3: new `MODPROBE_HYBRID` template = `options nvidia NVreg_DynamicPowerManagement=0x02`, selected for `GraphicsMode::Hybrid`; `MODPROBE_COMPUTE` gained the same option line but kept its `nvidia-drm`/`nvidia-modeset` blacklists — a literal "Compute → MODPROBE_HYBRID" would have dropped those and let the dGPU attach to displays, i.e. broken compute-only mode). This host: AD106M RTX 4070 Max-Q + Raphael iGPU, NVIDIA modules loaded, `/etc/modprobe.d/system76-power.conf` currently only S3 `NVreg_PreserveVideoMemoryAllocations` (pre-change binary). `cargo test` 24 passed.
- Step 8 done (`system76-power doctor`: new `src/doctor.rs` `run() -> anyhow::Result<bool>` — read-only, no daemon; active profile from `platform_profile` (low-power/quiet→Battery, balanced, performance) or `auto_switch`+power-supply fallback, unknown profile ⇒ profile-dependent checks SKIP; per-knob `OK`/`DRIFT`/`SKIP` for ACPI platform profile, CPU boost, governor, EPP (incl. battery availability fallback), laptop_mode, dirty_writeback, PCIe ASPM (bracketed active policy), HDA power save, `iw` Wi-Fi power save, rfkill bluetooth, SMU (always SKIP); final `N OK, N DRIFT, N SKIP`, exit 1 on any DRIFT; dispatched in `client.rs` before the zbus connection so it runs with the daemon stopped; 7 pure unit tests for the mapping/expectation/parse/classify helpers). `main.rs` unchanged (Doctor falls into the `_ => client::client(&args)` arm); `Cargo.toml` unchanged. `cargo build` green.
- ReviewStep8 (external review): 1 critical + 3 minor findings. Fixed: (a) `doctor` no longer asserts Bluetooth soft-block `0` on non-Battery profiles — `apply_device_policies` only blocks on battery and never unblocks, so a user-off Bluetooth on AC produced a false `DRIFT`/exit 1; the check now SKIPs outside Battery via the new pure `bluetooth_expectation()` (+ regression test). (b) `set_charge_thresholds` no longer raises the end threshold to 100 when the platform has no start attribute (momentary uncap + redundant write); the pre-write is now conditional and the comment matches the kernel constraint (start must not exceed the current end). (c) `find_thresholds` sorts `BAT*` candidates so `BAT0` deterministically wins on multi-battery systems. Rejected (with reason): moving the doctor dispatch from `client.rs` to `main.rs` — tracker 8.2 specifies the client dispatch and the current-thread runtime is already created for every client invocation. `cargo test` 32 passed.
- Step 9 done (hygiene: `version()` now reports `env!("CARGO_PKG_VERSION")` (1.2.8, was hardcoded 1.2.1); D-Bus introspection XML gains `AutoGraphicsPower`, `SetGraphicsRuntime(vendor)`, `GraphicsModeChanged(mode)`, `GraphicsInitramfsDone(mode, success)` — validated with `xmllint`; the two commented logs restored at `debug!` (fan.rs discover error, hid_backlight missing kbd_backlight); repo `system76-power.conf` `[display_modes] enabled = false` with `performance = 240` kept; Makefile gained `sysconfdir ?= /etc` plus `install-config`/`uninstall-config` targets that are deliberately NOT part of `install` (`make -n install` touches no conf file); README documents `[profile] [usb] [audio] [wifi] [pci] [radio] [cpu]` defaults, `dim_on_battery`, the `doctor` subcommand with sample output, and the `install-config` step). `cargo test` 32 passed.
- Docs sync after the Step 4 behavior change and Steps 2-5 config work: `system76-power.conf.examples` was still describing the old model (auto-switch "replaces profile switching when display_modes is enabled", refresh rate "only if display_modes is disabled", "config is loaded on daemon startup, restart to apply") and lacked every new section. Now: corrected auto-switch/refresh-rate trigger docs (composite AC event; refresh-rate changes never alter the resolution), added `[profile] [usb] [audio] [wifi] [pci] [radio] [cpu]` with defaults, documented `doctor` + `charge-thresholds` (real flags: `--profile`, `--list-profiles`, positional `start end`) and SIGHUP reload. README: new-features table rows (device policies, dGPU D3, charge thresholds, doctor, SIGHUP), new `## Charge Thresholds` section + TOC entry, hybrid rtD3 modprobe note, device-policy pointer in Power Profiles. Verified by parsing `system76-power.conf.examples` through `config::parse` (temp test, then deleted): every section equals `Config::default()` except the intentional `[display_modes]` example values (`enabled = false`, 2560x1440@165 / 1920x1080@60).
- STOP POINT: plan complete (all of Steps 1-9). Remaining work is hardware verification only (V2-V9: install + daemon restart, SIGHUP reload, per-profile knob tally, Legion conservation_mode, dGPU D3 after reboot, doctor clean path).
