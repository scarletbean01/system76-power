// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Graphical session discovery: which user owns the display and what environment
//! their compositor runs with.

use std::collections::BTreeMap;
use std::path::PathBuf;
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

/// A login session as listed by `loginctl list-sessions`.
struct SessionEntry {
    id: String,
    user: String,
}

/// Session information from `loginctl show-session`.
#[derive(Debug, Default)]
struct SessionInfo {
    seat: String,
    session_type: Option<String>,
    state: Option<String>,
}

impl SessionInfo {
    /// A Wayland or X11 session (as opposed to a tty).
    fn is_graphical(&self) -> bool {
        matches!(self.session_type.as_deref(), Some("wayland" | "x11"))
    }

    fn is_active(&self) -> bool {
        self.state.as_deref() == Some("active")
    }

    /// On the primary graphics seat.
    fn is_primary_seat(&self) -> bool {
        self.seat == "seat0"
    }
}

/// The account owning the graphical session.
#[derive(Debug, Clone)]
pub struct UserAccount {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl UserAccount {
    /// Look the account up in the system user database (honours NSS, not just `/etc/passwd`).
    pub(super) fn lookup(name: &str) -> Result<Self, DisplayError> {
        let output = Command::new("getent").arg("passwd").arg(name).output()?;

        output
            .status
            .success()
            .then(|| Self::parse_passwd_entry(&String::from_utf8_lossy(&output.stdout)))
            .flatten()
            .ok_or_else(|| DisplayError::UserNotFound(name.to_string()))
    }

    /// Parse a `name:password:uid:gid:gecos:home:shell` line.
    fn parse_passwd_entry(line: &str) -> Option<Self> {
        let mut fields = line.trim().split(':');

        let name = fields.next()?.to_string();
        let uid = fields.nth(1)?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        let home = fields.nth(1)?.into();

        Some(Self { name, uid, gid, home })
    }
}

/// Session context containing user and environment variables
/// This is fetched once and passed to all helper functions to avoid redundant lookups
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub account: UserAccount,
    pub env: SessionEnv,
}

