//! Anti-advertising link filter.
//!
//! On every human message the bot scans for links; if any link points to a
//! domain that is NOT on the admin-managed whitelist, the whole message is
//! deleted, the author's strike count goes up, and at `strike_threshold`
//! strikes the configured jail role is assigned. Config + whitelist live in a
//! single JSON config blob (slash-command-editable); strikes live in a
//! dedicated store so the count is atomic and queryable.
//!
//! This module holds the PURE, unit-testable core (config shape, domain
//! matching with `*.` wildcards, link extraction, strike-decay math). The
//! gateway wiring (delete message / DM / assign role) lives in the bot binary.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::ports::ConfigStore;

pub const CONFIG_BLOB_KEY: &str = "link_filter_config";

/// Admin-editable filter configuration (stored as one JSON blob).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkFilterConfig {
    /// Master switch. `false` => the bot skips filtering entirely.
    #[serde(default)]
    pub enabled: bool,
    /// Guild the filter applies to (snowflake as string). Empty => inactive.
    #[serde(default)]
    pub guild_id: String,
    /// Whitelisted domains. `example.com` matches the apex AND its subdomains;
    /// `*.example.com` matches subdomains only. Case-insensitive. A leading
    /// `https://`/`www.` a user pastes in is tolerated (we normalise).
    #[serde(default)]
    pub whitelist: Vec<String>,
    /// Strikes at which the jail role is assigned. Default 3.
    #[serde(default = "default_threshold")]
    pub strike_threshold: u32,
    /// Role assigned when a user reaches the threshold (snowflake). Empty =>
    /// delete + count only, no auto-jail (works before the Jail role exists).
    #[serde(default)]
    pub jail_role_id: String,
    /// Strikes with no new violation for this many days are forgiven (the
    /// count resets before the next one is recorded). `0` => never expire
    /// (permanent until an admin resets). Admin-tunable.
    #[serde(default)]
    pub decay_days: u32,
    /// DM the offender a private strike notice on each deletion. (A channel
    /// message visible to only one user is impossible for a message event —
    /// Discord only allows that as a slash-command ephemeral reply — so the
    /// "only the user sees it" notice is delivered as a DM.)
    #[serde(default = "default_true")]
    pub warn_user: bool,
    /// Channels exempt from filtering (snowflakes as strings).
    #[serde(default)]
    pub exempt_channel_ids: Vec<String>,
    /// Roles whose holders are never filtered (mods/staff). Snowflakes as
    /// strings. Bot-admins (config owners) are always exempt regardless.
    #[serde(default)]
    pub exempt_role_ids: Vec<String>,
    /// Per-(user, channel) exemptions: the listed user is unfiltered ONLY in
    /// the listed channel; every other channel still filters them normally.
    /// Finer-grained than `exempt_channel_ids` (whole channel) or
    /// `exempt_role_ids` (whole role).
    #[serde(default)]
    pub exempt_user_channels: Vec<UserChannelExempt>,
    /// Per-user strike-threshold overrides. A user listed here is jailed at
    /// their own threshold instead of the global `strike_threshold`.
    #[serde(default)]
    pub user_thresholds: Vec<UserThreshold>,
    /// Master switch for the Discord-invite sub-filter, independent of the host
    /// whitelist. When on, a Discord *server invite* (`discord.gg/CODE`,
    /// `discord.com/invite/CODE`, …) is allowed ONLY if its code is in
    /// `allowed_invite_codes` or it resolves to an allowed guild (the filter's
    /// own `guild_id` plus `allowed_guild_ids`); every other server invite is
    /// treated as advertising and deleted + struck. Default off (opt-in).
    #[serde(default)]
    pub filter_invites: bool,
    /// Fast-path allowlist of permitted Discord invite codes (the Airforce
    /// vanity + any curated partner invites). An invite whose code is listed
    /// here is allowed without a network lookup. Case-sensitive — Discord invite
    /// codes are (vanities are lowercased by Discord).
    #[serde(default)]
    pub allowed_invite_codes: Vec<String>,
    /// Guild ids (snowflakes) whose invites are permitted IN ADDITION to the
    /// filter's own `guild_id`. The gateway resolves an unknown invite code to
    /// its guild and allows it when that guild is the own guild or listed here.
    #[serde(default)]
    pub allowed_guild_ids: Vec<String>,
}

