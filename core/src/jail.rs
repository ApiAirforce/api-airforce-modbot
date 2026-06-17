//! Configuration for the Discord "real jail" — the secure variant where, instead
//! of merely adding a role, the bot SNAPSHOTS the member's current roles, STRIPS
//! them all (set roles to just the Jail role), and RESTORES the snapshot on
//! unjail. The Jail role's channel-overwrites (admin-configured: deny View
//! everywhere except #jail) do the hiding.
//!
//! This module holds only the PURE config shape + validation. The serenity-
//! coupled mechanics (snapshot/strip/restore, re-apply on rejoin, expiry sweep)
//! live in the bot binary, generic over the [`crate::ports::JailStore`].

use serde::{Deserialize, Serialize};

use crate::ports::ConfigStore;

pub const CONFIG_BLOB_KEY: &str = "jail_config";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JailConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub guild_id: String,
    /// The Jail role (snowflake). Its channel overwrites must deny View on the
    /// server's categories and allow it only on the #jail channel.
    #[serde(default)]
    pub jail_role_id: String,
    /// The #jail channel (snowflake). Informational for the panel / DM; the
    /// hiding is enforced by the role's overwrites, not this field.
    #[serde(default)]
    pub jail_channel_id: String,
    /// DM the user a private notice on jail/unjail.
    #[serde(default = "default_true")]
    pub dm_user: bool,
    /// Default sentence length in minutes when none is given. `0` => indefinite.
    #[serde(default)]
    pub default_minutes: u32,
}

fn default_true() -> bool {
    true
}

impl Default for JailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            jail_role_id: String::new(),
            jail_channel_id: String::new(),
            dm_user: true,
            default_minutes: 0,
        }
    }
}

impl JailConfig {
    /// Load from the config-blob store; missing/corrupt => disabled defaults.
    pub fn load(store: &impl ConfigStore) -> Self {
        store
            .get_config_blob(CONFIG_BLOB_KEY)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist this config back to the store.
    pub fn save(&self, store: &impl ConfigStore) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(CONFIG_BLOB_KEY, &json)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && (self.guild_id.trim().is_empty() || self.jail_role_id.trim().is_empty()) {
            return Err("guild_id and jail_role_id are required when jail is enabled".into());
        }
        if self.default_minutes > 525_600 {
            return Err("default_minutes must be 0 (indefinite) .. 525600 (1 year)".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_requires_role_when_enabled() {
        let mut c = JailConfig { enabled: true, guild_id: "1".into(), ..Default::default() };
        assert!(c.validate().is_err()); // no jail_role_id
        c.jail_role_id = "2".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_caps_default_minutes() {
        let c = JailConfig { default_minutes: 525_601, ..Default::default() };
        assert!(c.validate().is_err());
    }

    #[test]
    fn legacy_blob_loads_with_defaults() {
        let legacy = r#"{"enabled":true,"guild_id":"1","jail_role_id":"2"}"#;
        let c: JailConfig = serde_json::from_str(legacy).unwrap();
        assert!(c.dm_user); // serde default = true
        assert_eq!(c.default_minutes, 0);
    }
}
