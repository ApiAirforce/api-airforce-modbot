//! Content automod — the pure, unit-tested rule engine.
//!
//! Scans message *content* for the spam/abuse patterns the link/flood filters
//! don't cover: a word/regex **blocklist**, **ALL-CAPS**, **mention spam**,
//! **emoji spam**, **zalgo** (combining-mark soup), and **duplicate** content
//! (the same text repeated). Each rule is a pure function over the raw message
//! string (+ a few host-supplied booleans for exemptions), so the whole engine
//! is testable with no Discord and no database.
//!
//! The stateless rules live in [`AutomodConfig::evaluate`]; the one stateful rule
//! (duplicate-in-a-window) is the in-memory [`DuplicateTracker`], mirroring the
//! flood filter's sliding-window shape. AI moderation (a later plan) layers on at
//! the host as an extra check feeding the same [`AutomodVerdict`] → action path,
//! so this engine needs no change to gain it.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::link_filter::UserChannelExempt;
use crate::ports::ConfigStore;

/// Config-blob key for the content automod (sibling of the other `*_config`s).
pub const CONFIG_BLOB_KEY: &str = "automod_config";

/// What to do when a message trips a content rule. Serialized lowercase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutomodAction {
    /// Delete the message + DM the user. No strike.
    Warn,
    /// Delete + record a strike (DM gated by `warn_user`). The default.
    Delete,
    /// Delete + strike + native timeout for `timeout_minutes`.
    Timeout,
    /// Delete + strike + the escape-proof jail.
    Jail,
}

/// How blocklist patterns are matched against the (optionally lowercased) text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Pattern occurs anywhere as a substring.
    Substring,
    /// Pattern occurs as a whole word (ASCII word boundaries).
    Word,
    /// Pattern is a regular expression (`regex` crate — linear time, no
    /// catastrophic backtracking).
    Regex,
}

fn d_action() -> AutomodAction {
    AutomodAction::Delete
}
fn d_match_mode() -> MatchMode {
    MatchMode::Word
}
fn d_true() -> bool {
    true
}
fn d_timeout_minutes() -> u32 {
    10
}
fn d_dup_window() -> u32 {
    30
}

/// Admin-editable content-automod configuration. Every threshold of `0` disables
/// that one rule, so an admin enables exactly the checks they want. `#[serde(default)]`
/// on each field keeps an older/partial blob loadable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomodConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub guild_id: String,
    #[serde(default = "d_action")]
    pub action: AutomodAction,
    #[serde(default = "d_timeout_minutes")]
    pub timeout_minutes: u32,
    #[serde(default = "d_true")]
    pub warn_user: bool,

    // ── blocklist ──
    /// Banned words / patterns. Empty => the blocklist rule is off.
    #[serde(default)]
    pub blocklist: Vec<String>,
    #[serde(default = "d_match_mode")]
    pub match_mode: MatchMode,
    #[serde(default = "d_true")]
    pub case_insensitive: bool,

    // ── caps ──
    /// Trip when the share of UPPERCASE among letters is >= this percent
    /// (`0` => off, else 1..=100). Only messages with >= `min_caps_letters`
    /// letters are checked (short shouts are ignored).
    #[serde(default)]
    pub max_caps_ratio: u8,
    #[serde(default)]
    pub min_caps_letters: u32,

    // ── mention spam ──
    /// Trip at this many user/role mentions (+ `@everyone`/`@here`). `0` => off.
    #[serde(default)]
    pub max_mentions: u32,

    // ── emoji spam ──
    /// Trip at this many emoji (custom `<:name:id>` + unicode). `0` => off.
    #[serde(default)]
    pub max_emojis: u32,

    // ── zalgo ──
    /// Trip when combining marks are >= this percent of all chars (`0` => off).
    #[serde(default)]
    pub max_zalgo_ratio: u8,

    // ── duplicate content ──
    /// Trip when the same normalized message is sent this many times within
    /// `duplicate_window_secs` (`0` => off; needs the [`DuplicateTracker`]).
    #[serde(default)]
    pub duplicate_threshold: u32,
    #[serde(default = "d_dup_window")]
    pub duplicate_window_secs: u32,

    // ── exemptions (mirrors the link/flood filter surface) ──
    #[serde(default)]
    pub exempt_channel_ids: Vec<String>,
    #[serde(default)]
    pub exempt_role_ids: Vec<String>,
    #[serde(default)]
    pub exempt_user_channels: Vec<UserChannelExempt>,
}

