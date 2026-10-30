// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Display refresh rate management
//!
//! This module provides functionality to control display refresh rates based on
//! power profiles. It supports GNOME Wayland (via gnome-randr), KDE Wayland
//! (kscreen-doctor), Hyprland (hyprctl), other wlroots compositors such as
//! Mango (wlr-randr) and X11 (xrandr).

use std::collections::BTreeMap;
use std::io;
use std::process::Command;

use crate::config::ModeSpec;
use crate::errors::DisplayError;

/// Compositor processes to search for, in priority order
const COMPOSITOR_PROCESSES: &[&str] = &[
    "gnome-shell",  // GNOME Wayland/X11
    "kwin_wayland", // KDE Plasma Wayland
    "kwin_x11",     // KDE Plasma X11
    "plasmashell",  // KDE Plasma (fallback)
    "sway",         // Sway
    "Hyprland",     // Hyprland
    "wayfire",      // Wayfire
    "labwc",        // labwc
    "river",        // River
    "dwl",          // dwl
    "mango",        // MangoWM
    "Xorg",         // X11 server (fallback)
];

/// Environment variables to extract from user session
const SESSION_ENV_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "WAYLAND_SOCKET",
    "QT_QPA_PLATFORM",
    "HYPRLAND_INSTANCE_SIGNATURE",
];

/// Represents a display mode with resolution and refresh rate
#[derive(Debug, Clone)]
pub struct DisplayMode {
    /// Full mode string (e.g., "2560x1440@165.001+vrr")
    pub mode_string: String,
    /// Resolution as (width, height)
    pub resolution: (u32, u32),
    /// Refresh rate in Hz
    pub refresh_rate: f32,
    /// Whether VRR (Variable Refresh Rate) is supported
    pub has_vrr: bool,
    /// Whether this is the mode currently applied to the display
    pub is_current: bool,
}

/// Display-related environment variables of a user session.
///
/// Only the variables listed in [`SESSION_ENV_VARS`] (plus ones we discover
/// ourselves, such as `WAYLAND_DISPLAY`) are stored.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv(BTreeMap<String, String>);

impl SessionEnv {
    /// Collect the session variables from this process's own environment.
    pub fn from_process() -> Self {
        SESSION_ENV_VARS
            .iter()
            .filter_map(|&key| std::env::var(key).ok().map(|value| (key.to_string(), value)))
            .collect()
    }

    /// Parse a NUL-separated `/proc/<pid>/environ` block, keeping session variables only.
    fn from_environ_block(block: &[u8]) -> Self {
        block
            .split(|&byte| byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok()?.split_once('='))
            .filter(|(key, _)| SESSION_ENV_VARS.contains(key))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn insert(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(key.to_string(), value.into());
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.0.iter().map(|(key, value)| (key.as_str(), value.as_str()))
    }

    fn is_wayland_session(&self) -> bool {
        self.get("XDG_SESSION_TYPE") == Some("wayland")
    }

    fn desktop_contains(&self, name: &str) -> bool {
        self.get("XDG_CURRENT_DESKTOP").is_some_and(|desktop| desktop.contains(name))
    }
}

impl FromIterator<(String, String)> for SessionEnv {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Display server types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    GnomeWayland,
    KdeWayland,
    Hyprland,
    Wayland,
    X11,
    Unknown,
}

impl DisplayServer {
    /// Wayland desktops with a dedicated backend, matched against `XDG_CURRENT_DESKTOP`.
    const WAYLAND_DESKTOPS: &'static [(&'static str, DisplayServer)] =
        &[("GNOME", Self::GnomeWayland), ("KDE", Self::KdeWayland), ("Hyprland", Self::Hyprland)];

    /// Detect the display server a session environment belongs to.
    pub fn detect(env: &SessionEnv) -> Option<Self> {
        log::debug!("Detecting display server from {:?}", env);

        // The compositor's own environment never carries WAYLAND_DISPLAY (it only
        // exports it to its children), so the session type is the reliable signal.
        if env.is_wayland_session() {
            let desktop = Self::WAYLAND_DESKTOPS
                .iter()
                .find(|(name, _)| env.desktop_contains(name))
                .map(|&(_, server)| server);

            return Some(desktop.unwrap_or(Self::Wayland));
        }

        if env.contains("WAYLAND_DISPLAY") {
            return Some(Self::Wayland);
        }

        env.contains("DISPLAY").then_some(Self::X11)
    }
}

/// Detect which display server is currently running
pub fn detect_display_server() -> DisplayServer {
    // Direct invocation: this process carries the session environment.
    DisplayServer::detect(&SessionEnv::from_process())
        // System service: query the user session environment instead.
        .or_else(detect_from_user_session)
        .unwrap_or(DisplayServer::Unknown)
}

/// Detect display server from the user session environment
/// This is used when running as a system service without access to session variables
fn detect_from_user_session() -> Option<DisplayServer> {
    log::debug!("Attempting to detect display server from user session");

    let user = get_display_user()
        .inspect_err(|e| log::debug!("Failed to get display user: {}", e))
        .ok()?;
    let env = get_user_session_env(&user)
        .inspect_err(|e| log::debug!("Failed to get user session environment: {}", e))
        .ok()?;

    DisplayServer::detect(&env)
}

/// Session information from loginctl
#[derive(Debug)]
struct SessionInfo {
    id: String,
    user: String,
    seat: String,
    session_type: Option<String>,
    state: Option<String>,
}

/// Session context containing user and environment variables
/// This is fetched once and passed to all helper functions to avoid redundant lookups
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub user: String,
    pub env: SessionEnv,
}

/// Query detailed information about a specific session
fn query_session_details(session_id: &str) -> Result<SessionInfo, io::Error> {
    let output = Command::new("loginctl")
        .arg("show-session")
        .arg(session_id)
        .arg("--property=Type")
        .arg("--property=State")
        .arg("--property=Seat")
        .arg("--property=Name")
        .output()?;

    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to query session {}", session_id),
        ));
    }

    let info = String::from_utf8_lossy(&output.stdout);

    let mut session_type = None;
    let mut state = None;
    let mut seat = String::from("seat0"); // Default
    let mut user = String::new();

    for line in info.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "Type" => session_type = Some(value.to_string()),
                "State" => state = Some(value.to_string()),
                "Seat" => seat = value.to_string(),
                "Name" => user = value.to_string(),
                _ => {}
            }
        }
    }

    log::debug!(
        "Session {} details: type={:?}, state={:?}, seat={}, user={}",
        session_id,
        session_type,
        state,
        seat,
        user
    );

    Ok(SessionInfo { id: session_id.to_string(), user, seat, session_type, state })
}

/// Check if a session is a graphical session (Wayland or X11)
fn is_graphical_session(session: &SessionInfo) -> bool {
    matches!(session.session_type.as_deref(), Some("wayland") | Some("x11"))
}

/// Check if a session is active
fn is_active_session(session: &SessionInfo) -> bool {
    session.state.as_deref() == Some("active")
}

/// Check if a session is on the primary graphics seat
fn is_primary_seat(session: &SessionInfo) -> bool {
    session.seat == "seat0"
}

