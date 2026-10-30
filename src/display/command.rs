// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Running display tools (`gnome-randr`, `xrandr`, `hyprctl`, `wlr-randr`, …) as
//! the session user from the privileged daemon.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

use serde::de::DeserializeOwned;

use super::SessionContext;
use crate::errors::DisplayError;

/// `PATH` for the child when the daemon itself has none.
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Build a command that runs as the session user with the session environment.
///
/// The child starts from a clean environment: only `PATH`, the account's
/// `HOME`/`USER`/`LOGNAME` and the session variables (`WAYLAND_DISPLAY`,
/// `DBUS_SESSION_BUS_ADDRESS`, …) are set, so nothing of the daemon's own
/// (root) environment leaks into a user process.
fn user_command(ctx: &SessionContext, program: &str, args: &[&str]) -> Command {
    let account = &ctx.account;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| FALLBACK_PATH.into()))
        .env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .envs(ctx.env.iter());

    // Only switch identity when running as a different user (the daemon is
    // root). `std` also drops the daemon's supplementary groups when it does.
    if unsafe { libc::geteuid() } != account.uid {
        cmd.uid(account.uid).gid(account.gid);
    }

    cmd
}

/// Run an external program as the session user and return its output, whatever
/// its exit status.
pub(super) fn run_as_user(
    ctx: &SessionContext,
    program: &str,
    args: &[&str],
) -> Result<Output, io::Error> {
    log::debug!("Running as {}: {}", ctx.account.name, command_line(program, args));

    let output = user_command(ctx, program, args).output()?;
    log::trace!("{} stdout:\n{}", program, String::from_utf8_lossy(&output.stdout));

    Ok(output)
}

fn command_line(program: &str, args: &[&str]) -> String {
    std::iter::once(program).chain(args.iter().copied()).collect::<Vec<_>>().join(" ")
}

/// Run an external program as the session user, failing unless it exits with
/// status 0.
///
/// This is the single way the backends invoke display tools.
pub(super) fn run_checked(
    ctx: &SessionContext,
    program: &str,
    args: &[&str],
) -> Result<Output, DisplayError> {
    let output = run_as_user(ctx, program, args)?;

    if output.status.success() {
        return Ok(output);
    }

    Err(DisplayError::CommandFailed {
        command: command_line(program, args),
        stderr: failure_text(&output),
    })
}

/// Run an external program as the session user and deserialize the JSON it
/// prints, so malformed or incomplete output names the offending field.
pub(super) fn run_json<T: DeserializeOwned>(
    ctx: &SessionContext,
    program: &str,
    args: &[&str],
) -> Result<T, DisplayError> {
    let output = run_checked(ctx, program, args)?;

    serde_json::from_slice(&output.stdout).map_err(|e| DisplayError::ParseError {
        command: command_line(program, args),
        reason: e.to_string(),
    })
}

/// Extract the diagnostic text from a failed command.
///
/// Some tools (`hyprctl`: `HYPRLAND_INSTANCE_SIGNATURE not set!`, Lua errors, …)
/// print errors to **stdout** and leave stderr empty, so stderr alone would lose
/// the message.
pub(super) fn failure_text(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        return stderr.trim().to_string();
    }

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;
    use crate::display::session::{SessionEnv, UserAccount};

    /// A context for the current user, so no privileges are needed to run it.
    fn current_user_context(env: &[(&str, &str)]) -> SessionContext {
        let account = UserAccount {
            name: "tester".to_string(),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            home: "/tmp".into(),
        };
        let env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<SessionEnv>();

        SessionContext { account, env }
    }

    fn failed(stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(1 << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn test_failure_text_falls_back_to_stdout() {
        assert_eq!(
            failure_text(&failed("HYPRLAND_INSTANCE_SIGNATURE not set!", "")),
            "HYPRLAND_INSTANCE_SIGNATURE not set!"
        );
        assert_eq!(failure_text(&failed("ignored", "real error")), "real error");
    }

    /// The child sees the session variables and account, and nothing else of the
    /// daemon's own environment.
    #[test]
    fn test_run_checked_uses_clean_session_environment() {
        let ctx = current_user_context(&[("WAYLAND_DISPLAY", "wayland-7")]);

        let output = run_checked(&ctx, "env", &[]).expect("env runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let vars: Vec<&str> = stdout.lines().collect();

        assert!(vars.contains(&"WAYLAND_DISPLAY=wayland-7"));
        assert!(vars.contains(&"HOME=/tmp"));
        assert!(vars.contains(&"USER=tester"));
        assert!(!vars.iter().any(|v| v.starts_with("CARGO_")), "daemon env leaked: {:?}", vars);
    }

    #[test]
    fn test_run_checked_reports_failure() {
        let ctx = current_user_context(&[]);

        match run_checked(&ctx, "sh", &["-c", "echo boom >&2; exit 3"]) {
            Err(DisplayError::CommandFailed { command, stderr }) => {
                assert_eq!(command, "sh -c echo boom >&2; exit 3");
                assert_eq!(stderr, "boom");
            }
            other => panic!("expected CommandFailed, got {:?}", other),
        }
    }
}
