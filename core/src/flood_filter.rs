//! Cross-channel **flood / raid filter** — the pure, unit-tested core.
//!
//! Catches the classic scam pattern: one account blasting the same thing across
//! many channels in seconds (or hammering one channel). It is deliberately
//! independent of the link filter — a raider may post image-only spam with no
//! links at all — but it reuses the same strike + real-jail machinery so an
//! admin manages one quarantine system.
//!
//! Two thresholds fire it, either alone (both fully admin-tunable at runtime):
//!   * **spread** — messages in `>= channel_threshold` DISTINCT channels within
//!     `channel_window_secs` (the cross-channel signal), and
//!   * **burst** — `>= msg_threshold` messages within `msg_window_secs`
//!     (single- or multi-channel volume).
//!
//! `scope` decides which messages even count (all / attachments only / with an
//! attachment or a link) — the host classifies each message and only records the
//! ones that count, so this module stays Discord-agnostic. Config lives in a
//! JSON blob ([`ConfigStore`]); the live sliding-window state is the in-memory
//! [`FloodTracker`], which takes an explicit monotonic clock so tests drive time.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::link_filter::UserChannelExempt;
use crate::ports::ConfigStore;

/// Config-blob key for the flood filter (sibling of `link_filter_config`).
pub const CONFIG_BLOB_KEY: &str = "flood_filter_config";

/// What to do when a user trips the filter. Serialized as a lowercase string so
/// the admin panel can offer a plain dropdown.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FloodAction {
    /// Delete the flooded messages + DM the user. No role change.
    Warn,
    /// Delete the flooded messages only (DM gated by `warn_user`).
    Delete,
    /// Delete + record a strike + assign the jail role (escape-proof real jail
    /// is the host's job). The default — scam floods earn immediate quarantine.
    Jail,
}

/// Which messages count toward the thresholds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FloodScope {
    /// Every message counts. Cross-channel-in-seconds is itself the signal.
    All,
    /// Only messages carrying an attachment/image.
    Attachments,
    /// Messages with an attachment OR a link.
    AttachmentsOrLinks,
}

/// Per-user threshold override (a trusted poster gets looser limits, a known
/// repeat-offender tighter). Either field `0` means "inherit the global value".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FloodUserOverride {
    pub user_id: String,
    #[serde(default)]
    pub channel_threshold: u32,
    #[serde(default)]
    pub msg_threshold: u32,
}

fn d_channel_threshold() -> u32 {
    3
}
fn d_channel_window() -> u32 {
    10
}
fn d_msg_threshold() -> u32 {
    5
}
fn d_msg_window() -> u32 {
    15
}
fn d_true() -> bool {
    true
}
fn d_action() -> FloodAction {
    FloodAction::Jail
}
fn d_scope() -> FloodScope {
    FloodScope::All
}

/// Admin-editable flood-filter configuration. Every knob is runtime-tunable via
/// the admin panel; `#[serde(default)]` on each field keeps an older/partial
/// blob loadable instead of resetting the whole config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FloodFilterConfig {
    /// Master switch. `false` => the tracker is never consulted.
    #[serde(default)]
    pub enabled: bool,
    /// Guild this applies to (snowflake string). Empty => inactive.
    #[serde(default)]
    pub guild_id: String,

    /// Spread: trip when a user posts in this many DISTINCT channels within
    /// `channel_window_secs`. Set `channel_threshold` to 0 to disable the spread
    /// rule and rely on burst alone.
    #[serde(default = "d_channel_threshold")]
    pub channel_threshold: u32,
    #[serde(default = "d_channel_window")]
    pub channel_window_secs: u32,

    /// Burst: trip when a user posts this many messages within `msg_window_secs`
    /// (any channels). Set `msg_threshold` to 0 to disable the burst rule.
    #[serde(default = "d_msg_threshold")]
    pub msg_threshold: u32,
    #[serde(default = "d_msg_window")]
    pub msg_window_secs: u32,

    /// What happens on a trip.
    #[serde(default = "d_action")]
    pub action: FloodAction,
    /// Which messages count.
    #[serde(default = "d_scope")]
    pub scope: FloodScope,

    /// Jail role assigned when `action == Jail` (snowflake). Empty => the host
    /// falls back to delete+strike only (works before a Jail role exists).
    #[serde(default)]
    pub jail_role_id: String,
    /// Strike decay window in days for flood strikes (`0` => never expire).
    #[serde(default)]
    pub decay_days: u32,
    /// DM the user a notice on a trip.
    #[serde(default = "d_true")]
    pub warn_user: bool,

    /// Channels never tracked (snowflakes).
    #[serde(default)]
    pub exempt_channel_ids: Vec<String>,
    /// Roles whose holders are never tracked (mods/staff). Bot-admins are always
    /// exempt regardless (enforced by the host).
    #[serde(default)]
    pub exempt_role_ids: Vec<String>,
    /// Per-(user, channel) exemptions — reuses the link filter's shape.
    #[serde(default)]
    pub exempt_user_channels: Vec<UserChannelExempt>,
    /// Per-user threshold overrides.
    #[serde(default)]
    pub user_overrides: Vec<FloodUserOverride>,
}