impl Default for AutomodConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            action: d_action(),
            timeout_minutes: d_timeout_minutes(),
            warn_user: true,
            blocklist: Vec::new(),
            match_mode: d_match_mode(),
            case_insensitive: true,
            max_caps_ratio: 0,
            min_caps_letters: 0,
            max_mentions: 0,
            max_emojis: 0,
            max_zalgo_ratio: 0,
            duplicate_threshold: 0,
            duplicate_window_secs: d_dup_window(),
            exempt_channel_ids: Vec::new(),
            exempt_role_ids: Vec::new(),
            exempt_user_channels: Vec::new(),
        }
    }
}

/// The outcome of a content rule firing: which rule + a human reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodVerdict {
    pub rule: &'static str,
    pub reason: String,
}

/// Compiled-program size cap for a blocklist regex. Bounds BOTH compile time and
/// match memory, so a pathological pattern (e.g. `(\p{L}\p{M}*){50}`) fails to
/// build in microseconds instead of hanging the gateway — defence-in-depth on top
/// of caching the compiled regex so it is never rebuilt per message.
const REGEX_SIZE_LIMIT: usize = 1 << 20; // 1 MiB

/// Build one blocklist regex with the size cap + case flag (None if it does not
/// compile or exceeds the size limit — the caller fails closed).
fn build_blocklist_regex(pattern: &str, case_insensitive: bool) -> Option<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .ok()
}

/// Pre-compiled blocklist matchers, built **once** from a config and reused for
/// every message. Compiling a regex per message is a CPU-DoS, so the host builds
/// this when the config changes and passes it to [`AutomodConfig::evaluate`].
/// Empty unless `match_mode == Regex` (Substring/Word need no compilation).
#[derive(Default)]
pub struct CompiledBlocklist {
    /// `(original_pattern, compiled)` — the pattern is kept for the verdict reason.
    regexes: Vec<(String, regex::Regex)>,
}

impl CompiledBlocklist {
    /// Compile the regex-mode blocklist once (size-capped; a pattern that fails to
    /// build is skipped — it is also rejected at [`AutomodConfig::validate`]).
    pub fn build(cfg: &AutomodConfig) -> Self {
        if cfg.match_mode != MatchMode::Regex {
            return Self::default();
        }
        let regexes = cfg
            .blocklist
            .iter()
            .filter_map(|p| build_blocklist_regex(p, cfg.case_insensitive).map(|re| (p.clone(), re)))
            .collect();
        Self { regexes }
    }
}

impl AutomodConfig {
    /// Load from the config-blob store; missing/corrupt => disabled defaults.
    pub fn load_for_guild(store: &impl ConfigStore, guild_id: &str) -> Self {
        let mut cfg: Self = store
            .get_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        cfg.guild_id = guild_id.to_string();
        cfg
    }

