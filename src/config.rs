// Copyright 2018-2022 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

//! Single INI-style configuration subsystem for the daemon.
//!
//! Replaces the three hand-rolled, section-unaware parsers that previously
//! lived in [`crate::daemon`]. The file at [`CONFIG_PATH`] is read once at
//! daemon start and re-read on `SIGHUP` via [`reload`].
//!
//! The format is deliberately the pre-existing one (`[section]` headers,
//! `key = value`, `#`/`;` comments, optionally quoted values), so an existing
//! `/etc/system76-power.conf` keeps working. Parsing is section-aware: a key is
//! only honoured inside its own section. Unknown sections and keys are logged
//! and ignored, and a malformed value falls back to that key's default without
//! aborting the daemon.

use std::{
    fs,
    sync::{LazyLock, RwLock},
};

/// Path to the configuration file.
pub const CONFIG_PATH: &str = "/etc/system76-power.conf";

// ── Config structs ───────────────────────────────────────────────────────────

/// Refresh-rate policy applied per power profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRateConfig {
    pub enabled:     bool,
    pub battery:     u32,
    pub balanced:    u32,
    pub performance: u32,
}

impl Default for RefreshRateConfig {
    fn default() -> Self {
        Self { enabled: true, battery: 60, balanced: 60, performance: 165 }
    }
}

/// Display mode specification — an explicit mode string or resolution + rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeSpec {
    /// Explicit mode string (e.g. `"2560x1440@165.001+vrr"`).
    ModeString(String),
    /// Resolution and refresh rate (width, height, hz).
    ResolutionAndRate(u32, u32, u32),
}

/// Display mode configuration for AC auto-switching.
///
/// This allows changing both resolution and refresh rate when plugging or
/// unplugging AC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayModeConfig {
    pub enabled:      bool,
    pub ac_mode:      Option<ModeSpec>,
    pub battery_mode: Option<ModeSpec>,
}

impl Default for DisplayModeConfig {
    fn default() -> Self {
        Self { enabled: false, ac_mode: None, battery_mode: None }
    }
}

/// Backlight policy applied while profiles are switched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileConfig {
    /// When `true`, entering the Battery profile dims the backlight even after
    /// the initial boot-time set.
    pub dim_on_battery: bool,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self { dim_on_battery: false }
    }
}

/// USB device runtime power-management policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbConfig {
    pub autosuspend:       bool,
    /// Interface driver names that must never be autosuspended.
    pub blacklist_drivers: Vec<String>,
    /// `vid:pid` device IDs that must never be autosuspended.
    pub blacklist:         Vec<String>,
}

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            autosuspend:       true,
            blacklist_drivers: vec!["usblp".to_string()],
            blacklist:         Vec::new(),
        }
    }
}

/// HDA audio power-save policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioConfig {
    pub power_save: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { power_save: true }
    }
}

/// Wi-Fi power-save policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConfig {
    pub power_save: bool,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self { power_save: true }
    }
}

/// PCI device runtime power-management policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciConfig {
    pub runtime_pm:        bool,
    /// Drivers whose devices must be left out of runtime PM on the AC profile.
    pub blacklist_drivers: Vec<String>,
}

impl Default for PciConfig {
    fn default() -> Self {
        Self {
            runtime_pm:        true,
            blacklist_drivers: vec![
                "amdgpu".to_string(),
                "nvidia".to_string(),
                "nouveau".to_string(),
                "radeon".to_string(),
            ],
        }
    }
}

/// Bluetooth radio policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioConfig {
    pub bluetooth_off_on_battery: bool,
}

impl Default for RadioConfig {
    fn default() -> Self {
        Self { bluetooth_off_on_battery: false }
    }
}

/// CPU frequency / SMU limit policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuConfig {
    pub battery_stapm_mw: u32,
    pub battery_fast_mw:  u32,
    pub battery_slow_mw:  u32,
    pub battery_tctl_c:   u32,
    pub ac_stapm_mw:      u32,
    pub ac_fast_mw:       u32,
    pub ac_slow_mw:       u32,
    pub ac_tctl_c:        u32,
    /// EPP preference for the Battery profile on `amd-pstate-epp`.
    pub battery_epp:      String,
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            battery_stapm_mw: 12_000,
            battery_fast_mw:  18_000,
            battery_slow_mw:  10_000,
            battery_tctl_c:   60,
            ac_stapm_mw:      55_000,
            ac_fast_mw:       80_000,
            ac_slow_mw:       80_000,
            ac_tctl_c:        95,
            battery_epp:      "power".to_string(),
        }
    }
}