/// Run `loginctl` and return its stdout.
fn loginctl(args: &[&str]) -> Result<String, DisplayError> {
    let output = Command::new("loginctl").args(args).output()?;

    if !output.status.success() {
        return Err(DisplayError::CommandFailed {
            command: format!("loginctl {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// List the login sessions of all users.
fn list_sessions() -> Result<Vec<SessionEntry>, DisplayError> {
    let sessions = loginctl(&["list-sessions", "--no-legend"])?;

    // Columns: SESSION UID USER SEAT TTY
    Ok(sessions
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let id = columns.next()?.to_string();
            let user = columns.nth(1)?.to_string();

            Some(SessionEntry { id, user })
        })
        .collect())
}

/// Query detailed information about a specific session
fn query_session_details(session_id: &str) -> Result<SessionInfo, DisplayError> {
    let info = loginctl(&[
        "show-session",
        session_id,
        "--property=Type",
        "--property=State",
        "--property=Seat",
    ])?;

    let mut session = SessionInfo { seat: String::from("seat0"), ..Default::default() };

    for (key, value) in info.lines().filter_map(|line| line.split_once('=')) {
        match key {
            "Type" => session.session_type = Some(value.to_string()),
            "State" => session.state = Some(value.to_string()),
            "Seat" => session.seat = value.to_string(),
            _ => {}
        }
    }

    log::debug!("Session {} details: {:?}", session_id, session);

    Ok(session)
}

/// Get the username running the active graphical session
fn get_display_user() -> Result<String, DisplayError> {
    log::debug!("Detecting display user");

    // Method 1: Check SUDO_USER environment variable (when run via sudo)
    if let Some(user) = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root") {
        log::debug!("Found display user from SUDO_USER: {}", user);
        return Ok(user);
    }

    // Method 2: Find an active graphical session via loginctl
    let candidates: Vec<SessionEntry> =
        list_sessions()?.into_iter().filter(|session| session.user != "root").collect();

    log::debug!("Found {} non-root sessions to evaluate", candidates.len());

    let sessions: Vec<(&SessionEntry, SessionInfo)> = candidates
        .iter()
        .filter_map(|entry| match query_session_details(&entry.id) {
            Ok(info) => Some((entry, info)),
            Err(e) => {
                log::debug!("Failed to query session {}: {}", entry.id, e);
                None
            }
        })
        .collect();

    // Priority 1: Active graphical session on seat0
    if let Some((entry, info)) = sessions
        .iter()
        .find(|(_, info)| info.is_graphical() && info.is_active() && info.is_primary_seat())
    {
        log::info!(
            "Found ideal session: user={}, session={}, type={:?}, seat={}",
            entry.user,
            entry.id,
            info.session_type,
            info.seat
        );
        return Ok(entry.user.clone());
    }

    // Priority 2: Any graphical session (relaxed seat/state requirements)
    if let Some((entry, info)) = sessions.iter().find(|(_, info)| info.is_graphical()) {
        log::warn!(
            "Using graphical session (not ideal): user={}, session={}, type={:?}, state={:?}, seat={}",
            entry.user,
            entry.id,
            info.session_type,
            info.state,
            info.seat
        );
        return Ok(entry.user.clone());
    }

    // Priority 3: Fallback - first non-root user (original behavior)
    let first = candidates.first().ok_or(DisplayError::SessionNotFound)?;
    log::warn!("No graphical session found, using first non-root user as fallback: {}", first.user);

    Ok(first.user.clone())
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
fn find_session_leader_pid(user: &str) -> Result<String, DisplayError> {
    log::debug!("Finding session leader for user: {}", user);

    list_sessions()?
        .into_iter()
        .filter(|session| session.user == user)
        .find_map(|session| {
            loginctl(&["show-session", &session.id, "--property=Leader", "--value"]).ok()
        })
        .map(|leader| leader.trim().to_string())
        .inspect(|pid| log::debug!("Session leader PID: {}", pid))
        .ok_or_else(|| DisplayError::SessionLeaderNotFound(user.to_string()))
}

/// Read the session environment variables from /proc/<pid>/environ
fn read_process_environment(pid: &str) -> Result<SessionEnv, DisplayError> {
    log::debug!("Reading environment from PID: {}", pid);

    let environ_data = std::fs::read(format!("/proc/{}/environ", pid))
        .map_err(|source| DisplayError::ProcessEnvironment { pid: pid.to_string(), source })?;

    let env = SessionEnv::from_environ_block(&environ_data);

    if env.is_empty() {
        log::warn!("No display environment variables found in PID {}", pid);
    } else {
        for (key, value) in env.iter() {
            log::debug!("Found env: {}={}", key, value);
        }
        log::debug!("Extracted {} environment variables from PID {}", env.len(), pid);
    }

    Ok(env)
}

/// Get the display environment of the user session
pub(super) fn get_user_session_env(account: &UserAccount) -> Result<SessionEnv, DisplayError> {
    let user = account.name.as_str();
    log::debug!("Getting session environment for user: {}", user);

    // Strategy 1: Find compositor process (most reliable for display environment)
    if let Some(pid) = find_compositor_pid(user) {
        let mut env = read_process_environment(&pid)?;

        if !env.is_empty() {
            complete_session_env(account, &mut env);
            return Ok(env);
        }

        log::warn!("Compositor PID {} had no display environment, trying fallback", pid);
    }

    // Strategy 2: Use session leader (fallback)
    log::debug!("Falling back to session leader PID");
    let pid = find_session_leader_pid(user)?;
    let mut env = read_process_environment(&pid)?;

    if env.is_empty() {
        return Err(DisplayError::NoDisplayEnvironment(user.to_string()));
    }

    complete_session_env(account, &mut env);
    Ok(env)
}

/// Add the variables a compositor exports to its children only, and which
/// therefore never appear in the compositor process's own environment.
fn complete_session_env(account: &UserAccount, env: &mut SessionEnv) {
    add_hyprland_signature(account, env);
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
    log::debug!("Fetching session context");

    let account = get_display_account()?;
    let env = get_user_session_env(&account)?;

    log::debug!("Session context ready: user={}, env_vars={}", account.name, env.len());

    Ok(SessionContext { account, env })
}

/// Get the account running the active graphical session.
pub(super) fn get_display_account() -> Result<UserAccount, DisplayError> {
    UserAccount::lookup(&get_display_user()?)
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

    #[test]
    fn test_parse_passwd_entry() {
        let account = UserAccount::parse_passwd_entry(
            "deplague:x:1000:985:Dep:/home/deplague:/usr/bin/fish\n",
        )
        .expect("valid entry");

        assert_eq!(account.name, "deplague");
        assert_eq!(account.uid, 1000);
        assert_eq!(account.gid, 985);
        assert_eq!(account.home, PathBuf::from("/home/deplague"));

        assert!(
            UserAccount::parse_passwd_entry("deplague:x:notanumber:985::/home:/bin/sh").is_none()
        );
        assert!(UserAccount::parse_passwd_entry("").is_none());
    }
}