/// One (user, channel) exemption pair — see `LinkFilterConfig::exempt_user_channels`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserChannelExempt {
    pub user_id: String,
    pub channel_id: String,
}

/// A per-user strike-threshold override — see `LinkFilterConfig::user_thresholds`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserThreshold {
    pub user_id: String,
    pub threshold: u32,
}

fn default_threshold() -> u32 {
    3
}
fn default_true() -> bool {
    true
}

impl Default for LinkFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            guild_id: String::new(),
            whitelist: Vec::new(),
            strike_threshold: 3,
            jail_role_id: String::new(),
            decay_days: 0,
            warn_user: true,
            exempt_channel_ids: Vec::new(),
            exempt_role_ids: Vec::new(),
            exempt_user_channels: Vec::new(),
            user_thresholds: Vec::new(),
            filter_invites: false,
            allowed_invite_codes: Vec::new(),
            allowed_guild_ids: Vec::new(),
        }
    }
}

impl LinkFilterConfig {
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

    /// Admin-write validation. Bounds keep a typo from disabling the threshold
    /// or scheduling a year-long decay.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.guild_id.trim().is_empty() {
            return Err("guild_id is required when the filter is enabled".into());
        }
        if !(1..=20).contains(&self.strike_threshold) {
            return Err("strike_threshold must be between 1 and 20".into());
        }
        if self.decay_days > 3650 {
            return Err("decay_days must be 0 (never) .. 3650".into());
        }
        if self.whitelist.len() > 1000 {
            return Err("whitelist capped at 1000 entries".into());
        }
        if self.exempt_user_channels.len() > 1000 {
            return Err("exempt_user_channels capped at 1000 entries".into());
        }
        if self.user_thresholds.len() > 1000 {
            return Err("user_thresholds capped at 1000 entries".into());
        }
        if self.allowed_invite_codes.len() > 1000 {
            return Err("allowed_invite_codes capped at 1000 entries".into());
        }
        if self.allowed_guild_ids.len() > 1000 {
            return Err("allowed_guild_ids capped at 1000 entries".into());
        }
        for u in &self.user_thresholds {
            if !(1..=20).contains(&u.threshold) {
                return Err("per-user threshold must be between 1 and 20".into());
            }
        }
        Ok(())
    }

    /// Effective strike threshold for `user_id`: the per-user override when one
    /// is configured, otherwise the global `strike_threshold`.
    pub fn threshold_for(&self, user_id: &str) -> u32 {
        self.user_thresholds
            .iter()
            .find(|u| u.user_id == user_id)
            .map(|u| u.threshold)
            .unwrap_or(self.strike_threshold)
    }

    /// True when `user_id` is exempt from filtering specifically in `channel_id`
    /// (a per-(user, channel) exemption). Other channels still filter the user.
    pub fn is_user_channel_exempt(&self, user_id: &str, channel_id: &str) -> bool {
        self.exempt_user_channels
            .iter()
            .any(|e| e.user_id == user_id && e.channel_id == channel_id)
    }

    /// True when invite `code` is on the fast-path allowlist (case-sensitive — a
    /// Discord invite code is case-sensitive). Lets the gateway skip a network
    /// resolution for the common case (the Airforce vanity + curated partners).
    pub fn is_invite_code_allowed(&self, code: &str) -> bool {
        self.allowed_invite_codes.iter().any(|c| c == code)
    }

    /// True when an invite that resolves to `guild_id` is permitted: it is the
    /// filter's own guild (`self.guild_id`, the server the bot moderates) or an
    /// admin-listed partner in `allowed_guild_ids`. So invites to THIS server
    /// always pass with no extra config; everything else is advertising.
    pub fn is_guild_allowed(&self, guild_id: &str) -> bool {
        if guild_id.is_empty() {
            return false;
        }
        guild_id == self.guild_id || self.allowed_guild_ids.iter().any(|g| g == guild_id)
    }
}

