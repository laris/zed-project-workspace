//! Sync hook configuration.
//!
//! Configuration is loaded from a JSON file with environment variable overrides.
//!
//! ## Config file location
//!
//! `~/.config/dylib-hooks/{app_id}/zed-prj-workspace-hook.json`
//!
//! ## Precedence (highest wins)
//!
//! 1. Environment variable (for terminal testing)
//! 2. Config file
//! 3. Built-in defaults

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Top-level sync hook configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Master enable/disable toggle.
    pub enabled: bool,
    /// Tracing filter level.
    pub log_level: String,
    /// Delay (ms) after detecting workspace write before querying DB.
    /// Must be >200ms to ensure Zed's transaction commits.
    pub sync_delay_ms: u64,
    /// Minimum interval (ms) between sync operations per workspace.
    pub sync_cooldown_ms: u64,
    /// Minimum interval (s) between discovery attempts on failure.
    pub discovery_cooldown_s: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_level: "info".to_string(),
            sync_delay_ms: 300,
            sync_cooldown_ms: 1000,
            discovery_cooldown_s: 30,
        }
    }
}

impl SyncConfig {
    /// Load config: env vars override config file, which overrides defaults.
    pub fn load(app_id: &str) -> Self {
        let mut config = Self::load_from_file(app_id).unwrap_or_default();

        // Env var overrides (for terminal testing)
        if let Ok(val) = std::env::var("ZED_PRJ_WORKSPACE_SYNC") {
            config.enabled = !matches!(val.as_str(), "0" | "off" | "disabled" | "false");
        }
        if let Ok(val) = std::env::var("ZED_PRJ_WORKSPACE_SYNC_LOG") {
            if !val.is_empty() {
                config.log_level = val;
            }
        }

        config
    }

    /// Legacy: load from env var only (for backward compatibility in tests).
    pub fn from_env() -> Self {
        // Try config file first, then env overrides
        let app_id = detect_app_id();
        Self::load(&app_id)
    }

    fn load_from_file(app_id: &str) -> Option<Self> {
        let path = config_path(app_id)?;
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save current config to file.
    pub fn save(&self, app_id: &str) -> std::io::Result<()> {
        let path = config_path(app_id)
            .ok_or_else(|| std::io::Error::other("cannot determine config path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn sync_delay(&self) -> Duration {
        Duration::from_millis(self.sync_delay_ms)
    }

    pub fn sync_cooldown(&self) -> Duration {
        Duration::from_millis(self.sync_cooldown_ms)
    }

    pub fn discovery_cooldown(&self) -> Duration {
        Duration::from_secs(self.discovery_cooldown_s)
    }
}

/// Config file path: `~/.config/dylib-hooks/{app_id}/zed-prj-workspace-hook.json`
pub fn config_path(app_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".config")
            .join("dylib-hooks")
            .join(app_id)
            .join("zed-prj-workspace-hook.json"),
    )
}

/// Detect app_id from executable path.
pub fn detect_app_id() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let s = exe.to_string_lossy().to_string();
            if s.contains("Zed Preview") {
                Some("zed-preview".to_string())
            } else if s.contains("Zed.app") {
                Some("zed-stable".to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "zed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled() {
        let config = SyncConfig::default();
        assert!(config.enabled);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.sync_delay_ms, 300);
        assert_eq!(config.sync_cooldown_ms, 1000);
        assert_eq!(config.discovery_cooldown_s, 30);
    }

    #[test]
    fn roundtrip_json() {
        let config = SyncConfig {
            enabled: false,
            log_level: "debug".to_string(),
            sync_delay_ms: 500,
            sync_cooldown_ms: 2000,
            discovery_cooldown_s: 60,
        };
        let json = serde_json::to_string(&config).unwrap();
        let loaded: SyncConfig = serde_json::from_str(&json).unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.sync_delay_ms, 500);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let json = r#"{ "enabled": false }"#;
        let config: SyncConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.sync_delay_ms, 300); // default
    }

    #[test]
    fn timing_methods() {
        let config = SyncConfig::default();
        assert_eq!(config.sync_delay(), Duration::from_millis(300));
        assert_eq!(config.sync_cooldown(), Duration::from_millis(1000));
        assert_eq!(config.discovery_cooldown(), Duration::from_secs(30));
    }
}