/// The complete daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub auto_switch:   bool,
    pub refresh_rate:  RefreshRateConfig,
    pub display_modes: DisplayModeConfig,
    pub profile:       ProfileConfig,
    pub usb:           UsbConfig,
    pub audio:         AudioConfig,
    pub wifi:          WifiConfig,
    pub pci:           PciConfig,
    pub radio:         RadioConfig,
    pub cpu:           CpuConfig,
}

// `Default` must not be derived for `Config` because `auto_switch` defaults to
// `true` and the sub-structs have non-`Default`-derived defaults; derive would
// give `false`/`RefreshRateConfig::default()`. Provide it manually.

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_switch:   true,
            refresh_rate:  RefreshRateConfig::default(),
            display_modes: DisplayModeConfig::default(),
            profile:       ProfileConfig::default(),
            usb:           UsbConfig::default(),
            audio:         AudioConfig::default(),
            wifi:          WifiConfig::default(),
            pci:           PciConfig::default(),
            radio:         RadioConfig::default(),
            cpu:           CpuConfig::default(),
        }
    }
}

// ── Global state ─────────────────────────────────────────────────────────────

static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(Config::default()));

/// Return a clone of the current configuration.
#[must_use]
pub fn current() -> Config {
    CONFIG.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Re-read the configuration file and install it as the current configuration.
///
/// The parsed configuration is returned so callers can react to the reload
/// without a second lock acquisition. Never fails: unreadable file or bad
/// values fall back to defaults.
pub fn reload() -> Config {
    let config = load();

    log::info!(
        "configuration reloaded: auto_switch={}, refresh_rate={}, display_modes={}, usb={}, \
         audio={}, wifi={}, pci={}, radio={}, cpu_epp={}",
        config.auto_switch,
        config.refresh_rate.enabled,
        config.display_modes.enabled,
        config.usb.autosuspend,
        config.audio.power_save,
        config.wifi.power_save,
        config.pci.runtime_pm,
        config.radio.bluetooth_off_on_battery,
        config.cpu.battery_epp,
    );

    *CONFIG.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
    config
}

/// Load the configuration from [`CONFIG_PATH`].
#[must_use]
pub fn load() -> Config { load_from(CONFIG_PATH) }

fn load_from(path: &str) -> Config {
    match fs::read_to_string(path) {
        Ok(content) => parse(&content),
        Err(_) => {
            log::info!("no configuration file found at {}, using defaults", path);
            Config::default()
        }
    }
}

// ── Parser ───────────────────────────────────────────────────────────────────

/// Parse configuration from an in-memory string (testable entry point).
#[must_use]
pub fn parse(content: &str) -> Config {
    let mut config = Config::default();
    let mut section = String::new();

    // Deferred display-mode components: mode strings win over resolution+rate.
    let mut ac_mode_string: Option<String> = None;
    let mut ac_resolution: Option<String> = None;
    let mut ac_refresh_rate: Option<u32> = None;
    let mut battery_mode_string: Option<String> = None;
    let mut battery_resolution: Option<String> = None;
    let mut battery_refresh_rate: Option<u32> = None;

    for (index, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim();

        if line.is_empty() {
            continue;
        }

        let lineno = index + 1;

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            log::warn!("{}:{}: ignoring malformed line {:?}", CONFIG_PATH, lineno, raw);
            continue;
        };

        let key = key.trim().to_ascii_lowercase();
        let value = unquote(value.trim());

        if section.is_empty() {
            log::warn!(
                "{}:{}: key '{}' outside any section, ignoring",
                CONFIG_PATH,
                lineno,
                key
            );
            continue;
        }

        if section == "display_modes" {
            match key.as_str() {
                "enabled" => set_bool(&mut config.display_modes.enabled, &value, &section, &key),
                "ac_mode" => ac_mode_string = Some(value.clone()),
                "ac_resolution" => ac_resolution = Some(value.clone()),
                "ac_refresh_rate" => match parse_u32(&value) {
                    Some(v) => ac_refresh_rate = Some(v),
                    None => warn_invalid(&section, &key, &value),
                },
                "battery_mode" => battery_mode_string = Some(value.clone()),
                "battery_resolution" => battery_resolution = Some(value.clone()),
                "battery_refresh_rate" => match parse_u32(&value) {
                    Some(v) => battery_refresh_rate = Some(v),
                    None => warn_invalid(&section, &key, &value),
                },
                _ => warn_unknown(&section, &key),
            }
            continue;
        }

        if !apply(&mut config, &section, &key, &value) {
            warn_unknown(&section, &key);
        }
    }

    config.display_modes.ac_mode =
        build_mode(ac_mode_string, ac_resolution, ac_refresh_rate);
    config.display_modes.battery_mode =
        build_mode(battery_mode_string, battery_resolution, battery_refresh_rate);

    config
}

