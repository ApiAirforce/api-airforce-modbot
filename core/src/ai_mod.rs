//! AI moderation — the pure, LLM-agnostic core.
//!
//! Context-aware moderation for what rule-based automod can't catch (subtle
//! toxicity, scams, context-dependent rule-breaking, multilingual abuse). The
//! actual LLM call is the **host's** job — an adapter over the owner's
//! api.airforce account; this module imports no HTTP client and no async runtime.
//!
//! It owns the platform-free pieces:
//!   * [`AiModConfig`] — the per-guild settings (model, policy, action, the cost
//!     guards), with the same load/save/validate shape as the other filters,
//!   * [`AiVerdict`] — what an adapter returns for one message (with a fail-open
//!     [`AiVerdict::allow`] for when the API is down — AI must never block legit
//!     chat),
//!   * [`AiModConfig::should_classify`] — the cheap pre-filter that decides
//!     whether a message is even worth a (paid) API call,
//!   * [`AiModConfig::within_budget`] — the hard per-day call cap on the owner's
//!     account, and
//!   * [`AiModConfig::action_for`] — mapping a verdict to an [`AutomodAction`] so
//!     AI moderation feeds the exact same action path as the rule engine.
//!
//! The `AiClassifier` port (an async HTTP call) lives with the host adapter, not
//! here, so this crate stays dependency-clean for the api.airforce backend that
//! vendors it.

use serde::{Deserialize, Serialize};

use crate::automod::AutomodAction;
use crate::link_filter::UserChannelExempt;
use crate::ports::ConfigStore;

/// Config-blob key for AI moderation (sibling of the other `*_config`s).
pub const CONFIG_BLOB_KEY: &str = "ai_mod_config";

/// A classifier's judgment of one message. An adapter fills this in from the LLM
/// response; the core maps it to an action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiVerdict {
    /// Whether the message violates the guild's policy.
    pub flagged: bool,
    /// Short machine category for the audit/mod-log (e.g. `toxicity`, `scam`,
    /// `harassment`, `none`). Free-form — the model picks it.
    pub category: String,
    /// 0..=100 confidence that `flagged` is correct.
    pub confidence: u8,
    /// Human-readable justification (shown in the mod-log).
    pub reason: String,
}

impl AiVerdict {
    /// The safe "nothing wrong" verdict — the **fail-open** default the host
    /// returns when the classifier errors, times out, or is rate-limited, so a
    /// down API never blocks or punishes legitimate chat.
    pub fn allow() -> Self {
        Self {
            flagged: false,
            category: "none".to_string(),
            confidence: 0,
            reason: String::new(),
        }
    }

    /// Clamp a freshly-built verdict into a sane shape (confidence 0..=100,
    /// category never empty) — defensive against a malformed model response.
    pub fn sanitized(mut self) -> Self {
        if self.confidence > 100 {
            self.confidence = 100;
        }
        if self.category.trim().is_empty() {
            self.category = if self.flagged { "flagged".to_string() } else { "none".to_string() };
        }
        self
    }
}

fn d_action() -> AutomodAction {
    AutomodAction::Delete
}
fn d_timeout_minutes() -> u32 {
    10
}
fn d_confidence() -> u8 {
    75
}
fn d_min_chars() -> u32 {
    12
}
fn d_max_chars() -> u32 {
    2000
}
fn d_daily_cap() -> u32 {
    500
}
fn d_true() -> bool {
    true
}

/// Admin-editable AI-moderation configuration. Disabled by default and gated
/// behind explicit cost guards because every enabled call spends the **owner's**
/// api.airforce credit. `#[serde(default)]` on each field keeps an older/partial
/// blob loadable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiModConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub guild_id: String,

    /// api.airforce model id the adapter calls (host-resolved). Empty => not
    /// configured (the filter can't run).
    #[serde(default)]
    pub model: String,
    /// Per-guild policy prompt: what counts as a violation in THIS server. Empty
    /// => the adapter uses its built-in default policy.
    #[serde(default)]
    pub policy: String,

    /// Action when a message is flagged at or above `confidence_threshold`.
    #[serde(default = "d_action")]
    pub action: AutomodAction,
    /// Timeout length when `action == Timeout`.
    #[serde(default = "d_timeout_minutes")]
    pub timeout_minutes: u32,
    /// Only act when the verdict confidence is at least this (0..=100). A
    /// low-confidence guess never punishes a user.
    #[serde(default = "d_confidence")]
    pub confidence_threshold: u8,

    // ── cost guards (critical — the owner's account pays) ──
    /// Pre-filter: skip the API for messages shorter than this many characters
    /// (not enough signal to be worth a paid call).
    #[serde(default = "d_min_chars")]
    pub min_chars: u32,
    /// The adapter truncates a message to this many characters before sending
    /// (bounds token cost on a pasted wall of text).
    #[serde(default = "d_max_chars")]
    pub max_chars: u32,
    /// Hard cap on classifier calls **per UTC day** for this guild. The host
    /// counts calls and stops at the cap (fail-open: stop calling, don't block).
    #[serde(default = "d_daily_cap")]
    pub daily_call_cap: u32,

    /// DM the user when AI moderation acts on their message.
    #[serde(default = "d_true")]
    pub warn_user: bool,

    // ── exemptions (mirrors the automod surface) ──
    #[serde(default)]
    pub exempt_channel_ids: Vec<String>,
    #[serde(default)]
    pub exempt_role_ids: Vec<String>,
    #[serde(default)]
    pub exempt_user_channels: Vec<UserChannelExempt>,
}

