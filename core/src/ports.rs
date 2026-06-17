//! Storage/config **ports** the moderation core needs. A host implements these
//! over whatever backend it likes (the bundled bot uses an embedded `redb`
//! store); the core logic never names a concrete database.

use serde::{Deserialize, Serialize};

// ── Data types (store-agnostic) ──────────────────────────────────────────────

/// A Discord user's current anti-ad strike standing. One per offending user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkStrike {
    pub discord_user_id: String,
    pub guild_id: String,
    pub count: u32,
    pub first_strike_unix: i64,
    pub last_strike_unix: i64,
    pub last_reason: Option<String>,
}

/// A jail record: a currently-jailed member plus the snapshot of roles stripped
/// from them (restored on unjail). Survives bot restart and member leave/rejoin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JailRecord {
    pub discord_user_id: String,
    pub guild_id: String,
    /// Role ids (as strings) the member held before being jailed; restored on
    /// unjail. Excludes `@everyone` (which Discord never lists on a member).
    pub prior_roles: Vec<String>,
    pub reason: Option<String>,
    pub jailed_by: String,
    pub jailed_at_unix: i64,
    /// `None` => indefinite (manual unjail only).
    pub expires_at_unix: Option<i64>,
}

// ── Ports ────────────────────────────────────────────────────────────────────

/// Read/write access to the core's JSON config blobs (the link-filter config
/// and the jail config live one-blob-each, keyed by a stable string). Runtime-
/// editable via the bot's slash commands; persisted so a restart keeps them.
pub trait ConfigStore {
    /// The config blob (JSON string) for `key`, or `None` if unset.
    fn get_config_blob(&self, key: &str) -> Option<String>;
    /// Persist `value_json` under `key` (insert-or-replace).
    fn set_config_blob(&self, key: &str, value_json: &str) -> Result<(), String>;
}

/// Persistence for anti-ad strike counts. `record_link_strike` MUST perform the
/// decay-aware read-modify-write atomically so two rapid repeat violations from
/// the same user can never lose an increment.
pub trait StrikeStore {
    /// Record one link violation, applying decay (`decay_days == 0` => never
    /// expires). Returns the NEW count so the caller can decide whether the jail
    /// threshold is reached.
    fn record_link_strike(
        &self,
        discord_user_id: &str,
        guild_id: &str,
        reason: &str,
        now_unix: i64,
        decay_days: u32,
    ) -> Result<u32, String>;

    /// Most-recent strike rows, newest first (admin list).
    fn list_link_strikes(&self, limit: u32) -> Vec<LinkStrike>;

    /// Clear a user's strikes (admin "forgive"). Idempotent.
    fn reset_link_strikes(&self, discord_user_id: &str) -> Result<(), String>;
}

/// Persistence for the "real jail": who is jailed and the role snapshot to
/// restore on release.
pub trait JailStore {
    /// Insert/replace a jail record. Re-jailing an already-jailed user MUST
    /// refresh the sentence but KEEP the original `prior_roles` snapshot.
    fn record_jail(
        &self,
        discord_user_id: &str,
        guild_id: &str,
        prior_roles: &[String],
        reason: &str,
        jailed_by: &str,
        jailed_at_unix: i64,
        expires_at_unix: Option<i64>,
    ) -> Result<(), String>;

    /// The active jail record for a user, if any.
    fn get_jail(&self, discord_user_id: &str) -> Option<JailRecord>;

    /// Remove a jail record (on unjail). Idempotent.
    fn remove_jail(&self, discord_user_id: &str) -> Result<(), String>;

    /// All jail records, newest first (admin list).
    fn list_jails(&self, limit: u32) -> Vec<JailRecord>;

    /// Jail records whose timed sentence has elapsed (for the expiry sweep).
    fn jails_due_for_unjail(&self, now_unix: i64) -> Vec<JailRecord>;
}
