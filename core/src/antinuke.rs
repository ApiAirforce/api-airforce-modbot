//! Anti-nuke — the pure, unit-tested core.
//!
//! Catches a rogue or compromised admin "nuking" a server: mass-deleting
//! channels/roles, mass-banning, or spawning webhooks. The host feeds each
//! destructive privileged action (read from the audit log) keyed by the actor;
//! this decides when one actor crosses the destructive-action threshold inside a
//! window. Acting on a trip — stripping the actor's dangerous roles + alerting —
//! is the gateway's job. **Trusted** actors (the owner, vetted admins, the bot
//! itself) never trip; defaults are conservative because a false positive strips
//! a real admin.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::ports::ConfigStore;

pub const CONFIG_BLOB_KEY: &str = "antinuke_config";

/// A destructive privileged action anti-nuke counts (the common nuke vectors).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DestructiveAction {
    ChannelDelete,
    RoleDelete,
    Ban,
    Kick,
    WebhookCreate,
}

impl DestructiveAction {
    pub fn label(self) -> &'static str {
        match self {
            DestructiveAction::ChannelDelete => "channel delete",
            DestructiveAction::RoleDelete => "role delete",
            DestructiveAction::Ban => "ban",
            DestructiveAction::Kick => "kick",
            DestructiveAction::WebhookCreate => "webhook create",
        }
    }
}

fn d_max_actions() -> u32 {
    // Conservative: a real nuke is dozens of destructive actions in seconds, so a
    // threshold of 10/window still trips near-instantly, while a routine admin
    // restructure (deleting a handful of channels) does not.
    10
}
fn d_window() -> u32 {
    30
}

/// Admin-editable anti-nuke configuration. Conservative by default (off until an
/// admin enables it, since acting strips roles from the offending actor).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AntinukeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub guild_id: String,
    /// Trip when one actor performs this many destructive actions within
    /// `window_secs` (`0` => off).
    #[serde(default = "d_max_actions")]
    pub max_actions: u32,
    #[serde(default = "d_window")]
    pub window_secs: u32,
    /// Actors that never trip anti-nuke (the guild owner is always exempt at the
    /// host regardless): vetted admins, known good bots.
    #[serde(default)]
    pub trusted_ids: Vec<String>,
    /// Alert-only: detect + alert but do NOT strip the actor's roles (a safe
    /// first-run / tuning mode).
    #[serde(default)]
    pub dry_run: bool,
}

impl Default for AntinukeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            max_actions: d_max_actions(),
            window_secs: d_window(),
            trusted_ids: Vec::new(),
            dry_run: false,
        }
    }
}

impl AntinukeConfig {
    /// Load this guild's anti-nuke config (missing/corrupt => defaults); stamps `guild_id`.
    pub fn load_for_guild(store: &impl ConfigStore, guild_id: &str) -> Self {
        let mut cfg: Self = store
            .get_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.guild_id = guild_id.to_string();
        cfg
    }

    /// Persist this guild's anti-nuke config.
    pub fn save_for_guild(&self, store: &impl ConfigStore, guild_id: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY), &json)
    }

    /// Admin-write validation.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.guild_id.trim().is_empty() {
            return Err("guild_id is required when anti-nuke is enabled".into());
        }
        if self.max_actions != 0 && !(2..=100).contains(&self.max_actions) {
            return Err("max_actions must be 0 (off) or between 2 and 100".into());
        }
        if !(1..=3600).contains(&self.window_secs) {
            return Err("window_secs must be between 1 and 3600".into());
        }
        if self.trusted_ids.len() > 1000 {
            return Err("trusted_ids capped at 1000 entries".into());
        }
        Ok(())
    }

    /// True when `actor_id` is on the trusted allowlist (never trips anti-nuke).
    /// The guild owner + the bot itself are exempted by the host separately.
    pub fn is_trusted(&self, actor_id: &str) -> bool {
        self.trusted_ids.iter().any(|t| t == actor_id)
    }
}

/// In-memory per-(guild,actor) sliding window of destructive-action times. Trips
/// when one actor's actions in the window reach the threshold. Memory-bounded to
/// active actors. Not serialized.
#[derive(Default)]
pub struct ActionTracker {
    by_actor: HashMap<String, VecDeque<u64>>,
}

impl ActionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one destructive action by `actor_key` (the host scopes it, e.g.
    /// `"{guild}:{actor}"`) at `now_ms` and report whether that actor reached
    /// `threshold` actions within `window_secs` (a nuke). On a trip the actor's
    /// window is cleared so the same burst is not re-reported.
    pub fn record_action(&mut self, actor_key: &str, now_ms: u64, threshold: u32, window_secs: u32) -> bool {
        if threshold < 2 {
            return false;
        }
        let cutoff = now_ms.saturating_sub((window_secs as u64).saturating_mul(1000));
        let dq = self.by_actor.entry(actor_key.to_string()).or_default();
        dq.push_back(now_ms);
        while dq.front().is_some_and(|&t| t < cutoff) {
            dq.pop_front();
        }
        if dq.len() as u32 >= threshold {
            self.by_actor.remove(actor_key);
            return true;
        }
        if dq.is_empty() {
            self.by_actor.remove(actor_key);
        }
        false
    }

    /// Forget an actor's window.
    pub fn clear_actor(&mut self, actor_key: &str) {
        self.by_actor.remove(actor_key);
    }

    /// Number of actors currently tracked (diagnostics/tests).
    pub fn tracked_actors(&self) -> usize {
        self.by_actor.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_actor_check() {
        let cfg = AntinukeConfig { trusted_ids: vec!["111".into()], ..Default::default() };
        assert!(cfg.is_trusted("111"));
        assert!(!cfg.is_trusted("222"));
    }

    #[test]
    fn nuke_trips_at_threshold_per_actor() {
        let mut t = ActionTracker::new();
        // threshold 5 in 30s for actor a
        for i in 0..4 {
            assert!(!t.record_action("g:a", i * 1000, 5, 30));
        }
        assert!(t.record_action("g:a", 4000, 5, 30)); // 5th destructive action => nuke
        assert_eq!(t.tracked_actors(), 0); // cleared after a trip
        // a different actor is independent
        assert!(!t.record_action("g:b", 0, 5, 30));
    }

    #[test]
    fn nuke_window_evicts_old_actions() {
        let mut t = ActionTracker::new();
        assert!(!t.record_action("g:a", 0, 3, 10));
        assert!(!t.record_action("g:a", 1000, 3, 10));
        // 12s later the first two are outside the 10s window => only 1 counts
        assert!(!t.record_action("g:a", 13_000, 3, 10));
    }

    #[test]
    fn validate_bounds() {
        assert!(AntinukeConfig::default().validate().is_ok());
        assert!(AntinukeConfig { max_actions: 1, ..Default::default() }.validate().is_err());
        assert!(AntinukeConfig { window_secs: 0, ..Default::default() }.validate().is_err());
    }

    #[test]
    fn action_labels() {
        assert_eq!(DestructiveAction::ChannelDelete.label(), "channel delete");
        assert_eq!(DestructiveAction::Ban.label(), "ban");
    }
}
