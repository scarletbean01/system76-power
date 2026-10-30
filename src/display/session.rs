// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Graphical session discovery: which user owns the display and what environment
//! their compositor runs with.

use std::collections::BTreeMap;
use std::io;
use std::process::Command;

use crate::errors::DisplayError;

use super::backend::add_hyprland_signature;

/// Compositor processes to search for, in priority order
pub(super) const COMPOSITOR_PROCESSES: &[&str] = &[
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
pub(super) const SESSION_ENV_VARS: &[&str] = &[
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

    pub(super) fn is_wayland_session(&self) -> bool {
        self.get("XDG_SESSION_TYPE") == Some("wayland")
    }

    pub(super) fn desktop_contains(&self, name: &str) -> bool {
        self.get("XDG_CURRENT_DESKTOP").is_some_and(|desktop| desktop.contains(name))
    }
}

impl FromIterator<(String, String)> for SessionEnv {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
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
pub(super) fn get_display_user() -> Result<String, io::Error> {
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
pub(super) fn find_compositor_pid(user: &str) -> Option<String> {
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
pub(super) fn get_user_session_env(user: &str) -> Result<SessionEnv, io::Error> {
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
pub(super) fn run_command_as_user(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_environ_block_keeps_session_variables_only() {
        let block = b"PATH=/usr/bin\0XDG_SESSION_TYPE=wayland\0SECRET=x\0DISPLAY=:0\0garbage\0";
        let env = SessionEnv::from_environ_block(block);

        assert_eq!(env.len(), 2);
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(env.get("DISPLAY"), Some(":0"));
        assert!(!env.contains("PATH"));
    }
}