/// Get the username running the active graphical session
fn get_display_user() -> Result<String, io::Error> {
    log::debug!("Detecting display user");

    // Method 1: Check SUDO_USER environment variable (when run via sudo)
    if let Ok(user) = std::env::var("SUDO_USER") {
        if !user.is_empty() && user != "root" {
            log::debug!("Found display user from SUDO_USER: {}", user);
            return Ok(user);
        }
    }

    // Method 2: Find active graphical session via loginctl
    let output = Command::new("loginctl").arg("list-sessions").arg("--no-legend").output()?;

    if !output.status.success() {
        return Err(io::Error::new(io::ErrorKind::Other, "loginctl list-sessions failed"));
    }

    let sessions = String::from_utf8_lossy(&output.stdout);

    log::debug!("Scanning sessions for active graphical session");

    // Collect and analyze sessions
    let mut candidates: Vec<(String, String)> = Vec::new(); // (session_id, user)

    for line in sessions.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let session_id = parts[0];
        let user = parts[2];

        // Skip root sessions
        if user == "root" {
            log::debug!("Skipping root session: {}", session_id);
            continue;
        }

        candidates.push((session_id.to_string(), user.to_string()));
    }

    log::debug!("Found {} non-root sessions to evaluate", candidates.len());

    // Priority 1: Active graphical session on seat0
    for (session_id, user) in &candidates {
        match query_session_details(session_id) {
            Ok(session) => {
                if is_graphical_session(&session)
                    && is_active_session(&session)
                    && is_primary_seat(&session)
                {
                    log::info!(
                        "Found ideal session: user={}, session={}, type={:?}, seat={}",
                        user,
                        session_id,
                        session.session_type,
                        session.seat
                    );
                    return Ok(user.clone());
                }
            }
            Err(e) => {
                log::debug!("Failed to query session {}: {}", session_id, e);
            }
        }
    }

    // Priority 2: Any graphical session (relaxed seat/state requirements)
    for (session_id, user) in &candidates {
        match query_session_details(session_id) {
            Ok(session) => {
                if is_graphical_session(&session) {
                    log::warn!(
                        "Using graphical session (not ideal): user={}, session={}, type={:?}, state={:?}, seat={}",
                        user,
                        session_id,
                        session.session_type,
                        session.state,
                        session.seat
                    );
                    return Ok(user.clone());
                }
            }
            Err(e) => {
                log::debug!("Failed to query session {}: {}", session_id, e);
            }
        }
    }

    // Priority 3: Fallback - first non-root user (original behavior)
    if let Some((_, user)) = candidates.first() {
        log::warn!("No graphical session found, using first non-root user as fallback: {}", user);
        return Ok(user.clone());
    }

    Err(io::Error::new(io::ErrorKind::NotFound, "No display user found (no non-root sessions)"))
}

/// Find the PID of a compositor process for a specific user
fn find_compositor_pid(user: &str) -> Option<String> {
    log::debug!("Searching for compositor process for user: {}", user);

    for process_name in COMPOSITOR_PROCESSES {
        log::debug!("Trying process: {}", process_name);

        let output = Command::new("pgrep").arg("-u").arg(user).arg("-x").arg(process_name).output();

        match output {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                let pid_str = String::from_utf8_lossy(&output.stdout);
                let first_pid = pid_str.lines().next().unwrap_or("").trim();

                if !first_pid.is_empty() {
                    log::info!("Found compositor: {} (PID: {})", process_name, first_pid);
                    return Some(first_pid.to_string());
                }
            }
            Ok(_) => {
                log::debug!("Process '{}' not found", process_name);
            }
            Err(e) => {
                log::debug!("Failed to search for '{}': {}", process_name, e);
            }
        }
    }

    log::debug!("No compositor process found");
    None
}

/// Find the session leader PID for a user via loginctl
fn find_session_leader_pid(user: &str) -> Result<String, io::Error> {
    log::debug!("Finding session leader for user: {}", user);

    let session_output =
        Command::new("loginctl").arg("list-sessions").arg("--no-legend").output()?;

    let sessions = String::from_utf8_lossy(&session_output.stdout);

    for line in sessions.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[2] == user {
            let session_id = parts[0];
            log::debug!("Found session {} for user {}", session_id, user);

            let leader_output = Command::new("loginctl")
                .arg("show-session")
                .arg(session_id)
                .arg("--property=Leader")
                .arg("--value")
                .output()?;

            if leader_output.status.success() {
                let pid = String::from_utf8_lossy(&leader_output.stdout).trim().to_string();
                log::info!("Session leader PID: {}", pid);
                return Ok(pid);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Could not find session leader for user {}", user),
    ))
}

/// Read the session environment variables from /proc/<pid>/environ
fn read_process_environment(pid: &str) -> Result<SessionEnv, io::Error> {
    log::debug!("Reading environment from PID: {}", pid);

    let environ_path = format!("/proc/{}/environ", pid);
    let environ_data = std::fs::read(&environ_path)
        .map_err(|e| io::Error::new(e.kind(), format!("Failed to read {}: {}", environ_path, e)))?;

    let env = SessionEnv::from_environ_block(&environ_data);

    if env.is_empty() {
        log::warn!("No display environment variables found in PID {}", pid);
    } else {
        for (key, value) in env.iter() {
            log::debug!("Found env: {}={}", key, value);
        }
        log::info!("Extracted {} environment variables from PID {}", env.len(), pid);
    }

    Ok(env)
}

/// Get the display environment of the user session
fn get_user_session_env(user: &str) -> Result<SessionEnv, io::Error> {
    log::info!("Getting session environment for user: {}", user);

    // Strategy 1: Find compositor process (most reliable for display environment)
    if let Some(pid) = find_compositor_pid(user) {
        let mut env = read_process_environment(&pid)?;

        if !env.is_empty() {
            complete_session_env(user, &mut env);
            return Ok(env);
        }

        log::warn!("Compositor PID {} had no display environment, trying fallback", pid);
    }

    // Strategy 2: Use session leader (fallback)
    log::debug!("Falling back to session leader PID");
    let pid = find_session_leader_pid(user)?;
    let mut env = read_process_environment(&pid)?;

    if env.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("No display environment variables found for user {}", user),
        ));
    }

    complete_session_env(user, &mut env);
    Ok(env)
}

/// Add the variables a compositor exports to its children only, and which
/// therefore never appear in the compositor process's own environment.
fn complete_session_env(user: &str, env: &mut SessionEnv) {
    add_hyprland_signature(user, env);
    add_wayland_display(env);
}

/// Ensure the environment carries `WAYLAND_DISPLAY` on Wayland sessions.
///
/// Wayland clients (e.g. `wlr-randr`) need it to find the compositor socket,
/// so look it up in the runtime dir.
fn add_wayland_display(env: &mut SessionEnv) {
    if !env.is_wayland_session() || env.contains("WAYLAND_DISPLAY") {
        return;
    }

    match discover_wayland_display(env) {
        Some(display) => {
            log::debug!("Discovered WAYLAND_DISPLAY: {}", display);
            env.insert("WAYLAND_DISPLAY", display);
        }
        None => log::warn!("Wayland session but no compositor socket found in XDG_RUNTIME_DIR"),
    }
}

/// Find the Wayland socket (`wayland-N`) in the session's `XDG_RUNTIME_DIR`.
///
/// Nested compositors create additional sockets; the lowest-numbered one
/// belongs to the session's own compositor.
fn discover_wayland_display(env: &SessionEnv) -> Option<String> {
    use std::os::unix::fs::FileTypeExt;

    let runtime_dir = env.get("XDG_RUNTIME_DIR")?;

    std::fs::read_dir(runtime_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_socket()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("wayland-"))
        .min()
}

/// Ensure the environment carries `HYPRLAND_INSTANCE_SIGNATURE` when the
/// session runs Hyprland.
///
/// `hyprctl` locates its IPC socket via `HYPRLAND_INSTANCE_SIGNATURE`, but the
/// compositor's own environment (the process we read env vars from) never sets
/// that variable — it is only exported to Hyprland's children.
fn add_hyprland_signature(user: &str, env: &mut SessionEnv) {
    if !env.desktop_contains("Hyprland") || env.contains("HYPRLAND_INSTANCE_SIGNATURE") {
        return;
    }

    if let Some(signature) = discover_hyprland_signature(user, env) {
        log::debug!("Discovered HYPRLAND_INSTANCE_SIGNATURE: {}", signature);
        env.insert("HYPRLAND_INSTANCE_SIGNATURE", signature);
    }
}

/// Find the signature of the active Hyprland instance for `user`.
///
/// Asks `hyprctl instances -j`, which works without `HYPRLAND_INSTANCE_SIGNATURE`
/// set, and picks the instance whose PID matches the user's compositor process.
/// Falls back to the single instance directory under `/run/user/<uid>/hypr/`
/// when `hyprctl` is unavailable; stale directories from crashed sessions rule
/// that fallback out, so it is only a last resort.
fn discover_hyprland_signature(user: &str, env: &SessionEnv) -> Option<String> {
    let ctx = SessionContext { user: user.to_string(), env: env.clone() };

    if let Ok(output) = run_command_as_user(&ctx, "hyprctl", &["instances", "-j"]) {
        if output.status.success() {
            match serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) {
                Ok(instances) => {
                    if instances.len() == 1 {
                        if let Some(sig) = instances[0]["instance"].as_str() {
                            return Some(sig.to_string());
                        }
                    }

                    // Multiple instances (e.g. one left over from a crashed
                    // session): identify ours by the compositor PID.
                    let compositor_pid =
                        find_compositor_pid(user).and_then(|pid| pid.parse::<u64>().ok());

                    if let Some(pid) = compositor_pid {
                        if let Some(sig) = instances
                            .iter()
                            .find(|i| i["pid"].as_u64() == Some(pid))
                            .and_then(|i| i["instance"].as_str())
                        {
                            return Some(sig.to_string());
                        }
                    }

                    log::debug!(
                        "Found {} Hyprland instances, none matched our compositor",
                        instances.len()
                    );
                }
                Err(e) => log::debug!("Failed to parse 'hyprctl instances -j': {}", e),
            }
        } else {
            log::debug!("hyprctl instances failed: {}", hyprctl_error_text(&output));
        }
    }

    let output = Command::new("id").arg("-u").arg(user).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut entries = std::fs::read_dir(format!("/run/user/{}/hypr", uid))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned());

    let first = entries.next()?;
    (entries.next().is_none()).then_some(first)
}