    /// Persist this config back to the store (guild-scoped).
    pub fn save_for_guild(&self, store: &impl ConfigStore, guild_id: &str) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        store.set_config_blob(&crate::guild_blob_key(guild_id, CONFIG_BLOB_KEY), &json)
    }

    /// Admin-write validation. Bounds keep a typo from nuking every message, and
    /// reject a blocklist regex that does not compile.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.guild_id.trim().is_empty() {
            return Err("guild_id is required when automod is enabled".into());
        }
        if self.max_caps_ratio > 100 {
            return Err("max_caps_ratio must be 0 (off) .. 100".into());
        }
        if self.max_zalgo_ratio > 100 {
            return Err("max_zalgo_ratio must be 0 (off) .. 100".into());
        }
        if self.action == AutomodAction::Timeout && !(1..=40_320).contains(&self.timeout_minutes) {
            return Err("timeout_minutes must be 1 .. 40320 (Discord's 28-day cap)".into());
        }
        if self.blocklist.len() > 1000 {
            return Err("blocklist capped at 1000 entries".into());
        }
        for p in &self.blocklist {
            if p.len() > 200 {
                return Err("a blocklist pattern is too long (max 200 chars)".into());
            }
            if self.match_mode == MatchMode::Regex && build_blocklist_regex(p, self.case_insensitive).is_none() {
                return Err(format!("blocklist regex does not compile or is too large: {p}"));
            }
        }
        if !(1..=3600).contains(&self.duplicate_window_secs) {
            return Err("duplicate_window_secs must be between 1 and 3600".into());
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

    /// Run every enabled **stateless** rule against `content`, returning the first
    /// rule that trips. Duplicate-content is evaluated separately via the
    /// [`DuplicateTracker`] (it needs state). Pure: the host has already applied
    /// the channel/role/user exemptions before calling this.
    pub fn evaluate(&self, content: &str, compiled: &CompiledBlocklist) -> Option<AutomodVerdict> {
        // 1) Blocklist.
        if let Some(hit) = self.blocklist_hit(content, compiled) {
            return Some(AutomodVerdict {
                rule: "blocklist",
                reason: format!("blocked term: {hit}"),
            });
        }
        // 2) Mention spam.
        if self.max_mentions > 0 {
            let n = mention_count(content);
            if n >= self.max_mentions {
                return Some(AutomodVerdict {
                    rule: "mentions",
                    reason: format!("{n} mentions (limit {})", self.max_mentions),
                });
            }
        }
        // 3) Emoji spam.
        if self.max_emojis > 0 {
            let n = emoji_count(content);
            if n >= self.max_emojis {
                return Some(AutomodVerdict {
                    rule: "emoji",
                    reason: format!("{n} emoji (limit {})", self.max_emojis),
                });
            }
        }
        // 4) Caps.
        if self.max_caps_ratio > 0 {
            let (letters, ratio) = caps_ratio(content);
            if letters >= self.min_caps_letters && ratio >= self.max_caps_ratio {
                return Some(AutomodVerdict {
                    rule: "caps",
                    reason: format!("{ratio}% caps (limit {}%)", self.max_caps_ratio),
                });
            }
        }
        // 5) Zalgo.
        if self.max_zalgo_ratio > 0 {
            let ratio = zalgo_ratio(content);
            if ratio >= self.max_zalgo_ratio {
                return Some(AutomodVerdict {
                    rule: "zalgo",
                    reason: format!("{ratio}% combining marks (limit {}%)", self.max_zalgo_ratio),
                });
            }
        }
        None
    }

    /// The first blocklist pattern that matches `content`, if any. Substring/Word
    /// case-fold by lowercasing both sides; Regex uses the **pre-compiled**
    /// matchers in `compiled` (the host compiles them once on config change —
    /// compiling per message is a CPU-DoS).
    fn blocklist_hit(&self, content: &str, compiled: &CompiledBlocklist) -> Option<String> {
        if self.blocklist.is_empty() {
            return None;
        }
        match self.match_mode {
            MatchMode::Substring | MatchMode::Word => {
                let hay = if self.case_insensitive { content.to_lowercase() } else { content.to_string() };
                for pat in &self.blocklist {
                    let needle = if self.case_insensitive { pat.to_lowercase() } else { pat.clone() };
                    let hit = match self.match_mode {
                        MatchMode::Word => contains_word(&hay, &needle),
                        _ => hay.contains(&needle),
                    };
                    if hit {
                        return Some(pat.clone());
                    }
                }
                None
            }
            MatchMode::Regex => compiled
                .regexes
                .iter()
                .find(|(_, re)| re.is_match(content))
                .map(|(pat, _)| pat.clone()),
        }
    }
}

// ── pure counting helpers ────────────────────────────────────────────────────

/// `(letter_count, percent_uppercase)` among ASCII letters in `s` (0/0 if none).
pub fn caps_ratio(s: &str) -> (u32, u8) {
    let mut letters = 0u32;
    let mut upper = 0u32;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            letters += 1;
            if c.is_ascii_uppercase() {
                upper += 1;
            }
        }
    }
    if letters == 0 {
        return (0, 0);
    }
    (letters, ((upper * 100) / letters) as u8)
}

/// Count of distinct mention tokens in `s`: `<@id>` / `<@!id>` (user),
/// `<@&id>` (role), plus each `@everyone` / `@here`.
pub fn mention_count(s: &str) -> u32 {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@[!&]?\d+>").unwrap());
    let tagged = RE.find_iter(s).count() as u32;
    let everyone = s.matches("@everyone").count() as u32;
    let here = s.matches("@here").count() as u32;
    tagged + everyone + here
}

/// Count of emoji in `s`: custom `<:name:id>` / `<a:name:id>` plus unicode emoji
/// (a pragmatic codepoint-range check covering the common pictographic blocks).
pub fn emoji_count(s: &str) -> u32 {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<a?:\w{2,32}:\d+>").unwrap());
    let custom = RE.find_iter(s).count() as u32;
    // Strip custom-emoji tokens so their ascii digits aren't recounted, then scan
    // for unicode pictographs.
    let stripped = RE.replace_all(s, "");
    let unicode = stripped.chars().filter(|&c| is_emoji_char(c)).count() as u32;
    custom + unicode
}

