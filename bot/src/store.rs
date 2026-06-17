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

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use airforce_modbot_core::link_filter::next_strike_count;
use airforce_modbot_core::{ConfigStore, JailRecord, JailStore, LinkStrike, StrikeStore};

const CONFIG: TableDefinition<&str, &str> = TableDefinition::new("config");
const STRIKES: TableDefinition<&str, &str> = TableDefinition::new("strikes");
const JAILS: TableDefinition<&str, &str> = TableDefinition::new("jails");

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
    fn record_link_strike(
        &self,
        discord_user_id: &str,
        guild_id: &str,
        reason: &str,
        now_unix: i64,
        decay_days: u32,
    ) -> Result<u32, String> {
        // Decay-aware read-modify-write, all inside ONE write transaction so a
        // concurrent repeat strike can't read a stale count and lose an increment.
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        let new_count;
        {
            let mut t = w.open_table(STRIKES).map_err(|e| e.to_string())?;
            let prev: Option<LinkStrike> = t
                .get(discord_user_id)
                .map_err(|e| e.to_string())?
                .and_then(|v| serde_json::from_str(v.value()).ok());
            let (prev_count, prev_first, prev_last) = match &prev {
                Some(p) => (p.count, p.first_strike_unix, p.last_strike_unix),
                None => (0, now_unix, 0),
            };
            new_count = next_strike_count(prev_count, prev_last, now_unix, decay_days);
            let first = if new_count == 1 { now_unix } else { prev_first };
            let rec = LinkStrike {
                discord_user_id: discord_user_id.to_string(),
                guild_id: guild_id.to_string(),
                count: new_count,
                first_strike_unix: first,
                last_strike_unix: now_unix,
                last_reason: Some(reason.to_string()),
            };
            let json = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
            t.insert(discord_user_id, json.as_str()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())?;
        Ok(new_count)
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
        let w = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut t = w.open_table(JAILS).map_err(|e| e.to_string())?;
            // Preserve the ORIGINAL role snapshot on re-jail (passing the freshly
            // read — already stripped — roles would otherwise lose them).
            let existing: Option<JailRecord> = t
                .get(discord_user_id)
                .map_err(|e| e.to_string())?
                .and_then(|v| serde_json::from_str(v.value()).ok());
            let roles = existing
                .map(|e| e.prior_roles)
                .unwrap_or_else(|| prior_roles.to_vec());
            let rec = JailRecord {
                discord_user_id: discord_user_id.to_string(),
                guild_id: guild_id.to_string(),
                prior_roles: roles,
                reason: Some(reason.to_string()),
                jailed_by: jailed_by.to_string(),
                jailed_at_unix,
                expires_at_unix,
            };
            let json = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
            t.insert(discord_user_id, json.as_str()).map_err(|e| e.to_string())?;
        }
        w.commit().map_err(|e| e.to_string())
    }

    fn get_jail(&self, discord_user_id: &str) -> Option<JailRecord> {
        self.get_str(JAILS, discord_user_id)
            .and_then(|s| serde_json::from_str(&s).ok())
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
}