/// Apply a single `key = value` within a known section. Returns whether the
/// option was recognised.
fn apply(config: &mut Config, section: &str, key: &str, value: &str) -> bool {
    match section {
        "auto_switch" => match key {
            "enabled" => set_bool(&mut config.auto_switch, value, section, key),
            _ => return false,
        },
        "refresh_rate" => match key {
            "enabled" => set_bool(&mut config.refresh_rate.enabled, value, section, key),
            "battery" => set_u32(&mut config.refresh_rate.battery, value, section, key),
            "balanced" => set_u32(&mut config.refresh_rate.balanced, value, section, key),
            "performance" => set_u32(&mut config.refresh_rate.performance, value, section, key),
            _ => return false,
        },
        "profile" => match key {
            "dim_on_battery" => {
                set_bool(&mut config.profile.dim_on_battery, value, section, key);
            }
            _ => return false,
        },
        "usb" => match key {
            "autosuspend" => set_bool(&mut config.usb.autosuspend, value, section, key),
            "blacklist_drivers" => config.usb.blacklist_drivers = parse_list(value),
            "blacklist" => config.usb.blacklist = parse_list(value),
            _ => return false,
        },
        "audio" => match key {
            "power_save" => set_bool(&mut config.audio.power_save, value, section, key),
            _ => return false,
        },
        "wifi" => match key {
            "power_save" => set_bool(&mut config.wifi.power_save, value, section, key),
            _ => return false,
        },
        "pci" => match key {
            "runtime_pm" => set_bool(&mut config.pci.runtime_pm, value, section, key),
            "blacklist_drivers" => config.pci.blacklist_drivers = parse_list(value),
            _ => return false,
        },
        "radio" => match key {
            "bluetooth_off_on_battery" => {
                set_bool(&mut config.radio.bluetooth_off_on_battery, value, section, key);
            }
            _ => return false,
        },
        "cpu" => match key {
            "battery_stapm_mw" => {
                set_u32(&mut config.cpu.battery_stapm_mw, value, section, key);
            }
            "battery_fast_mw" => set_u32(&mut config.cpu.battery_fast_mw, value, section, key),
            "battery_slow_mw" => set_u32(&mut config.cpu.battery_slow_mw, value, section, key),
            "battery_tctl_c" => set_u32(&mut config.cpu.battery_tctl_c, value, section, key),
            "ac_stapm_mw" => set_u32(&mut config.cpu.ac_stapm_mw, value, section, key),
            "ac_fast_mw" => set_u32(&mut config.cpu.ac_fast_mw, value, section, key),
            "ac_slow_mw" => set_u32(&mut config.cpu.ac_slow_mw, value, section, key),
            "ac_tctl_c" => set_u32(&mut config.cpu.ac_tctl_c, value, section, key),
            "battery_epp" => config.cpu.battery_epp = value.to_string(),
            _ => return false,
        },
        _ => return false,
    }

    true
}

fn build_mode(
    mode_string: Option<String>,
    resolution: Option<String>,
    refresh_rate: Option<u32>,
) -> Option<ModeSpec> {
    if let Some(mode) = mode_string.filter(|s| !s.is_empty()) {
        return Some(ModeSpec::ModeString(mode));
    }

    // Warn if exactly one of the two was provided — likely a user error.
    if resolution.is_some() != refresh_rate.is_some() {
        log::warn!(
            "{}: incomplete display mode — both resolution and refresh_rate are required, ignoring",
            CONFIG_PATH
        );
        return None;
    }

    let (Some(res), Some(hz)) = (resolution, refresh_rate) else {
        return None;
    };

    let lowered = res.to_ascii_lowercase();
    let (width, height) = lowered.split_once('x')?;
    let (Ok(width), Ok(height)) = (width.trim().parse::<u32>(), height.trim().parse::<u32>())
    else {
        log::warn!("{}: invalid resolution {:?}, ignoring", CONFIG_PATH, res);
        return None;
    };

    Some(ModeSpec::ResolutionAndRate(width, height, hz))
}

// ── Value helpers ────────────────────────────────────────────────────────────

fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quoted = false;

    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b'#' | b';' if !quoted => return &line[..index],
            _ => (),
        }
    }

    line
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn parse_u32(value: &str) -> Option<u32> { value.trim().parse::<u32>().ok() }

