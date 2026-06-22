//! Admin slash commands — the bot's runtime "control panel". Everything that the
//! api.airforce web admin panel does for the link filter / jail is exposed here
//! as guild slash commands, gated to bot owners (config) or members with the
//! Manage Server permission.
//!
//! Responses use defer-then-edit so a slow Discord call (e.g. jailing fetches a
//! member, edits roles and DMs) never trips the 3-second interaction deadline.

use serenity::all::{
    ChannelId, CommandDataOption, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    Context, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, GuildId, Permissions, RoleId, UserId,
};

use airforce_modbot_core::link_filter::{normalize_host, UserChannelExempt, UserThreshold};
use airforce_modbot_core::flood_filter::FloodUserOverride;
use airforce_modbot_core::{
    FloodAction, FloodFilterConfig, FloodScope, JailConfig, JailStore, LinkFilterConfig,
    StrikeStore,
};

use crate::config::BotConfig;
use crate::jail;
use crate::store::RedbStore;

/// All guild slash-command definitions, registered in `ready`.
pub fn command_defs() -> Vec<CreateCommand> {
    let role = |name: &str, desc: &str| {
        CreateCommandOption::new(CommandOptionType::Role, name, desc)
    };
    let int = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::Integer, name, desc);
    let user = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::User, name, desc);
    let chan = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::Channel, name, desc);
    let boolean = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::Boolean, name, desc);
    let string = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::String, name, desc);
    let sub = |name: &str, desc: &str| CreateCommandOption::new(CommandOptionType::SubCommand, name, desc);

    vec![
        CreateCommand::new("modstatus")
            .description("Show the current link-filter and jail configuration"),
        CreateCommand::new("setfilter")
            .description("Configure the anti-ad link filter (only the options you pass are changed)")
            .add_option(boolean("enabled", "Turn the filter on or off"))
            .add_option(int("threshold", "Strikes before auto-jail (1-20)"))
            .add_option(int("decay_days", "Days of no violations after which strikes reset (0 = never)"))
            .add_option(role("jail_role", "Role to assign at the strike threshold"))
            .add_option(boolean("warn_user", "DM the user a private strike notice"))
            .add_option(boolean("filter_invites", "Also block Discord invites for OTHER servers (allowlist-gated)")),
        CreateCommand::new("whitelist")
            .description("Manage the domain whitelist (apex matches subdomains; use *.example.com for subdomains only)")
            .add_option(sub("add", "Whitelist a domain").add_sub_option(string("domain", "e.g. example.com or *.cdn.example.com").required(true)))
            .add_option(sub("remove", "Remove a whitelisted domain").add_sub_option(string("domain", "Exact entry to remove").required(true)))
            .add_option(sub("list", "List whitelisted domains")),
        CreateCommand::new("exempt")
            .description("Add a filter exemption")
            .add_option(sub("channel", "Never filter a whole channel").add_sub_option(chan("channel", "Channel").required(true)))
            .add_option(sub("role", "Never filter holders of a role").add_sub_option(role("role", "Role").required(true)))
            .add_option(sub("userchannel", "Don't filter a user in ONE channel").add_sub_option(user("user", "User").required(true)).add_sub_option(chan("channel", "Channel").required(true))),
        CreateCommand::new("unexempt")
            .description("Remove a filter exemption")
            .add_option(sub("channel", "Re-enable filtering in a channel").add_sub_option(chan("channel", "Channel").required(true)))
            .add_option(sub("role", "Re-enable filtering for a role").add_sub_option(role("role", "Role").required(true)))
            .add_option(sub("userchannel", "Remove a per-(user, channel) exemption").add_sub_option(user("user", "User").required(true)).add_sub_option(chan("channel", "Channel").required(true))),
        CreateCommand::new("userlimit")
            .description("Set a per-user strike threshold (overrides the global one)")
            .add_option(user("user", "User").required(true))
            .add_option(int("threshold", "Their threshold (1-20), or 0 to remove the override").required(true)),
        CreateCommand::new("allowinvite")
            .description("Allow specific Discord invite codes (yours + partners); others are blocked when the invite filter is on")
            .add_option(sub("add", "Allow a Discord invite code").add_sub_option(string("code", "The part after discord.gg/ , e.g. airforce").required(true)))
            .add_option(sub("remove", "Remove an allowed invite code").add_sub_option(string("code", "Exact code to remove").required(true)))
            .add_option(sub("list", "List allowed invite codes")),
        CreateCommand::new("allowserver")
            .description("Allow invites to specific partner servers by guild ID (your own server is always allowed)")
            .add_option(sub("add", "Allow invites to a server").add_sub_option(string("guild_id", "The server (guild) ID").required(true)))
            .add_option(sub("remove", "Remove an allowed server").add_sub_option(string("guild_id", "Guild ID to remove").required(true)))
            .add_option(sub("list", "List allowed server IDs")),
        CreateCommand::new("strikes")
            .description("View or clear anti-ad strikes")
            .add_option(sub("list", "List recent strikes"))
            .add_option(sub("reset", "Clear a user's strikes").add_sub_option(user("user", "User").required(true))),
        CreateCommand::new("jail")
            .description("Restrict a member (snapshots and strips their roles)")
            .add_option(user("user", "Member to jail").required(true))
            .add_option(int("minutes", "Minutes (0 = indefinite; omit for the configured default)"))
            .add_option(string("reason", "Reason")),
        CreateCommand::new("unjail")
            .description("Release a member and restore their previous roles")
            .add_option(user("user", "Member to release").required(true)),
        CreateCommand::new("setjail")
            .description("Configure the jail (only the options you pass are changed)")
            .add_option(boolean("enabled", "Turn the jail system on or off"))
            .add_option(role("role", "The Jail role (deny View everywhere except #jail)"))
            .add_option(chan("channel", "The #jail channel (informational)"))
            .add_option(int("default_minutes", "Default sentence length (0 = indefinite)")),
        CreateCommand::new("setflood")
            .description("Configure the cross-channel flood/raid filter (only options you pass change)")
            .add_option(boolean("enabled", "Turn the flood filter on or off"))
            .add_option(int("channel_threshold", "Trip at N distinct channels in the window (0 = off, else 2-50)"))
            .add_option(int("channel_window", "Spread window in seconds (1-3600)"))
            .add_option(int("msg_threshold", "Trip at N messages in the window (0 = off, else 2-100)"))
            .add_option(int("msg_window", "Burst window in seconds (1-3600)"))
            .add_option(
                string("action", "What to do on a trip")
                    .add_string_choice("warn (delete + DM)", "warn")
                    .add_string_choice("delete only", "delete")
                    .add_string_choice("delete + jail", "jail"),
            )
            .add_option(
                string("scope", "Which messages count toward the thresholds")
                    .add_string_choice("all messages", "all")
                    .add_string_choice("attachments only", "attachments")
                    .add_string_choice("attachments or links", "attachments_or_links"),
            )
            .add_option(role("jail_role", "Role to assign when action = jail"))
            .add_option(int("decay_days", "Days of no violations after which strikes reset (0 = never)"))
            .add_option(boolean("warn_user", "DM the user when their messages are removed")),
        CreateCommand::new("floodexempt")
            .description("Add a flood-filter exemption")
            .add_option(sub("channel", "Never flood-check a whole channel").add_sub_option(chan("channel", "Channel").required(true)))
            .add_option(sub("role", "Never flood-check holders of a role").add_sub_option(role("role", "Role").required(true)))
            .add_option(sub("userchannel", "Don't flood-check a user in ONE channel").add_sub_option(user("user", "User").required(true)).add_sub_option(chan("channel", "Channel").required(true))),
        CreateCommand::new("floodunexempt")
            .description("Remove a flood-filter exemption")
            .add_option(sub("channel", "Re-enable flood-checking in a channel").add_sub_option(chan("channel", "Channel").required(true)))
            .add_option(sub("role", "Re-enable flood-checking for a role").add_sub_option(role("role", "Role").required(true)))
            .add_option(sub("userchannel", "Remove a per-(user, channel) exemption").add_sub_option(user("user", "User").required(true)).add_sub_option(chan("channel", "Channel").required(true))),
        CreateCommand::new("floodlimit")
            .description("Set per-user flood thresholds (override the global ones; 0 = inherit)")
            .add_option(user("user", "User").required(true))
            .add_option(int("channel_threshold", "Their channel threshold (0 = inherit)"))
            .add_option(int("msg_threshold", "Their message threshold (0 = inherit)")),
    ]
}

