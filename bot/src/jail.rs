//! The serenity-coupled "real jail" mechanics: snapshot → strip → restore.
//!
//! Generic over the storage/config ports, so the same logic drives the gateway
//! handler, the slash commands, and the expiry sweep. All paths take a serenity
//! `&Context` (the sweep uses one cloned from the `ready` event), so there is no
//! token-juggling raw-REST path.

use chrono::Utc;
use serenity::all::{Context, EditMember, GuildId, RoleId, UserId};

use airforce_modbot_core::JailConfig;

use crate::store::RedbStore;

fn parse_role(cfg: &JailConfig) -> Option<RoleId> {
    cfg.jail_role_id.trim().parse::<u64>().ok().map(RoleId::new)
}

/// The role set a jailed member should end up with: the jail role PLUS any of the
/// member's **managed** roles (bot-integration / Nitro Booster). Managed roles
/// cannot be removed through the member-edit endpoint, so omitting them makes
/// Discord reject the whole edit with `Missing Permissions` — meaning a Booster
/// (or a bot) could otherwise never be jailed. Best-effort: if the member or the
/// guild roles can't be fetched, fall back to just the jail role.
async fn jailed_role_set(
    ctx: &Context,
    guild_id: GuildId,
    user_id: UserId,
    jail_role: RoleId,
) -> Vec<RoleId> {
    let mut roles = vec![jail_role];
    if let (Ok(member), Ok(guild_roles)) = (
        guild_id.member(&ctx.http, user_id).await,
        guild_id.roles(&ctx.http).await,
    ) {
        for r in &member.roles {
            if *r != jail_role && guild_roles.get(r).is_some_and(|role| role.managed) {
                roles.push(*r);
            }
        }
    }
    roles
}

/// Snapshot → strip → persist → DM. Re-jailing an already-jailed user refreshes
/// the sentence but keeps the original role snapshot. `minutes = None` uses the
/// config default; `Some(0)` is indefinite.
pub async fn jail_member(
    ctx: &Context,
    store: &RedbStore,
    guild_id: GuildId,
    user_id: UserId,
    reason: &str,
    minutes: Option<u32>,
    jailed_by: &str,
) -> Result<(), String> {
    let guild = guild_id.get().to_string();
    let cfg = JailConfig::load_for_guild(store, &guild);
    let Some(jail_role) = parse_role(&cfg) else {
        return Err("jail not configured (no jail role set)".into());
    };
    let uid = user_id.to_string();

    // 1) Snapshot the member's current roles (minus the jail role), UNLESS we
    //    already have a snapshot (re-jail / they re-acquired roles somehow).
    let existing = store.get_jail_in(&guild, &uid);
    let prior: Vec<String> = match &existing {
        Some(rec) => rec.prior_roles.clone(),
        None => {
            let member = guild_id
                .member(&ctx.http, user_id)
                .await
                .map_err(|e| format!("fetch member: {e}"))?;
            member
                .roles
                .iter()
                .filter(|r| **r != jail_role)
                .map(|r| r.get().to_string())
                .collect()
        }
    };

    // 2) Persist BEFORE stripping. The strip emits a GUILD_MEMBER_UPDATE; writing
    //    the jail record first means the manual-role watcher already sees
    //    "jailed" and ignores that self-induced update (no infinite loop, no
    //    snapshot-loss race). Rolled back if the strip fails on a fresh jail.
    let now = Utc::now().timestamp();
    let mins = minutes.unwrap_or(cfg.default_minutes);
    let expires = if mins == 0 { None } else { Some(now + mins as i64 * 60) };
    store.record_jail_in(&guild, &uid, &prior, reason, jailed_by, now, expires)?;

    // 3) Strip to the jail role (preserving managed roles — see jailed_role_set).
    let builder = EditMember::new()
        .roles(jailed_role_set(ctx, guild_id, user_id, jail_role).await)
        .audit_log_reason("jail");
    if let Err(e) = guild_id.edit_member(&ctx.http, user_id, builder).await {
        if existing.is_none() {
            let _ = store.remove_jail_in(&guild, &uid);
        }
        return Err(format!(
            "edit_member failed (check the bot has Manage Roles AND the jail role sits below the bot's top role): {e}"
        ));
    }

    // 4) Optional private DM.
    if cfg.dm_user {
        let dur = if mins == 0 {
            "until a moderator releases you".to_string()
        } else {
            format!("for {mins} minutes")
        };
        if let Ok(user) = user_id.to_user(&ctx.http).await {
            if let Ok(ch) = user.create_dm_channel(&ctx.http).await {
                let _ = ch
                    .say(
                        &ctx.http,
                        format!("🔒 You have been restricted in the server {dur}. Reason: {reason}"),
                    )
                    .await;
            }
        }
    }
    println!("🔒 jailed {uid} (by {jailed_by}) — {reason}");
    Ok(())
}