// ── Domain matching ──────────────────────────────────────────────────────

/// Reduce a host or a pasted whitelist entry to a bare lowercase host:
/// strips a scheme, a `www.` is kept (it's part of the host), any path/port,
/// and surrounding dots/whitespace.
pub fn normalize_host(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest.to_string();
    }
    // Drop anything from the first path/query/port separator onward.
    if let Some(idx) = s.find(['/', '?', '#', ':']) {
        s.truncate(idx);
    }
    s.trim_matches('.').trim().to_string()
}

/// Does whitelist `entry` cover `host`?
/// - `*.example.com` => subdomains only (a.example.com), NOT the apex.
/// - bare `example.com` => the apex AND any subdomain (friendly default).
fn host_matches_entry(host: &str, entry: &str) -> bool {
    let host = normalize_host(host);
    let entry = normalize_host(entry);
    if host.is_empty() || entry.is_empty() {
        return false;
    }
    if let Some(base) = entry.strip_prefix("*.") {
        let base = base.trim_start_matches('.');
        !base.is_empty() && host.ends_with(&format!(".{base}"))
    } else {
        host == entry || host.ends_with(&format!(".{entry}"))
    }
}

/// True when `host` is covered by any whitelist entry.
pub fn host_is_whitelisted(host: &str, whitelist: &[String]) -> bool {
    whitelist.iter().any(|e| host_matches_entry(host, e))
}

// ── Link extraction ──────────────────────────────────────────────────────

static URL_RE: Lazy<Regex> = Lazy::new(|| {
    // scheme? + host (one-or-more dot-separated labels + 2..24-letter TLD) + path?
    Regex::new(
        r"(?i)(?P<scheme>https?://)?(?P<host>(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,24})(?P<path>[/?#]\S*)?",
    )
    .unwrap()
});

/// Recognised TLDs for the *bare* case (no scheme, no path). Keeps the filter
/// from tripping on `report.pdf`, `node.js`, `index.html`, version strings,
/// etc., while still catching the TLDs spammers actually use. A token with an
/// explicit `http(s)://` scheme or a path is always treated as a link
/// regardless of TLD. Tunable; not meant to be exhaustive of all ~1500 TLDs.
static KNOWN_TLDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "com", "net", "org", "io", "co", "me", "gg", "xyz", "app", "dev", "info", "biz", "online",
        "site", "shop", "store", "club", "live", "fun", "link", "click", "top", "vip", "ru", "de",
        "uk", "fr", "nl", "eu", "us", "ca", "au", "in", "jp", "cn", "br", "es", "it", "pl", "se",
        "no", "fi", "dk", "ch", "at", "be", "cz", "pt", "gr", "tr", "ua", "kr", "tv", "cc", "ws",
        "to", "ly", "sh", "gl", "im", "pw", "nu", "ai", "so", "st", "media", "news", "blog",
        "space", "website", "pro", "mobi", "name", "tech", "cloud", "email", "tk", "ml", "ga",
        "cf", "gq", "su", "work", "life", "world", "page", "gdn", "rest", "monster", "sbs", "cyou",
        "lol", "bond", "icu", "buzz", "fit", "rocks", "wtf", "gift", "discord",
    ]
    .into_iter()
    .collect()
});

/// Distinct link hosts in `text` that count as real links. A token counts when
/// it has an explicit scheme, a path, OR a recognised TLD.
pub fn extract_link_hosts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for caps in URL_RE.captures_iter(text) {
        let host = normalize_host(&caps["host"]);
        if host.is_empty() {
            continue;
        }
        let has_scheme = caps.name("scheme").is_some();
        let has_path = caps.name("path").is_some();
        let tld = host.rsplit('.').next().unwrap_or("");
        if has_scheme || has_path || KNOWN_TLDS.contains(tld) {
            if !out.iter().any(|h| h == &host) {
                out.push(host);
            }
        }
    }
    out
}

/// The link hosts in `text` that are NOT whitelisted. Non-empty => the message
/// is an ad/violation and should be deleted + counted.
pub fn offending_hosts(text: &str, whitelist: &[String]) -> Vec<String> {
    extract_link_hosts(text)
        .into_iter()
        .filter(|h| !host_is_whitelisted(h, whitelist))
        .collect()
}