// ── option helpers ───────────────────────────────────────────────────────────

fn find<'a>(opts: &'a [CommandDataOption], name: &str) -> Option<&'a CommandDataOptionValue> {
    opts.iter().find(|o| o.name == name).map(|o| &o.value)
}
fn get_str(opts: &[CommandDataOption], name: &str) -> Option<String> {
    match find(opts, name) {
        Some(CommandDataOptionValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}
fn get_int(opts: &[CommandDataOption], name: &str) -> Option<i64> {
    match find(opts, name) {
        Some(CommandDataOptionValue::Integer(i)) => Some(*i),
        _ => None,
    }
}
fn get_bool(opts: &[CommandDataOption], name: &str) -> Option<bool> {
    match find(opts, name) {
        Some(CommandDataOptionValue::Boolean(b)) => Some(*b),
        _ => None,
    }
}
fn get_role(opts: &[CommandDataOption], name: &str) -> Option<RoleId> {
    match find(opts, name) {
        Some(CommandDataOptionValue::Role(r)) => Some(*r),
        _ => None,
    }
}
fn get_channel(opts: &[CommandDataOption], name: &str) -> Option<ChannelId> {
    match find(opts, name) {
        Some(CommandDataOptionValue::Channel(c)) => Some(*c),
        _ => None,
    }
}
fn get_user(opts: &[CommandDataOption], name: &str) -> Option<UserId> {
    match find(opts, name) {
        Some(CommandDataOptionValue::User(u)) => Some(*u),
        _ => None,
    }
}
/// The chosen subcommand `(name, its options)`, if this command uses subcommands.
fn subcommand(opts: &[CommandDataOption]) -> Option<(&str, &[CommandDataOption])> {
    opts.first().and_then(|o| match &o.value {
        CommandDataOptionValue::SubCommand(inner) => Some((o.name.as_str(), inner.as_slice())),
        _ => None,
    })
}

/// Bot owner (config) or a member holding Manage Server.
fn authorized(cmd: &CommandInteraction, config: &BotConfig) -> bool {
    if config.is_owner(&cmd.user.id.get().to_string()) {
        return true;
    }
    cmd.member
        .as_ref()
        .and_then(|m| m.permissions)
        .is_some_and(|p| p.contains(Permissions::MANAGE_GUILD))
}

// ── dispatch ─────────────────────────────────────────────────────────────────

/// Handle one application command. Defers ephemerally, runs the action, then
/// edits in the result.
pub async fn dispatch(ctx: &Context, cmd: &CommandInteraction, store: &RedbStore, config: &BotConfig) {
    if !authorized(cmd, config) {
        let resp = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("⛔ You need the Manage Server permission (or to be a bot owner) to use this.")
                .ephemeral(true),
        );
        let _ = cmd.create_response(&ctx.http, resp).await;
        return;
    }
    if cmd.defer_ephemeral(&ctx.http).await.is_err() {
        return;
    }

    let opts = cmd.data.options.as_slice();
    let result: Result<String, String> = match cmd.data.name.as_str() {
        "modstatus" => Ok(render_status(store)),
        "setfilter" => set_filter(store, opts),
        "whitelist" => whitelist(store, opts),
        "exempt" => exempt(store, opts, true),
        "unexempt" => exempt(store, opts, false),
        "userlimit" => user_limit(store, opts),
        "allowinvite" => allow_invite(store, opts),
        "allowserver" => allow_server(store, opts),
        "strikes" => strikes(store, opts),
        "jail" => jail_cmd(ctx, store, cmd, opts).await,
        "unjail" => unjail_cmd(ctx, store, cmd, opts).await,
        "setjail" => set_jail(store, opts),
        "setflood" => set_flood(store, opts),
        "floodexempt" => flood_exempt(store, opts, true),
        "floodunexempt" => flood_exempt(store, opts, false),
        "floodlimit" => flood_limit(store, opts),
        other => Err(format!("unknown command /{other}")),
    };

    let msg = match result {
        Ok(m) => m,
        Err(e) => format!("❌ {e}"),
    };
    let _ = cmd
        .edit_response(&ctx.http, EditInteractionResponse::new().content(msg))
        .await;
}

fn render_status(store: &RedbStore) -> String {
    let f = LinkFilterConfig::load(store);
    let j = JailConfig::load(store);
    let fl = FloodFilterConfig::load(store);
    let strikes = store.list_link_strikes(10_000).len();
    let jails = store.list_jails(10_000).len();
    let flood_action = match fl.action {
        FloodAction::Warn => "warn",
        FloodAction::Delete => "delete",
        FloodAction::Jail => "delete + jail",
    };
    let flood_scope = match fl.scope {
        FloodScope::All => "all messages",
        FloodScope::Attachments => "attachments only",
        FloodScope::AttachmentsOrLinks => "attachments or links",
    };
    format!(
        "**Link filter** — {}\n\
         • guild: `{}`\n• threshold: {} • decay: {} days • warn DM: {}\n\
         • whitelist: {} domains • exempt channels: {} • exempt roles: {}\n\
         • per-user channel exemptions: {} • per-user limits: {}\n\
         • invite filter: {} • allowed invite codes: {} • partner servers: {}\n\
         • active strike records: {}\n\n\
         **Jail** — {}\n\
         • role: `{}` • channel: `{}` • default: {} min • DM: {}\n\
         • currently jailed: {}\n\n\
         **Flood / raid filter** — {}\n\
         • spread: {} channels / {}s • burst: {} msgs / {}s\n\
         • action: {} • scope: {} • warn DM: {}\n\
         • exempt channels: {} • exempt roles: {} • per-user channel exemptions: {} • per-user limits: {}",
        on_off(f.enabled),
        empty_dash(&f.guild_id),
        f.strike_threshold,
        f.decay_days,
        f.warn_user,
        f.whitelist.len(),
        f.exempt_channel_ids.len(),
        f.exempt_role_ids.len(),
        f.exempt_user_channels.len(),
        f.user_thresholds.len(),
        on_off(f.filter_invites),
        f.allowed_invite_codes.len(),
        f.allowed_guild_ids.len(),
        strikes,
        on_off(j.enabled),
        empty_dash(&j.jail_role_id),
        empty_dash(&j.jail_channel_id),
        j.default_minutes,
        j.dm_user,
        jails,
        on_off(fl.enabled),
        fl.channel_threshold,
        fl.channel_window_secs,
        fl.msg_threshold,
        fl.msg_window_secs,
        flood_action,
        flood_scope,
        fl.warn_user,
        fl.exempt_channel_ids.len(),
        fl.exempt_role_ids.len(),
        fl.exempt_user_channels.len(),
        fl.user_overrides.len(),
    )
}

fn set_filter(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let mut f = LinkFilterConfig::load(store);
    let mut changed = Vec::new();
    if let Some(b) = get_bool(opts, "enabled") {
        f.enabled = b;
        changed.push(format!("enabled = {b}"));
    }
    if let Some(t) = get_int(opts, "threshold") {
        f.strike_threshold = t.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("threshold = {}", f.strike_threshold));
    }
    if let Some(d) = get_int(opts, "decay_days") {
        f.decay_days = d.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("decay_days = {}", f.decay_days));
    }
    if let Some(r) = get_role(opts, "jail_role") {
        f.jail_role_id = r.get().to_string();
        changed.push("jail_role set".to_string());
    }
    if let Some(w) = get_bool(opts, "warn_user") {
        f.warn_user = w;
        changed.push(format!("warn_user = {w}"));
    }
    if let Some(b) = get_bool(opts, "filter_invites") {
        f.filter_invites = b;
        changed.push(format!("filter_invites = {b}"));
    }
    if changed.is_empty() {
        return Err("nothing to change — pass at least one option".into());
    }
    f.validate()?;
    f.save(store)?;
    Ok(format!("✅ filter updated: {}", changed.join(", ")))
}

