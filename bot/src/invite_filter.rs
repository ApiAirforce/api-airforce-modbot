//! The Discord-invite sub-filter's **gateway side**: resolve an unknown invite
//! code to its guild (a network call) and cache the verdict. The pure detection
//! + allowlist checks live in `airforce-modbot-core::link_filter`; this is the
//! adapter that does the I/O, mirroring how the jail mechanics sit on top of the
//! pure jail config.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serenity::all::Context;

use airforce_modbot_core::link_filter::{extract_discord_invites, extract_dsc_gg_codes};
use airforce_modbot_core::LinkFilterConfig;

/// A redirect-following-disabled HTTP client used only to peek at where a
/// `dsc.gg/<slug>` vanity points (its `Location` header) without chasing the
/// redirect chain. Short timeout + graceful fallback so a slow/broken dsc.gg
/// never stalls or panics the moderation path.
static NOREDIRECT_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .user_agent("DiscordBot (https://api.airforce, 1.0)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

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

/// Resolve a raw Discord invite `code` to whether it is allowed. A *working*
/// invite to a non-allowed guild is `false`; everything else — a Group-DM invite
/// (no guild), an invalid/expired code, or a transient API error — is `true`
/// (**fail-open**: never strike a user for our own network hiccup, and a dead
/// invite renders nothing anyway).
async fn invite_code_allowed(ctx: &Context, code: &str, cfg: &LinkFilterConfig) -> bool {
    match ctx.http.get_invite(code, false, false, None).await {
        Ok(invite) => match invite.guild {
            Some(g) => cfg.is_guild_allowed(&g.id.get().to_string()),
            None => true,
        },
        Err(_) => true,
    }
}

/// Resolve a `dsc.gg/<slug>` vanity: peek at its `Location` redirect, and if it
/// points at a real Discord invite, judge that invite's guild. Fail-open at every
/// step (network error, no redirect, redirect not to a Discord invite).
async fn dsc_gg_slug_allowed(ctx: &Context, slug: &str, cfg: &LinkFilterConfig) -> bool {
    let resp = match NOREDIRECT_HTTP.get(format!("https://dsc.gg/{slug}")).send().await {
        Ok(r) => r,
        Err(_) => return true,
    };
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok());
    let Some(location) = location else {
        return true;
    };
    match extract_discord_invites(location).into_iter().next() {
        Some(code) => invite_code_allowed(ctx, &code, cfg).await,
        None => true,
    }
}

/// The first invite in `text` that advertises a NON-allowed server, as a display
/// form (`discord.gg/<code>` or `dsc.gg/<slug>`), or `None` when every invite is
/// permitted (an allowlisted code/slug, the bot's own guild, or a partner guild)
/// or the message has none. Covers direct `discord.gg` / `discord.com/invite`
/// links and `dsc.gg` vanity shorteners (resolved one redirect hop). Fail-open
/// throughout; results are cached and the per-message network fan-out is capped.
pub async fn first_offending_invite(
    ctx: &Context,
    text: &str,
    cfg: &LinkFilterConfig,
    cache: &InviteCache,
) -> Option<String> {
    let mut lookups = 0usize;

    // 1) Direct discord.gg / discord.com invites.
    for code in extract_discord_invites(text) {
        if cfg.is_invite_code_allowed(&code) {
            continue;
        }
        if let Some(allowed) = cache.cached(&code) {
            if allowed {
                continue;
            }
            return Some(format!("discord.gg/{code}"));
        }
        if lookups >= MAX_LOOKUPS_PER_MSG {
            break;
        }
        lookups += 1;
        let allowed = invite_code_allowed(ctx, &code, cfg).await;
        cache.store(&code, allowed);
        if !allowed {
            return Some(format!("discord.gg/{code}"));
        }
    }

    // 2) dsc.gg vanity shorteners → resolve the redirect to the real invite.
    for slug in extract_dsc_gg_codes(text) {
        if cfg.is_invite_code_allowed(&slug) {
            continue;
        }
        let key = format!("dsc:{slug}");
        if let Some(allowed) = cache.cached(&key) {
            if allowed {
                continue;
            }
            return Some(format!("dsc.gg/{slug}"));
        }
        if lookups >= MAX_LOOKUPS_PER_MSG {
            break;
        }
        lookups += 1;
        let allowed = dsc_gg_slug_allowed(ctx, &slug, cfg).await;
        cache.store(&key, allowed);
        if !allowed {
            return Some(format!("dsc.gg/{slug}"));
        }
    }

    None
}