/// Get session context (user + environment) - call this once and pass it around
/// This avoids redundant session detection and environment extraction
pub fn get_session_context() -> Result<SessionContext, DisplayError> {
    log::info!("=== Fetching Session Context (once) ===");

    let user = get_display_user()?;
    let env = get_user_session_env(&user)?;

    log::info!("Session context ready: user={}, env_vars={}", user, env.len());

    Ok(SessionContext { user, env })
}

/// Run an external program as the session user, injecting the session environment
///
/// Builds: `sudo -u <user> env KEY=VALUE... <program> [args...]`
/// This is the single canonical way to invoke display tools (gnome-randr, xrandr)
/// from a privileged daemon context.
fn run_command_as_user(
    ctx: &SessionContext,
    program: &str,
    args: &[&str],
) -> Result<std::process::Output, io::Error> {
    let mut cmd = Command::new("sudo");
    cmd.arg("-u").arg(&ctx.user).arg("env");
    for (key, value) in ctx.env.iter() {
        cmd.arg(format!("{}={}", key, value));
    }
    cmd.arg(program).args(args);
    cmd.output()
}

// ── Display backend abstraction ───────────────────────────────────────────────

/// Abstraction over GNOME Wayland and X11 display backends.
///
/// Each implementation queries the display and applies modes using the
/// appropriate tool (gnome-randr for GNOME Wayland, xrandr for X11).
trait DisplayManager {
    /// Query the built-in display name and all available modes in one call.
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError>;

    /// Apply a specific mode to the named display.
    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError>;
}

/// GNOME Wayland backend — uses `gnome-randr`
struct GnomeManager<'a> {
    ctx: &'a SessionContext,
}

impl DisplayManager for GnomeManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying gnome-randr for display info (name + modes)");

        let output = run_command_as_user(self.ctx, "gnome-randr", &["query"])?;

        if !output.status.success() {
            return Err(DisplayError::CommandFailed {
                command: "gnome-randr query".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Find the built-in display name (eDP-*)
        let display_name = find_builtin_display_name(stdout.lines(), |line| {
            let trimmed = line.trim();
            is_builtin_display_name(trimmed) && !trimmed.contains("disconnected")
        })
        .ok_or(DisplayError::BuiltinDisplayNotFound)?;

        // Collect modes for the detected display
        let mut modes = Vec::new();
        let mut in_display_section = false;

        log::debug!("Parsing gnome-randr output for display '{}'", display_name);

        for line in stdout.lines() {
            let trimmed = line.trim_start();

            if !line.starts_with(' ') && !line.starts_with('\t') {
                if line.starts_with(display_name.as_str()) {
                    log::debug!("Found display section: {}", line);
                    in_display_section = true;
                    continue;
                } else if in_display_section {
                    log::debug!("Exiting display section (found different display)");
                    break;
                }
            }

            if in_display_section && trimmed.len() < line.len() && trimmed.contains('@') {
                if let Some(mode) = parse_gnome_mode_line(line) {
                    modes.push(mode);
                }
            }
        }

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "gnome-randr", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        log::debug!(
            "Executing: sudo -u {} env [vars...] gnome-randr modify {} --mode {}",
            self.ctx.user,
            display_name,
            mode.mode_string
        );
        let output = run_command_as_user(
            self.ctx,
            "gnome-randr",
            &["modify", display_name, "--mode", &mode.mode_string],
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("gnome-randr modify failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "gnome-randr modify".to_string(),
                stderr: stderr.into_owned(),
            });
        }
        Ok(())
    }
}

/// X11 backend — uses `xrandr`
struct X11Manager<'a> {
    ctx: &'a SessionContext,
}

