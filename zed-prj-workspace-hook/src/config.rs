//! Sync hook configuration.
//!
//! Controls whether the hook is active and timing parameters.

use std::time::Duration;

/// Delay after detecting a workspace write before querying the DB.
/// Must be > Zed's 200ms serialization debounce to ensure the transaction is committed.
pub const SYNC_DELAY: Duration = Duration::from_millis(300);

/// Minimum interval between sync operations to prevent rapid-fire updates.
pub const SYNC_COOLDOWN: Duration = Duration::from_millis(1000);

/// Minimum interval between discovery attempts when discovery fails.
pub const DISCOVERY_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub enabled: bool,
}

impl SyncConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: Self::parse_enabled(
                std::env::var("ZED_PRJ_WORKSPACE_SYNC").ok().as_deref(),
            ),
        }
    }

    fn parse_enabled(val: Option<&str>) -> bool {
        match val {
            Some("0") | Some("off") | Some("disabled") | Some("false") => false,
            _ => true, // enabled by default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled() {
        assert!(SyncConfig::parse_enabled(None));
    }

    #[test]
    fn disabled_variants() {
        assert!(!SyncConfig::parse_enabled(Some("0")));
        assert!(!SyncConfig::parse_enabled(Some("off")));
        assert!(!SyncConfig::parse_enabled(Some("disabled")));
        assert!(!SyncConfig::parse_enabled(Some("false")));
    }

    #[test]
    fn enabled_variants() {
        assert!(SyncConfig::parse_enabled(Some("1")));
        assert!(SyncConfig::parse_enabled(Some("on")));
        assert!(SyncConfig::parse_enabled(Some("anything")));
        assert!(SyncConfig::parse_enabled(Some("")));
    }
}
