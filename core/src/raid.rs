//! Join-raid detection + per-member join gate — the pure, unit-tested core.
//!
//! Two independent defenses against raids:
//!   * a **join gate** that screens each new member by account age and whether
//!     they have a custom avatar, and
//!   * **join-velocity** detection — a burst of joins in a short window (a
//!     coordinated raid) that the host reacts to (lockdown).
//!
//! Both are pure: the host supplies the member facts + a monotonic clock, so the
//! whole thing is testable with no Discord. Acting on a decision (kick / ban /
//! quarantine / raise the verification level) is the gateway's job.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::ports::ConfigStore;

pub const CONFIG_BLOB_KEY: &str = "raid_config";

/// What to do to a member that fails the join gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GateAction {
    Kick,
    Ban,
    /// Apply the quarantine/jail role so a human can review (the safest default —
    /// a false positive is reversible, unlike a ban).
    Quarantine,
}

fn d_gate_action() -> GateAction {
    GateAction::Quarantine
}
fn d_join_window() -> u32 {
    60
}

/// Admin-editable raid configuration. A threshold of `0` disables that rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RaidConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub guild_id: String,

    // ── join gate (per joining member) ──
    /// Reject accounts younger than this many hours (`0` => no age gate).
    #[serde(default)]
    pub min_account_age_hours: u32,
    /// Reject members with the default (no custom) avatar.
    #[serde(default)]
    pub require_avatar: bool,
    /// What to do to a member that fails the gate.
    #[serde(default = "d_gate_action")]
    pub gate_action: GateAction,

    // ── join-velocity raid (server-wide) ──
    /// Trip when this many members join within `join_window_secs` (`0` => off).
    #[serde(default)]
    pub join_threshold: u32,
    #[serde(default = "d_join_window")]
    pub join_window_secs: u32,

    /// Server-wide lockdown: while true, EVERY join is met with `gate_action`.
    /// Set automatically when a join-velocity raid trips; cleared via the
    /// `/lockdown off` command. Persists across restarts.
    #[serde(default)]
    pub lockdown_active: bool,
}

impl Default for RaidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            min_account_age_hours: 0,
            require_avatar: false,
            gate_action: d_gate_action(),
            join_threshold: 0,
            join_window_secs: d_join_window(),
            lockdown_active: false,
        }
    }
}

impl RaidConfig {
    /// Load this guild's raid config (missing/corrupt => defaults); stamps `guild_id`.
    pub fn load_for_guild(store: &impl ConfigStore, guild_id: &str) -> Self {
        let mut cfg: Self = store
            .get_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.guild_id = guild_id.to_string();
        cfg
    }

    /// Persist this guild's raid config.
    pub fn save_for_guild(&self, store: &impl ConfigStore, guild_id: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY), &json)
    }

    /// Admin-write validation.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.guild_id.trim().is_empty() {
            return Err("guild_id is required when raid protection is enabled".into());
        }
        if self.min_account_age_hours > 87_600 {
            return Err("min_account_age_hours must be 0 (off) .. 87600 (10 years)".into());
        }
        if self.join_threshold != 0 && !(2..=1000).contains(&self.join_threshold) {
            return Err("join_threshold must be 0 (off) or between 2 and 1000".into());
        }
        if !(1..=3600).contains(&self.join_window_secs) {
            return Err("join_window_secs must be between 1 and 3600".into());
        }
        Ok(())
    }

    /// Screen one joining member. `None` => allow; `Some(action)` => the member
    /// failed the gate. Pure — the host checks `enabled` and supplies the facts
    /// (`account_age_secs` = now − account-creation; `has_avatar` = custom avatar).
    pub fn screen_join(&self, account_age_secs: i64, has_avatar: bool) -> Option<GateAction> {
        let age_ok = self.min_account_age_hours == 0
            || account_age_secs >= self.min_account_age_hours as i64 * 3600;
        let avatar_ok = !self.require_avatar || has_avatar;
        if age_ok && avatar_ok {
            None
        } else {
            Some(self.gate_action)
        }
    }
}

/// In-memory per-guild sliding window of recent join times. Trips when joins in
/// the window reach the threshold (a coordinated raid). Memory-bounded to active
/// guilds (a guild's deque is dropped once it prunes empty). Not serialized.
#[derive(Default)]
pub struct JoinTracker {
    by_guild: HashMap<String, VecDeque<u64>>,
}

impl JoinTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a join at `now_ms` and report whether joins within `window_secs`
    /// reached `threshold` (a raid). `now_ms` is a monotonic clock.
    pub fn record_join(&mut self, guild_id: &str, now_ms: u64, threshold: u32, window_secs: u32) -> bool {
        if threshold < 2 {
            return false;
        }
        let cutoff = now_ms.saturating_sub((window_secs as u64).saturating_mul(1000));
        let dq = self.by_guild.entry(guild_id.to_string()).or_default();
        dq.push_back(now_ms);
        while dq.front().is_some_and(|&t| t < cutoff) {
            dq.pop_front();
        }
        let tripped = dq.len() as u32 >= threshold;
        if dq.is_empty() {
            self.by_guild.remove(guild_id);
        }
        tripped
    }

    /// Number of guilds currently tracked (diagnostics/tests).
    pub fn tracked_guilds(&self) -> usize {
        self.by_guild.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_gate_age_and_avatar() {
        let cfg = RaidConfig {
            min_account_age_hours: 24,
            require_avatar: true,
            gate_action: GateAction::Quarantine,
            ..Default::default()
        };
        let hour = 3600;
        // young account fails
        assert_eq!(cfg.screen_join(hour, true), Some(GateAction::Quarantine));
        // no avatar fails
        assert_eq!(cfg.screen_join(48 * hour, false), Some(GateAction::Quarantine));
        // old enough + has avatar passes
        assert_eq!(cfg.screen_join(48 * hour, true), None);
    }

    #[test]
    fn join_gate_off_when_thresholds_zero() {
        let cfg = RaidConfig { min_account_age_hours: 0, require_avatar: false, ..Default::default() };
        assert_eq!(cfg.screen_join(0, false), None); // brand-new, no avatar, still allowed
    }

    #[test]
    fn join_velocity_trips_on_burst() {
        let mut t = JoinTracker::new();
        // threshold 5 in 60s
        for i in 0..4 {
            assert!(!t.record_join("g", i * 1000, 5, 60));
        }
        assert!(t.record_join("g", 4000, 5, 60)); // 5th join within window => raid
        // a different guild is independent
        assert!(!t.record_join("g2", 0, 5, 60));
    }

    #[test]
    fn join_velocity_window_evicts_old() {
        let mut t = JoinTracker::new();
        assert!(!t.record_join("g", 0, 3, 10));
        assert!(!t.record_join("g", 1000, 3, 10));
        // 12s later the first two are outside the 10s window => only 1 in window
        assert!(!t.record_join("g", 13_000, 3, 10));
    }

    #[test]
    fn validate_bounds() {
        assert!(RaidConfig::default().validate().is_ok());
        assert!(RaidConfig { join_threshold: 1, ..Default::default() }.validate().is_err());
        assert!(RaidConfig { join_window_secs: 0, ..Default::default() }.validate().is_err());
        assert!(RaidConfig { min_account_age_hours: 99_999, ..Default::default() }.validate().is_err());
    }
}
