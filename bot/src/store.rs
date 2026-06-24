//! `redb`-backed adapter implementing the core's storage/config ports.
//!
//! Everything is stored as JSON strings keyed by a string, in three tables:
//! `config` (blob key → JSON config), `strikes` (user id → [`LinkStrike`] JSON),
//! and `jails` (user id → [`JailRecord`] JSON). `redb` is a pure-Rust, ACID,
//! single-file embedded store — no C toolchain, easy to self-host.
//!
//! `record_link_strike` runs its decay-aware read-modify-write inside a single
//! write transaction, so two rapid repeat violations from the same user can
//! never lose an increment (the atomicity guarantee the port requires).
//!
//! ## Multi-guild keying
//!
//! One instance can moderate many servers. Per-user rows (`strikes`, `jails`)
//! and per-feature config blobs are keyed by **`"{guild_id}:{id}"`** so the same
//! user/feature is isolated per guild. This lives entirely in THIS adapter: the
//! core [`StrikeStore`]/[`JailStore`]/[`ConfigStore`] **port signatures are
//! unchanged**, so the api.airforce backend (single-guild, its own adapter) is
//! unaffected. The single-key trait methods are kept (single-guild / backend
//! parity); the bot uses the guild-scoped `*_in` / `*_for_guild` inherent
//! methods + the `*_for_guild` config helpers. [`RedbStore::migrate_legacy_to_guild_keys`]
//! upgrades an existing single-guild DB to the guild-scoped scheme once.

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use airforce_modbot_core::cases::next_case_number;
use airforce_modbot_core::link_filter::next_strike_count;
use airforce_modbot_core::{
    guild_blob_key, Case, CaseAction, ConfigStore, JailRecord, JailStore, LinkStrike, StrikeStore,
};

const CONFIG: TableDefinition<&str, &str> = TableDefinition::new("config");
const STRIKES: TableDefinition<&str, &str> = TableDefinition::new("strikes");
const JAILS: TableDefinition<&str, &str> = TableDefinition::new("jails");
/// Moderation cases (mod-log), keyed `"{guild}:{case_id}"`; the per-guild case
/// counter lives in `CONFIG` under `"case_seq:{guild}"`.
const CASES: TableDefinition<&str, &str> = TableDefinition::new("cases");

/// Config-blob key recording which storage-key scheme the DB uses (so the
/// one-shot legacy→multi-guild migration runs at most once).
const SCHEMA_KEY: &str = "schema_version";
/// Current scheme: per-(guild,user) keys.
const SCHEMA_GUILD: &str = "2";

/// Per-(guild, user) storage key. Guild ids and user ids are snowflakes (digits
/// only), so the `:` separator can never collide with either part.
fn gkey(guild_id: &str, user_id: &str) -> String {
    format!("{guild_id}:{user_id}")
}