impl DisplayManager for X11Manager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying xrandr for display info (name + modes)");

        let output = run_command_as_user(self.ctx, "xrandr", &["--query"])?;

        if !output.status.success() {
            return Err(DisplayError::CommandFailed {
                command: "xrandr --query".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Find the built-in display name (eDP-* or LVDS-*)
        // The leading space keeps "disconnected" outputs from matching.
        let display_name = find_builtin_display_name(stdout.lines(), |line| {
            is_builtin_display_name(line) && line.contains(" connected")
        })
        .ok_or(DisplayError::BuiltinDisplayNotFound)?;

        // Collect modes for the detected display
        let mut modes = Vec::new();
        let mut in_display_section = false;
        #[allow(unused_assignments)]
        let mut current_resolution = String::new();

        log::debug!("Parsing xrandr output for display '{}'", display_name);

        for line in stdout.lines() {
            if line.starts_with(display_name.as_str()) && line.contains("connected") {
                log::debug!("Found display section: {}", line);
                in_display_section = true;
                continue;
            }

            if in_display_section && !line.starts_with(' ') && !line.starts_with('\t') {
                log::debug!("Exiting display section");
                break;
            }

            if in_display_section {
                let trimmed = line.trim_start();

                if trimmed.len() < line.len() && trimmed.contains('x') {
                    if let Some(first_token) = trimmed.split_whitespace().next() {
                        if first_token.contains('x') {
                            current_resolution = first_token.to_string();

                            if let Some(mode) = parse_xrandr_mode_line(line, &current_resolution) {
                                modes.push(mode);
                            }

                            let parts: Vec<&str> = trimmed.split_whitespace().collect();
                            for part in parts.iter().skip(1) {
                                let is_current = part.contains('*');
                                let rate_str = part.trim_end_matches('+').trim_end_matches('*');
                                if let Ok(rate) = rate_str.parse::<f32>() {
                                    if rate > 10.0 {
                                        let (width_str, height_str) =
                                            current_resolution.split_once('x').unwrap();
                                        if let (Ok(width), Ok(height)) =
                                            (width_str.parse::<u32>(), height_str.parse::<u32>())
                                        {
                                            let mode_string =
                                                format!("{}@{:.2}", current_resolution, rate);
                                            modes.push(DisplayMode {
                                                mode_string,
                                                resolution: (width, height),
                                                refresh_rate: rate,
                                                has_vrr: false,
                                                is_current,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "xrandr", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        log::debug!(
            "Executing: sudo -u {} env [vars...] xrandr --output {} --mode {}x{} --rate {:.2}",
            self.ctx.user,
            display_name,
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate
        );
        let output = run_command_as_user(
            self.ctx,
            "xrandr",
            &[
                "--output",
                display_name,
                "--mode",
                &format!("{}x{}", mode.resolution.0, mode.resolution.1),
                "--rate",
                &format!("{:.2}", mode.refresh_rate),
            ],
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("xrandr command failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "xrandr".to_string(),
                stderr: stderr.into_owned(),
            });
        }
        Ok(())
    }
}

/// Remove ANSI escape sequences (CSI: `ESC [ params final-byte`) from text.
///
/// `kscreen-doctor --outputs` colorizes its output unconditionally — it wraps the
/// currently active mode in `ESC [01;32m ... ESC [0;0m` even when stdout is a
/// pipe, and offers no `--no-color`/`NO_COLOR` option. Left in place, the escape
/// bytes make that token unparseable, which silently drops the active mode from
/// the discovered mode list (e.g. 2560x1600@60 disappearing while it is in use).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }

        if chars.peek() == Some(&'[') {
            chars.next();
            // Consume parameter bytes until the final byte (0x40..=0x7e)
            while let Some(&next) = chars.peek() {
                chars.next();
                if ('\x40'..='\x7e').contains(&next) {
                    break;
                }
            }
        }
    }

    out
}

/// KDE Wayland backend — uses `kscreen-doctor`
struct KdeManager<'a> {
    ctx: &'a SessionContext,
}

impl DisplayManager for KdeManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying kscreen-doctor for display info (name + modes)");

        let output = run_command_as_user(self.ctx, "kscreen-doctor", &["--outputs"])?;

        if !output.status.success() {
            return Err(DisplayError::CommandFailed {
                command: "kscreen-doctor --outputs".to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        log::debug!("kscreen-doctor output:\n{}", stdout);

        let (display_name, modes) =
            parse_kscreen_builtin_output(&stdout).ok_or(DisplayError::BuiltinDisplayNotFound)?;

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "kscreen-doctor", &stdout)?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // Build mode argument: output.<name>.mode.<W>x<H>@<Hz>
        let mode_arg = format!(
            "output.{}.mode.{}x{}@{}",
            display_name,
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate.round() as u32
        );

        // Also set VRR policy to automatic for high refresh rate modes
        let vrr_arg = format!("output.{}.vrrpolicy.automatic", display_name);

        log::debug!(
            "Executing: sudo -u {} env [vars...] kscreen-doctor {} {}",
            self.ctx.user,
            mode_arg,
            vrr_arg
        );

        let output = run_command_as_user(self.ctx, "kscreen-doctor", &[&mode_arg, &vrr_arg])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("kscreen-doctor command failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "kscreen-doctor".to_string(),
                stderr: stderr.into_owned(),
            });
        }

        log::info!(
            "Successfully applied mode {}x{}@{}Hz with VRR policy automatic",
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate.round() as u32
        );

        Ok(())
    }
}

/// Hyprland Wayland backend — uses `hyprctl`
///
/// Queries `hyprctl monitors all -j` (JSON) for the built-in display's modes and
/// applies modes via the `hl.monitor` Lua API, rebuilding the full monitor
/// rule from the current position and scale so only the refresh rate changes.
struct HyprlandManager<'a> {
    ctx: &'a SessionContext,
}

/// Extract the diagnostic text from a failed `hyprctl` invocation.
///
/// `hyprctl` prints errors (`HYPRLAND_INSTANCE_SIGNATURE not set!`, Lua errors, …)
/// to **stdout** and leaves stderr empty, so `stderr` alone loses the message.
fn hyprctl_error_text(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        return stderr.into_owned();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Run `hyprctl monitors all -j` and parse the JSON array.
fn hyprctl_monitors(ctx: &SessionContext) -> Result<Vec<serde_json::Value>, DisplayError> {
    let output = run_command_as_user(ctx, "hyprctl", &["monitors", "all", "-j"])?;

    if !output.status.success() {
        return Err(DisplayError::CommandFailed {
            command: "hyprctl monitors all -j".to_string(),
            stderr: hyprctl_error_text(&output),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    serde_json::from_str(&stdout).map_err(|e| DisplayError::CommandFailed {
        command: "hyprctl monitors all -j (parse)".to_string(),
        stderr: format!("invalid JSON: {}", e),
    })
}

/// Parse an `availableModes` string like "2560x1600@240.00Hz" into a DisplayMode.
fn parse_hyprland_mode(mode_str: &str) -> Option<DisplayMode> {
    let spec = mode_str.strip_suffix("Hz")?;
    let (resolution_str, rate_str) = spec.split_once('@')?;
    let (width_str, height_str) = resolution_str.split_once('x')?;

    Some(DisplayMode {
        // Canonical form without the "Hz" suffix, matching the other backends.
        mode_string: spec.to_string(),
        resolution: (width_str.parse().ok()?, height_str.parse().ok()?),
        refresh_rate: rate_str.parse().ok()?,
        has_vrr: false,
        is_current: false,
    })
}

impl DisplayManager for HyprlandManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying hyprctl for display info (name + modes)");

        let monitors = hyprctl_monitors(self.ctx)?;

        // Prefer the built-in display, fall back to the first monitor.
        let monitor = monitors
            .iter()
            .find(|m| m["name"].as_str().is_some_and(is_builtin_display_name))
            .or_else(|| monitors.first())
            .ok_or(DisplayError::SessionNotFound)?;

        let display_name = monitor["name"]
            .as_str()
            .ok_or_else(|| DisplayError::ParseError {
                command: "hyprctl monitors all -j".to_string(),
            })?
            .to_string();

        let current_res = (monitor["width"].as_u64(), monitor["height"].as_u64());
        let current_rate = monitor["refreshRate"].as_f64();

        let modes: Vec<DisplayMode> = monitor["availableModes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| {
                        let mode = parse_hyprland_mode(s)?;
                        let is_current = current_res
                            == (Some(mode.resolution.0 as u64), Some(mode.resolution.1 as u64))
                            && current_rate
                                .map(|r| (r - mode.refresh_rate as f64).abs() < 1.0)
                                .unwrap_or(false);
                        Some(DisplayMode { is_current, ..mode })
                    })
                    .collect()
            })
            .unwrap_or_default();

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(
            &modes,
            &display_name,
            "hyprctl",
            &serde_json::to_string(&monitor).unwrap_or_default(),
        )?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        // Re-query to rebuild the full monitor rule, preserving position and scale.
        let monitors = hyprctl_monitors(self.ctx)?;
        let monitor = monitors
            .iter()
            .find(|m| m["name"].as_str() == Some(display_name))
            .ok_or_else(|| DisplayError::CommandFailed {
                command: "hyprctl monitors all -j".to_string(),
                stderr: format!("monitor '{}' not found", display_name),
            })?;

        let x = monitor["x"].as_i64().unwrap_or(0);
        let y = monitor["y"].as_i64().unwrap_or(0);
        let scale = monitor["scale"].as_f64().unwrap_or(1.0);

        // Canonical Hyprland mode token; the reported "2560x1600@240.00Hz" form is
        // also accepted, but "Hz" is not part of the documented monitor-rule syntax.
        let mode_arg = mode.mode_string.trim_end_matches("Hz");

        // Hyprland >= 0.50 uses a non-legacy config parser: `hyprctl keyword`
        // rejects keywords (exit 0, no effect) and config changes must go through
        // the Lua API. Legacy parsers make `eval` fail, so fall back to `keyword`.
        let lua = format!(
            "hl.monitor({{ output = \"{}\", mode = \"{}\", position = \"{}x{}\", scale = {} }})",
            display_name,
            mode_arg,
            x,
            y,
            format_scale(scale)
        );

        log::debug!("Executing: sudo -u {} env [vars...] hyprctl eval '{}'", self.ctx.user, lua);

        // Keep every attempt's diagnostic so a failed verification can explain itself.
        let mut attempt_errors = String::new();

        let eval_output = run_command_as_user(self.ctx, "hyprctl", &["eval", &lua])?;

        if !eval_output.status.success() {
            let eval_error = hyprctl_error_text(&eval_output);
            log::debug!("hyprctl eval failed ({}), falling back to keyword monitor", eval_error);
            attempt_errors.push_str(&format!("eval: {}; ", eval_error));

            let rule = format!("{},{},{}x{},{}", display_name, mode_arg, x, y, format_scale(scale));
            let keyword_output =
                run_command_as_user(self.ctx, "hyprctl", &["keyword", "monitor", &rule])?;

            if !keyword_output.status.success() {
                attempt_errors
                    .push_str(&format!("keyword: {}", hyprctl_error_text(&keyword_output)));
                log::error!("hyprctl apply failed: {}", attempt_errors);
                return Err(DisplayError::CommandFailed {
                    command: "hyprctl eval hl.monitor".to_string(),
                    stderr: attempt_errors,
                });
            }
        }

        // `hyprctl` exits 0 even when it rejects a rule, so confirm the mode
        // actually took effect before reporting success.
        match verify_applied_mode(self.ctx, display_name, mode) {
            Ok(()) => {}
            Err(DisplayError::CommandFailed { stderr, .. }) => {
                let stderr = format!("{}{}", attempt_errors, stderr);
                log::error!("=== Display Refresh Rate Change: FAILED - {} ===", stderr);
                return Err(DisplayError::CommandFailed {
                    command: "hyprctl monitor mode verification".to_string(),
                    stderr,
                });
            }
            Err(e) => return Err(e),
        }
        log::info!("Successfully applied mode {} to {}", mode.mode_string, display_name);

        Ok(())
    }
}

/// Verify the requested mode was actually applied.
///
/// `hyprctl` exits 0 even when it rejects a monitor rule, so a successful exit
/// status proves nothing. Re-query the monitor and compare the active
/// resolution and refresh rate against the requested mode.
fn verify_applied_mode(
    ctx: &SessionContext,
    display_name: &str,
    mode: &DisplayMode,
) -> Result<(), DisplayError> {
    let monitors = hyprctl_monitors(ctx)?;
    let monitor =
        monitors.iter().find(|m| m["name"].as_str() == Some(display_name)).ok_or_else(|| {
            DisplayError::CommandFailed {
                command: "hyprctl monitors all -j".to_string(),
                stderr: format!("monitor '{}' not found after applying mode", display_name),
            }
        })?;

    let width = monitor["width"].as_u64();
    let height = monitor["height"].as_u64();
    let rate = monitor["refreshRate"].as_f64();

    let resolution_matches =
        width == Some(mode.resolution.0 as u64) && height == Some(mode.resolution.1 as u64);
    let rate_matches = rate.map(|r| (r - mode.refresh_rate as f64).abs() < 1.0).unwrap_or(false);

    if resolution_matches && rate_matches {
        return Ok(());
    }

    Err(DisplayError::CommandFailed {
        command: "hyprctl monitor mode verification".to_string(),
        stderr: format!(
            "requested {}x{}@{:.2}Hz but display reports {}x{}@{:.2}Hz",
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate,
            width.unwrap_or(0),
            height.unwrap_or(0),
            rate.unwrap_or(0.0)
        ),
    })
}

/// Format a Hyprland scale without a trailing ".0" (Hyprland expects e.g. "1.6" or "1").
fn format_scale(scale: f64) -> String {
    if (scale - scale.round()).abs() < f64::EPSILON {
        format!("{}", scale.round() as i64)
    } else {
        let formatted = format!("{:.6}", scale);
        formatted.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// wlroots Wayland backend — uses `wlr-randr`
///
/// Works with any compositor implementing `wlr-output-management` (Mango, sway,
/// labwc, river, wayfire, dwl, …). Requires the `wlr-randr` package.
struct WlrootsManager<'a> {
    ctx: &'a SessionContext,
}

/// Whether an output name belongs to a laptop's built-in panel (eDP-*, LVDS-*).
fn is_builtin_display_name(name: &str) -> bool {
    name.starts_with("eDP") || name.starts_with("LVDS")
}

/// Run `wlr-randr --json` and parse the array of outputs.
fn wlr_randr_outputs(ctx: &SessionContext) -> Result<Vec<serde_json::Value>, DisplayError> {
    let output = run_command_as_user(ctx, "wlr-randr", &["--json"])?;

    if !output.status.success() {
        return Err(DisplayError::CommandFailed {
            command: "wlr-randr --json".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|e| DisplayError::CommandFailed {
        command: "wlr-randr --json (parse)".to_string(),
        stderr: format!("invalid JSON: {}", e),
    })
}

/// Pick the enabled built-in output, falling back to the first enabled output.
fn select_wlr_randr_output(outputs: &[serde_json::Value]) -> Option<&serde_json::Value> {
    let mut enabled = outputs.iter().filter(|o| o["enabled"].as_bool().unwrap_or(false));

    enabled
        .clone()
        .find(|o| o["name"].as_str().is_some_and(is_builtin_display_name))
        .or_else(|| enabled.next())
}

/// Parse one entry of an output's `modes` array from `wlr-randr --json`.
fn parse_wlr_randr_mode(mode: &serde_json::Value) -> Option<DisplayMode> {
    let width = u32::try_from(mode["width"].as_u64()?).ok()?;
    let height = u32::try_from(mode["height"].as_u64()?).ok()?;
    let refresh = mode["refresh"].as_f64()?;

    Some(DisplayMode {
        // wlr-randr matches the refresh rate exactly, so keep the full precision.
        mode_string: format!("{}x{}@{:.6}", width, height, refresh),
        resolution: (width, height),
        refresh_rate: refresh as f32,
        has_vrr: false,
        is_current: mode["current"].as_bool().unwrap_or(false),
    })
}

impl DisplayManager for WlrootsManager<'_> {
    fn get_display_info(&self) -> Result<(String, Vec<DisplayMode>), DisplayError> {
        log::debug!("Querying wlr-randr for display info (name + modes)");

        let outputs = wlr_randr_outputs(self.ctx)?;
        let output = select_wlr_randr_output(&outputs).ok_or(DisplayError::SessionNotFound)?;

        let display_name = output["name"]
            .as_str()
            .ok_or_else(|| DisplayError::ParseError { command: "wlr-randr --json".to_string() })?
            .to_string();

        let modes: Vec<DisplayMode> = output["modes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_wlr_randr_mode)
            .collect();

        log::info!("Found {} available modes for display '{}'", modes.len(), display_name);

        ensure_modes_found(&modes, &display_name, "wlr-randr", &output.to_string())?;

        Ok((display_name, modes))
    }

    fn apply_mode(&self, display_name: &str, mode: &DisplayMode) -> Result<(), DisplayError> {
        log::debug!(
            "Executing: sudo -u {} env [vars...] wlr-randr --output {} --mode {}",
            self.ctx.user,
            display_name,
            mode.mode_string
        );

        // wlr-randr waits for the compositor to accept or reject the
        // configuration and exits non-zero on rejection.
        let output = run_command_as_user(
            self.ctx,
            "wlr-randr",
            &["--output", display_name, "--mode", &mode.mode_string],
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::error!("wlr-randr failed: {}", stderr);
            return Err(DisplayError::CommandFailed {
                command: "wlr-randr".to_string(),
                stderr: stderr.into_owned(),
            });
        }

        log::info!("Successfully applied mode {} to {}", mode.mode_string, display_name);

        Ok(())
    }
}

/// Construct the correct `DisplayManager` for the active session.
///
/// Returns an error for display servers that cannot be detected (Unknown).
fn create_manager(ctx: &SessionContext) -> Result<Box<dyn DisplayManager + '_>, DisplayError> {
    let display_server = DisplayServer::detect(&ctx.env)
        .or_else(|| DisplayServer::detect(&SessionEnv::from_process()))
        .unwrap_or(DisplayServer::Unknown);

    log::info!("Detected display server: {:?}", display_server);

    match display_server {
        DisplayServer::GnomeWayland => {
            log::info!("Using GNOME Wayland backend (gnome-randr)");
            Ok(Box::new(GnomeManager { ctx }))
        }
        DisplayServer::KdeWayland => {
            log::info!("Using KDE Wayland backend (kscreen-doctor)");
            Ok(Box::new(KdeManager { ctx }))
        }
        DisplayServer::Hyprland => {
            log::info!("Using Hyprland backend (hyprctl)");
            Ok(Box::new(HyprlandManager { ctx }))
        }
        DisplayServer::X11 => {
            log::info!("Using X11 backend (xrandr)");
            Ok(Box::new(X11Manager { ctx }))
        }
        DisplayServer::Wayland => {
            log::info!("Using wlroots backend (wlr-randr)");
            Ok(Box::new(WlrootsManager { ctx }))
        }
        DisplayServer::Unknown => {
            log::error!("Could not detect display server");
            Err(DisplayError::SessionNotFound)
        }
    }
}

/// Find the built-in display name from command output lines
///
/// Searches for display names matching common laptop display patterns (eDP-*, LVDS-*).
/// Returns `None` when no built-in display is listed, so callers never act on a guessed name.
///
/// # Arguments
/// * `lines` - Iterator over output lines from display query command
/// * `line_filter` - Filter function applied to each line (e.g., check "connected")
fn find_builtin_display_name<'a>(
    lines: impl Iterator<Item = &'a str>,
    line_filter: impl Fn(&str) -> bool,
) -> Option<String> {
    let name = lines
        .filter(|line| line_filter(line))
        .flat_map(str::split_whitespace)
        .find(|part| is_builtin_display_name(part))?;

    log::debug!("Found built-in display: {}", name);
    Some(name.to_string())
}

/// Ensure modes were found, returning an appropriate error if empty
///
/// This helper encapsulates the common "no modes found" error handling pattern
/// used by all display managers.
fn ensure_modes_found(
    modes: &[DisplayMode],
    display_name: &str,
    tool_name: &str,
    raw_output: &str,
) -> Result<(), DisplayError> {
    if modes.is_empty() {
        log::warn!("No modes found! {} output:\n{}", tool_name, raw_output);
        return Err(DisplayError::ModeNotFound {
            display: display_name.to_string(),
            spec: "any".to_string(),
        });
    }
    Ok(())
}

/// Parse a mode line from gnome-randr output
/// Example input: "    2560x1440@165.001+vrr	2560x1440 	165.00    	[scales...]"
fn parse_gnome_mode_line(line: &str) -> Option<DisplayMode> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Extract the mode string (first whitespace-separated token)
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let mode_string = parts[0].to_string();

    // Parse mode string: resolution@rate+flags
    // Examples: "2560x1440@165.001+vrr", "1920x1080@60.002"
    let has_vrr = mode_string.contains("+vrr");
    let clean = mode_string.replace("+vrr", "");

    // Split into resolution and rate parts
    let (res_part, rate_part) = clean.split_once('@')?;
    let (width_str, height_str) = res_part.split_once('x')?;

    // Parse numeric values
    let width = width_str.parse::<u32>().ok()?;
    let height = height_str.parse::<u32>().ok()?;
    let refresh_rate = rate_part.parse::<f32>().ok()?;

    log::debug!(
        "Parsed mode: {} → {}x{} @ {:.2}Hz (VRR: {})",
        mode_string,
        width,
        height,
        refresh_rate,
        has_vrr
    );

    // gnome-randr does not mark the active mode in a machine-readable way here
    Some(DisplayMode {
        mode_string,
        resolution: (width, height),
        refresh_rate,
        has_vrr,
        is_current: false,
    })
}

/// Parse xrandr mode line
/// Example input: "   2560x1440     59.95*+  165.00"
fn parse_xrandr_mode_line(line: &str, resolution: &str) -> Option<DisplayMode> {
    let trimmed = line.trim();

    // Check if line starts with resolution
    if !trimmed.starts_with(resolution) {
        return None;
    }

    // Extract refresh rates from the rest of the line
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    // Find refresh rates (numbers with optional + or *)
    // The first rate is usually the active one (marked with *)
    for i in 1..parts.len() {
        let is_current = parts[i].contains('*');
        let rate_str = parts[i].trim_end_matches('+').trim_end_matches('*');
        if let Ok(rate) = rate_str.parse::<f32>() {
            let mode_string = format!("{}@{:.2}", resolution, rate);

            log::debug!("Parsed X11 mode: {} @ {:.2}Hz", resolution, rate);

            let (width_str, height_str) = resolution.split_once('x')?;
            let width = width_str.parse::<u32>().ok()?;
            let height = height_str.parse::<u32>().ok()?;

            return Some(DisplayMode {
                mode_string,
                resolution: (width, height),
                refresh_rate: rate,
                has_vrr: false, // X11 doesn't report VRR in mode strings
                is_current,
            });
        }
    }

    None
}

/// Extract the built-in output's name and modes from `kscreen-doctor --outputs`.
///
/// Parses the multi-line output format:
/// ```text
/// Output: 1 eDP-1 <uuid>
///         enabled
///         Modes:  1:2560x1440@60.00*!  2:2560x1440@165.00  ...
/// Output: 2 HDMI-1 <uuid>
///         ...
/// ```
/// Returns `None` when no built-in output (eDP-*, LVDS-*) is listed.
fn parse_kscreen_builtin_output(stdout: &str) -> Option<(String, Vec<DisplayMode>)> {
    let mut display_name: Option<String> = None;
    let mut modes = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();

        if trimmed.contains("Output:") {
            // Any further output ends the built-in output's section.
            if display_name.is_some() {
                break;
            }

            display_name = trimmed
                .split_whitespace()
                .find(|part| is_builtin_display_name(part))
                .map(str::to_string);
        } else if display_name.is_some() {
            if let Some((_, modes_str)) = trimmed.split_once("Modes:") {
                log::debug!("Found modes line: {}", modes_str.trim());
                modes = parse_kscreen_modes(modes_str.trim());
            }
        }
    }

    display_name.map(|name| (name, modes))
}

/// Parse kscreen-doctor mode strings from the Modes: section
/// Example input: "1:2560x1440@60.00*!  2:2560x1440@165.00  3:1920x1200@60.00"
/// Each mode is formatted as "id:WxH@Hz" with optional *! suffixes
/// * = current mode, ! = preferred mode
fn parse_kscreen_modes(modes_str: &str) -> Vec<DisplayMode> {
    let mut modes = Vec::new();

    for mode_token in modes_str.split_whitespace() {
        // Mode format: "id:WxH@Hz*!" or similar
        // Strip the mode ID if present (e.g., "1:2560x1440@60.00*!" -> "2560x1440@60.00*!")
        let mode_part =
            if let Some((_id, rest)) = mode_token.split_once(':') { rest } else { mode_token };

        // Parse "WxH@Hz" (strip * and ! suffixes from rate)
        if let Some((res_part, rate_str)) = mode_part.split_once('@') {
            // Remove * and ! markers from rate string
            let rate_clean =
                rate_str.trim_end_matches('*').trim_end_matches('!').trim_end_matches('*');

            if let Some((width_str, height_str)) = res_part.split_once('x') {
                if let (Ok(width), Ok(height), Ok(rate)) =
                    (width_str.parse::<u32>(), height_str.parse::<u32>(), rate_clean.parse::<f32>())
                {
                    let mode_string = format!("{}x{}@{}", width, height, rate as u32);

                    log::debug!(
                        "Parsed KDE mode: {} → {}x{} @ {:.2}Hz",
                        mode_token,
                        width,
                        height,
                        rate
                    );

                    modes.push(DisplayMode {
                        mode_string,
                        resolution: (width, height),
                        refresh_rate: rate,
                        has_vrr: false, // kscreen-doctor doesn't report VRR in mode listing
                        // '*' marks the mode currently applied to the output
                        is_current: mode_token.contains('*'),
                    });
                }
            }
        }
    }

    modes
}

/// Find the best matching mode for a target refresh rate
/// Prefers VRR modes if available, and selects closest matching rate
fn find_best_mode<'a>(
    modes: &[&'a DisplayMode],
    target_rate: u32,
    prefer_vrr: bool,
) -> Option<&'a DisplayMode> {
    if modes.is_empty() {
        log::warn!("No modes available for matching");
        return None;
    }

    log::info!(
        "Finding best mode for target {}Hz from {} available modes (prefer VRR: {})",
        target_rate,
        modes.len(),
        prefer_vrr
    );

    let target_f32 = target_rate as f32;

    // First, try to find VRR modes if preferred
    let vrr_modes: Option<Vec<&'a DisplayMode>> = if prefer_vrr {
        let vrr_modes: Vec<&'a DisplayMode> = modes.iter().copied().filter(|m| m.has_vrr).collect();
        if vrr_modes.is_empty() {
            log::debug!("No VRR modes found, using all {} modes", modes.len());
            None
        } else {
            log::debug!("Found {} VRR modes, preferring those", vrr_modes.len());
            Some(vrr_modes)
        }
    } else {
        None
    };

    // Find the closest match by refresh rate
    let best = vrr_modes
        .as_deref()
        .unwrap_or(modes)
        .iter()
        .copied()
        .min_by_key(|mode| {
            let diff = (mode.refresh_rate - target_f32).abs();
            (diff * 1000.0) as u32 // Convert to millihertz for integer comparison
        });

    if let Some(mode) = best {
        log::info!(
            "Selected best mode: {} ({}x{} @ {:.2}Hz, VRR: {}) - diff: {:.2}Hz",
            mode.mode_string,
            mode.resolution.0,
            mode.resolution.1,
            mode.refresh_rate,
            mode.has_vrr,
            (mode.refresh_rate - target_f32).abs()
        );
    }

    best
}

/// Candidate modes for a refresh-rate-only change.
///
/// A refresh rate change must not alter the resolution, so when the backend
/// reports the active mode the candidates are restricted to modes of the same
/// resolution (always non-empty: the active mode itself is included). Falls
/// back to every mode when the active mode is unknown (e.g. gnome-randr).
fn modes_for_rate_change(modes: &[DisplayMode]) -> Vec<&DisplayMode> {
    let Some(resolution) = modes.iter().find(|m| m.is_current).map(|m| m.resolution) else {
        return modes.iter().collect();
    };

    let same_resolution: Vec<&DisplayMode> =
        modes.iter().filter(|m| m.resolution == resolution).collect();

    log::info!(
        "Keeping current resolution {}x{} ({} matching modes)",
        resolution.0,
        resolution.1,
        same_resolution.len()
    );

    same_resolution
}

/// Set display refresh rate based on detected display server
///
/// Automatically detects GNOME Wayland, X11, or other Wayland compositors
/// and uses the appropriate method to set the refresh rate.
///
/// # Arguments
///
/// * `rate` - Target refresh rate in Hz (e.g., 60, 120, 144, 165)
///
/// # Errors
///
/// Returns an error if:
/// - Display server cannot be detected
/// - Display user cannot be found
/// - No suitable mode is found for the requested refresh rate
/// - The underlying command (gnome-randr/xrandr) fails
pub fn set_refresh_rate(rate: u32) -> Result<(), DisplayError> {
    log::info!("=== Display Refresh Rate Change Request ===");
    log::info!("Target refresh rate: {}Hz", rate);

    let ctx = get_session_context()?;
    let manager = create_manager(&ctx)?;
    let (display_name, available_modes) = manager.get_display_info()?;

    log::info!("Target display: {}", display_name);

    // A refresh rate change must not alter the resolution.
    let candidates = modes_for_rate_change(&available_modes);

    let best_mode = find_best_mode(&candidates, rate, true).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("No suitable mode found for {}Hz on {}", rate, display_name),
        )
    })?;

    log::info!(
        "Applying mode: {} ({}x{} @ {:.2}Hz, VRR: {})",
        best_mode.mode_string,
        best_mode.resolution.0,
        best_mode.resolution.1,
        best_mode.refresh_rate,
        best_mode.has_vrr
    );

    let result = manager.apply_mode(&display_name, best_mode);

    match &result {
        Ok(_) => log::info!("=== Display Refresh Rate Change: SUCCESS ==="),
        Err(e) => log::error!("=== Display Refresh Rate Change: FAILED - {} ===", e),
    }

    result
}