// ── Discord invite detection ──────────────────────────────────────────────

static INVITE_RE: Lazy<Regex> = Lazy::new(|| {
    // optional scheme + optional www./ptb./canary. + a word boundary (so a
    // look-alike host like `mydiscord.gg` is NOT matched) + either
    // `discord.gg/CODE` or `discord(app).com/invite/CODE`. The match is
    // case-insensitive but the captured CODE keeps its original case (invite
    // codes are case-sensitive). Two capture groups — the code is whichever
    // branch matched.
    Regex::new(
        r"(?i)(?:https?://)?(?:(?:www|ptb|canary)\.)?\b(?:discord\.gg/([A-Za-z0-9-]{1,64})|discord(?:app)?\.com/invite/([A-Za-z0-9-]{1,64}))",
    )
    .unwrap()
});

/// Distinct Discord **server-invite codes** mentioned in `text`. Recognises
/// `discord.gg/CODE`, `discord.com/invite/CODE`, `discordapp.com/invite/CODE`
/// (with or without a scheme / `www.` / `ptb.` / `canary.` — Discord renders the
/// bare form as a clickable invite too). The invite CODE is case-sensitive so
/// its original case is preserved. Order-preserving and de-duplicated.
///
/// Pure detection only — deciding which codes are allowed (the fast-path
/// allowlist + guild resolution) is the gateway's job; see
/// [`LinkFilterConfig::is_invite_code_allowed`] / [`LinkFilterConfig::is_guild_allowed`].
pub fn extract_discord_invites(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for caps in INVITE_RE.captures_iter(text) {
        if let Some(code) = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str()) {
            if !code.is_empty() && !out.iter().any(|c| c == code) {
                out.push(code.to_string());
            }
        }
    }
    out
}

// ── Strike decay math ────────────────────────────────────────────────────

/// Given the time of the last recorded strike, `now`, and the decay window,
/// decide whether the prior strike streak has lapsed and should reset to 0
/// before recording the new one. `decay_days == 0` => never lapses.
pub fn strikes_lapsed(last_strike_unix: i64, now_unix: i64, decay_days: u32) -> bool {
    if decay_days == 0 {
        return false;
    }
    let window = decay_days as i64 * 86_400;
    now_unix.saturating_sub(last_strike_unix) > window
}

