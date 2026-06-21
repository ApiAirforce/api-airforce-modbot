//! The Discord-invite sub-filter's **gateway side**: resolve an unknown invite
//! code to its guild (a network call) and cache the verdict. The pure detection
//! + allowlist checks live in `airforce-modbot-core::link_filter`; this is the
//! adapter that does the I/O, mirroring how the jail mechanics sit on top of the
//! pure jail config.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::all::Context;

use airforce_modbot_core::link_filter::extract_discord_invites;
use airforce_modbot_core::LinkFilterConfig;

/// How long a resolved verdict is trusted before re-querying Discord. Keeps the
/// invite endpoint's rate limit happy when the same code is posted repeatedly.
const CACHE_TTL: Duration = Duration::from_secs(3600);

/// Soft cap on cached verdicts. When exceeded, expired entries are swept before
/// inserting, so the map stays proportional to distinct codes seen within one
/// TTL window rather than growing without bound (a spammer can otherwise post
/// unlimited unique codes — one permanent entry each).
const CACHE_CAP: usize = 10_000;

/// Most network invite-resolutions performed for a single message. One
/// non-allowlisted invite already triggers deletion, so we never need to
/// resolve every code; this bounds the Discord-API fan-out a message can cause.
const MAX_LOOKUPS_PER_MSG: usize = 5;

/// Resolve-cache: invite code → (allowed, cached_at). A plain mutex-guarded map;
/// the lock is never held across the `.await` below.
#[derive(Default)]
pub struct InviteCache {
    map: Mutex<HashMap<String, (bool, Instant)>>,
}

impl InviteCache {
    fn cached(&self, code: &str) -> Option<bool> {
        let map = self.map.lock().ok()?;
        match map.get(code) {
            Some((allowed, at)) if at.elapsed() < CACHE_TTL => Some(*allowed),
            _ => None,
        }
    }

    fn store(&self, code: &str, allowed: bool) {
        if let Ok(mut map) = self.map.lock() {
            if map.len() >= CACHE_CAP {
                map.retain(|_, (_, at)| at.elapsed() < CACHE_TTL);
            }
            map.insert(code.to_string(), (allowed, Instant::now()));
        }
    }
}

/// The first Discord invite in `text` that advertises a NON-allowed server, or
/// `None` when every invite is permitted (an allowlisted code, the bot's own
/// guild, or a partner guild) or the message has no invites.
///
/// **Fail-open**: a resolution error or an invalid/expired invite counts as
/// allowed — we never strike a user for our own network hiccup, and a dead
/// invite renders nothing in Discord anyway. Only a *working* invite that
/// resolves to a non-allowed guild is treated as advertising.
pub async fn first_offending_invite(
    ctx: &Context,
    text: &str,
    cfg: &LinkFilterConfig,
    cache: &InviteCache,
) -> Option<String> {
    let mut lookups = 0usize;
    for code in extract_discord_invites(text) {
        // Fast path: an explicitly allowlisted code never needs a lookup.
        if cfg.is_invite_code_allowed(&code) {
            continue;
        }
        // Cached verdict from a recent resolution?
        if let Some(allowed) = cache.cached(&code) {
            if allowed {
                continue;
            }
            return Some(code);
        }
        // Bound the per-message Discord-API fan-out: a single non-allowlisted
        // invite already triggers deletion, so we never need to resolve every
        // code — stop after a few uncached lookups.
        if lookups >= MAX_LOOKUPS_PER_MSG {
            break;
        }
        lookups += 1;
        // Resolve the code to its guild (no member counts / expiration needed).
        match ctx.http.get_invite(&code, false, false, None).await {
            Ok(invite) => {
                // No guild on the invite (e.g. a Group-DM invite) → not a server
                // ad. Otherwise allow only the own/partner guilds.
                let allowed = match invite.guild {
                    Some(g) => cfg.is_guild_allowed(&g.id.get().to_string()),
                    None => true,
                };
                cache.store(&code, allowed);
                if !allowed {
                    return Some(code);
                }
            }
            // Invalid/expired invite or a transient error → fail open.
            Err(_) => continue,
        }
    }
    None
}