impl Default for AiModConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            model: String::new(),
            policy: String::new(),
            action: d_action(),
            timeout_minutes: d_timeout_minutes(),
            confidence_threshold: d_confidence(),
            min_chars: d_min_chars(),
            max_chars: d_max_chars(),
            daily_call_cap: d_daily_cap(),
            warn_user: true,
            exempt_channel_ids: Vec::new(),
            exempt_role_ids: Vec::new(),
            exempt_user_channels: Vec::new(),
        }
    }
}

impl AiModConfig {
    /// Load this guild's AI-mod config (missing/corrupt => disabled defaults);
    /// stamps `guild_id`.
    pub fn load_for_guild(store: &impl ConfigStore, guild_id: &str) -> Self {
        let mut cfg: Self = store
            .get_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.guild_id = guild_id.to_string();
        cfg
    }

    /// Persist this guild's AI-mod config.
    pub fn save_for_guild(&self, store: &impl ConfigStore, guild_id: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY), &json)
    }

    /// Admin-write validation. Because acting spends real credit, a config that
    /// is `enabled` must name a model and carry a non-zero daily cap.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled {
            if self.guild_id.trim().is_empty() {
                return Err("guild_id is required when AI moderation is enabled".into());
            }
            if self.model.trim().is_empty() {
                return Err("a model is required when AI moderation is enabled".into());
            }
            if self.daily_call_cap == 0 {
                return Err("daily_call_cap must be > 0 when AI moderation is enabled (cost guard)".into());
            }
        }
        if self.confidence_threshold > 100 {
            return Err("confidence_threshold must be 0 .. 100".into());
        }
        if self.action == AutomodAction::Timeout && !(1..=40_320).contains(&self.timeout_minutes) {
            return Err("timeout_minutes must be 1 .. 40320 (Discord's 28-day cap)".into());
        }
        if self.daily_call_cap > 1_000_000 {
            return Err("daily_call_cap must be <= 1000000".into());
        }
        if !(1..=8000).contains(&self.max_chars) {
            return Err("max_chars must be between 1 and 8000".into());
        }
        if self.min_chars > self.max_chars {
            return Err("min_chars must be <= max_chars".into());
        }
        if self.model.len() > 200 {
            return Err("model id too long (max 200 chars)".into());
        }
        if self.policy.len() > 4000 {
            return Err("policy prompt too long (max 4000 chars)".into());
        }
        for cap in [
            ("exempt_channel_ids", self.exempt_channel_ids.len()),
            ("exempt_role_ids", self.exempt_role_ids.len()),
            ("exempt_user_channels", self.exempt_user_channels.len()),
        ] {
            if cap.1 > 1000 {
                return Err(format!("{} capped at 1000 entries", cap.0));
            }
        }
        Ok(())
    }

    /// True when `user_id` is exempt specifically in `channel_id`.
    pub fn is_user_channel_exempt(&self, user_id: &str, channel_id: &str) -> bool {
        self.exempt_user_channels
            .iter()
            .any(|e| e.user_id == user_id && e.channel_id == channel_id)
    }

    /// Cheap pre-filter (the first cost guard): is this message even worth a paid
    /// classifier call? Skips messages below `min_chars` (too little signal). The
    /// host applies channel/role/user exemptions and the budget check separately.
    pub fn should_classify(&self, content: &str) -> bool {
        content.chars().count() as u32 >= self.min_chars.max(1)
    }

    /// The second cost guard: whether another call is allowed given how many were
    /// already made for this guild today. The host persists/raises `used_today`
    /// and resets it per UTC day.
    pub fn within_budget(&self, used_today: u32) -> bool {
        used_today < self.daily_call_cap
    }

    /// Map a classifier verdict to the configured action, or `None` for no action.
    /// Acts only when the message is flagged AND the confidence meets the
    /// threshold — so an uncertain guess never punishes a user.
    pub fn action_for(&self, verdict: &AiVerdict) -> Option<AutomodAction> {
        if verdict.flagged && verdict.confidence >= self.confidence_threshold {
            Some(self.action)
        } else {
            None
        }
    }

    /// Truncate `content` to `max_chars` characters (char-safe) for sending to the
    /// classifier — bounds token cost on a pasted wall of text.
    pub fn truncate_for_call<'a>(&self, content: &'a str) -> std::borrow::Cow<'a, str> {
        let max = self.max_chars as usize;
        if content.chars().count() <= max {
            std::borrow::Cow::Borrowed(content)
        } else {
            std::borrow::Cow::Owned(content.chars().take(max).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AiModConfig {
        AiModConfig {
            enabled: true,
            guild_id: "g".into(),
            model: "some-model".into(),
            ..Default::default()
        }
    }

    #[test]
    fn action_only_when_flagged_and_confident() {
        let c = cfg(); // confidence_threshold default 75, action Delete
        // flagged + confident => act
        let v = AiVerdict { flagged: true, category: "scam".into(), confidence: 90, reason: "x".into() };
        assert_eq!(c.action_for(&v), Some(AutomodAction::Delete));
        // flagged but below threshold => no action
        let low = AiVerdict { flagged: true, category: "scam".into(), confidence: 50, reason: "x".into() };
        assert_eq!(c.action_for(&low), None);
        // not flagged => no action regardless of confidence
        let clean = AiVerdict { flagged: false, category: "none".into(), confidence: 99, reason: String::new() };
        assert_eq!(c.action_for(&clean), None);
        // the fail-open default never acts
        assert_eq!(c.action_for(&AiVerdict::allow()), None);
    }

    #[test]
    fn prefilter_skips_short_messages() {
        let mut c = cfg();
        c.min_chars = 10;
        assert!(!c.should_classify("hi")); // too short => skip the paid call
        assert!(c.should_classify("this is long enough"));
        // min_chars 0 is treated as 1 (never classify a truly empty string)
        c.min_chars = 0;
        assert!(!c.should_classify(""));
        assert!(c.should_classify("a"));
    }

    #[test]
    fn budget_cap_stops_calls() {
        let mut c = cfg();
        c.daily_call_cap = 3;
        assert!(c.within_budget(0));
        assert!(c.within_budget(2));
        assert!(!c.within_budget(3)); // cap reached
        assert!(!c.within_budget(10));
    }

    #[test]
    fn truncate_bounds_token_cost() {
        let mut c = cfg();
        c.max_chars = 5;
        assert_eq!(c.truncate_for_call("hello world"), "hello");
        assert_eq!(c.truncate_for_call("hi"), "hi"); // untouched when short
        // char-safe (doesn't split a multi-byte char)
        c.max_chars = 2;
        assert_eq!(c.truncate_for_call("héllo"), "hé");
    }

    #[test]
    fn verdict_sanitized_clamps_confidence_and_category() {
        let v = AiVerdict { flagged: true, category: "  ".into(), confidence: 250, reason: "r".into() }.sanitized();
        assert_eq!(v.confidence, 100);
        assert_eq!(v.category, "flagged");
        let clean = AiVerdict { flagged: false, category: String::new(), confidence: 0, reason: String::new() }.sanitized();
        assert_eq!(clean.category, "none");
    }

    #[test]
    fn validate_enforces_cost_guards() {
        // enabled without a model => rejected
        let mut no_model = cfg();
        no_model.model = String::new();
        assert!(no_model.validate().is_err());
        // enabled with a zero daily cap => rejected (would be unbounded spend)
        let mut no_cap = cfg();
        no_cap.daily_call_cap = 0;
        assert!(no_cap.validate().is_err());
        // confidence over 100 => rejected
        let mut bad_conf = cfg();
        bad_conf.confidence_threshold = 101;
        assert!(bad_conf.validate().is_err());
        // min > max => rejected
        let mut bad_len = cfg();
        bad_len.min_chars = 5000;
        bad_len.max_chars = 100;
        assert!(bad_len.validate().is_err());
        // a sensible enabled config passes; the disabled default passes too
        assert!(cfg().validate().is_ok());
        assert!(AiModConfig::default().validate().is_ok());
    }
}
