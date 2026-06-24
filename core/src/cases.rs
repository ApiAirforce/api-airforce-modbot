//! Moderation **case log** — the pure, unit-tested core.
//!
//! Every moderator action (ban / kick / timeout / warn / jail / unjail / note)
//! is recorded as a numbered [`Case`] so a server keeps an auditable per-user
//! history — the classic mod-log + case system the bigger bots have and this one
//! lacked. This module holds only the platform-agnostic shapes plus the
//! **warn-escalation decision** (N warns inside a window auto-apply a harsher
//! action). Assigning case numbers and persisting cases is the host's job (the
//! bot's `CaseStore` over `redb`); posting the mod-log embed is the gateway's.

use serde::{Deserialize, Serialize};

use crate::ports::ConfigStore;

/// The kind of moderator action a [`Case`] records. Serialized lowercase so a
/// config / panel can offer a plain dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseAction {
    Warn,
    Timeout,
    Kick,
    Ban,
    /// The bot's escape-proof role-snapshot jail (see [`crate::JailConfig`]).
    Jail,
    Unjail,
    /// A free-form moderator note attached to a user (no Discord action).
    Note,
}

impl CaseAction {
    /// Human label for a mod-log embed.
    pub fn label(self) -> &'static str {
        match self {
            CaseAction::Warn => "Warn",
            CaseAction::Timeout => "Timeout",
            CaseAction::Kick => "Kick",
            CaseAction::Ban => "Ban",
            CaseAction::Jail => "Jail",
            CaseAction::Unjail => "Unjail",
            CaseAction::Note => "Note",
        }
    }
}

/// One recorded moderation action. `id` is assigned by the store, monotonically
/// **per guild** (case #1, #2, … within each server). `duration_secs` applies to
/// timed actions (timeout / a timed jail); `None` for instantaneous ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub id: u64,
    pub guild_id: String,
    pub user_id: String,
    /// The moderator (or `"AutoMod"` / a feature name for automated actions).
    pub mod_id: String,
    pub action: CaseAction,
    pub reason: String,
    pub created_unix: i64,
    #[serde(default)]
    pub duration_secs: Option<u64>,
}

/// The harsher action auto-applied when a user accrues too many warns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationAction {
    Timeout,
    Jail,
    Ban,
}

fn d_escalation_action() -> EscalationAction {
    EscalationAction::Timeout
}
fn d_timeout_minutes() -> u32 {
    60
}

/// Per-guild warn-escalation policy: at `threshold` warns within `window_days`
/// (0 = warns never expire), auto-apply `action`. `threshold == 0` disables it.
/// Stored inside the per-guild mod config blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarnEscalation {
    /// Warns that trigger escalation. `0` => escalation off.
    #[serde(default)]
    pub threshold: u32,
    /// Sliding window in days within which warns count. `0` => never expire.
    #[serde(default)]
    pub window_days: u32,
    /// What to do at the threshold.
    #[serde(default = "d_escalation_action")]
    pub action: EscalationAction,
    /// Timeout length when `action == Timeout`.
    #[serde(default = "d_timeout_minutes")]
    pub timeout_minutes: u32,
}

impl Default for WarnEscalation {
    fn default() -> Self {
        Self {
            threshold: 0,
            window_days: 0,
            action: d_escalation_action(),
            timeout_minutes: d_timeout_minutes(),
        }
    }
}

impl WarnEscalation {
    /// Bounds-check an admin write.
    pub fn validate(&self) -> Result<(), String> {
        if self.threshold > 100 {
            return Err("warn escalation threshold must be 0 (off) .. 100".into());
        }
        if self.window_days > 3650 {
            return Err("warn escalation window_days must be 0 (never) .. 3650".into());
        }
        if self.action == EscalationAction::Timeout
            && !(1..=40_320).contains(&self.timeout_minutes)
        {
            // Discord caps a timeout at 28 days = 40 320 minutes.
            return Err("timeout_minutes must be 1 .. 40320 (Discord's 28-day cap)".into());
        }
        Ok(())
    }
}

/// Config-blob key for the per-guild moderation config (mod-log + escalation).
pub const MOD_CONFIG_BLOB_KEY: &str = "mod_config";

/// Per-guild moderation config: the mod-log channel plus the warn-escalation
/// policy. Stored as its own blob, loaded/saved guild-scoped like the other
/// configs (its `guild_id` is self-stamped on load).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModConfig {
    #[serde(default)]
    pub guild_id: String,
    /// Mod-log channel (snowflake). Empty => mod-log embeds are skipped.
    #[serde(default)]
    pub mod_log_channel_id: String,
    #[serde(default)]
    pub escalation: WarnEscalation,
}

