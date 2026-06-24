//! `airforce-modbot-core` — the platform-agnostic core of the api.airforce
//! Discord moderation bot.
//!
//! This crate holds the **pure, fully unit-tested logic** (anti-advertising link
//! detection, domain whitelist matching, strike-decay math, and the config
//! shapes) plus the **storage/config ports** the bot needs. It depends on NO
//! Discord library and NO concrete database — the [`ports`] traits are the seam
//! a host implements (the bundled bot backs them with an embedded `redb` store).
//!
//! This is the same Ports-&-Adapters core that runs inside api.airforce; lifting
//! it into this standalone crate is what lets the bot be self-hosted by anyone.

pub mod cases;
pub mod flood_filter;
pub mod jail;
pub mod link_filter;
pub mod ports;

pub use cases::{Case, CaseAction, EscalationAction, ModConfig, WarnEscalation};
pub use flood_filter::{FloodAction, FloodFilterConfig, FloodScope, FloodTracker, FloodVerdict};
pub use jail::JailConfig;
pub use link_filter::LinkFilterConfig;
pub use ports::{ConfigStore, JailRecord, JailStore, LinkStrike, StrikeStore};

/// Compose a per-guild config-blob key: `"{guild_id}:{base_key}"`.
///
/// One store can then hold isolated per-feature config for *many* guilds
/// (multi-guild hosting) without changing the [`ConfigStore`] signature: the
/// single-guild path keeps using the bare `*_BLOB_KEY` (and the api.airforce
/// backend, which is single-guild, is unaffected), while a multi-guild host
/// reads/writes via the `*_for_guild` config helpers. Guild ids are snowflakes
/// (digits only), so the `:` separator can never collide with a base key.
pub fn guild_blob_key(guild_id: &str, base_key: &str) -> String {
    format!("{guild_id}:{base_key}")
}

#[cfg(test)]
mod guild_config_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory [`ConfigStore`] for exercising the per-guild config
    /// layer without a real database.
    #[derive(Default)]
    struct MapStore(Mutex<HashMap<String, String>>);
    impl ConfigStore for MapStore {
        fn get_config_blob(&self, key: &str) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn set_config_blob(&self, key: &str, value_json: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(key.to_string(), value_json.to_string());
            Ok(())
        }
    }

    #[test]
    fn guild_blob_key_is_prefixed_and_distinct_per_guild() {
        assert_eq!(guild_blob_key("123", "flood_filter_config"), "123:flood_filter_config");
        assert_ne!(
            guild_blob_key("1", "link_filter_config"),
            guild_blob_key("2", "link_filter_config"),
        );
    }

    #[test]
    fn per_guild_configs_are_isolated_and_default_when_unset() {
        let s = MapStore::default();

        // Two guilds, same feature, different settings — must not collide.
        FloodFilterConfig { enabled: true, guild_id: "111".into(), channel_threshold: 4, ..Default::default() }
            .save_for_guild(&s, "111").unwrap();
        FloodFilterConfig { enabled: true, guild_id: "222".into(), channel_threshold: 9, ..Default::default() }
            .save_for_guild(&s, "222").unwrap();
        assert_eq!(FloodFilterConfig::load_for_guild(&s, "111").channel_threshold, 4);
        assert_eq!(FloodFilterConfig::load_for_guild(&s, "222").channel_threshold, 9);
        // A guild with nothing saved gets disabled defaults, never another guild's.
        assert!(!FloodFilterConfig::load_for_guild(&s, "333").enabled);

        // The same isolation holds for the link + jail configs.
        LinkFilterConfig { enabled: true, guild_id: "111".into(), strike_threshold: 5, ..Default::default() }
            .save_for_guild(&s, "111").unwrap();
        assert_eq!(LinkFilterConfig::load_for_guild(&s, "111").strike_threshold, 5);
        assert_eq!(LinkFilterConfig::load_for_guild(&s, "222").strike_threshold, 3); // default

        JailConfig { enabled: true, guild_id: "111".into(), jail_role_id: "9".into(), default_minutes: 30, ..Default::default() }
            .save_for_guild(&s, "111").unwrap();
        assert_eq!(JailConfig::load_for_guild(&s, "111").default_minutes, 30);
        assert_eq!(JailConfig::load_for_guild(&s, "222").default_minutes, 0); // default

        ModConfig { mod_log_channel_id: "999".into(), ..Default::default() }
            .save_for_guild(&s, "111").unwrap();
        assert_eq!(ModConfig::load_for_guild(&s, "111").mod_log_channel_id, "999");
        assert_eq!(ModConfig::load_for_guild(&s, "222").mod_log_channel_id, ""); // default

        // Per-guild writes never touch the legacy single-guild (bare-key) blob,
        // so the api.airforce backend's single-guild path stays untouched.
        assert!(s.get_config_blob(flood_filter::CONFIG_BLOB_KEY).is_none());
        assert!(s.get_config_blob(link_filter::CONFIG_BLOB_KEY).is_none());
        assert!(s.get_config_blob(jail::CONFIG_BLOB_KEY).is_none());
    }
}