/// Restore the snapshotted roles and clear the jail record. Best-effort on the
/// Discord side (the member may have left); the record is always cleared.
pub async fn unjail_member(
    ctx: &Context,
    store: &RedbStore,
    guild_id: GuildId,
    user_id: UserId,
    by: &str,
) -> Result<(), String> {
    let guild = guild_id.get().to_string();
    let cfg = JailConfig::load_for_guild(store, &guild);
    let uid = user_id.to_string();
    let rec = store.get_jail_in(&guild, &uid);

    // Clear the record FIRST so the role-restore's GUILD_MEMBER_UPDATE isn't
    // treated as a hand-removed jail role by the watcher (no re-entrant unjail).
    store.remove_jail_in(&guild, &uid)?;

    if let Some(rec) = &rec {
        let restored: Vec<RoleId> = rec
            .prior_roles
            .iter()
            .filter_map(|s| s.parse::<u64>().ok())
            .map(RoleId::new)
            .collect();
        let builder = EditMember::new().roles(restored).audit_log_reason("unjail");
        if let Err(e) = guild_id.edit_member(&ctx.http, user_id, builder).await {
            eprintln!("⚠️ unjail {uid}: role restore failed (member left?): {e}");
        }
    } else if let Some(role) = parse_role(&cfg) {
        // No snapshot — just pull the jail role.
        let _ = ctx
            .http
            .remove_member_role(guild_id, user_id, role, Some("unjail"))
            .await;
    }

    println!("🔓 unjailed {uid} (by {by})");
    Ok(())
}

/// Re-apply the jail role when a jailed user rejoins the guild (the escape-proof
/// path; needs the GUILD_MEMBERS intent so GUILD_MEMBER_ADD fires).
pub async fn reapply_if_jailed(
    ctx: &Context,
    store: &RedbStore,
    guild_id: GuildId,
    user_id: UserId,
) {
    let guild = guild_id.get().to_string();
    let cfg = JailConfig::load_for_guild(store, &guild);
    if !cfg.enabled {
        return;
    }
    let uid = user_id.to_string();
    if store.get_jail_in(&guild, &uid).is_none() {
        return;
    }
    let Some(jail_role) = parse_role(&cfg) else {
        return;
    };
    let builder = EditMember::new()
        .roles(jailed_role_set(ctx, guild_id, user_id, jail_role).await)
        .audit_log_reason("jail: re-applied on rejoin");
    match guild_id.edit_member(&ctx.http, user_id, builder).await {
        Ok(_) => println!("🔒 re-jailed {uid} on rejoin"),
        Err(e) => eprintln!("❌ failed to re-jail {uid} on rejoin: {e}"),
    }
}

/// Called by the link filter at the strike threshold. Returns `true` when jail
/// is configured (so the caller skips its legacy simple role-add), even if the
/// Discord call failed — the failure is logged loudly, not silently swallowed.
pub async fn try_jail(
    ctx: &Context,
    store: &RedbStore,
    guild_id: GuildId,
    user_id: UserId,
    reason: &str,
    by: &str,
) -> bool {
    let cfg = JailConfig::load_for_guild(store, &guild_id.get().to_string());
    if !cfg.enabled || parse_role(&cfg).is_none() {
        return false;
    }
    if let Err(e) = jail_member(ctx, store, guild_id, user_id, reason, None, by).await {
        eprintln!("❌ link-filter auto-jail failed for {user_id}: {e}");
    }
    true
}