/// A pragmatic "is this codepoint a pictographic emoji" check (not exhaustive of
/// the full Unicode emoji set, but covers the blocks spammers actually use).
fn is_emoji_char(c: char) -> bool {
    let u = c as u32;
    (0x1F300..=0x1FAFF).contains(&u)   // misc symbols & pictographs … symbols ext-A
        || (0x2600..=0x27BF).contains(&u) // misc symbols + dingbats
        || (0x1F000..=0x1F0FF).contains(&u) // mahjong/dominoes/cards
        || u == 0x2764 // heart
}

/// Percent of `s`'s chars that are Unicode combining marks (the zalgo signal).
pub fn zalgo_ratio(s: &str) -> u8 {
    let mut total = 0u32;
    let mut combining = 0u32;
    for c in s.chars() {
        total += 1;
        if is_combining_mark(c) {
            combining += 1;
        }
    }
    if total == 0 {
        return 0;
    }
    ((combining * 100) / total) as u8
}

/// The common Unicode combining-diacritic ranges used to build zalgo text.
fn is_combining_mark(c: char) -> bool {
    let u = c as u32;
    (0x0300..=0x036F).contains(&u)   // combining diacritical marks
        || (0x1AB0..=0x1AFF).contains(&u) // … extended
        || (0x1DC0..=0x1DFF).contains(&u) // … supplement
        || (0x20D0..=0x20FF).contains(&u) // … for symbols
        || (0xFE20..=0xFE2F).contains(&u) // combining half marks
}

/// True when `needle` occurs in `hay` bounded by non-alphanumeric edges (a whole
/// word, so `ass` does not match `class`). Both should already be same-cased.
fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end == hay.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ── duplicate-content tracker (the one stateful rule) ────────────────────────

#[derive(Debug, Clone)]
struct DupEvent {
    norm: String,
    at_ms: u64,
}

/// In-memory per-user window of recent (normalized) messages. Trips when the
/// same text recurs `threshold` times inside the window. Memory is bounded: a
/// user's entry is dropped on a trip, and an opportunistic sweep (past a size
/// cap) evicts users whose whole window has aged out — so the map stays
/// proportional to recently-active posters. Not serialized; a restart starts fresh.
#[derive(Default)]
pub struct DuplicateTracker {
    by_user: HashMap<String, VecDeque<DupEvent>>,
}

impl DuplicateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `content` for `user_key` at `now_ms` and report whether it is the
    /// `threshold`-th identical message within `window_secs`. `now_ms` is a
    /// monotonic clock (tests pass synthetic values); `user_key` is whatever the
    /// host scopes by (e.g. `"{guild}:{user}"`).
    pub fn record_and_check(
        &mut self,
        user_key: &str,
        content: &str,
        now_ms: u64,
        threshold: u32,
        window_secs: u32,
    ) -> bool {
        if threshold < 2 {
            return false;
        }
        let norm = normalize(content);
        if norm.is_empty() {
            return false;
        }
        let horizon = (window_secs as u64).saturating_mul(1000);
        let cutoff = now_ms.saturating_sub(horizon);

        // Opportunistic memory reclaim: past a size cap, drop users whose whole
        // window has aged out (one-shot posters that never tripped). Runs only
        // when the map is large, so it stays amortized-cheap.
        if self.by_user.len() > 4096 {
            self.by_user.retain(|_, d| d.back().is_some_and(|e| e.at_ms >= cutoff));
        }

        let dq = self.by_user.entry(user_key.to_string()).or_default();
        while dq.front().is_some_and(|e| e.at_ms < cutoff) {
            dq.pop_front();
        }
        dq.push_back(DupEvent { norm: norm.clone(), at_ms: now_ms });
        let same = dq.iter().filter(|e| e.norm == norm).count() as u32;
        if same >= threshold {
            // Clear so the same burst isn't re-reported on the next message.
            self.by_user.remove(user_key);
            return true;
        }
        false
    }

    /// Forget a user's window.
    pub fn clear_user(&mut self, user_key: &str) {
        self.by_user.remove(user_key);
    }

    /// Number of users currently tracked (diagnostics/tests).
    pub fn tracked_users(&self) -> usize {
        self.by_user.len()
    }
}