/// Set display mode (resolution and refresh rate) based on ModeSpec
///
/// This function changes both resolution and refresh rate based on the provided mode specification.
/// It can work with either explicit mode strings or resolution + refresh rate combinations.
///
/// # Arguments
///
/// * `mode_spec` - The mode specification (either explicit mode string or resolution+rate)
///
/// # Errors
///
/// Select the best matching display mode from available modes based on specification
///
/// This helper function encapsulates the common mode selection logic used by both
/// GNOME Wayland and X11 backends.
///
/// # Arguments
/// * `available_modes` - Slice of available DisplayMode options from the display
/// * `mode_spec` - The desired mode specification (explicit string or resolution+rate)
/// * `display_name` - Name of the display (for error messages)
/// * `prefer_vrr` - Whether to prefer VRR modes when using ResolutionAndRate
///
/// # Returns
/// The best matching DisplayMode (cloned), or an error if no suitable mode found
fn select_best_mode(
    available_modes: &[DisplayMode],
    mode_spec: &ModeSpec,
    display_name: &str,
    prefer_vrr: bool,
) -> Result<DisplayMode, io::Error> {
    match mode_spec {
        ModeSpec::ModeString(mode_str) => {
            // Accept the "…Hz" suffix that `hyprctl monitors` prints, so specs
            // copied from it (or from `system76-power.conf` examples) still match
            // the canonical, suffix-free mode strings we store.
            let mode_str = mode_str.trim_end_matches("Hz").trim_end_matches("hz");

            // Try exact mode string match first
            if let Some(mode) = available_modes.iter().find(|m| m.mode_string == *mode_str) {
                return Ok(mode.clone());
            }

            log::warn!("Exact mode string '{}' not found, trying fuzzy match", mode_str);

            // Fuzzy match: strip VRR suffix and compare
            let mode_str_no_vrr = mode_str.replace("+vrr", "");
            if let Some(mode) = available_modes
                .iter()
                .find(|m| m.mode_string.replace("+vrr", "") == mode_str_no_vrr)
            {
                return Ok(mode.clone());
            }

            // More aggressive fuzzy match: parse resolution and rate, find closest match
            // This handles cases like "2560x1440@60.002" matching "2560x1440@60"
            log::warn!("VRR-stripped match failed, trying resolution+rate parsing");

            if let Some((res_part, rate_part)) = mode_str_no_vrr.split_once('@') {
                if let Some((width_str, height_str)) = res_part.split_once('x') {
                    if let (Ok(width), Ok(height), Ok(target_rate)) = (
                        width_str.parse::<u32>(),
                        height_str.parse::<u32>(),
                        rate_part.parse::<f32>(),
                    ) {
                        // Filter by resolution and find closest refresh rate
                        let filtered: Vec<_> = available_modes
                            .iter()
                            .filter(|m| m.resolution.0 == width && m.resolution.1 == height)
                            .collect();

                        if let Some(best) = filtered.iter().min_by_key(|m| {
                            let diff = (m.refresh_rate - target_rate).abs();
                            (diff * 1000.0) as u32
                        }) {
                            log::info!(
                                "Fuzzy matched '{}' to '{}' (diff: {:.3}Hz)",
                                mode_str,
                                best.mode_string,
                                (best.refresh_rate - target_rate).abs()
                            );
                            return Ok((*best).clone());
                        }
                    }
                }
            }

            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Mode '{}' not found on display {}", mode_str, display_name),
            ))
        }
        ModeSpec::ResolutionAndRate(width, height, hz) => {
            // Filter modes by resolution
            let filtered: Vec<&DisplayMode> = available_modes
                .iter()
                .filter(|m| m.resolution.0 == *width && m.resolution.1 == *height)
                .collect();

            if filtered.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "No modes found for resolution {}x{} on display {}",
                        width, height, display_name
                    ),
                ));
            }

            // Find best match by refresh rate (with optional VRR preference)
            find_best_mode(&filtered, *hz, prefer_vrr).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "No suitable mode found for {}x{}@{}Hz on display {}",
                        width, height, hz, display_name
                    ),
                )
            })
        }
    }
}

