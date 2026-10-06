# Testing

This document provides a guideline for testing and verifying the expected behaviors of the project. When a patch is ready for testing, the checklists may be copied and marked as they are proven to be working.

## Checklists

Tasks for a tester to verify when approving a patch.

### CLI

Tasks which test the behavior of the CLI client.

- [ ] Power profiles can be queried and set
- [ ] Laptop with switchable graphics:
    - [ ] Switchable graphics can be queried
        - Command returns `not switchable` on a non-switchable system
        - Command returns `switchable` on a laptop with switchable graphics
    - [ ] Switching from Integrated to NVIDIA
    - [ ] Switching from Integrated to Hybrid
    - [ ] Switching from Integrated to Compute
    - [ ] Switching from NVIDIA to Integrated
    - [ ] Switching from NVIDIA to Hybrid
    - [ ] Switching from NVIDIA to Compute
    - [ ] Switching from Hybrid to Integrated
    - [ ] Switching from Hybrid to NVIDIA
    - [ ] Switching from Hybrid to Compute
    - [ ] Switching from Compute to Integrated
    - [ ] Switching from Compute to NVIDIA
    - [ ] Switching from Compute to Hybrid
    - [ ] Discrete graphics power state can be queried and set


### GNOME Shell

Tasks which test the behavior of the shell extension.

- [ ] Test that the power profile can be switched, and that the dots are correct
- [ ] Test that any power profile change from the CLI is reflected in the extension
- [ ] When switching to balanced, with screen brightness maxed, screen brightness drops to 50%
- [ ] When switching to battery, with screen brightness maxed, screen brightness drops to 10%
- [ ] When switching to balanced, with screen brightness minimized, screen brightness does not change
- [ ] When restarting the daemon, and the daemon defaults to a balanced profile, the brightness should not change
- [ ] When restarting the system, screen brightness should be the same as before
- [ ] Laptop with switchable graphics:
    - [ ] Switching from Integrated to NVIDIA
    - [ ] Switching from Integrated to Hybrid
    - [ ] Switching from Integrated to Compute
    - [ ] Switching from NVIDIA to Integrated
    - [ ] Switching from NVIDIA to Hybrid
    - [ ] Switching from NVIDIA to Compute
    - [ ] Switching from Hybrid to Integrated
    - [ ] Switching from Hybrid to NVIDIA
    - [ ] Switching from Hybrid to Compute
    - [ ] Switching from Compute to Integrated
    - [ ] Switching from Compute to NVIDIA
    - [ ] Switching from Compute to Hybrid
    - [ ] Test that switchable graphics changes from the CLI are reflected in the extension

## How To

Instructions for interacting with features for first-time testers.

### CLI

- Query power profile information
    ```sh
    system76-power profile
    ```
- Set power profile information
    ```sh
    system76-power profile [ battery | balanced | performance ]
    ```
- Query switchable graphics capability
    ```sh
    system76-power graphics switchable
    ```
- Query active graphics
    ```sh
    system76-power graphics
    ```
- Set graphics to integrated
    ```sh
    system76-power graphics integrated
    ```
- Set graphics to NVIDIA
    ```sh
    system76-power graphics nvidia
    ```
- Set graphics to hybrid
    ```sh
    system76-power graphics hybrid
    ```
- Set graphics to compute mode
    ```sh
    system76-power graphics compute
    ```
- Query discrete graphics power state
    ```sh
    system76-power graphics power
    ```
- Set discrete graphics power state
    ```sh
    system76-power graphics power [ on | off]
    ```

### Runtime Graphics Switching (No-Reboot)

Tasks for verifying runtime (no-reboot) graphics switches on hardware.

- [ ] **M1: Integrated → Hybrid (`runtime_restart_dm = "always"`)**
    - [ ] Display manager restarts cleanly
    - [ ] `nvidia_drm` loaded with `modeset=Y`
    - [ ] `nvidia_uvm` loaded
    - [ ] `power/control` settles to `auto` after 5 seconds
    - [ ] PRIME offload works (`prime-run glxinfo` or `__NV_PRIME_RENDER_OFFLOAD=1`)
    - [ ] Text consoles (Ctrl+Alt+F3) remain functional
    - [ ] `GraphicsInitramfsDone(true)` emitted and status file updated
- [ ] **M2: Integrated → Hybrid (`runtime_restart_dm = "auto"`, Wayland)**
    - [ ] Display manager is NOT restarted; user desktop session survives intact
    - [ ] NVIDIA card appears in `switcherooctl`
- [ ] **M3: Integrated → Compute**
    - [ ] `nvidia` loaded; `nvidia_drm` remains absent/blacklisted
    - [ ] CUDA / compute workloads functional
- [ ] **M4: Integrated → NVIDIA (runtime)**
    - [ ] Rejected immediately with actionable reboot hint (`RuntimeDiscreteUnsupported`); no system state altered
- [ ] **M5: Injected failure rollback**
    - [ ] Injected failure during module load triggers automated rollback
    - [ ] Display manager restarted/restored
    - [ ] Config files and vendor mode reverted to previous mode
    - [ ] `system76-power graphics runtime --status` reports failure stage and `rolled_back: true`
- [ ] **M6: No-Change / Idempotency**
    - [ ] Switching to the current mode (e.g. Integrated → Integrated) exits immediately without DM restart
- [ ] **M7: No DM / Headless / TTY**
    - [ ] Running runtime switch from TTY without an active DM succeeds without attempting to start GDM
- [ ] **M8: Connected dGPU displays**
    - [ ] Switching away from Hybrid with an active monitor on a dGPU port is rejected early
- [ ] **M9: Stress Test**
    - [ ] Toggling Integrated ↔ Hybrid 10 times consecutively produces no leaks, hangs, or kernel Xid errors
- [ ] **M10: Suspend / Resume**
    - [ ] System suspends and resumes properly after runtime switch
- [ ] **M11: Concurrency Guard**
    - [ ] Launching two concurrent `set_graphics_runtime` calls rejects the second with "Graphics switch in progress"
- [ ] **M12: Boot consistency**
    - [ ] Rebooting after runtime switch boots directly into the chosen mode

### How To: Runtime Switching

- Switch graphics mode and wait for completion:
    ```sh
    system76-power graphics runtime hybrid --wait
    ```
- Query last switch status:
    ```sh
    system76-power graphics runtime --status
    ```