impl Default for FloodFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            channel_threshold: d_channel_threshold(),
            channel_window_secs: d_channel_window(),
            msg_threshold: d_msg_threshold(),
            msg_window_secs: d_msg_window(),
            action: d_action(),
            scope: d_scope(),
            jail_role_id: String::new(),
            decay_days: 0,
            warn_user: true,
            exempt_channel_ids: Vec::new(),
            exempt_role_ids: Vec::new(),
            exempt_user_channels: Vec::new(),
            user_overrides: Vec::new(),
        }
    }
}

impl FloodFilterConfig {
    /// Load from the config-blob store; missing/corrupt => disabled defaults
    /// (never panics — a bad manual edit must not take the gateway down).
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

    /// Admin-write validation. Bounds keep a typo from turning the filter into a
    /// footgun (e.g. a 1-message threshold that nukes every post).
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.guild_id.trim().is_empty() {
            return Err("guild_id is required when the flood filter is enabled".into());
        }
        if self.channel_threshold == 0 && self.msg_threshold == 0 {
            return Err("at least one of channel_threshold / msg_threshold must be > 0".into());
        }
        if self.channel_threshold != 0 && !(2..=50).contains(&self.channel_threshold) {
            return Err("channel_threshold must be 0 (off) or between 2 and 50".into());
        }
        if self.msg_threshold != 0 && !(2..=100).contains(&self.msg_threshold) {
            return Err("msg_threshold must be 0 (off) or between 2 and 100".into());
        }
        if !(1..=3600).contains(&self.channel_window_secs) {
            return Err("channel_window_secs must be between 1 and 3600".into());
        }
        if !(1..=3600).contains(&self.msg_window_secs) {
            return Err("msg_window_secs must be between 1 and 3600".into());
        }
        if self.decay_days > 3650 {
            return Err("decay_days must be 0 (never) .. 3650".into());
        }
        for cap in [
            ("exempt_channel_ids", self.exempt_channel_ids.len()),
            ("exempt_role_ids", self.exempt_role_ids.len()),
            ("exempt_user_channels", self.exempt_user_channels.len()),
            ("user_overrides", self.user_overrides.len()),
        ] {
            if cap.1 > 1000 {
                return Err(format!("{} capped at 1000 entries", cap.0));
            }
        }
        Ok(())
    }

    /// Effective (channel_threshold, msg_threshold) for `user_id`: a per-user
    /// override replaces the global value only where it is non-zero.
    pub fn thresholds_for(&self, user_id: &str) -> (u32, u32) {
        let mut ch = self.channel_threshold;
        let mut ms = self.msg_threshold;
        if let Some(o) = self.user_overrides.iter().find(|o| o.user_id == user_id) {
            if o.channel_threshold != 0 {
                ch = o.channel_threshold;
            }
            if o.msg_threshold != 0 {
                ms = o.msg_threshold;
            }
        }
        (ch, ms)
    }

    /// True when `user_id` is exempt specifically in `channel_id`.
    pub fn is_user_channel_exempt(&self, user_id: &str, channel_id: &str) -> bool {
        self.exempt_user_channels
            .iter()
            .any(|e| e.user_id == user_id && e.channel_id == channel_id)
    }

    /// Whether a message with the given traits counts toward the thresholds
    /// under the configured `scope`. The host supplies the booleans (it alone
    /// can parse Discord attachments/links), keeping this crate platform-free.
    pub fn message_counts(&self, has_attachment: bool, has_link: bool) -> bool {
        match self.scope {
            FloodScope::All => true,
            FloodScope::Attachments => has_attachment,
            FloodScope::AttachmentsOrLinks => has_attachment || has_link,
        }
    }
}

/// One recorded message in the sliding window.
#[derive(Debug, Clone)]
struct FloodEvent {
    channel_id: String,
    message_id: String,
    at_ms: u64,
}