fn set_flood(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let mut f = FloodFilterConfig::load(store);
    let mut changed = Vec::new();
    if let Some(b) = get_bool(opts, "enabled") {
        f.enabled = b;
        changed.push(format!("enabled = {b}"));
    }
    if let Some(t) = get_int(opts, "channel_threshold") {
        f.channel_threshold = t.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("channel_threshold = {}", f.channel_threshold));
    }
    if let Some(w) = get_int(opts, "channel_window") {
        f.channel_window_secs = w.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("channel_window = {}s", f.channel_window_secs));
    }
    if let Some(t) = get_int(opts, "msg_threshold") {
        f.msg_threshold = t.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("msg_threshold = {}", f.msg_threshold));
    }
    if let Some(w) = get_int(opts, "msg_window") {
        f.msg_window_secs = w.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("msg_window = {}s", f.msg_window_secs));
    }
    if let Some(a) = get_str(opts, "action") {
        f.action = match a.as_str() {
            "warn" => FloodAction::Warn,
            "delete" => FloodAction::Delete,
            "jail" => FloodAction::Jail,
            other => return Err(format!("invalid action `{other}`")),
        };
        changed.push(format!("action = {a}"));
    }
    if let Some(s) = get_str(opts, "scope") {
        f.scope = match s.as_str() {
            "all" => FloodScope::All,
            "attachments" => FloodScope::Attachments,
            "attachments_or_links" => FloodScope::AttachmentsOrLinks,
            other => return Err(format!("invalid scope `{other}`")),
        };
        changed.push(format!("scope = {s}"));
    }
    if let Some(r) = get_role(opts, "jail_role") {
        f.jail_role_id = r.get().to_string();
        changed.push("jail_role set".to_string());
    }
    if let Some(d) = get_int(opts, "decay_days") {
        f.decay_days = d.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("decay_days = {}", f.decay_days));
    }
    if let Some(w) = get_bool(opts, "warn_user") {
        f.warn_user = w;
        changed.push(format!("warn_user = {w}"));
    }
    if changed.is_empty() {
        return Err("nothing to change — pass at least one option".into());
    }
    f.validate()?;
    f.save(store)?;
    Ok(format!("✅ flood filter updated: {}", changed.join(", ")))
}