/// New strike count after recording one violation now, applying decay.
pub fn next_strike_count(prev_count: u32, last_strike_unix: i64, now_unix: i64, decay_days: u32) -> u32 {
    if strikes_lapsed(last_strike_unix, now_unix, decay_days) {
        1
    } else {
        prev_count.saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_entry_matches_apex_and_subdomains() {
        let w = wl(&["example.com"]);
        assert!(host_is_whitelisted("example.com", &w));
        assert!(host_is_whitelisted("www.example.com", &w));
        assert!(host_is_whitelisted("a.b.example.com", &w));
        assert!(!host_is_whitelisted("notexample.com", &w));
        assert!(!host_is_whitelisted("example.com.evil.com", &w));
    }

    #[test]
    fn wildcard_entry_matches_subdomains_only() {
        let w = wl(&["*.example.com"]);
        assert!(host_is_whitelisted("cdn.example.com", &w));
        assert!(host_is_whitelisted("a.b.example.com", &w));
        // apex is NOT covered by a subdomain-only wildcard
        assert!(!host_is_whitelisted("example.com", &w));
    }

    #[test]
    fn whitelist_tolerates_pasted_scheme_and_path() {
        let w = wl(&["https://discord.com/"]);
        assert!(host_is_whitelisted("discord.com", &w));
        assert!(host_is_whitelisted("ptb.discord.com", &w));
    }

    #[test]
    fn extracts_scheme_bare_and_path_links() {
        let hosts = extract_link_hosts("check https://Evil.Site/free and discord.gg/abc plus Foo.COM");
        assert!(hosts.contains(&"evil.site".to_string()));
        assert!(hosts.contains(&"discord.gg".to_string()));
        assert!(hosts.contains(&"foo.com".to_string()));
    }

    #[test]
    fn ignores_non_link_dotted_tokens() {
        // file names, versions, code refs — no scheme, no path, unknown TLD
        let hosts = extract_link_hosts("see report.pdf and node.js v1.20 and index.html");
        assert!(hosts.is_empty(), "got {hosts:?}");
    }

    #[test]
    fn path_makes_unknown_tld_count() {
        // a path signals a real link even with an oddball TLD
        let hosts = extract_link_hosts("grab it at freestuff.weirdtld/now");
        assert_eq!(hosts, vec!["freestuff.weirdtld".to_string()]);
    }

    #[test]
    fn offending_filters_out_whitelisted() {
        let w = wl(&["example.com", "*.cdn.net"]);
        let off = offending_hosts(
            "ok https://example.com/x and asset.cdn.net but spam at promo.xyz/win",
            &w,
        );
        assert_eq!(off, vec!["promo.xyz".to_string()]);
    }

    #[test]
    fn clean_message_has_no_offenders() {
        assert!(offending_hosts("hello everyone, no links here!", &wl(&[])).is_empty());
    }

    #[test]
    fn decay_zero_never_lapses() {
        assert!(!strikes_lapsed(0, 999_999_999, 0));
        assert_eq!(next_strike_count(2, 0, 999_999_999, 0), 3);
    }

    #[test]
    fn decay_window_resets_old_streak() {
        let now = 1_000_000_000;
        let day = 86_400;
        // last strike 31 days ago, 30-day window => lapsed => resets to 1
        assert!(strikes_lapsed(now - 31 * day, now, 30));
        assert_eq!(next_strike_count(2, now - 31 * day, now, 30), 1);
        // last strike 10 days ago => still counts => increments
        assert!(!strikes_lapsed(now - 10 * day, now, 30));
        assert_eq!(next_strike_count(2, now - 10 * day, now, 30), 3);
    }

    #[test]
    fn per_user_threshold_override() {
        let cfg = LinkFilterConfig {
            user_thresholds: vec![UserThreshold { user_id: "111".into(), threshold: 10 }],
            ..Default::default()
        };
        assert_eq!(cfg.threshold_for("111"), 10); // override
        assert_eq!(cfg.threshold_for("222"), 3); // falls back to global default (3)
    }

    #[test]
    fn per_user_channel_exemption_is_exact_pair() {
        let cfg = LinkFilterConfig {
            exempt_user_channels: vec![UserChannelExempt {
                user_id: "111".into(),
                channel_id: "c1".into(),
            }],
            ..Default::default()
        };
        assert!(cfg.is_user_channel_exempt("111", "c1")); // exact pair => exempt
        assert!(!cfg.is_user_channel_exempt("111", "c2")); // same user, other channel => filtered
        assert!(!cfg.is_user_channel_exempt("222", "c1")); // other user, same channel => filtered
    }

    #[test]
    fn validate_bounds_per_user_threshold() {
        let bad_low = LinkFilterConfig {
            user_thresholds: vec![UserThreshold { user_id: "1".into(), threshold: 0 }],
            ..Default::default()
        };
        assert!(bad_low.validate().is_err());
        let bad_high = LinkFilterConfig {
            user_thresholds: vec![UserThreshold { user_id: "1".into(), threshold: 21 }],
            ..Default::default()
        };
        assert!(bad_high.validate().is_err());
        let ok = LinkFilterConfig {
            user_thresholds: vec![UserThreshold { user_id: "1".into(), threshold: 5 }],
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn legacy_config_loads_and_new_fields_roundtrip() {
        // A blob written before these fields existed must still deserialize.
        let legacy = r#"{"enabled":true,"guild_id":"1","strike_threshold":3}"#;
        let cfg: LinkFilterConfig = serde_json::from_str(legacy).unwrap();
        assert!(cfg.exempt_user_channels.is_empty());
        assert!(cfg.user_thresholds.is_empty());
        // And the new fields survive a round-trip.
        let populated = LinkFilterConfig {
            exempt_user_channels: vec![UserChannelExempt { user_id: "1".into(), channel_id: "2".into() }],
            user_thresholds: vec![UserThreshold { user_id: "1".into(), threshold: 7 }],
            ..Default::default()
        };
        let s = serde_json::to_string(&populated).unwrap();
        let back: LinkFilterConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(populated, back);
    }

    // ── Discord invite detection ──────────────────────────────────────────

    #[test]
    fn extracts_discord_invites_all_forms() {
        let codes = extract_discord_invites(
            "join https://discord.gg/AbCdEf and discord.com/invite/xyz123 plus \
             www.discordapp.com/invite/Foo-Bar and bare discord.gg/airforce",
        );
        assert!(codes.contains(&"AbCdEf".to_string()), "{codes:?}");
        assert!(codes.contains(&"xyz123".to_string()), "{codes:?}");
        assert!(codes.contains(&"Foo-Bar".to_string()), "{codes:?}");
        assert!(codes.contains(&"airforce".to_string()), "{codes:?}");
    }

    #[test]
    fn invite_code_case_is_preserved() {
        // Discord invite codes are case-sensitive — must NOT be lowercased.
        assert_eq!(extract_discord_invites("discord.gg/HeLLo"), vec!["HeLLo".to_string()]);
    }

    #[test]
    fn invite_extraction_dedups_and_orders() {
        let codes = extract_discord_invites("discord.gg/a then discord.gg/a then discord.gg/b");
        assert_eq!(codes, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn ptb_and_canary_invites_match() {
        assert_eq!(extract_discord_invites("ptb.discord.com/invite/zzz"), vec!["zzz".to_string()]);
        assert_eq!(extract_discord_invites("canary.discord.com/invite/qqq"), vec!["qqq".to_string()]);
    }

    #[test]
    fn non_invites_and_lookalikes_are_ignored() {
        // channel link, not an invite
        assert!(extract_discord_invites("discord.com/channels/123/456").is_empty());
        // look-alike hosts must NOT match (word-boundary guard)
        assert!(extract_discord_invites("mydiscord.gg/scam").is_empty());
        assert!(extract_discord_invites("notreallydiscord.com/invite/scam").is_empty());
        // discord.com without /invite/ is not an invite
        assert!(extract_discord_invites("discord.com/blog/x").is_empty());
    }

    #[test]
    fn invite_trailing_punctuation_stops_the_code() {
        assert_eq!(extract_discord_invites("come to discord.gg/abc!"), vec!["abc".to_string()]);
        assert_eq!(extract_discord_invites("discord.gg/abc?ref=1"), vec!["abc".to_string()]);
    }

    #[test]
    fn invite_code_allowlist_is_case_sensitive() {
        let cfg = LinkFilterConfig {
            allowed_invite_codes: vec!["airforce".into(), "Partner1".into()],
            ..Default::default()
        };
        assert!(cfg.is_invite_code_allowed("airforce"));
        assert!(cfg.is_invite_code_allowed("Partner1"));
        assert!(!cfg.is_invite_code_allowed("AirForce")); // case-sensitive
        assert!(!cfg.is_invite_code_allowed("other"));
    }

    #[test]
    fn guild_allowlist_covers_own_guild_and_partners() {
        let cfg = LinkFilterConfig {
            guild_id: "111".into(),                // the bot's own (Airforce) guild
            allowed_guild_ids: vec!["222".into()], // a curated partner server
            ..Default::default()
        };
        assert!(cfg.is_guild_allowed("111")); // own guild always allowed
        assert!(cfg.is_guild_allowed("222")); // listed partner
        assert!(!cfg.is_guild_allowed("333")); // any other server = advertising
        assert!(!cfg.is_guild_allowed("")); // empty = not allowed
    }

    #[test]
    fn legacy_config_loads_without_invite_fields() {
        // A blob written before the invite fields existed must still deserialize.
        let legacy = r#"{"enabled":true,"guild_id":"1","strike_threshold":3}"#;
        let cfg: LinkFilterConfig = serde_json::from_str(legacy).unwrap();
        assert!(!cfg.filter_invites);
        assert!(cfg.allowed_invite_codes.is_empty());
        assert!(cfg.allowed_guild_ids.is_empty());
    }
}