/// The outcome of a trip: the messages to delete (across channels) and a human
/// reason for the strike/audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloodVerdict {
    /// `(channel_id, message_id)` pairs to delete, oldest first.
    pub messages_to_delete: Vec<(String, String)>,
    /// Which rule fired, for the strike reason + audit.
    pub reason: String,
}

/// In-memory per-user sliding window. Lives for the bot's lifetime (one per
/// guild is fine); not serialized. Memory is bounded to *active* users — a
/// user's deque is dropped once it prunes empty.
#[derive(Default)]
pub struct FloodTracker {
    by_user: HashMap<String, VecDeque<FloodEvent>>,
}

impl FloodTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one *counting* message (the host must already have applied
    /// `scope` + exemptions) and evaluate both rules. Returns a [`FloodVerdict`]
    /// when the user tripped — and in that case the user's window is cleared so
    /// the same burst is not re-reported on the next message.
    ///
    /// `now_ms` is a monotonic millisecond clock (tests pass synthetic values).
    pub fn record_and_check(
        &mut self,
        user_id: &str,
        channel_id: &str,
        message_id: &str,
        now_ms: u64,
        cfg: &FloodFilterConfig,
        channel_threshold: u32,
        msg_threshold: u32,
    ) -> Option<FloodVerdict> {
        let horizon_secs = cfg.channel_window_secs.max(cfg.msg_window_secs) as u64;
        let horizon_ms = horizon_secs.saturating_mul(1000);

        let dq = self.by_user.entry(user_id.to_string()).or_default();
        dq.push_back(FloodEvent {
            channel_id: channel_id.to_string(),
            message_id: message_id.to_string(),
            at_ms: now_ms,
        });
        // Prune everything older than the widest window we care about.
        let cutoff = now_ms.saturating_sub(horizon_ms);
        while dq.front().is_some_and(|e| e.at_ms < cutoff) {
            dq.pop_front();
        }

        // Spread rule: distinct channels within the channel window.
        let mut tripped: Option<String> = None;
        if channel_threshold >= 2 {
            let ch_cutoff = now_ms.saturating_sub((cfg.channel_window_secs as u64) * 1000);
            let distinct: HashSet<&str> = dq
                .iter()
                .filter(|e| e.at_ms >= ch_cutoff)
                .map(|e| e.channel_id.as_str())
                .collect();
            if distinct.len() as u32 >= channel_threshold {
                tripped = Some(format!(
                    "flood: {} channels in {}s",
                    distinct.len(),
                    cfg.channel_window_secs
                ));
            }
        }
        // Burst rule: message count within the message window.
        if tripped.is_none() && msg_threshold >= 2 {
            let ms_cutoff = now_ms.saturating_sub((cfg.msg_window_secs as u64) * 1000);
            let count = dq.iter().filter(|e| e.at_ms >= ms_cutoff).count() as u32;
            if count >= msg_threshold {
                tripped = Some(format!(
                    "flood: {} messages in {}s",
                    count, cfg.msg_window_secs
                ));
            }
        }

        match tripped {
            Some(reason) => {
                // Delete every still-tracked message from this burst, oldest
                // first, then clear so we don't re-fire on the next post.
                let messages_to_delete = dq
                    .iter()
                    .map(|e| (e.channel_id.clone(), e.message_id.clone()))
                    .collect();
                self.by_user.remove(user_id);
                Some(FloodVerdict {
                    messages_to_delete,
                    reason,
                })
            }
            None => {
                // Drop the user entirely once their window empties — keeps the
                // map proportional to currently-active posters.
                if dq.is_empty() {
                    self.by_user.remove(user_id);
                }
                None
            }
        }
    }

    /// Forget a user's window (e.g. right after jailing them).
    pub fn clear_user(&mut self, user_id: &str) {
        self.by_user.remove(user_id);
    }

    /// Number of users currently tracked (diagnostics/tests).
    pub fn tracked_users(&self) -> usize {
        self.by_user.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FloodFilterConfig {
        FloodFilterConfig {
            enabled: true,
            guild_id: "g".into(),
            channel_threshold: 3,
            channel_window_secs: 10,
            msg_threshold: 5,
            msg_window_secs: 15,
            ..Default::default()
        }
    }

    fn rec(
        t: &mut FloodTracker,
        c: &FloodFilterConfig,
        user: &str,
        chan: &str,
        msg: &str,
        now_ms: u64,
    ) -> Option<FloodVerdict> {
        let (ch, ms) = c.thresholds_for(user);
        t.record_and_check(user, chan, msg, now_ms, c, ch, ms)
    }

    #[test]
    fn spread_rule_trips_on_distinct_channels() {
        let c = cfg();
        let mut t = FloodTracker::new();
        assert!(rec(&mut t, &c, "u", "c1", "m1", 0).is_none());
        assert!(rec(&mut t, &c, "u", "c2", "m2", 1000).is_none());
        let v = rec(&mut t, &c, "u", "c3", "m3", 2000).expect("3 channels in 10s trips");
        assert_eq!(v.messages_to_delete.len(), 3);
        assert!(v.reason.contains("channels"));
        // window cleared after a trip
        assert_eq!(t.tracked_users(), 0);
    }

    #[test]
    fn spread_rule_ignores_repeats_in_one_channel() {
        let c = cfg();
        let mut t = FloodTracker::new();
        // 3 messages but all in c1 => spread does not trip (only 1 distinct);
        // and 3 < msg_threshold(5) so burst doesn't either.
        assert!(rec(&mut t, &c, "u", "c1", "m1", 0).is_none());
        assert!(rec(&mut t, &c, "u", "c1", "m2", 500).is_none());
        assert!(rec(&mut t, &c, "u", "c1", "m3", 900).is_none());
    }

    #[test]
    fn burst_rule_trips_on_volume_single_channel() {
        let c = cfg();
        let mut t = FloodTracker::new();
        for i in 0..4 {
            assert!(rec(&mut t, &c, "u", "c1", &format!("m{i}"), i * 100).is_none());
        }
        let v = rec(&mut t, &c, "u", "c1", "m4", 500).expect("5 msgs in 15s trips");
        assert_eq!(v.messages_to_delete.len(), 5);
        assert!(v.reason.contains("messages"));
    }

    #[test]
    fn old_events_outside_window_do_not_count() {
        let c = cfg();
        let mut t = FloodTracker::new();
        // two channels long ago, beyond the 10s spread window
        assert!(rec(&mut t, &c, "u", "c1", "m1", 0).is_none());
        assert!(rec(&mut t, &c, "u", "c2", "m2", 1000).is_none());
        // 12s later a 3rd channel: c1/c2 are now >10s old => only 1 in window
        assert!(rec(&mut t, &c, "u", "c3", "m3", 13_000).is_none());
    }

    #[test]
    fn per_user_override_tightens_threshold() {
        let mut c = cfg();
        c.user_overrides.push(FloodUserOverride {
            user_id: "spammer".into(),
            channel_threshold: 2,
            msg_threshold: 0,
        });
        let mut t = FloodTracker::new();
        assert!(rec(&mut t, &c, "spammer", "c1", "m1", 0).is_none());
        // 2 distinct channels already trips for the override'd user
        assert!(rec(&mut t, &c, "spammer", "c2", "m2", 500).is_some());
        // a normal user still needs 3
        assert!(rec(&mut t, &c, "normal", "c1", "n1", 0).is_none());
        assert!(rec(&mut t, &c, "normal", "c2", "n2", 500).is_none());
    }

    #[test]
    fn scope_filters_counting_messages() {
        let mut c = cfg();
        c.scope = FloodScope::Attachments;
        assert!(c.message_counts(true, false));
        assert!(!c.message_counts(false, true));
        c.scope = FloodScope::AttachmentsOrLinks;
        assert!(c.message_counts(false, true));
        assert!(!c.message_counts(false, false));
        c.scope = FloodScope::All;
        assert!(c.message_counts(false, false));
    }

    #[test]
    fn validate_rejects_footguns() {
        let mut c = cfg();
        c.channel_threshold = 0;
        c.msg_threshold = 0;
        assert!(c.validate().is_err());
        let mut c2 = cfg();
        c2.msg_threshold = 1; // below the min-2 floor
        assert!(c2.validate().is_err());
        assert!(cfg().validate().is_ok());
    }

    #[test]
    fn idle_user_is_evicted() {
        let c = cfg();
        let mut t = FloodTracker::new();
        assert!(rec(&mut t, &c, "u", "c1", "m1", 0).is_none());
        assert_eq!(t.tracked_users(), 1);
        // a single message far in the future prunes the old one to empty first,
        // leaving exactly the new one (still 1 tracked, not a trip)
        assert!(rec(&mut t, &c, "u", "c1", "m2", 100_000).is_none());
        assert_eq!(t.tracked_users(), 1);
    }
}
