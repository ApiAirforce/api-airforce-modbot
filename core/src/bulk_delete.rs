//! Partitioning a burst of messages into Discord **bulk-delete** batches — pure.
//!
//! Discord's bulk-delete endpoint removes 2..=100 messages in a single call but
//! rejects any message older than 14 days, and a lone message cannot be
//! bulk-deleted at all. This helper turns a flood burst's
//! `(channel_id, message_id)` list into a concrete plan — per-channel batches of
//! up to 100 recent ids to bulk-delete, plus the stragglers (a lone message, or
//! one older than 14 days) to delete one-by-one — so the host just executes it
//! and never sends an out-of-range request that Discord would reject wholesale.

use std::collections::HashMap;

/// Discord bulk-delete accepts at most this many message ids per call.
pub const BULK_DELETE_MAX: usize = 100;
/// Discord rejects bulk-deleting messages older than 14 days.
pub const BULK_DELETE_MAX_AGE_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// Discord epoch (2015-01-01T00:00:00Z) in milliseconds since the Unix epoch.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Creation time of a Discord snowflake, in ms since the Unix epoch.
fn snowflake_unix_ms(id: u64) -> u64 {
    (id >> 22) + DISCORD_EPOCH_MS
}

/// A concrete deletion plan for a burst (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeletionPlan {
    /// Per channel: one or more batches of 2..=100 message ids to bulk-delete.
    pub bulk: Vec<(String, Vec<String>)>,
    /// `(channel_id, message_id)` pairs that must be deleted individually (a lone
    /// message in its channel, an id older than 14 days, or an unparseable id).
    pub single: Vec<(String, String)>,
}

/// Partition `messages` (`(channel_id, message_id)` pairs) into a [`DeletionPlan`].
///
/// `now_unix_ms` is the current wall-clock time in ms (used to route messages too
/// old to bulk-delete to the single path). Channel order follows first
/// appearance; within a channel, ids keep their order. An unparseable message id
/// is treated as a single delete — never smuggled into a bulk batch.
pub fn plan_deletions(messages: &[(String, String)], now_unix_ms: u64) -> DeletionPlan {
    let mut order: Vec<String> = Vec::new();
    // channel -> (bulk-eligible recent ids, straggler ids that must go single)
    let mut by_channel: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for (cid, mid) in messages {
        let slot = by_channel.entry(cid.clone()).or_insert_with(|| {
            order.push(cid.clone());
            (Vec::new(), Vec::new())
        });
        let bulk_ok = mid.parse::<u64>().ok().is_some_and(|id| {
            now_unix_ms.saturating_sub(snowflake_unix_ms(id)) < BULK_DELETE_MAX_AGE_MS
        });
        if bulk_ok {
            slot.0.push(mid.clone());
        } else {
            slot.1.push(mid.clone());
        }
    }
    let mut plan = DeletionPlan::default();
    for cid in order {
        let (recent, old) = by_channel.remove(&cid).unwrap_or_default();
        for chunk in recent.chunks(BULK_DELETE_MAX) {
            if chunk.len() >= 2 {
                plan.bulk.push((cid.clone(), chunk.to_vec()));
            } else {
                // A lone recent id can't be bulk-deleted (the endpoint needs >=2).
                plan.single.push((cid.clone(), chunk[0].clone()));
            }
        }
        for mid in old {
            plan.single.push((cid.clone(), mid));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: u64 = 1_420_070_400_000;
    /// Build a snowflake whose creation time is `unix_ms`.
    fn id_at(unix_ms: u64) -> String {
        (((unix_ms - EPOCH) << 22) | 1).to_string()
    }

    #[test]
    fn batches_recent_messages_one_per_channel() {
        let now = EPOCH + 1_000_000_000;
        let m = id_at(now - 1000);
        let msgs = vec![
            ("c1".to_string(), m.clone()),
            ("c1".to_string(), m.clone()),
            ("c2".to_string(), m.clone()),
            ("c2".to_string(), m.clone()),
        ];
        let plan = plan_deletions(&msgs, now);
        assert_eq!(plan.bulk.len(), 2); // one batch per channel
        assert!(plan.single.is_empty());
        assert_eq!(plan.bulk[0].0, "c1"); // first-seen order preserved
        assert_eq!(plan.bulk[0].1.len(), 2);
    }

    #[test]
    fn lone_message_in_channel_goes_single() {
        let now = EPOCH + 1_000_000_000;
        let plan = plan_deletions(&[("c1".to_string(), id_at(now - 1000))], now);
        assert!(plan.bulk.is_empty());
        assert_eq!(plan.single.len(), 1);
    }

    #[test]
    fn over_100_splits_and_remainder_goes_single() {
        let now = EPOCH + 1_000_000_000;
        let m = id_at(now - 1000);
        let msgs: Vec<(String, String)> = (0..101).map(|_| ("c1".to_string(), m.clone())).collect();
        let plan = plan_deletions(&msgs, now);
        assert_eq!(plan.bulk.len(), 1);
        assert_eq!(plan.bulk[0].1.len(), 100);
        assert_eq!(plan.single.len(), 1); // the 101st, alone, can't bulk
    }

    #[test]
    fn old_messages_route_to_single_while_recent_batch() {
        let now = EPOCH + 30 * 24 * 60 * 60 * 1000; // 30 days past the epoch
        let old = id_at(EPOCH + 1000); // ~30 days old => not bulk-deletable
        let recent1 = id_at(now - 1000);
        let recent2 = id_at(now - 2000);
        let plan = plan_deletions(
            &[
                ("c1".to_string(), old),
                ("c1".to_string(), recent1),
                ("c1".to_string(), recent2),
            ],
            now,
        );
        assert_eq!(plan.bulk.len(), 1);
        assert_eq!(plan.bulk[0].1.len(), 2); // the two recent ones
        assert_eq!(plan.single.len(), 1); // the old one
    }

    #[test]
    fn unparseable_id_goes_single() {
        let now = EPOCH + 1_000_000_000;
        let plan = plan_deletions(&[("c1".to_string(), "not-a-number".to_string())], now);
        assert!(plan.bulk.is_empty());
        assert_eq!(plan.single.len(), 1);
    }
}