impl ModConfig {
    /// Load this guild's mod config (missing/corrupt => defaults); stamps `guild_id`.
    pub fn load_for_guild(store: &impl ConfigStore, guild_id: &str) -> Self {
        let mut cfg: Self = store
            .get_config_blob(&crate::guild_blob_key(guild_id, MOD_CONFIG_BLOB_KEY))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.guild_id = guild_id.to_string();
        cfg
    }

    /// Persist this guild's mod config.
    pub fn save_for_guild(&self, store: &impl ConfigStore, guild_id: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(&crate::guild_blob_key(guild_id, MOD_CONFIG_BLOB_KEY), &json)
    }

    /// Bounds-check an admin write (delegates to the escalation policy).
    pub fn validate(&self) -> Result<(), String> {
        self.escalation.validate()
    }
}

/// The next case number for a guild given the current highest seen (`0` if the
/// guild has no cases yet). Monotonic, never reuses a number.
pub fn next_case_number(current_max: u64) -> u64 {
    current_max.saturating_add(1)
}

/// Decide whether issuing a warn *now* should auto-escalate. `prior_warn_unixes`
/// are the timestamps of the user's earlier warns (any order); a warn older than
/// `window_days` (when `> 0`) does not count. The warn being issued now is
/// included in the count. Returns the escalation action once the in-window count
/// reaches the policy threshold, else `None`.
pub fn warn_escalation(
    prior_warn_unixes: &[i64],
    now_unix: i64,
    policy: &WarnEscalation,
) -> Option<EscalationAction> {
    if policy.threshold == 0 {
        return None;
    }
    let counts = |t: i64| {
        policy.window_days == 0
            || now_unix.saturating_sub(t) <= policy.window_days as i64 * 86_400
    };
    let in_window = prior_warn_unixes.iter().filter(|&&t| counts(t)).count() as u32;
    // +1 for the warn being issued now.
    if in_window + 1 >= policy.threshold {
        Some(policy.action)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&CaseAction::Ban).unwrap(), "\"ban\"");
        let a: CaseAction = serde_json::from_str("\"timeout\"").unwrap();
        assert_eq!(a, CaseAction::Timeout);
        assert_eq!(CaseAction::Kick.label(), "Kick");
    }

    #[test]
    fn case_roundtrips_and_defaults_duration() {
        let legacy = r#"{"id":1,"guild_id":"g","user_id":"u","mod_id":"m","action":"warn","reason":"spam","created_unix":100}"#;
        let c: Case = serde_json::from_str(legacy).unwrap();
        assert_eq!(c.duration_secs, None); // serde default
        let back: Case = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn case_numbering_is_monotonic() {
        assert_eq!(next_case_number(0), 1);
        assert_eq!(next_case_number(41), 42);
        assert_eq!(next_case_number(u64::MAX), u64::MAX); // saturates, never wraps
    }

    #[test]
    fn escalation_off_when_threshold_zero() {
        let p = WarnEscalation { threshold: 0, ..Default::default() };
        assert_eq!(warn_escalation(&[1, 2, 3], 100, &p), None);
    }

    #[test]
    fn escalation_fires_at_threshold_counting_the_new_warn() {
        let p = WarnEscalation { threshold: 3, window_days: 0, action: EscalationAction::Jail, ..Default::default() };
        // two prior + this one == 3 => escalate
        assert_eq!(warn_escalation(&[10, 20], 100, &p), Some(EscalationAction::Jail));
        // one prior + this one == 2 < 3 => no
        assert_eq!(warn_escalation(&[10], 100, &p), None);
    }

    #[test]
    fn escalation_ignores_warns_outside_the_window() {
        let day = 86_400;
        let p = WarnEscalation { threshold: 3, window_days: 30, action: EscalationAction::Timeout, timeout_minutes: 60 };
        let now = 100 * day;
        // two warns 40 and 50 days ago are outside the 30-day window => only the
        // new one counts => 1 < 3 => no escalation.
        assert_eq!(warn_escalation(&[now - 40 * day, now - 50 * day], now, &p), None);
        // two warns 5 and 10 days ago are inside => 2 + 1 == 3 => escalate.
        assert_eq!(
            warn_escalation(&[now - 5 * day, now - 10 * day], now, &p),
            Some(EscalationAction::Timeout)
        );
    }

    #[test]
    fn validate_bounds() {
        assert!(WarnEscalation::default().validate().is_ok());
        assert!(WarnEscalation { threshold: 101, ..Default::default() }.validate().is_err());
        assert!(WarnEscalation { action: EscalationAction::Timeout, timeout_minutes: 0, ..Default::default() }.validate().is_err());
        assert!(WarnEscalation { action: EscalationAction::Timeout, timeout_minutes: 50_000, ..Default::default() }.validate().is_err());
        // a huge timeout is fine when the action isn't Timeout
        assert!(WarnEscalation { action: EscalationAction::Ban, timeout_minutes: 50_000, ..Default::default() }.validate().is_ok());
    }
}