fn parse_list(value: &str) -> Vec<String> {
    value
        .split_ascii_whitespace()
        .map(|item| item.trim_matches(',').to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

fn set_bool(target: &mut bool, value: &str, section: &str, key: &str) {
    match parse_bool(value) {
        Some(v) => *target = v,
        None => warn_invalid(section, key, value),
    }
}

fn set_u32(target: &mut u32, value: &str, section: &str, key: &str) {
    match parse_u32(value) {
        Some(v) => *target = v,
        None => warn_invalid(section, key, value),
    }
}

fn warn_invalid(section: &str, key: &str, value: &str) {
    log::warn!(
        "{}: [{}] {} = {:?} is not a valid value, keeping default",
        CONFIG_PATH,
        section,
        key,
        value
    );
}

fn warn_unknown(section: &str, key: &str) {
    log::warn!("{}: unknown option [{}] {}, ignoring", CONFIG_PATH, section, key);
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped reference config, embedded so the test always exercises the
    /// file the daemon would actually read.
    const REPO_CONF: &str = include_str!("../system76-power.conf");

    #[test]
    fn defaults_when_file_missing() {
        let config = load_from("/nonexistent/system76-power.conf");
        assert_eq!(config, Config::default());
        assert!(config.auto_switch);
        assert!(config.refresh_rate.enabled);
        assert_eq!(config.refresh_rate.performance, 165);
        assert!(!config.display_modes.enabled);
        assert_eq!(config.cpu.battery_epp, "power");
    }

    #[test]
    fn empty_input_yields_defaults() {
        assert_eq!(parse(""), Config::default());
    }

    #[test]
    fn parses_repo_conf() {
        let config = parse(REPO_CONF);
        assert!(config.auto_switch);
        assert!(config.refresh_rate.enabled);
        assert_eq!(config.refresh_rate.battery, 60);
        assert_eq!(config.refresh_rate.balanced, 60);
        assert_eq!(config.refresh_rate.performance, 240);
        assert_eq!(
            config.display_modes.ac_mode,
            Some(ModeSpec::ResolutionAndRate(2560, 1600, 240))
        );
        assert_eq!(
            config.display_modes.battery_mode,
            Some(ModeSpec::ResolutionAndRate(2560, 1600, 60))
        );
    }

    /// Regression for the old section-unaware parser: `[refresh_rate] enabled`
    /// must not leak into `[auto_switch]`.
    #[test]
    fn keys_are_section_scoped() {
        let config = parse(
            "# [auto_switch]\n# enabled = true\n\n[refresh_rate]\nenabled = false\n",
        );
        assert!(config.auto_switch, "auto_switch must keep its default");
        assert!(!config.refresh_rate.enabled, "refresh_rate.enabled must be set");
    }

    #[test]
    fn malformed_value_falls_back_to_default_without_panic() {
        let config = parse(
            "[refresh_rate]\nperformance = not-a-number\n\n[usb]\nautosuspend = maybe\n",
        );
        assert_eq!(config.refresh_rate.performance, 165);
        assert!(config.usb.autosuspend);
    }

    #[test]
    fn unknown_section_is_ignored() {
        let config = parse("[nonsense]\nfoo = bar\nenabled = false\n");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn explicit_mode_string_wins_over_resolution() {
        let config = parse(
            "[display_modes]\nenabled = true\nac_mode = \"2560x1440@165.001+vrr\"\n\
             ac_resolution = \"1920x1080\"\nac_refresh_rate = 60\n",
        );
        assert!(config.display_modes.enabled);
        assert_eq!(
            config.display_modes.ac_mode,
            Some(ModeSpec::ModeString("2560x1440@165.001+vrr".to_string()))
        );
    }

    #[test]
    fn lists_are_split_on_whitespace() {
        let config = parse("[usb]\nblacklist = \"1234:5678 ABCD:EF01\"\nblacklist_drivers = usblp uas\n");
        assert_eq!(config.usb.blacklist, vec!["1234:5678", "abcd:ef01"]);
        assert_eq!(config.usb.blacklist_drivers, vec!["usblp", "uas"]);
    }

    #[test]
    fn comment_chars_inside_quotes_are_preserved() {
        let config = parse("[display_modes]\nac_mode = \"2560x1600@240.00#1\"\n");
        assert_eq!(
            config.display_modes.ac_mode,
            Some(ModeSpec::ModeString("2560x1600@240.00#1".to_string()))
        );

        // A real trailing comment is still stripped.
        let config = parse("[refresh_rate]\nperformance = 240 # max\n");
        assert_eq!(config.refresh_rate.performance, 240);
    }
}