/// Normalize text for duplicate comparison: trimmed, lowercased, inner runs of
/// whitespace collapsed to one space.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AutomodConfig {
        AutomodConfig { enabled: true, guild_id: "g".into(), ..Default::default() }
    }

    /// Evaluate with a freshly-built compiled blocklist (the host caches it; tests
    /// just rebuild each call).
    fn eval(ac: &AutomodConfig, s: &str) -> Option<AutomodVerdict> {
        ac.evaluate(s, &CompiledBlocklist::build(ac))
    }

    #[test]
    fn blocklist_word_mode_is_whole_word() {
        let mut c = cfg();
        c.blocklist = vec!["ass".into()];
        c.match_mode = MatchMode::Word;
        assert!(eval(&c,"you ass").is_some());
        assert!(eval(&c,"ASS!").is_some()); // case-insensitive + punctuation boundary
        assert!(eval(&c,"classic glass").is_none()); // not a whole word
    }

    #[test]
    fn blocklist_substring_and_regex_modes() {
        let mut sub = cfg();
        sub.blocklist = vec!["spam".into()];
        sub.match_mode = MatchMode::Substring;
        assert!(eval(&sub,"antispambot").is_some());

        let mut re = cfg();
        re.blocklist = vec![r"https?://\S*\.ru".into()];
        re.match_mode = MatchMode::Regex;
        assert!(eval(&re,"visit http://x.ru now").is_some());
        assert!(eval(&re,"visit example.com").is_none());
    }

    #[test]
    fn caps_rule_respects_min_length() {
        let mut c = cfg();
        c.max_caps_ratio = 70;
        c.min_caps_letters = 6;
        assert!(eval(&c,"STOP RIGHT THERE").is_some()); // long + ~100% caps
        assert!(eval(&c,"OK").is_none()); // below min letters => ignored
        assert!(eval(&c,"hello there friend").is_none()); // lowercase
    }

    #[test]
    fn mention_and_emoji_counts() {
        assert_eq!(mention_count("<@123> hi <@!456> <@&789> @everyone @here"), 5);
        assert_eq!(emoji_count("<:wave:111> hey 🎉🔥"), 3);
        let mut c = cfg();
        c.max_mentions = 3;
        assert!(eval(&c,"<@1><@2><@3>").is_some());
        assert!(eval(&c,"<@1><@2>").is_none());
    }

    #[test]
    fn zalgo_rule_trips_on_combining_soup() {
        let mut c = cfg();
        c.max_zalgo_ratio = 40;
        let zalgo = "h\u{0301}\u{0300}\u{0489}e\u{0301}\u{0300}l\u{0301}\u{0300}l\u{0301}o\u{0301}";
        assert!(eval(&c,zalgo).is_some());
        assert!(eval(&c,"hello").is_none());
    }

    #[test]
    fn evaluate_is_none_when_all_rules_off() {
        assert!(eval(&cfg(),"WHATEVER http://x.ru 🎉🎉🎉 <@1><@2>").is_none());
    }

    #[test]
    fn validate_rejects_bad_regex_and_bounds() {
        let mut c = cfg();
        c.match_mode = MatchMode::Regex;
        c.blocklist = vec!["(".into()]; // unbalanced => won't compile
        assert!(c.validate().is_err());
        let mut c2 = cfg();
        c2.max_caps_ratio = 200;
        assert!(c2.validate().is_err());
        assert!(cfg().validate().is_ok());
    }

    #[test]
    fn duplicate_tracker_trips_on_repeat_in_window() {
        let mut t = DuplicateTracker::new();
        // threshold 3, 30s window
        assert!(!t.record_and_check("g:u", "hi there", 0, 3, 30));
        assert!(!t.record_and_check("g:u", "HI   there", 1000, 3, 30)); // normalizes equal
        let tripped = t.record_and_check("g:u", "hi there", 2000, 3, 30);
        assert!(tripped);
        assert_eq!(t.tracked_users(), 0); // cleared after a trip
    }

    #[test]
    fn duplicate_tracker_ignores_old_and_distinct() {
        let mut t = DuplicateTracker::new();
        assert!(!t.record_and_check("g:u", "a", 0, 2, 10));
        // 11s later the first "a" is outside the 10s window => not a repeat
        assert!(!t.record_and_check("g:u", "a", 11_000, 2, 10));
        // distinct messages never trip
        assert!(!t.record_and_check("g:v", "x", 0, 2, 10));
        assert!(!t.record_and_check("g:v", "y", 100, 2, 10));
    }
}