fn flood_exempt(
    store: &RedbStore,
    opts: &[CommandDataOption],
    add: bool,
) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    let mut f = FloodFilterConfig::load(store);
    let msg = match sub {
        "channel" => {
            let c = get_channel(sopts, "channel").ok_or("missing channel")?.get().to_string();
            if add {
                if !f.exempt_channel_ids.contains(&c) {
                    f.exempt_channel_ids.push(c);
                }
                "channel exempted from flood check"
            } else {
                f.exempt_channel_ids.retain(|x| x != &c);
                "channel re-enabled for flood check"
            }
        }
        "role" => {
            let r = get_role(sopts, "role").ok_or("missing role")?.get().to_string();
            if add {
                if !f.exempt_role_ids.contains(&r) {
                    f.exempt_role_ids.push(r);
                }
                "role exempted from flood check"
            } else {
                f.exempt_role_ids.retain(|x| x != &r);
                "role re-enabled for flood check"
            }
        }
        "userchannel" => {
            let u = get_user(sopts, "user").ok_or("missing user")?.get().to_string();
            let c = get_channel(sopts, "channel").ok_or("missing channel")?.get().to_string();
            if add {
                if !f.exempt_user_channels.iter().any(|e| e.user_id == u && e.channel_id == c) {
                    f.exempt_user_channels.push(
                        airforce_modbot_core::link_filter::UserChannelExempt {
                            user_id: u,
                            channel_id: c,
                        },
                    );
                }
                "user exempted in that channel"
            } else {
                f.exempt_user_channels.retain(|e| !(e.user_id == u && e.channel_id == c));
                "per-(user, channel) exemption removed"
            }
        }
        other => return Err(format!("unknown subcommand `{other}`")),
    };
    f.validate()?;
    f.save(store)?;
    Ok(format!("✅ {msg}"))
}

