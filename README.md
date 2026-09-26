# System76 Power Management (Enhanced Fork)

**system76-power** is a utility for managing graphics, power profiles, and display settings.

This fork extends the original [System76 power management daemon](https://github.com/pop-os/system76-power) with additional features for automatic power optimization, display refresh rate management, and runtime GPU switching.

---

## Table of Contents

- [New Features in This Fork](#new-features-in-this-fork)
- [AC Power Auto-Switching](#ac-power-auto-switching)
- [Display Management](#display-management)
- [Runtime GPU Switching](#runtime-gpu-switching)
- [Switchable Graphics](#switchable-graphics)
- [Power Profiles](#power-profiles)
- [Configuration](#configuration)
- [Charge Thresholds](#charge-thresholds)
- [CLI Reference](#cli-reference)
- [Desktop Environment Compatibility](#desktop-environment-compatibility)
- [Troubleshooting](#troubleshooting)
- [Hotplug Detection](#hotplug-detection)

---

## New Features in This Fork

| Feature | Description |
|---------|-------------|
| **AC Power Auto-Switching** | Automatically switch power profiles when AC adapter is plugged/unplugged |
| **Configurable Device Policies** | `[profile] [usb] [audio] [wifi] [pci] [radio] [cpu]` sections drive every knob per profile |
| **Display Refresh Rate Management** | Per-profile refresh rate settings (e.g., 165Hz on performance, 60Hz on battery) |
| **Display Mode Switching** | Change resolution AND refresh rate based on AC power state |
| **Runtime GPU Switching** | Switch graphics modes without rebooting |
| **dGPU Runtime D3** | `NVreg_DynamicPowerManagement=0x02` for hybrid/compute modes |
| **Charge Thresholds** | Generic `charge_control_*` sysfs or Lenovo `conservation_mode` |
| **`doctor` Audit** | Read-only per-knob `OK`/`DRIFT`/`SKIP` report; works without root or daemon |
| **SIGHUP Reload** | Re-read the config and re-apply the active profile without restarting |
| **KDE Wayland Support** | Display management for KDE Plasma via `kscreen-doctor` |
| **AMD P-State Fixes** | Proper frequency scaling for `amd-pstate` and `amd-pstate-epp` drivers |
| **RyzenAdj Integration** | Fine-grained AMD CPU power limit control |

---

## AC Power Auto-Switching

Automatically switch power profiles based on AC adapter connection state.

### How It Works

- Uses **Netlink kernel events** for instant detection (no polling)
- Zero CPU usage when idle — event-driven architecture
- Hardware interrupt notifications from the kernel power_supply subsystem

### Behavior

| AC State | Action |
|----------|--------|
| **Plugged in** | Switch to Balanced profile |
| **Unplugged** | Switch to Battery profile |

### Manual Override

Manual profile changes temporarily disable auto-switching until the next AC power state change. This allows you to override the automatic choice when needed.

### Configuration

```ini
[auto_switch]
enabled = true
```

---

## Display Management

Manage display refresh rates and resolutions based on power profiles or AC state.

### Supported Backends

| Desktop Environment | Backend Tool | Status |
|--------------------|--------------|--------|
| GNOME Wayland | `gnome-randr` | ✅ Full support |
| KDE Plasma Wayland | `kscreen-doctor` | ✅ Full support |
| X11 (any DE) | `xrandr` | ✅ Full support |
| Sway, Hyprland, etc. | — | ⚠️ Detection only |

### Method 1: Profile-Based Refresh Rate

Set different refresh rates for each power profile:

```ini
[refresh_rate]
enabled = true
battery = 60        # Hz when on battery profile
balanced = 60       # Hz when on balanced profile
performance = 165   # Hz when on performance profile
```

**Use case:** You want the display to match your power profile. When you manually switch to Performance mode for gaming, the refresh rate increases automatically.

### Method 2: AC-Based Display Mode Switching

Change both resolution AND refresh rate when AC power state changes:

```ini
[display_modes]
enabled = true

# Simple method: resolution + refresh rate
ac_resolution = "2560x1440"
ac_refresh_rate = 165
battery_resolution = "1920x1080"
battery_refresh_rate = 60
```

**Use case:** You want maximum quality when plugged in (native resolution, high refresh) and power savings on battery (lower resolution, 60Hz).

### Method 3: Explicit Mode Strings

For precise control, including VRR (Variable Refresh Rate):

```ini
[display_modes]
enabled = true
ac_mode = "2560x1440@165.001+vrr"
battery_mode = "1920x1080@60.002"
```

Get available mode strings from:
- GNOME: `gnome-randr query | grep eDP`
- KDE: `kscreen-doctor --outputs`
- X11: `xrandr --query`

### Features

- **Dynamic mode discovery** — queries available modes at runtime
- **VRR support** — preserves Variable Refresh Rate when available
- **Fuzzy matching** — tolerates minor refresh rate differences (e.g., 60.002 matches 60)
- **Built-in display detection** — automatically finds eDP/LVDS laptop displays

---

## Runtime GPU Switching

Switch graphics modes **without rebooting** on supported hardware.

### Usage

```bash
# Switch to integrated graphics (no reboot required)
sudo system76-power graphics runtime integrated

# Switch to hybrid mode
sudo system76-power graphics runtime hybrid

# Switch to NVIDIA mode
sudo system76-power graphics runtime nvidia

# Switch to compute mode
sudo system76-power graphics runtime compute
```

### How It Works

1. Stops display manager (GDM/SDDM/LightDM)
2. Unbinds framebuffers and VT consoles
3. Terminates processes using NVIDIA devices
4. Unloads NVIDIA kernel modules
5. Removes/rescans PCI devices
6. Loads appropriate drivers for new mode
7. Restarts display manager

### D-Bus Interface

```
Method: SetGraphicsRuntime(vendor: &str)
Signal: GraphicsModeChanged — emitted when switch completes
Signal: GraphicsInitramfsDone — emitted when background initramfs rebuild finishes
```

### Limitations

- Some models with external displays connected to dGPU may not support switching away from NVIDIA mode
- Active NVIDIA processes must be terminated for the switch to succeed
- Wayland sessions handle the transition better than X11

---

## Switchable Graphics

Switchable graphics is a feature for laptops and all-in-one PCs. It is not supported on desktops.

### Traditional Mode Switching (Reboot Required)

```bash
# These commands require a reboot to take effect
sudo system76-power graphics integrated
sudo system76-power graphics hybrid
sudo system76-power graphics nvidia
sudo system76-power graphics compute
```

### Integrated

The integrated graphics controller on the Intel or AMD CPU is used exclusively.

- Lower graphical performance with longer battery life
- External displays connected to dGPU ports cannot be used

### NVIDIA

The dGPU (NVIDIA) is used exclusively.

- Higher graphical performance at the expense of shorter battery life
- Allows using external displays

### Hybrid

Enables PRIME render offloading. The iGPU is used as the primary renderer, with the ability to have specific applications render using the dGPU.

**Requirements:**
- NVIDIA drivers 435.17 or later for PRIME render offloading
- NVIDIA drivers 450.57 or later for display offload sinks ("reverse PRIME")

**Launching applications on dGPU:**

```bash
# Vulkan applications
__NV_PRIME_RENDER_OFFLOAD=1 ./my-vulkan-app

# OpenGL applications
__NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia ./my-opengl-app
```

Applications must use [GLVND](https://gitlab.freedesktop.org/glvnd/libglvnd) to take advantage of this feature.

**Runtime Power Management:**
GPU support for run-time power management is required for the device to enter a low power state when not used. Only Turing cards and newer fully implement this functionality.

For `hybrid` and `compute` modes this fork writes
`options nvidia NVreg_DynamicPowerManagement=0x02` (fine-grained runtime D3) to
`/etc/modprobe.d/system76-power.conf`; `compute` additionally keeps the
`nvidia-drm`/`nvidia-modeset` blacklists so the dGPU never joins the display
pipeline.

```bash
# Check if your GPU supports runtime PM
cat /sys/bus/pci/devices/0000:01:00.0/device
# 0x1f15

jq '.chips[] | select(.devid=="0x1F15")' < /usr/share/doc/nvidia-driver-460/supported-gpus.json
# Look for "runtimepm" in features
```

### Compute

The integrated graphics controller is used exclusively for rendering. The dGPU is made available as a compute node.

- Useful for CUDA/OpenCL workloads without display overhead
- Lower power consumption than full NVIDIA mode

---

## Power Profiles

> AMD power limits (RyzenAdj) are applied best effort: if `ryzenadj` is missing,
> the SMU is unsupported, or PCI config space is write-protected, a warning is
> logged and the rest of the profile still applies. The limits below are tuned
> for HX-class Ryzen packages (55 W sustained); scale down for U-series parts.

Device-level policy (USB autosuspend, HDA power-save, Wi-Fi power-save, PCI
runtime PM, Bluetooth blocking) is configurable per profile through the
`[usb] [audio] [wifi] [pci] [radio]` sections — see [Configuration](#configuration).
Run `system76-power doctor` to verify what actually took effect.

### Battery

Optimized for maximum battery life:

- Dirty writeback increased to 30s
- CPU frequency capped at 60% (~2.7GHz typical)
- Intel P-State: `max_perf_pct=25`, `no_turbo=true`
- PCIe ASPM set to `powersupersave`
- Screen brightness reduced
- Keyboard backlight off
- **AMD (RyzenAdj):** STAPM=12W, Fast=18W, Slow=10W, Tctl=60°C

### Balanced

Good balance of performance and efficiency:

- Dirty writeback at 15s
- Laptop mode enabled in kernel
- SCSI/SATA link time power management enabled
- Intel P-State values optimized
- PCIe ASPM set to `default`
- **AMD (RyzenAdj):** STAPM=55W, Fast=80W, Slow=80W, Tctl=95°C (HX-class package)

### Performance

Maximum performance:

- Dirty writeback reduced to 10s
- ACPI Platform profile used if supported
- PCIe ASPM set to `default`
- I2C runtime PM disabled (lowest latency)
- **AMD (RyzenAdj):** STAPM=55W, Fast=80W, Slow=80W, Tctl=95°C (explicit limits, not `--max-performance`)

---

## Configuration

Configuration file: `/etc/system76-power.conf`

Reload the configuration without restarting the daemon:

```bash
sudo kill -HUP $(pidof system76-power)
```

On `SIGHUP` the file is re-read and the active profile is re-applied immediately,
so changes to any section below take effect at once.

### Full Configuration Reference

```ini
################################################################################
# System76 Power Configuration
################################################################################

#-------------------------------------------------------------------------------
# Auto-Switch: Automatic profile switching based on AC power state
#-------------------------------------------------------------------------------
[auto_switch]
# Enable automatic profile switching when AC adapter is plugged/unplugged
# When enabled:
#   - AC connected  → Balanced profile
#   - AC disconnected → Battery profile
# Manual profile changes temporarily disable auto-switching until next AC change
enabled = true


#-------------------------------------------------------------------------------
# Refresh Rate: Per-profile display refresh rate settings
#-------------------------------------------------------------------------------
[refresh_rate]
# Enable profile-based refresh rate switching
# Changes refresh rate when power profile changes (manual or auto)
enabled = true

# Refresh rates in Hz for each profile
battery = 60
balanced = 60
performance = 165


#-------------------------------------------------------------------------------
# Display Modes: AC-based resolution and refresh rate switching
#-------------------------------------------------------------------------------
[display_modes]
# Enable AC-based display mode switching
# Changes both resolution AND refresh rate on AC plug/unplug
# NOTE: If enabled, this takes priority over [refresh_rate] for AC events
enabled = false

# METHOD 1: Resolution + Refresh Rate (auto-discovers best matching mode)
ac_resolution = "2560x1440"
ac_refresh_rate = 165
battery_resolution = "1920x1080"
battery_refresh_rate = 60

# METHOD 2: Explicit mode strings (takes priority over METHOD 1)
# Get mode strings from: gnome-randr query | grep eDP
# ac_mode = "2560x1440@165.001+vrr"
# battery_mode = "1920x1080@60.002"


#-------------------------------------------------------------------------------
# Profile: Battery-profile display dimming
#-------------------------------------------------------------------------------
[profile]
# Dim the screen and turn off the keyboard backlight when the BATTERY profile is
# applied (opt-in; other profiles never touch brightness)
dim_on_battery = false


#-------------------------------------------------------------------------------
# USB: Device autosuspend
#-------------------------------------------------------------------------------
[usb]
# Autosuspend USB devices under battery/balanced (minus blacklists);
# performance always sets `on`
autosuspend = true

# Interface drivers never autosuspended (space-separated)
blacklist_drivers = "usblp"

# Devices never autosuspended, as lowercase `vid:pid` (space-separated)
blacklist = ""


#-------------------------------------------------------------------------------
# Audio: HDA power saving
#-------------------------------------------------------------------------------
[audio]
# Enable HDA controller/device power-save on battery/balanced, clear it for
# performance
power_save = true


#-------------------------------------------------------------------------------
# Wi-Fi: Radio power saving
#-------------------------------------------------------------------------------
[wifi]
# Enable `iw` power-save on battery, disable it for balanced/performance
power_save = true


#-------------------------------------------------------------------------------
# PCI: Runtime power management
#-------------------------------------------------------------------------------
[pci]
# Enable runtime PM on battery/balanced, disable it for performance
runtime_pm = true

# Drivers never touched (space-separated)
blacklist_drivers = "amdgpu nvidia nouveau radeon"


#-------------------------------------------------------------------------------
# Radio: Bluetooth on battery
#-------------------------------------------------------------------------------
[radio]
# Soft-block Bluetooth while on battery (never unblocked automatically)
bluetooth_off_on_battery = false


#-------------------------------------------------------------------------------
# CPU: AMD SMU limits and energy preference
#-------------------------------------------------------------------------------
[cpu]
# AMD SMU limits in milliwatts, applied through `ryzenadj` (best effort).
# Defaults suit an HX-class package (55W sustained); scale down for U-series.
battery_stapm_mw = 12000
battery_fast_mw = 18000
battery_slow_mw = 10000
battery_tctl_c = 60
ac_stapm_mw = 55000
ac_fast_mw = 80000
ac_slow_mw = 80000
ac_tctl_c = 95

# `energy_performance_preference` for the battery profile on `amd-pstate-epp`
battery_epp = "power"
```

### Example Configurations

#### Gaming Laptop (High Refresh Display)

```ini
[auto_switch]
enabled = true

[refresh_rate]
enabled = true
battery = 60
balanced = 120
performance = 165

[display_modes]
enabled = false
```

#### Productivity Focus (Resolution Priority)

```ini
[auto_switch]
enabled = true

[refresh_rate]
enabled = false

[display_modes]
enabled = true
ac_resolution = "2560x1440"
ac_refresh_rate = 60
battery_resolution = "1920x1080"
battery_refresh_rate = 60
```

#### Maximum Battery Life

```ini
[auto_switch]
enabled = true

[refresh_rate]
enabled = true
battery = 48        # Use lowest available
balanced = 60
performance = 60    # Don't increase for performance

[display_modes]
enabled = true
ac_resolution = "2560x1440"
ac_refresh_rate = 60
battery_resolution = "1280x720"
battery_refresh_rate = 48
```

---

## Charge Thresholds

Limit battery charging to extend battery lifespan. The mechanism is detected at
runtime, so no firmware-specific configuration is needed:

| Mechanism | Detection | Behaviour |
|-----------|-----------|-----------|
| Generic sysfs pair | `BAT*/charge_control_end_threshold` (start optional) | Writes the requested `(start, end)`; end-only platforms report `start = 0` |
| Lenovo conservation mode | `ideapad_acpi/*/conservation_mode` | `1` (charge to ~60%) when `end <= 70`, else `0` (full charge); reported as `(50,60)` / `(90,100)` |
| Unsupported | neither present | The D-Bus call returns an error |

```bash
# Query current thresholds (shows the matching profile, if any)
system76-power charge-thresholds

# Apply a built-in profile
system76-power charge-thresholds --profile max_lifespan
system76-power charge-thresholds --profile balanced
system76-power charge-thresholds --profile full_charge

# Or set explicit start/end values
system76-power charge-thresholds 50 60

# List the built-in profiles
system76-power charge-thresholds --list-profiles
```

Built-in profiles: `full_charge` (90–100%), `balanced` (86–90%), `max_lifespan`
(50–60%).

---

## CLI Reference

### Power Profiles

```bash
# Get current profile
system76-power profile

# Set profile
system76-power profile battery
system76-power profile balanced
system76-power profile performance
```

### Doctor

Audit that the configured power settings are actually in effect. `doctor` is
read-only, needs no root, and talks to neither the daemon nor D-Bus, so it also
works while the daemon is stopped. Every knob is reported as `OK` (in effect),
`DRIFT` (configured but not in effect) or `SKIP` (not auditable on this system);
the exit code is `1` when at least one knob drifted.

```bash
system76-power doctor
```

```
Active profile: Battery (from /sys/firmware/acpi/platform_profile=low-power)
OK     ACPI platform profile: low-power (expected low-power)
DRIFT  CPU boost: 1 (expected 0)
SKIP   SMU/ryzenadj limits: requires root to read the SMU table
2 OK, 1 DRIFT, 1 SKIP
```

The active profile is inferred from `/sys/firmware/acpi/platform_profile`;
without platform-profile support it falls back to the power-supply state when
`[auto_switch]` is enabled, and profile-dependent knobs report `SKIP` when the
profile cannot be determined.

### Graphics

```bash
# Get current graphics mode
system76-power graphics

# Set graphics mode (reboot required)
system76-power graphics integrated
system76-power graphics hybrid
system76-power graphics nvidia
system76-power graphics compute

# Runtime graphics switching (no reboot)
sudo system76-power graphics runtime integrated
sudo system76-power graphics runtime hybrid
sudo system76-power graphics runtime nvidia
sudo system76-power graphics runtime compute

# Check switchable graphics capability
system76-power graphics switchable
```

### Daemon

```bash
# Start daemon
sudo system76-power daemon

# Start daemon with verbose logging
sudo system76-power daemon --verbose
```

---

## Desktop Environment Compatibility

| Environment | Profile Switching | Display Modes | Runtime GPU | Notes |
|-------------|-------------------|---------------|-------------|-------|
| GNOME Wayland | ✅ | ✅ | ✅ | Requires `gnome-randr` |
| GNOME X11 | ✅ | ✅ | ✅ | Uses `xrandr` |
| KDE Plasma Wayland | ✅ | ✅ | ✅ | Uses `kscreen-doctor` |
| KDE Plasma X11 | ✅ | ✅ | ✅ | Uses `xrandr` |
| XFCE | ✅ | ✅ | ✅ | Uses `xrandr` |
| MATE | ✅ | ✅ | ✅ | Uses `xrandr` |
| Sway | ✅ | ⚠️ | ✅ | Display modes not yet implemented |
| Hyprland | ✅ | ⚠️ | ✅ | Display modes not yet implemented |

### Required Tools

| Backend | Tool | Installation |
|---------|------|--------------|
| GNOME Wayland | `gnome-randr` | `pip install gnome-randr` or system package |
| KDE Wayland | `kscreen-doctor` | Included with KDE Plasma |
| X11 | `xrandr` | Included with X.Org |

---

## Troubleshooting

### Display mode switching not working

1. **Check if the daemon is detecting your display server:**
   ```bash
   journalctl -u system76-power -f
   # Look for "Detected display server: GnomeWayland" or similar
   ```

2. **Verify the backend tool is available:**
   ```bash
   # GNOME
   which gnome-randr
   gnome-randr query
   
   # KDE
   which kscreen-doctor
   kscreen-doctor --outputs
   
   # X11
   xrandr --query
   ```

3. **Check available modes for your display:**
   ```bash
   # GNOME
   gnome-randr query | grep -A 50 eDP
   
   # KDE
   kscreen-doctor --outputs | grep -A 20 eDP
   
   # X11
   xrandr | grep -A 20 eDP
   ```

### Auto-switching not working

1. **Verify AC adapter detection:**
   ```bash
   cat /sys/class/power_supply/*/online
   # Should show 1 (plugged) or 0 (unplugged)
   ```

2. **Check daemon logs:**
   ```bash
   journalctl -u system76-power -f
   # Plug/unplug AC adapter and watch for events
   ```

3. **Verify configuration:**
   ```bash
   cat /etc/system76-power.conf
   # Ensure [auto_switch] enabled = true
   ```

### Runtime GPU switching fails

1. **Check for processes using NVIDIA:**
   ```bash
   fuser -v /dev/nvidia*
   lsof /dev/nvidia*
   ```

2. **Try switching from a TTY:**
   ```bash
   # Press Ctrl+Alt+F3 to switch to TTY3
   sudo system76-power graphics runtime integrated
   ```

3. **Check kernel module status:**
   ```bash
   lsmod | grep nvidia
   ```

### AMD P-State not working correctly

1. **Verify driver:**
   ```bash
   cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver
   # Should show amd-pstate or amd-pstate-epp
   ```

2. **Check frequency limits:**
   ```bash
   cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_min_freq
   cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq
   ```

---

## Hotplug Detection

The D-Bus signal `HotPlugDetect` is sent when a display is plugged into a port connected to the dGPU. If in integrated or compute mode, the [GNOME extension](https://github.com/pop-os/gnome-shell-extension-system76-power) will prompt to switch to hybrid mode so the display can be used.

### Adding Hotplug Detection

#### Intel-based Systems

The GPIO (sideband) port and pins for the display ports can be determined with the schematics and output of [coreboot-collector](https://github.com/system76/coreboot-collector). The schematics will indicate which GPIOs are display ports (`*_HPD`). The corresponding `GPP_*` entry in `coreboot-collector.txt` will have the port/pin tuple.

##### Muxed DisplayPort

Some models have muxed DisplayPort output from mDP and USB-C. These units have a separate data switch pin that is used to determine which output is used.

#### AMD-based Systems

A MMIO region for FCH GPIO controls is used to detect external display plug events. Display ports use `*_HPD` as Intel systems, but may not map to a literal GPIO (e.g., `HDMI_HPD` maps to `DP3_HPD` on kudu6). Generating a diff from coreboot-collector in NVIDIA mode before and after plugging in a display should provide the GPIO number.

---

## Building from Source

```bash
# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone repository
git clone https://github.com/AshkanRkmd/system76-power.git
cd system76-power

# Build
cargo build --release

# Install
sudo make install

# Optionally install the sample configuration (never done by `make install`,
# so upgrades cannot overwrite an existing /etc/system76-power.conf)
sudo make install-config

# Enable and start service
sudo systemctl enable --now system76-power
```

### Development Environment (Nix)

```bash
# Using devenv
direnv allow
devenv up

# Or manually
nix develop
```

---

## License

GPL-3.0-only

## Original Project

This is a fork of [pop-os/system76-power](https://github.com/pop-os/system76-power) with additional features by scarlet_bean.

## Contributing

Contributions are welcome! Please ensure:
1. Code compiles with `cargo check`
2. Format with `cargo fmt`
3. No new clippy warnings with `cargo clippy`