/// Returns an error if:
/// - Display server cannot be detected
/// - Display user cannot be found
/// - No suitable mode is found for the requested specification
/// - The underlying command (gnome-randr/xrandr) fails
pub fn set_display_mode(mode_spec: &ModeSpec) -> Result<(), DisplayError> {
    log::info!("=== Display Mode Change Request ===");

    match mode_spec {
        ModeSpec::ModeString(mode_str) => {
            log::info!("Target mode: {} (explicit mode string)", mode_str);
        }
        ModeSpec::ResolutionAndRate(width, height, hz) => {
            log::info!("Target mode: {}x{}@{}Hz", width, height, hz);
        }
    }

    let ctx = get_session_context()?;
    let manager = create_manager(&ctx)?;
    let (display_name, available_modes) = manager.get_display_info()?;

    log::info!("Target display: {}", display_name);

    let best_mode = select_best_mode(&available_modes, mode_spec, &display_name, true)?;

    log::info!(
        "Applying mode: {} ({}x{} @ {:.2}Hz, VRR: {})",
        best_mode.mode_string,
        best_mode.resolution.0,
        best_mode.resolution.1,
        best_mode.refresh_rate,
        best_mode.has_vrr
    );

    let result = manager.apply_mode(&display_name, &best_mode);

    match &result {
        Ok(_) => log::info!("=== Display Mode Change: SUCCESS ==="),
        Err(e) => log::error!("=== Display Mode Change: FAILED - {} ===", e),
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_display_server() {
        // Just verify it doesn't crash
        let _ = detect_display_server();
    }

    /// Real `kscreen-doctor --outputs` output colorizes the active mode with ANSI
    /// escapes. That token used to be dropped, losing the active mode entirely.
    #[test]
    fn test_strip_ansi_and_parse_kscreen_current_mode() {
        let colorized = "Modes:  1:2560x1600@240.00!  \
                         2:\x1b[01;32m2560x1600@60.00*\x1b[0;0m  \
                         3:1920x1200@240.00  19:2560x1440@59.96";
        let modes_str = strip_ansi(colorized).replace("Modes:", "");

        let modes = parse_kscreen_modes(&modes_str);

        assert_eq!(modes.len(), 4, "active mode must not be dropped");
        let current = modes.iter().find(|m| m.is_current).expect("active mode marked");
        assert_eq!(current.resolution, (2560, 1600));
        assert_eq!(current.refresh_rate, 60.0);
    }

    #[test]
    fn test_strip_ansi_removes_escapes_only() {
        assert_eq!(strip_ansi("\x1b[01;32m2560x1600@60.00*\x1b[0;0m"), "2560x1600@60.00*");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    fn mode(width: u32, height: u32, rate: f32, is_current: bool) -> DisplayMode {
        DisplayMode {
            mode_string: format!("{}x{}@{}", width, height, rate as u32),
            resolution: (width, height),
            refresh_rate: rate,
            has_vrr: false,
            is_current,
        }
    }

    /// Battery profile sets 60Hz: the panel's 2560x1440@59.96 mode must not win
    /// over the active resolution's 2560x1600@60 mode.
    #[test]
    fn test_rate_change_keeps_active_resolution() {
        let modes = vec![
            mode(2560, 1600, 240.0, true),
            mode(2560, 1600, 60.0, false),
            mode(2560, 1440, 59.96, false),
        ];

        let candidates = modes_for_rate_change(&modes);
        let best = find_best_mode(&candidates, 60, true).expect("mode found");

        assert_eq!(best.resolution, (2560, 1600));
        assert_eq!(best.refresh_rate, 60.0);
    }

    #[test]
    fn test_rate_change_without_current_mode_uses_all_modes() {
        let modes = vec![mode(2560, 1600, 240.0, false), mode(2560, 1440, 60.0, false)];

        let candidates = modes_for_rate_change(&modes);
        let best = find_best_mode(&candidates, 60, true).expect("mode found");

        assert_eq!(candidates.len(), 2);
        assert_eq!(best.resolution, (2560, 1440));
    }

    /// `hyprctl` reports modes as "2560x1600@240.00Hz"; the stored mode string must
    /// be the canonical, suffix-free form used to build monitor rules.
    #[test]
    fn test_parse_hyprland_mode() {
        let mode = parse_hyprland_mode("2560x1600@240.00Hz").expect("valid mode");
        assert_eq!(mode.resolution, (2560, 1600));
        assert_eq!(mode.refresh_rate, 240.0);
        assert_eq!(mode.mode_string, "2560x1600@240.00");
        assert!(!mode.is_current);

        assert!(parse_hyprland_mode("2560x1600@240.00").is_none(), "missing Hz suffix");
        assert!(parse_hyprland_mode("garbage").is_none());
    }

    fn session_env(pairs: &[(&str, &str)]) -> SessionEnv {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Mango's own environment (what we read from /proc) has no WAYLAND_DISPLAY,
    /// only the session type; that must still resolve to a Wayland backend.
    #[test]
    fn test_detect_wayland_without_wayland_display() {
        let mango = session_env(&[
            ("XDG_SESSION_TYPE", "wayland"),
            ("XDG_CURRENT_DESKTOP", "mango:wlroots"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        assert_eq!(DisplayServer::detect(&mango), Some(DisplayServer::Wayland));

        let tty = session_env(&[("XDG_SESSION_TYPE", "tty")]);
        assert_eq!(DisplayServer::detect(&tty), None);
    }

    #[test]
    fn test_detect_known_desktops_and_x11() {
        let detect = |pairs: &[(&str, &str)]| DisplayServer::detect(&session_env(pairs));

        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")]),
            Some(DisplayServer::GnomeWayland)
        );
        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "KDE")]),
            Some(DisplayServer::KdeWayland)
        );
        assert_eq!(
            detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", "Hyprland")]),
            Some(DisplayServer::Hyprland)
        );
        // GNOME on X11 is not a Wayland desktop backend.
        assert_eq!(
            detect(&[
                ("XDG_SESSION_TYPE", "x11"),
                ("XDG_CURRENT_DESKTOP", "GNOME"),
                ("DISPLAY", ":0")
            ]),
            Some(DisplayServer::X11)
        );
    }

    #[test]
    fn test_environ_block_keeps_session_variables_only() {
        let block = b"PATH=/usr/bin\0XDG_SESSION_TYPE=wayland\0SECRET=x\0DISPLAY=:0\0garbage\0";
        let env = SessionEnv::from_environ_block(block);

        assert_eq!(env.len(), 2);
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(env.get("DISPLAY"), Some(":0"));
        assert!(!env.contains("PATH"));
    }

    /// Modes of a later output must not replace the built-in display's modes, and a
    /// missing built-in display must not be papered over with a guessed name.
    #[test]
    fn test_parse_kscreen_builtin_output() {
        let two_outputs = "Output: 1 eDP-2 uuid\n  enabled\n  Modes:  1:2560x1600@240.00*!\n\
                           Output: 2 HDMI-1 uuid\n  Modes:  1:1920x1080@60.00*!\n";
        let (name, modes) = parse_kscreen_builtin_output(two_outputs).expect("built-in found");

        assert_eq!(name, "eDP-2");
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].resolution, (2560, 1600));

        let external_only = "Output: 1 HDMI-1 uuid\n  Modes:  1:1920x1080@60.00*!\n";
        assert!(parse_kscreen_builtin_output(external_only).is_none());
    }

    #[test]
    fn test_find_builtin_display_name_has_no_default() {
        let lines = ["HDMI-1 connected", "DP-1 disconnected"];
        assert_eq!(find_builtin_display_name(lines.into_iter(), |_| true), None);

        let lines = ["HDMI-1 connected", "LVDS-1 connected primary"];
        assert_eq!(
            find_builtin_display_name(lines.into_iter(), |line| line.contains(" connected")),
            Some("LVDS-1".to_string())
        );
    }

    #[test]
    fn test_parse_wlr_randr_mode() {
        let json = serde_json::json!(
            { "width": 2560, "height": 1600, "refresh": 59.999001, "current": true }
        );
        let mode = parse_wlr_randr_mode(&json).expect("valid mode");

        assert_eq!(mode.resolution, (2560, 1600));
        assert_eq!(mode.mode_string, "2560x1600@59.999001");
        assert!(mode.is_current);

        assert!(parse_wlr_randr_mode(&serde_json::json!({ "width": 1 })).is_none());
    }

    #[test]
    fn test_select_wlr_randr_output_prefers_enabled_builtin() {
        let outputs: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                { "name": "HDMI-A-1", "enabled": true },
                { "name": "eDP-2", "enabled": false },
                { "name": "eDP-3", "enabled": true }
            ]"#,
        )
        .unwrap();

        assert_eq!(select_wlr_randr_output(&outputs).unwrap()["name"], "eDP-3");
        assert_eq!(select_wlr_randr_output(&outputs[..2]).unwrap()["name"], "HDMI-A-1");
        assert!(select_wlr_randr_output(&[]).is_none());
    }

    #[test]
    fn test_format_scale() {
        assert_eq!(format_scale(1.6), "1.6");
        assert_eq!(format_scale(1.0), "1");
        assert_eq!(format_scale(2.0), "2");
        assert_eq!(format_scale(1.0 / 3.0), "0.333333");
    }

    /// Specs copied from `hyprctl monitors` output carry a "Hz" suffix that our
    /// stored mode strings do not; such specs must still resolve to the mode.
    #[test]
    fn test_select_best_mode_accepts_hz_suffix() {
        let modes = vec![mode(2560, 1600, 240.0, true), mode(2560, 1600, 60.0, false)];

        for spec in ["2560x1600@240.00Hz", "2560x1600@240", "2560x1600@240.00"] {
            let best = select_best_mode(
                &modes,
                &ModeSpec::ModeString(spec.to_string()),
                "eDP-1",
                false,
            )
            .unwrap_or_else(|e| panic!("spec '{}' must resolve: {}", spec, e));

            assert_eq!(best.refresh_rate, 240.0, "spec '{}'", spec);
        }
    }

    /// `hyprctl` reports errors on stdout; the message must not be lost.
    #[test]
    fn test_hyprctl_error_text_falls_back_to_stdout() {
        use std::os::unix::process::ExitStatusExt;

        let failed = |stdout: &str, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };

        assert_eq!(
            hyprctl_error_text(&failed("HYPRLAND_INSTANCE_SIGNATURE not set!", "")),
            "HYPRLAND_INSTANCE_SIGNATURE not set!"
        );
        assert_eq!(hyprctl_error_text(&failed("ignored", "real error")), "real error");
    }
}