fn flood_limit(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let u = get_user(opts, "user").ok_or("missing user")?.get().to_string();
    let ch = get_int(opts, "channel_threshold").map(|x| x.clamp(0, u32::MAX as i64) as u32);
    let ms = get_int(opts, "msg_threshold").map(|x| x.clamp(0, u32::MAX as i64) as u32);
    if ch.is_none() && ms.is_none() {
        return Err("pass channel_threshold and/or msg_threshold".into());
    }
    let mut f = FloodFilterConfig::load(store);
    f.user_overrides.retain(|o| o.user_id != u);
    let (ct, mt) = (ch.unwrap_or(0), ms.unwrap_or(0));
    let out = if ct == 0 && mt == 0 {
        "per-user flood override removed".to_string()
    } else {
        f.user_overrides.push(FloodUserOverride {
            user_id: u,
            channel_threshold: ct,
            msg_threshold: mt,
        });
        format!("per-user flood limit set (channel={ct}, msg={mt}; 0 = inherit)")
    };
    f.validate()?;
    f.save(store)?;
    Ok(format!("✅ {out}"))
}

fn whitelist(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    let mut f = LinkFilterConfig::load(store);
    match sub {
        "add" => {
            let raw = get_str(sopts, "domain").ok_or("missing domain")?;
            let entry = if let Some(base) = raw.trim().strip_prefix("*.") {
                let b = normalize_host(base);
                if b.is_empty() {
                    return Err("invalid domain".into());
                }
                format!("*.{b}")
            } else {
                normalize_host(&raw)
            };
            if entry.is_empty() {
                return Err("invalid domain".into());
            }
            if f.whitelist.iter().any(|e| e == &entry) {
                return Ok(format!("`{entry}` is already whitelisted"));
            }
            f.whitelist.push(entry.clone());
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ whitelisted `{entry}`"))
        }
        "remove" => {
            let raw = get_str(sopts, "domain").ok_or("missing domain")?;
            let before = f.whitelist.len();
            f.whitelist.retain(|e| e != raw.trim());
            if f.whitelist.len() == before {
                return Ok(format!("`{}` was not on the whitelist", raw.trim()));
            }
            f.save(store)?;
            Ok(format!("✅ removed `{}`", raw.trim()))
        }
        "list" => {
            if f.whitelist.is_empty() {
                Ok("the whitelist is empty".into())
            } else {
                Ok(format!("**Whitelist ({}):**\n{}", f.whitelist.len(), f.whitelist.join("\n")))
            }
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn exempt(store: &RedbStore, opts: &[CommandDataOption], add: bool) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    let mut f = LinkFilterConfig::load(store);
    let verb = if add { "added" } else { "removed" };
    match sub {
        "channel" => {
            let c = get_channel(sopts, "channel").ok_or("missing channel")?.get().to_string();
            if add {
                if !f.exempt_channel_ids.contains(&c) {
                    f.exempt_channel_ids.push(c.clone());
                }
            } else {
                f.exempt_channel_ids.retain(|x| x != &c);
            }
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ channel exemption {verb} (<#{c}>)"))
        }
        "role" => {
            let r = get_role(sopts, "role").ok_or("missing role")?.get().to_string();
            if add {
                if !f.exempt_role_ids.contains(&r) {
                    f.exempt_role_ids.push(r.clone());
                }
            } else {
                f.exempt_role_ids.retain(|x| x != &r);
            }
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ role exemption {verb} (<@&{r}>)"))
        }
        "userchannel" => {
            let u = get_user(sopts, "user").ok_or("missing user")?.get().to_string();
            let c = get_channel(sopts, "channel").ok_or("missing channel")?.get().to_string();
            if add {
                if !f.is_user_channel_exempt(&u, &c) {
                    f.exempt_user_channels.push(UserChannelExempt { user_id: u.clone(), channel_id: c.clone() });
                }
            } else {
                f.exempt_user_channels.retain(|e| !(e.user_id == u && e.channel_id == c));
            }
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ per-user channel exemption {verb} (<@{u}> in <#{c}>)"))
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn user_limit(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let u = get_user(opts, "user").ok_or("missing user")?.get().to_string();
    let t = get_int(opts, "threshold").ok_or("missing threshold")?;
    let mut f = LinkFilterConfig::load(store);
    f.user_thresholds.retain(|x| x.user_id != u);
    if t == 0 {
        f.save(store)?;
        return Ok(format!("✅ removed the per-user limit for <@{u}>"));
    }
    f.user_thresholds.push(UserThreshold { user_id: u.clone(), threshold: t.clamp(0, u32::MAX as i64) as u32 });
    f.validate()?;
    f.save(store)?;
    Ok(format!("✅ <@{u}> will be jailed at {t} strikes"))
}

fn allow_invite(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    let mut f = LinkFilterConfig::load(store);
    match sub {
        "add" => {
            let code = get_str(sopts, "code").ok_or("missing code")?.trim().to_string();
            if code.is_empty() {
                return Err("invalid code".into());
            }
            if f.allowed_invite_codes.iter().any(|c| c == &code) {
                return Ok(format!("`{code}` is already allowed"));
            }
            f.allowed_invite_codes.push(code.clone());
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ allowed invite code `{code}`"))
        }
        "remove" => {
            let code = get_str(sopts, "code").ok_or("missing code")?.trim().to_string();
            let before = f.allowed_invite_codes.len();
            f.allowed_invite_codes.retain(|c| c != &code);
            if f.allowed_invite_codes.len() == before {
                return Ok(format!("`{code}` was not on the invite allowlist"));
            }
            f.save(store)?;
            Ok(format!("✅ removed invite code `{code}`"))
        }
        "list" => {
            if f.allowed_invite_codes.is_empty() {
                Ok("the invite allowlist is empty (only your own server's invites pass)".into())
            } else {
                Ok(format!("**Allowed invite codes ({}):**\n{}", f.allowed_invite_codes.len(), f.allowed_invite_codes.join("\n")))
            }
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn allow_server(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    let mut f = LinkFilterConfig::load(store);
    match sub {
        "add" => {
            let gid = get_str(sopts, "guild_id").ok_or("missing guild_id")?.trim().to_string();
            if gid.parse::<u64>().is_err() {
                return Err("guild_id must be a numeric server ID".into());
            }
            if f.allowed_guild_ids.iter().any(|g| g == &gid) {
                return Ok(format!("`{gid}` is already allowed"));
            }
            f.allowed_guild_ids.push(gid.clone());
            f.validate()?;
            f.save(store)?;
            Ok(format!("✅ allowed invites to server `{gid}`"))
        }
        "remove" => {
            let gid = get_str(sopts, "guild_id").ok_or("missing guild_id")?.trim().to_string();
            let before = f.allowed_guild_ids.len();
            f.allowed_guild_ids.retain(|g| g != &gid);
            if f.allowed_guild_ids.len() == before {
                return Ok(format!("`{gid}` was not on the server allowlist"));
            }
            f.save(store)?;
            Ok(format!("✅ removed server `{gid}`"))
        }
        "list" => {
            if f.allowed_guild_ids.is_empty() {
                Ok("the server allowlist is empty (only your own server's invites pass)".into())
            } else {
                Ok(format!("**Allowed server IDs ({}):**\n{}", f.allowed_guild_ids.len(), f.allowed_guild_ids.join("\n")))
            }
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn strikes(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let (sub, sopts) = subcommand(opts).ok_or("missing subcommand")?;
    match sub {
        "list" => {
            let rows = store.list_link_strikes(25);
            if rows.is_empty() {
                return Ok("no strikes on record".into());
            }
            let lines: Vec<String> = rows
                .iter()
                .map(|s| format!("• <@{}> — {} strike(s){}", s.discord_user_id, s.count, s.last_reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()))
                .collect();
            Ok(format!("**Recent strikes:**\n{}", lines.join("\n")))
        }
        "reset" => {
            let u = get_user(sopts, "user").ok_or("missing user")?.get().to_string();
            store.reset_link_strikes(&u)?;
            Ok(format!("✅ cleared strikes for <@{u}>"))
        }
        other => Err(format!("unknown subcommand {other}")),
    }
}

async fn jail_cmd(ctx: &Context, store: &RedbStore, cmd: &CommandInteraction, opts: &[CommandDataOption]) -> Result<String, String> {
    let guild_id = cmd.guild_id.ok_or("this command must be used in a server")?;
    let target = get_user(opts, "user").ok_or("missing user")?;
    let minutes = get_int(opts, "minutes").map(|m| m.clamp(0, u32::MAX as i64) as u32);
    let reason = get_str(opts, "reason").unwrap_or_else(|| "moderator action".to_string());
    let by = cmd.user.id.to_string();
    jail::jail_member(ctx, store, guild_id, target, &reason, minutes, &by).await?;
    Ok(format!("🔒 jailed <@{}>. Reason: {reason}", target.get()))
}

async fn unjail_cmd(ctx: &Context, store: &RedbStore, cmd: &CommandInteraction, opts: &[CommandDataOption]) -> Result<String, String> {
    let guild_id = cmd.guild_id.ok_or("this command must be used in a server")?;
    let target = get_user(opts, "user").ok_or("missing user")?;
    let by = cmd.user.id.to_string();
    jail::unjail_member(ctx, store, guild_id, target, &by).await?;
    Ok(format!("🔓 released <@{}> and restored their roles", target.get()))
}

fn set_jail(store: &RedbStore, opts: &[CommandDataOption]) -> Result<String, String> {
    let mut j = JailConfig::load(store);
    let mut changed = Vec::new();
    if let Some(b) = get_bool(opts, "enabled") {
        j.enabled = b;
        changed.push(format!("enabled = {b}"));
    }
    if let Some(r) = get_role(opts, "role") {
        j.jail_role_id = r.get().to_string();
        changed.push("role set".to_string());
    }
    if let Some(c) = get_channel(opts, "channel") {
        j.jail_channel_id = c.get().to_string();
        changed.push("channel set".to_string());
    }
    if let Some(m) = get_int(opts, "default_minutes") {
        j.default_minutes = m.clamp(0, u32::MAX as i64) as u32;
        changed.push(format!("default_minutes = {}", j.default_minutes));
    }
    if changed.is_empty() {
        return Err("nothing to change — pass at least one option".into());
    }
    j.validate()?;
    j.save(store)?;
    Ok(format!("✅ jail updated: {}", changed.join(", ")))
}

fn on_off(b: bool) -> &'static str {
    if b {
        "🟢 ENABLED"
    } else {
        "🔴 disabled"
    }
}
fn empty_dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

/// Register all guild commands (bulk overwrite) for `guild`.
pub async fn register(ctx: &Context, guild: GuildId) {
    match guild.set_commands(&ctx.http, command_defs()).await {
        Ok(cmds) => println!("✅ registered {} slash commands", cmds.len()),
        Err(e) => eprintln!("❌ failed to register slash commands: {e}"),
    }
}