/// The embedded store. Cheap to clone-share behind an `Arc`; `redb::Database`
/// is `Send + Sync`.
pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    /// Open (creating if absent) the database at `path` and ensure every table
    /// exists, so later read transactions never trip over a missing table.
    pub fn open(path: &str) -> Result<Self, String> {
        let db = Database::create(path).map_err(|e| e.to_string())?;
        let w = db.begin_write().map_err(|e| e.to_string())?;
        {
            w.open_table(CONFIG).map_err(|e| e.to_string())?;
            w.open_table(STRIKES).map_err(|e| e.to_string())?;
            w.open_table(JAILS).map_err(|e| e.to_string())?;
            w.open_table(CASES).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(Self { db })
    }

    fn get_str(&self, table: TableDefinition<&str, &str>, key: &str) -> Option<String> {
        let r = self.db.begin_read().ok()?;
        let t = r.open_table(table).ok()?;
        let v = t.get(key).ok()??;
        Some(v.value().to_string())
    }

    fn put_str(&self, table: TableDefinition<&str, &str>, key: &str, val: &str) -> Result<(), String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = w.open_table(table).map_err(|e| e.to_string())?;
            t.insert(key, val).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())
    }

    fn del_key(&self, table: TableDefinition<&str, &str>, key: &str) -> Result<(), String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = w.open_table(table).map_err(|e| e.to_string())?;
            t.remove(key).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())
    }

    /// Deserialize every value in `table` into `T`, skipping anything corrupt.
    fn all_values<T: serde::de::DeserializeOwned>(&self, table: TableDefinition<&str, &str>) -> Vec<T> {
        let mut out = Vec::new();
        let Ok(r) = self.db.begin_read() else { return out };
        let Ok(t) = r.open_table(table) else { return out };
        if t.is_empty().unwrap_or(true) {
            return out;
        }
        if let Ok(iter) = t.iter() {
            for entry in iter.flatten() {
                if let Ok(v) = serde_json::from_str::<T>(entry.1.value()) {
                    out.push(v);
                }
            }
        }
        out
    }

    // ── canonical (key-parameterised) record logic ───────────────────────────
    // The single-key trait methods and the guild-scoped `*_in` methods both
    // route through these, so the atomic-RMW / snapshot-preserving logic lives
    // exactly once and only the storage KEY differs.

    /// Decay-aware atomic read-modify-write of a strike row at `key`. The stored
    /// record keeps the real `user_id`/`guild_id` regardless of the key scheme.
    fn record_strike_keyed(
        &self,
        key: &str,
        user_id: &str,
        guild_id: &str,
        reason: &str,
        now_unix: i64,
        decay_days: u32,
    ) -> Result<u32, String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        let new_count;
        {
            let mut t = w.open_table(STRIKES).map_err(|e| e.to_string())?;
            let prev: Option<LinkStrike> = t
                .get(key)
                .map_err(|e| e.to_string())?
                .and_then(|v| serde_json::from_str(v.value()).ok());
            let (prev_count, prev_first, prev_last) = match &prev {
                Some(p) => (p.count, p.first_strike_unix, p.last_strike_unix),
                None => (0, now_unix, 0),
            };
            new_count = next_strike_count(prev_count, prev_last, now_unix, decay_days);
            let first = if new_count == 1 { now_unix } else { prev_first };
            let rec = LinkStrike {
                discord_user_id: user_id.to_string(),
                guild_id: guild_id.to_string(),
                count: new_count,
                first_strike_unix: first,
                last_strike_unix: now_unix,
                last_reason: Some(reason.to_string()),
            };
            let json = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
            t.insert(key, json.as_str()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(new_count)
    }

    /// Insert/replace a jail row at `key`, preserving the ORIGINAL role snapshot
    /// on re-jail (passing the freshly read — already stripped — roles would
    /// otherwise lose them).
    #[allow(clippy::too_many_arguments)]
    fn record_jail_keyed(
        &self,
        key: &str,
        user_id: &str,
        guild_id: &str,
        prior_roles: &[String],
        reason: &str,
        jailed_by: &str,
        jailed_at_unix: i64,
        expires_at_unix: Option<i64>,
    ) -> Result<(), String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = w.open_table(JAILS).map_err(|e| e.to_string())?;
            let existing: Option<JailRecord> = t
                .get(key)
                .map_err(|e| e.to_string())?
                .and_then(|v| serde_json::from_str(v.value()).ok());
            let roles = existing
                .map(|e| e.prior_roles)
                .unwrap_or_else(|| prior_roles.to_vec());
            let rec = JailRecord {
                discord_user_id: user_id.to_string(),
                guild_id: guild_id.to_string(),
                prior_roles: roles,
                reason: Some(reason.to_string()),
                jailed_by: jailed_by.to_string(),
                jailed_at_unix,
                expires_at_unix,
            };
            let json = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
            t.insert(key, json.as_str()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())
    }

    fn get_jail_by_key(&self, key: &str) -> Option<JailRecord> {
        self.get_str(JAILS, key)
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    // ── guild-scoped API (what the multi-guild bot uses) ─────────────────────

    /// Guild-scoped strike record: the same user is counted independently per
    /// server. Same atomicity guarantee as the single-guild path.
    pub fn record_link_strike_in(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
        now_unix: i64,
        decay_days: u32,
    ) -> Result<u32, String> {
        self.record_strike_keyed(&gkey(guild_id, user_id), user_id, guild_id, reason, now_unix, decay_days)
    }

    /// Clear a user's strikes in ONE guild. Idempotent.
    pub fn reset_link_strikes_in(&self, guild_id: &str, user_id: &str) -> Result<(), String> {
        self.del_key(STRIKES, &gkey(guild_id, user_id))
    }

    /// Most-recent strike rows for ONE guild, newest first (admin list).
    pub fn list_link_strikes_for_guild(&self, guild_id: &str, limit: u32) -> Vec<LinkStrike> {
        let mut v: Vec<LinkStrike> = self
            .all_values::<LinkStrike>(STRIKES)
            .into_iter()
            .filter(|s| s.guild_id == guild_id)
            .collect();
        v.sort_by(|a, b| b.last_strike_unix.cmp(&a.last_strike_unix));
        v.truncate(limit as usize);
        v
    }

    /// Guild-scoped jail record (preserves the snapshot on re-jail).
    #[allow(clippy::too_many_arguments)]
    pub fn record_jail_in(
        &self,
        guild_id: &str,
        user_id: &str,
        prior_roles: &[String],
        reason: &str,
        jailed_by: &str,
        jailed_at_unix: i64,
        expires_at_unix: Option<i64>,
    ) -> Result<(), String> {
        self.record_jail_keyed(&gkey(guild_id, user_id), user_id, guild_id, prior_roles, reason, jailed_by, jailed_at_unix, expires_at_unix)
    }

    /// The active jail record for a user IN a specific guild, if any.
    pub fn get_jail_in(&self, guild_id: &str, user_id: &str) -> Option<JailRecord> {
        self.get_jail_by_key(&gkey(guild_id, user_id))
    }

    /// Remove a user's jail record in ONE guild (on unjail). Idempotent.
    pub fn remove_jail_in(&self, guild_id: &str, user_id: &str) -> Result<(), String> {
        self.del_key(JAILS, &gkey(guild_id, user_id))
    }

    /// All jail records for ONE guild, newest first (admin list).
    pub fn list_jails_for_guild(&self, guild_id: &str, limit: u32) -> Vec<JailRecord> {
        let mut v: Vec<JailRecord> = self
            .all_values::<JailRecord>(JAILS)
            .into_iter()
            .filter(|j| j.guild_id == guild_id)
            .collect();
        v.sort_by(|a, b| b.jailed_at_unix.cmp(&a.jailed_at_unix));
        v.truncate(limit as usize);
        v
    }

    // ── cases (mod-log / case system) ────────────────────────────────────────

    /// Record a moderation case, assigning the next **per-guild** case number
    /// atomically (counter + row written in one transaction). Returns the new id.
    #[allow(clippy::too_many_arguments)]
    pub fn add_case(
        &self,
        guild_id: &str,
        user_id: &str,
        mod_id: &str,
        action: CaseAction,
        reason: &str,
        created_unix: i64,
        duration_secs: Option<u64>,
    ) -> Result<u64, String> {
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        let id;
        {
            let seq_key = format!("case_seq:{guild_id}");
            let mut cfg_t = w.open_table(CONFIG).map_err(|e| e.to_string())?;
            let prev: u64 = cfg_t
                .get(seq_key.as_str())
                .map_err(|e| e.to_string())?
                .and_then(|v| v.value().parse().ok())
                .unwrap_or(0);
            id = next_case_number(prev);
            cfg_t
                .insert(seq_key.as_str(), id.to_string().as_str())
                .map_err(|e| e.to_string())?;

            let case = Case {
                id,
                guild_id: guild_id.to_string(),
                user_id: user_id.to_string(),
                mod_id: mod_id.to_string(),
                action,
                reason: reason.to_string(),
                created_unix,
                duration_secs,
            };
            let json = serde_json::to_string(&case).map_err(|e| e.to_string())?;
            let mut cases_t = w.open_table(CASES).map_err(|e| e.to_string())?;
            cases_t
                .insert(format!("{guild_id}:{id}").as_str(), json.as_str())
                .map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// A specific case by guild + id.
    pub fn get_case(&self, guild_id: &str, id: u64) -> Option<Case> {
        self.get_str(CASES, &format!("{guild_id}:{id}"))
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// One user's cases in a guild, newest first.
    pub fn list_cases_for_user(&self, guild_id: &str, user_id: &str, limit: u32) -> Vec<Case> {
        let mut v: Vec<Case> = self
            .all_values::<Case>(CASES)
            .into_iter()
            .filter(|c| c.guild_id == guild_id && c.user_id == user_id)
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        v.truncate(limit as usize);
        v
    }

    /// A guild's cases, newest first. Part of the case-store API; consumed by the
    /// web dashboard (Plan 06) / a future guild-wide mod-log view.
    #[allow(dead_code)]
    pub fn list_cases_for_guild(&self, guild_id: &str, limit: u32) -> Vec<Case> {
        let mut v: Vec<Case> = self
            .all_values::<Case>(CASES)
            .into_iter()
            .filter(|c| c.guild_id == guild_id)
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        v.truncate(limit as usize);
        v
    }

    // ── one-shot legacy → multi-guild migration ──────────────────────────────

    /// Migrate a legacy single-guild DB (bare `user_id` / bare config keys) to
    /// the guild-scoped scheme, so an existing self-hoster's strikes, jails, and
    /// config survive the multi-guild upgrade. Idempotent (guarded by the
    /// `schema_version` blob); uses the `guild_id` already stored in each
    /// record/config. A row without a guild id is left untouched (it was never
    /// multi-guild-reachable anyway). Safe to call on every startup.
    pub fn migrate_legacy_to_guild_keys(&self) -> Result<(), String> {
        if self.get_config_blob(SCHEMA_KEY).as_deref() == Some(SCHEMA_GUILD) {
            return Ok(());
        }
        // 1) Per-feature config blobs: copy each bare blob to its guild-scoped key.
        for base in ["link_filter_config", "jail_config", "flood_filter_config"] {
            if let Some(blob) = self.get_config_blob(base) {
                let guild = serde_json::from_str::<serde_json::Value>(&blob)
                    .ok()
                    .and_then(|v| v.get("guild_id").and_then(|g| g.as_str()).map(String::from))
                    .filter(|g| !g.is_empty());
                if let Some(guild) = guild {
                    let gk = guild_blob_key(&guild, base);
                    if self.get_config_blob(&gk).is_none() {
                        self.set_config_blob(&gk, &blob)?;
                    }
                }
            }
        }
        // 2) Per-user tables: re-key bare rows to "{guild}:{user}".
        self.rekey_table_to_guild::<LinkStrike>(STRIKES, |s| (s.guild_id.clone(), s.discord_user_id.clone()))?;
        self.rekey_table_to_guild::<JailRecord>(JAILS, |j| (j.guild_id.clone(), j.discord_user_id.clone()))?;

        self.set_config_blob(SCHEMA_KEY, SCHEMA_GUILD)
    }

    /// Re-key any bare-keyed (no `:`) row in `table` to `gkey(guild, user)` using
    /// the `(guild, user)` extracted from its value. Rows already guild-scoped or
    /// lacking a guild id are skipped. A pre-existing guild-scoped row is never
    /// clobbered. Two passes (read, then one write txn) so we don't mutate the
    /// table while iterating it.
    fn rekey_table_to_guild<T: serde::de::DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &str>,
        ids: impl Fn(&T) -> (String, String),
    ) -> Result<(), String> {
        let mut moves: Vec<(String, String, String)> = Vec::new(); // (old_key, new_key, json)
        {
            let r = self.db.begin_read().map_err(|e| e.to_string())?;
            let t = r.open_table(table).map_err(|e| e.to_string())?;
            if let Ok(iter) = t.iter() {
                for entry in iter.flatten() {
                    let key = entry.0.value().to_string();
                    if key.contains(':') {
                        continue; // already guild-scoped
                    }
                    let json = entry.1.value().to_string();
                    if let Ok(val) = serde_json::from_str::<T>(&json) {
                        let (guild, user) = ids(&val);
                        if !guild.is_empty() && !user.is_empty() {
                            moves.push((key, gkey(&guild, &user), json));
                        }
                    }
                }
            }
        }
        if moves.is_empty() {
            return Ok(());
        }
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = w.open_table(table).map_err(|e| e.to_string())?;
            for (old, new, json) in &moves {
                if t.get(new.as_str()).map_err(|e| e.to_string())?.is_none() {
                    t.insert(new.as_str(), json.as_str()).map_err(|e| e.to_string())?;
                }
                t.remove(old.as_str()).map_err(|e| e.to_string())?;
            }
        }
        w.commit().map_err(|e| e.to_string())
    }
}

impl ConfigStore for RedbStore {
    fn get_config_blob(&self, key: &str) -> Option<String> {
        self.get_str(CONFIG, key)
    }
    fn set_config_blob(&self, key: &str, value_json: &str) -> Result<(), String> {
        self.put_str(CONFIG, key, value_json)
    }
}

impl StrikeStore for RedbStore {
    /// Single-guild path (legacy / api.airforce-backend parity): keyed by the
    /// bare user id. The multi-guild bot uses [`RedbStore::record_link_strike_in`].
    fn record_link_strike(
        &self,
        discord_user_id: &str,
        guild_id: &str,
        reason: &str,
        now_unix: i64,
        decay_days: u32,
    ) -> Result<u32, String> {
        self.record_strike_keyed(discord_user_id, discord_user_id, guild_id, reason, now_unix, decay_days)
    }

    fn list_link_strikes(&self, limit: u32) -> Vec<LinkStrike> {
        let mut v = self.all_values::<LinkStrike>(STRIKES);
        v.sort_by(|a, b| b.last_strike_unix.cmp(&a.last_strike_unix));
        v.truncate(limit as usize);
        v
    }

    fn reset_link_strikes(&self, discord_user_id: &str) -> Result<(), String> {
        self.del_key(STRIKES, discord_user_id)
    }
}

impl JailStore for RedbStore {
    fn record_jail(
        &self,
        discord_user_id: &str,
        guild_id: &str,
        prior_roles: &[String],
        reason: &str,
        jailed_by: &str,
        jailed_at_unix: i64,
        expires_at_unix: Option<i64>,
    ) -> Result<(), String> {
        self.record_jail_keyed(
            discord_user_id,
            discord_user_id,
            guild_id,
            prior_roles,
            reason,
            jailed_by,
            jailed_at_unix,
            expires_at_unix,
        )
    }

    fn get_jail(&self, discord_user_id: &str) -> Option<JailRecord> {
        self.get_jail_by_key(discord_user_id)
    }

    fn remove_jail(&self, discord_user_id: &str) -> Result<(), String> {
        self.del_key(JAILS, discord_user_id)
    }

    fn list_jails(&self, limit: u32) -> Vec<JailRecord> {
        let mut v = self.all_values::<JailRecord>(JAILS);
        v.sort_by(|a, b| b.jailed_at_unix.cmp(&a.jailed_at_unix));
        v.truncate(limit as usize);
        v
    }

    fn jails_due_for_unjail(&self, now_unix: i64) -> Vec<JailRecord> {
        self.all_values::<JailRecord>(JAILS)
            .into_iter()
            .filter(|j| j.expires_at_unix.is_some_and(|e| e <= now_unix))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway store on a unique temp file (in-process; cleaned up on drop).
    struct TempStore {
        path: std::path::PathBuf,
        store: RedbStore,
    }
    impl TempStore {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("modbot-{}-{}-{}.redb", tag, std::process::id(), n));
            let _ = std::fs::remove_file(&path);
            let store = RedbStore::open(path.to_str().unwrap()).unwrap();
            Self { path, store }
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn strike_rmw_increments_and_resets() {
        let ts = TempStore::new("strike");
        let s = &ts.store;
        // first strike => 1, second => 2 (decay off)
        assert_eq!(s.record_link_strike("u1", "g", "ad", 1000, 0).unwrap(), 1);
        assert_eq!(s.record_link_strike("u1", "g", "ad", 1001, 0).unwrap(), 2);
        assert_eq!(s.list_link_strikes(10).len(), 1);
        assert_eq!(s.list_link_strikes(10)[0].count, 2);
        // reset => gone
        s.reset_link_strikes("u1").unwrap();
        assert!(s.list_link_strikes(10).is_empty());
    }

    #[test]
    fn strike_decay_resets_old_streak() {
        let ts = TempStore::new("decay");
        let s = &ts.store;
        let day = 86_400;
        assert_eq!(s.record_link_strike("u", "g", "ad", 0, 30).unwrap(), 1);
        // 31 days later with a 30-day window => streak lapsed => back to 1
        assert_eq!(s.record_link_strike("u", "g", "ad", 31 * day, 30).unwrap(), 1);
    }

    #[test]
    fn config_blob_roundtrip() {
        let ts = TempStore::new("cfg");
        let s = &ts.store;
        assert!(s.get_config_blob("k").is_none());
        s.set_config_blob("k", r#"{"a":1}"#).unwrap();
        assert_eq!(s.get_config_blob("k").as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn jail_record_preserves_snapshot_on_rejail_and_expiry_filters() {
        let ts = TempStore::new("jail");
        let s = &ts.store;
        s.record_jail("u", "g", &["r1".into(), "r2".into()], "spam", "mod", 100, Some(200)).unwrap();
        // re-jail with DIFFERENT (post-strip) roles must KEEP the original snapshot
        s.record_jail("u", "g", &[], "again", "mod", 150, None).unwrap();
        let rec = s.get_jail("u").unwrap();
        assert_eq!(rec.prior_roles, vec!["r1".to_string(), "r2".to_string()]);
        assert_eq!(rec.expires_at_unix, None); // sentence refreshed to indefinite

        // expiry filter: "u" is indefinite (never due); "v" expires at 300.
        s.record_jail("v", "g", &[], "x", "mod", 100, Some(300)).unwrap();
        assert_eq!(s.jails_due_for_unjail(250).len(), 0); // 300 > 250 => not yet due
        let due = s.jails_due_for_unjail(350); // 300 <= 350 => v is due
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].discord_user_id, "v");

        // remove clears it
        s.remove_jail("u").unwrap();
        assert!(s.get_jail("u").is_none());
        assert_eq!(s.list_jails(10).len(), 1); // only "v" remains
    }

    // ── multi-guild isolation (the guild-scoped API) ─────────────────────────

    #[test]
    fn strikes_are_isolated_per_guild() {
        let ts = TempStore::new("g-strike");
        let s = &ts.store;
        assert_eq!(s.record_link_strike_in("g1", "u", "ad", 1000, 0).unwrap(), 1);
        assert_eq!(s.record_link_strike_in("g1", "u", "ad", 1001, 0).unwrap(), 2);
        // same user, DIFFERENT guild => its own counter starts at 1
        assert_eq!(s.record_link_strike_in("g2", "u", "ad", 1002, 0).unwrap(), 1);
        assert_eq!(s.list_link_strikes_for_guild("g1", 10)[0].count, 2);
        assert_eq!(s.list_link_strikes_for_guild("g2", 10)[0].count, 1);
        assert_eq!(s.list_link_strikes_for_guild("g1", 10).len(), 1);
        // reset in one guild leaves the other intact
        s.reset_link_strikes_in("g1", "u").unwrap();
        assert!(s.list_link_strikes_for_guild("g1", 10).is_empty());
        assert_eq!(s.list_link_strikes_for_guild("g2", 10).len(), 1);
    }

    #[test]
    fn jails_are_isolated_per_guild() {
        let ts = TempStore::new("g-jail");
        let s = &ts.store;
        s.record_jail_in("g1", "u", &["r1".into()], "spam", "mod", 100, None).unwrap();
        assert!(s.get_jail_in("g1", "u").is_some());
        assert!(s.get_jail_in("g2", "u").is_none()); // other guild unaffected
        assert_eq!(s.list_jails_for_guild("g1", 10).len(), 1);
        assert!(s.list_jails_for_guild("g2", 10).is_empty());
        // snapshot preserved across re-jail, isolated per guild
        s.record_jail_in("g1", "u", &[], "again", "mod", 150, None).unwrap();
        assert_eq!(s.get_jail_in("g1", "u").unwrap().prior_roles, vec!["r1".to_string()]);
        s.remove_jail_in("g1", "u").unwrap();
        assert!(s.get_jail_in("g1", "u").is_none());
    }

    #[test]
    fn cases_number_monotonically_and_isolate_per_guild() {
        let ts = TempStore::new("cases");
        let s = &ts.store;
        assert_eq!(s.add_case("g1", "u", "mod", CaseAction::Warn, "a", 100, None).unwrap(), 1);
        assert_eq!(s.add_case("g1", "u", "mod", CaseAction::Ban, "b", 101, None).unwrap(), 2);
        // a different guild numbers independently, starting at 1
        assert_eq!(s.add_case("g2", "u", "mod", CaseAction::Kick, "c", 102, None).unwrap(), 1);
        // lookup is guild-scoped
        assert_eq!(s.get_case("g1", 2).unwrap().action, CaseAction::Ban);
        assert!(s.get_case("g2", 2).is_none());
        // listing per user + per guild, newest first
        s.add_case("g1", "v", "mod", CaseAction::Note, "n", 103, None).unwrap(); // id 3, user v
        assert_eq!(s.list_cases_for_user("g1", "u", 10).len(), 2); // u has #1, #2
        assert_eq!(s.list_cases_for_user("g1", "u", 10)[0].id, 2); // newest first
        assert_eq!(s.list_cases_for_guild("g1", 10).len(), 3);
        assert_eq!(s.list_cases_for_guild("g2", 10).len(), 1);
    }

    #[test]
    fn migration_rekeys_legacy_single_guild_data_idempotently() {
        let ts = TempStore::new("migrate");
        let s = &ts.store;
        // Simulate a legacy single-guild DB: bare-keyed strike + jail + config.
        s.record_link_strike("u1", "g", "ad", 1000, 0).unwrap();
        s.record_jail("u1", "g", &["r1".into()], "x", "mod", 100, None).unwrap();
        s.set_config_blob("link_filter_config", r#"{"enabled":true,"guild_id":"g","strike_threshold":3}"#).unwrap();

        s.migrate_legacy_to_guild_keys().unwrap();

        // Now reachable through the guild-scoped API…
        assert!(s.get_jail_in("g", "u1").is_some());
        assert_eq!(s.list_link_strikes_for_guild("g", 10).len(), 1);
        // …and the config blob copied to its guild-scoped key.
        assert_eq!(
            s.get_config_blob(&guild_blob_key("g", "link_filter_config")).as_deref(),
            Some(r#"{"enabled":true,"guild_id":"g","strike_threshold":3}"#),
        );
        // Bare per-user rows were moved, not duplicated.
        assert!(s.get_jail("u1").is_none());
        // Idempotent: a second run is a no-op (no duplicate rows).
        s.migrate_legacy_to_guild_keys().unwrap();
        assert_eq!(s.list_link_strikes_for_guild("g", 10).len(), 1);
        assert_eq!(s.list_jails_for_guild("g", 10).len(), 1);
    }
}
