//! The serenity gateway event handler: the anti-ad link filter, the manual
//! jail-role watcher, escape-proof re-apply on rejoin, and the timed-jail
//! expiry sweep. Slash commands (runtime configuration) are added on top of
//! this in `commands.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serenity::all::{
    AuditLogEntry, ChannelId, Context, EditMember, EventHandler, GuildId, GuildMemberUpdateEvent,
    Interaction, Member, Message, Ready, RoleId, UserId,
};
use serenity::async_trait;

use airforce_modbot_core::link_filter::offending_hosts;
use airforce_modbot_core::{
    ActionTracker, AntinukeConfig, AutomodAction, AutomodConfig, AutomodVerdict, CaseAction,
    CompiledBlocklist, DestructiveAction, DuplicateTracker, FloodAction, FloodFilterConfig,
    FloodTracker, GateAction, JailConfig, JailStore, JoinTracker, LinkFilterConfig, MatchMode,
    ModConfig, RaidConfig,
};

use crate::config::BotConfig;
use crate::store::RedbStore;
use crate::{commands, jail};

/// Monotonic process clock in milliseconds, shared by the raid/anti-nuke
/// trackers (a steady local clock is all the sliding windows need).
fn monotonic_ms() -> u64 {
    static EPOCH: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    EPOCH.elapsed().as_millis() as u64
}

/// Map a destructive audit-log action to the anti-nuke counter type (`None` =>
/// not a tracked destructive action, so it is ignored).
fn map_destructive(entry: &AuditLogEntry) -> Option<DestructiveAction> {
    use serenity::model::guild::audit_log::{Action, ChannelAction, MemberAction, RoleAction};
    // Only genuinely *destructive* privileged actions feed the burst counter.
    // Webhook-create is intentionally NOT counted: it deletes nothing, and wiring
    // up several logging/integration webhooks during normal server setup would
    // otherwise trip anti-nuke and strip a real admin (webhook *spam* is a content
    // problem the flood/automod filters handle, not a nuke).
    match entry.action {
        Action::Channel(ChannelAction::Delete) => Some(DestructiveAction::ChannelDelete),
        Action::Role(RoleAction::Delete) => Some(DestructiveAction::RoleDelete),
        Action::Member(MemberAction::BanAdd) => Some(DestructiveAction::Ban),
        Action::Member(MemberAction::Kick) => Some(DestructiveAction::Kick),
        _ => None,
    }
}

/// Cached compiled blocklist for a guild + the config fields it was built from,
/// so the (expensive) regex compilation runs only when an admin changes the
/// blocklist — never per message, which would be a CPU-DoS.
struct AutomodCacheEntry {
    blocklist: Vec<String>,
    match_mode: MatchMode,
    case_insensitive: bool,
    compiled: CompiledBlocklist,
}

pub struct Handler {
    pub store: Arc<RedbStore>,
    pub config: Arc<BotConfig>,
    /// Guards the expiry-sweep task so a gateway reconnect (another `ready`)
    /// doesn't spawn a second one.
    sweep_started: AtomicBool,
    /// Resolve-cache for the Discord-invite sub-filter (code → allowed verdict).
    invite_cache: crate::invite_filter::InviteCache,
    /// Live per-user sliding window for the cross-channel flood filter. In
    /// memory only (state, not config); locked solely for record/evaluate.
    flood_tracker: std::sync::Mutex<FloodTracker>,
    /// Live per-(guild,user) recent-message window for automod's duplicate rule.
    dup_tracker: std::sync::Mutex<DuplicateTracker>,
    /// Per-guild compiled-blocklist cache (rebuilt only when the config changes).
    automod_cache: std::sync::Mutex<HashMap<String, AutomodCacheEntry>>,
    /// Per-guild join-velocity window (raid detection).
    join_tracker: std::sync::Mutex<JoinTracker>,
    /// Per-(guild,actor) destructive-action window (anti-nuke).
    action_tracker: std::sync::Mutex<ActionTracker>,
    /// The bot's own user id (set on `ready`); anti-nuke never acts on it.
    bot_id: AtomicU64,
}

impl Handler {
    pub fn new(store: Arc<RedbStore>, config: Arc<BotConfig>) -> Self {
        Self {
            store,
            config,
            sweep_started: AtomicBool::new(false),
            invite_cache: crate::invite_filter::InviteCache::default(),
            flood_tracker: std::sync::Mutex::new(FloodTracker::new()),
            dup_tracker: std::sync::Mutex::new(DuplicateTracker::new()),
            automod_cache: std::sync::Mutex::new(HashMap::new()),
            join_tracker: std::sync::Mutex::new(JoinTracker::new()),
            action_tracker: std::sync::Mutex::new(ActionTracker::new()),
            bot_id: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("✅ connected as {} ({})", ready.user.name, ready.user.id);
        self.bot_id.store(ready.user.id.get(), Ordering::Relaxed);

        // Register the admin slash commands. A `guild_id` in config.toml registers
        // to that one guild instantly (best for dev or a single server); leaving it
        // empty registers GLOBALLY so the bot works in every server it is in
        // (multi-guild — global propagation can take up to ~1h).
        match self.config.guild_id.parse::<u64>() {
            Ok(gid) => commands::register(&ctx, GuildId::new(gid)).await,
            Err(_) if self.config.guild_id.is_empty() => commands::register_global(&ctx).await,
            Err(_) => eprintln!(
                "⚠️ config guild_id `{}` is not a valid id — slash commands not registered",
                self.config.guild_id
            ),
        }

        // Per-feature config is per-guild now (each config command stamps its own
        // guild_id), so there is nothing to seed here. An existing single-guild DB
        // was migrated to guild-scoped keys at startup (see main.rs).

        // Spawn the timed-jail expiry sweep exactly once. Uses a Context cloned
        // from this event so it needs no separate token handling.
        if !self.sweep_started.swap(true, Ordering::SeqCst) {
            let store = self.store.clone();
            let sweep_ctx = ctx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let now = Utc::now().timestamp();
                    for rec in store.jails_due_for_unjail(now) {
                        let gid = rec.guild_id.parse::<u64>().ok().map(GuildId::new);
                        let uid = rec.discord_user_id.parse::<u64>().ok().map(UserId::new);
                        if let (Some(g), Some(u)) = (gid, uid) {
                            if let Err(e) = jail::unjail_member(&sweep_ctx, &store, g, u, "expiry").await {
                                eprintln!("❌ expiry unjail {} failed: {e}", rec.discord_user_id);
                            }
                        }
                    }
                }
            });
        }
    }

    /// Admin slash commands (runtime configuration).
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(cmd) = interaction {
            commands::dispatch(&ctx, &cmd, &self.store, &self.config).await;
        }
    }

    /// Re-apply the jail when a jailed user rejoins (escape-proofing). Needs the
    /// GUILD_MEMBERS privileged intent.
    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        jail::reapply_if_jailed(&ctx, &self.store, new_member.guild_id, new_member.user.id).await;
        self.raid_check(&ctx, &new_member).await;
    }

    /// Anti-nuke: watch the audit log for destructive privileged actions and, when
    /// one actor crosses the threshold inside the window, strip their (non-managed)
    /// roles + alert — or just alert, in dry-run. Needs VIEW_AUDIT_LOG.
    async fn guild_audit_log_entry_create(&self, ctx: Context, entry: AuditLogEntry, guild_id: GuildId) {
        self.antinuke_check(&ctx, entry, guild_id).await;
    }

    /// Manual jail-role watcher: a moderator hand-assigning the jail role runs
    /// the real-jail snapshot/strip; hand-removing it restores the snapshot.
    /// `jail_member` writes the record before stripping and `unjail_member`
    /// clears it before restoring, so the self-induced GUILD_MEMBER_UPDATE
    /// matches neither branch — no loop, no snapshot-loss race.
    async fn guild_member_update(
        &self,
        ctx: Context,
        _old: Option<Member>,
        _new: Option<Member>,
        event: GuildMemberUpdateEvent,
    ) {
        let guild_str = event.guild_id.get().to_string();
        let cfg = JailConfig::load_for_guild(&*self.store, &guild_str);
        if !cfg.enabled {
            return;
        }
        let Some(jail_role) = cfg.jail_role_id.trim().parse::<u64>().ok().map(RoleId::new) else {
            return;
        };
        let user_id = event.user.id;
        let uid = user_id.to_string();
        let has_jail_role = event.roles.contains(&jail_role);
        let already_jailed = self.store_has_jail(&guild_str, &uid);

        if has_jail_role && !already_jailed {
            if let Err(e) = jail::jail_member(
                &ctx,
                &self.store,
                event.guild_id,
                user_id,
                "jail role assigned manually",
                None,
                "manual-role",
            )
            .await
            {
                eprintln!("❌ manual-jail (role added) for {uid}: {e}");
            }
        } else if !has_jail_role && already_jailed {
            if let Err(e) =
                jail::unjail_member(&ctx, &self.store, event.guild_id, user_id, "manual-role").await
            {
                eprintln!("❌ manual-unjail (role removed) for {uid}: {e}");
            }
        }
    }

    /// The anti-advertising link filter — runs for every human message in the
    /// configured guild.
    async fn message(&self, ctx: Context, message: Message) {
        if message.author.bot {
            return;
        }
        // Cross-channel flood / raid filter runs first (its own config + gates).
        // If it acted on this message, we're done.
        if self.flood_check(&ctx, &message).await {
            return;
        }
        // Content automod (blocklist/caps/mentions/emoji/zalgo/duplicate).
        if self.automod_check(&ctx, &message).await {
            return;
        }
        let Some(msg_guild) = message.guild_id.map(|g| g.get().to_string()) else {
            return;
        };
        let cfg = LinkFilterConfig::load_for_guild(&*self.store, &msg_guild);
        if !cfg.enabled {
            return;
        }
        let channel_id = message.channel_id.get().to_string();
        if cfg.exempt_channel_ids.iter().any(|c| c == &channel_id) {
            return;
        }
        let author_id = message.author.id.get().to_string();

        // Exemptions: bot owners (config), exempt roles, per-(user, channel).
        let is_owner = self.config.is_owner(&author_id);
        let has_exempt_role = message.member.as_ref().is_some_and(|m| {
            m.roles
                .iter()
                .any(|r| cfg.exempt_role_ids.iter().any(|er| er == &r.get().to_string()))
        });
        let user_channel_exempt = cfg.is_user_channel_exempt(&author_id, &channel_id);
        if is_owner || has_exempt_role || user_channel_exempt {
            return;
        }

        let threshold = cfg.threshold_for(&author_id);
        let offenders = offending_hosts(&message.content, &cfg.whitelist);
        // The Discord-invite sub-filter is independent of the host whitelist: a
        // server invite for any guild other than ours (or an allowlisted
        // partner) is advertising too, even when discord.gg is whitelisted.
        let offending_invite = if cfg.filter_invites {
            crate::invite_filter::first_offending_invite(&ctx, &message.content, &cfg, &self.invite_cache).await
        } else {
            None
        };
        if offenders.is_empty() && offending_invite.is_none() {
            return;
        }

        // 1) delete the offending message (needs Manage Messages).
        let _ = message.delete(&ctx.http).await;

        // 2) record the strike (atomic + decay-aware).
        let reason = match offending_invite.as_deref() {
            Some(form) if offenders.is_empty() => {
                format!("ad: non-whitelisted Discord invite {form}")
            }
            Some(form) => {
                format!("ad: links {} + Discord invite {form}", offenders.join(", "))
            }
            None => format!("ad link: {}", offenders.join(", ")),
        };
        let new_count = self
            .store
            .record_link_strike_in(&msg_guild, &author_id, &reason, Utc::now().timestamp(), cfg.decay_days)
            .unwrap_or(0);
        println!("🔗 link-filter: removed message from {author_id} (strike {new_count}/{threshold}) — {reason}");

        // 3) private DM notice.
        if cfg.warn_user {
            if let Ok(dm) = message.author.create_dm_channel(&ctx.http).await {
                let hit = new_count >= threshold;
                let _ = dm
                    .say(
                        &ctx.http,
                        format!(
                            "🚫 Your message was removed — links to non-approved sites aren't allowed here. Strike {new_count}/{threshold}.{}",
                            if hit { " You have been restricted." } else { "" }
                        ),
                    )
                    .await;
            }
        }

        // 4) auto-jail at threshold: prefer the real jail; otherwise fall back to
        //    simply adding the link-filter's own jail role.
        if new_count >= threshold {
            if let Some(gid) = message.guild_id {
                let jailed = jail::try_jail(
                    &ctx,
                    &self.store,
                    gid,
                    message.author.id,
                    "link filter: repeated non-whitelisted links",
                    "link-filter",
                )
                .await;
                if !jailed && !cfg.jail_role_id.is_empty() {
                    if let Ok(rid) = cfg.jail_role_id.parse::<u64>() {
                        match ctx
                            .http
                            .add_member_role(gid, message.author.id, RoleId::new(rid), Some("link-filter: strike threshold"))
                            .await
                        {
                            Ok(()) => println!("🔒 link-filter: added jail role {rid} to {author_id}"),
                            Err(e) => eprintln!(
                                "❌ link-filter: FAILED to add jail role {rid} to {author_id}: {e} — check Manage Roles + role hierarchy"
                            ),
                        }
                    }
                }
            }
        }
    }
}

impl Handler {
    fn store_has_jail(&self, guild_id: &str, uid: &str) -> bool {
        self.store.get_jail_in(guild_id, uid).is_some()
    }

    /// Post a plain alert to the configured mod-log channel (no-op if unset).
    async fn alert(&self, ctx: &Context, guild: &str, text: &str) {
        let mc = ModConfig::load_for_guild(&*self.store, guild);
        if let Ok(chan) = mc.mod_log_channel_id.parse::<u64>() {
            let _ = ChannelId::new(chan).say(&ctx.http, text).await;
        }
    }

    /// Join-raid defense: screen each new member (account age / avatar) and track
    /// join velocity. A burst latches a server lockdown (every join then gets
    /// `gate_action`); a member that fails the gate or arrives during a lockdown
    /// is kicked / banned / quarantined, with a case + mod-log entry.
    async fn raid_check(&self, ctx: &Context, member: &Member) {
        let guild = member.guild_id.get().to_string();
        let cfg = RaidConfig::load_for_guild(&*self.store, &guild);
        if !cfg.enabled {
            return;
        }
        let now = Utc::now().timestamp();
        let age_secs = now - member.user.id.created_at().unix_timestamp();
        let has_avatar = member.user.avatar.is_some();

        // Join velocity → latch lockdown on a burst.
        let raid = {
            let mut t = self.join_tracker.lock().unwrap();
            t.record_join(&guild, monotonic_ms(), cfg.join_threshold, cfg.join_window_secs)
        };
        let mut lockdown = cfg.lockdown_active;
        if raid && !cfg.lockdown_active {
            let mut latched = cfg.clone();
            latched.lockdown_active = true;
            let _ = latched.save_for_guild(&*self.store, &guild);
            lockdown = true;
            self.alert(ctx, &guild, &format!(
                "🚨 **Raid lockdown engaged** — {}+ joins in {}s. Every new join is now met with `{:?}`. Lift with `/lockdown off`.",
                cfg.join_threshold, cfg.join_window_secs, cfg.gate_action
            )).await;
        }

        // Act when locked down, otherwise when the member fails the join gate.
        let action = if lockdown {
            Some(cfg.gate_action)
        } else {
            cfg.screen_join(age_secs, has_avatar)
        };
        let Some(act) = action else {
            return;
        };
        let reason = if lockdown { "raid lockdown" } else { "join gate (account age / avatar)" };
        let uid = member.user.id;
        // Enforce, then log — but only write the case + mod-log entry if the
        // Discord action actually SUCCEEDED. Logging a "kicked/banned/jailed" case
        // for an enforcement that failed would fill the mod-log with phantom
        // actions exactly when admins triage a raid (the same reason the warn
        // escalation calls jail_member directly instead of try_jail).
        let outcome: Result<CaseAction, String> = match act {
            GateAction::Kick => member
                .guild_id
                .kick_with_reason(&ctx.http, uid, reason)
                .await
                .map(|_| CaseAction::Kick)
                .map_err(|e| e.to_string()),
            GateAction::Ban => member
                .guild_id
                .ban_with_reason(&ctx.http, uid, 0, reason)
                .await
                .map(|_| CaseAction::Ban)
                .map_err(|e| e.to_string()),
            GateAction::Quarantine => {
                jail::jail_member(ctx, &self.store, member.guild_id, uid, reason, None, "raid")
                    .await
                    .map(|_| CaseAction::Jail)
            }
        };
        let case_action = match outcome {
            Ok(a) => a,
            Err(e) => {
                eprintln!("❌ raid gate: failed to {act:?} {uid}: {e}");
                return;
            }
        };
        let detail = format!("raid: {reason}");
        let id = self
            .store
            .add_case(&guild, &uid.get().to_string(), "AutoMod", case_action, &detail, now, None)
            .unwrap_or(0);
        commands::post_modlog(ctx, &self.store, &guild, id, case_action, uid, "AutoMod", &detail, None).await;
    }

    /// Anti-nuke core (called from the audit-log event): count one actor's
    /// destructive actions and, on a trip, strip their non-managed roles + alert
    /// (or alert only, in dry-run). The bot itself, the guild owner, and trusted
    /// actors never trip.
    async fn antinuke_check(&self, ctx: &Context, entry: AuditLogEntry, guild_id: GuildId) {
        let guild = guild_id.get().to_string();
        let cfg = AntinukeConfig::load_for_guild(&*self.store, &guild);
        if !cfg.enabled || cfg.max_actions == 0 {
            return;
        }
        let Some(kind) = map_destructive(&entry) else {
            return;
        };
        let actor = entry.user_id;
        let actor_s = actor.get().to_string();
        if actor.get() == self.bot_id.load(Ordering::Relaxed) || cfg.is_trusted(&actor_s) {
            return;
        }
        let tripped = {
            let mut t = self.action_tracker.lock().unwrap();
            t.record_action(&format!("{guild}:{actor_s}"), monotonic_ms(), cfg.max_actions, cfg.window_secs)
        };
        if !tripped {
            return;
        }
        let now = Utc::now().timestamp();
        let detail = format!(
            "anti-nuke: {}+ destructive actions (last: {}) in {}s",
            cfg.max_actions,
            kind.label(),
            cfg.window_secs
        );

        // The guild owner is ALWAYS exempt. Resolve the owner fail-CLOSED: if we
        // cannot determine who it is, take NO action — a missed nuke is
        // recoverable, but stripping the legitimate owner is catastrophic. Try the
        // cache first (no network, and it stays populated even while the HTTP API
        // is rate-limited by the very flood a nuke produces); fall back to HTTP.
        let cached_owner = guild_id.to_guild_cached(&ctx.cache).map(|g| g.owner_id);
        let owner_id = match cached_owner {
            Some(id) => Some(id),
            None => guild_id.to_partial_guild(&ctx.http).await.map(|g| g.owner_id).ok(),
        };
        let Some(owner_id) = owner_id else {
            eprintln!("⚠️ anti-nuke: could not resolve owner of guild {guild} — refusing to strip {actor_s} (fail-closed)");
            self.alert(ctx, &guild, &format!("⚠️ **Anti-nuke** detected <@{actor_s}> ({detail}) but could NOT verify the guild owner — taking no action to avoid stripping the owner by mistake. Investigate manually.")).await;
            return;
        };
        if owner_id == actor {
            return;
        }

        if cfg.dry_run {
            self.alert(ctx, &guild, &format!("🚨 **[DRY-RUN]** anti-nuke would strip <@{actor_s}> — {detail}")).await;
            return;
        }

        // Strip the rogue actor's roles, preserving managed ones (Discord rejects
        // the edit otherwise — same constraint as the jail). Resolve the role set
        // fail-CLOSED: on a fetch error do NOT fall back to an empty keep-set —
        // that would try to remove ALL roles (incl. managed) and Discord rejects
        // the whole edit, so the strip would silently no-op. Alert + skip the
        // strip instead; the detection is still logged as a case below.
        match (
            guild_id.member(&ctx.http, actor).await,
            guild_id.roles(&ctx.http).await,
        ) {
            (Ok(member), Ok(roles)) => {
                let keep: Vec<RoleId> = member
                    .roles
                    .iter()
                    .filter(|r| roles.get(r).is_some_and(|role| role.managed))
                    .copied()
                    .collect();
                match guild_id
                    .edit_member(&ctx.http, actor, EditMember::new().roles(keep).audit_log_reason("anti-nuke"))
                    .await
                {
                    Ok(_) => self.alert(ctx, &guild, &format!("🚨 **Anti-nuke triggered** — stripped <@{actor_s}>'s roles. {detail}")).await,
                    Err(e) => {
                        eprintln!("❌ anti-nuke: failed to strip {actor_s}: {e}");
                        self.alert(ctx, &guild, &format!("🚨 **Anti-nuke** detected <@{actor_s}> ({detail}) but FAILED to strip roles ({e}) — check the bot's role hierarchy.")).await;
                    }
                }
            }
            _ => {
                eprintln!("❌ anti-nuke: could not resolve {actor_s}'s roles — not stripping (fail-closed)");
                self.alert(ctx, &guild, &format!("🚨 **Anti-nuke** detected <@{actor_s}> ({detail}) but could NOT resolve their roles to strip safely — manual intervention needed.")).await;
            }
        }
        let id = self.store.add_case(&guild, &actor_s, "AutoMod", CaseAction::Note, &detail, now, None).unwrap_or(0);
        commands::post_modlog(ctx, &self.store, &guild, id, CaseAction::Note, actor, "AutoMod", &detail, None).await;
    }

    /// Content automod: scans message text for the configured rules (blocklist /
    /// caps / mentions / emoji / zalgo + duplicate-in-a-window) and, on a trip,
    /// deletes the message and applies the configured action (warn / delete /
    /// timeout / jail) with a shared strike, a numbered case, and a mod-log entry.
    /// Returns `true` when it handled the message.
    async fn automod_check(&self, ctx: &Context, message: &Message) -> bool {
        let Some(msg_guild) = message.guild_id.map(|g| g.get().to_string()) else {
            return false;
        };
        let cfg = AutomodConfig::load_for_guild(&*self.store, &msg_guild);
        if !cfg.enabled {
            return false;
        }
        let channel_id = message.channel_id.get().to_string();
        if cfg.exempt_channel_ids.iter().any(|c| c == &channel_id) {
            return false;
        }
        let author_id = message.author.id.get().to_string();
        let is_owner = self.config.is_owner(&author_id);
        let has_exempt_role = message.member.as_ref().is_some_and(|m| {
            m.roles
                .iter()
                .any(|r| cfg.exempt_role_ids.iter().any(|er| er == &r.get().to_string()))
        });
        if is_owner || has_exempt_role || cfg.is_user_channel_exempt(&author_id, &channel_id) {
            return false;
        }

        // Stateless rules first (using a CACHED compiled blocklist — rebuilt only
        // when this guild's blocklist/mode/case config actually changes, never per
        // message), then the duplicate-in-a-window rule (stateful).
        let mut verdict = {
            let mut cache = self.automod_cache.lock().unwrap();
            let stale = cache.get(&msg_guild).is_none_or(|e| {
                e.blocklist != cfg.blocklist
                    || e.match_mode != cfg.match_mode
                    || e.case_insensitive != cfg.case_insensitive
            });
            if stale {
                cache.insert(
                    msg_guild.clone(),
                    AutomodCacheEntry {
                        blocklist: cfg.blocklist.clone(),
                        match_mode: cfg.match_mode,
                        case_insensitive: cfg.case_insensitive,
                        compiled: CompiledBlocklist::build(&cfg),
                    },
                );
            }
            // Lock held only across the synchronous evaluate (no await inside).
            cfg.evaluate(&message.content, &cache.get(&msg_guild).unwrap().compiled)
        };
        if verdict.is_none() && cfg.duplicate_threshold >= 2 {
            let now_ms = {
                static EPOCH: std::sync::LazyLock<std::time::Instant> =
                    std::sync::LazyLock::new(std::time::Instant::now);
                EPOCH.elapsed().as_millis() as u64
            };
            let key = format!("{msg_guild}:{author_id}");
            let tripped = {
                let mut t = self.dup_tracker.lock().unwrap();
                t.record_and_check(&key, &message.content, now_ms, cfg.duplicate_threshold, cfg.duplicate_window_secs)
            };
            if tripped {
                verdict = Some(AutomodVerdict {
                    rule: "duplicate",
                    reason: format!("{}x duplicate in {}s", cfg.duplicate_threshold, cfg.duplicate_window_secs),
                });
            }
        }
        let Some(v) = verdict else {
            return false;
        };

        // 1) delete the offending message.
        let _ = message.delete(&ctx.http).await;

        let now = Utc::now().timestamp();
        let reason = format!("automod [{}]: {}", v.rule, v.reason);
        let (case_action, duration_secs, strike) = match cfg.action {
            AutomodAction::Warn => (CaseAction::Warn, None, false),
            AutomodAction::Delete => (CaseAction::Warn, None, true),
            AutomodAction::Timeout => (
                CaseAction::Timeout,
                Some(cfg.timeout_minutes.clamp(1, 40_320) as u64 * 60),
                true,
            ),
            AutomodAction::Jail => (CaseAction::Jail, None, true),
        };

        // 2) strike (shared with the link/flood quarantine system).
        if strike {
            let _ = self.store.record_link_strike_in(&msg_guild, &author_id, &reason, now, 0);
        }

        // 3) apply the Discord action.
        if let Some(gid) = message.guild_id {
            match cfg.action {
                AutomodAction::Timeout => {
                    let mins = cfg.timeout_minutes.clamp(1, 40_320);
                    if let Ok(ts) = serenity::all::Timestamp::from_unix_timestamp(now + mins as i64 * 60) {
                        if let Err(e) = gid
                            .edit_member(
                                &ctx.http,
                                message.author.id,
                                serenity::all::EditMember::new()
                                    .disable_communication_until_datetime(ts)
                                    .audit_log_reason("automod"),
                            )
                            .await
                        {
                            eprintln!("❌ automod timeout failed for {author_id}: {e}");
                        }
                    }
                }
                AutomodAction::Jail => {
                    jail::try_jail(ctx, &self.store, gid, message.author.id, &reason, "automod").await;
                }
                AutomodAction::Warn | AutomodAction::Delete => {}
            }
        }

        // 4) numbered case + mod-log embed.
        let id = self
            .store
            .add_case(&msg_guild, &author_id, "AutoMod", case_action, &reason, now, duration_secs)
            .unwrap_or(0);
        commands::post_modlog(ctx, &self.store, &msg_guild, id, case_action, message.author.id, "AutoMod", &reason, duration_secs).await;

        // 5) DM notice.
        if cfg.warn_user {
            if let Ok(dm) = message.author.create_dm_channel(&ctx.http).await {
                let _ = dm
                    .say(&ctx.http, format!("🛡️ Your message was removed by automod ({}).", v.rule))
                    .await;
            }
        }
        true
    }

    /// Cross-channel flood / raid filter. Records every counting message into a
    /// per-user sliding window; on a trip it bulk-deletes the burst across
    /// channels, records a strike, and (by config) jails + DMs. Returns `true`
    /// when it handled the message so the caller stops processing it.
    async fn flood_check(&self, ctx: &Context, message: &Message) -> bool {
        let Some(msg_guild) = message.guild_id.map(|g| g.get().to_string()) else {
            return false;
        };
        let cfg = FloodFilterConfig::load_for_guild(&*self.store, &msg_guild);
        if !cfg.enabled {
            return false;
        }
        let channel_id = message.channel_id.get().to_string();
        if cfg.exempt_channel_ids.iter().any(|c| c == &channel_id) {
            return false;
        }
        let author_id = message.author.id.get().to_string();
        let is_owner = self.config.is_owner(&author_id);
        let has_exempt_role = message.member.as_ref().is_some_and(|m| {
            m.roles
                .iter()
                .any(|r| cfg.exempt_role_ids.iter().any(|er| er == &r.get().to_string()))
        });
        if is_owner || has_exempt_role || cfg.is_user_channel_exempt(&author_id, &channel_id) {
            return false;
        }

        // Only messages matching the configured scope count toward the window.
        let has_attachment = !message.attachments.is_empty();
        let has_link = {
            let c = message.content.to_ascii_lowercase();
            c.contains("http://") || c.contains("https://") || c.contains("discord.gg/")
        };
        if !cfg.message_counts(has_attachment, has_link) {
            return false;
        }

        // Monotonic process clock (ms). Serenity's Timestamp is `time`-based; a
        // steady local clock is all the sliding window needs.
        let now_ms = {
            static EPOCH: std::sync::LazyLock<std::time::Instant> =
                std::sync::LazyLock::new(std::time::Instant::now);
            EPOCH.elapsed().as_millis() as u64
        };
        let (ch_thr, ms_thr) = cfg.thresholds_for(&author_id);
        // Key the sliding window per (guild, user) so a user's floods never merge
        // across servers; thresholds still resolve on the bare user id (overrides).
        let flood_key = format!("{msg_guild}:{author_id}");
        // Hold the lock ONLY for record/evaluate — never across an await below.
        let verdict = {
            let mut tracker = self.flood_tracker.lock().unwrap();
            tracker.record_and_check(
                &flood_key,
                &channel_id,
                &message.id.get().to_string(),
                now_ms,
                &cfg,
                ch_thr,
                ms_thr,
            )
        };
        let Some(v) = verdict else {
            return false;
        };

        // 1) bulk-delete the burst across channels.
        let mut deleted = 0u32;
        for (cid, mid) in &v.messages_to_delete {
            if let (Ok(c), Ok(m)) = (cid.parse::<u64>(), mid.parse::<u64>()) {
                if serenity::all::ChannelId::new(c)
                    .delete_message(&ctx.http, serenity::all::MessageId::new(m))
                    .await
                    .is_ok()
                {
                    deleted += 1;
                }
            }
        }
        // 2) record a strike (decay-aware; reuses the link strike store).
        let new_count = self
            .store
            .record_link_strike_in(&msg_guild, &author_id, &v.reason, Utc::now().timestamp(), cfg.decay_days)
            .unwrap_or(0);
        println!(
            "🌊 flood-filter: {} — removed {deleted} msg(s) from {author_id} (strike {new_count})",
            v.reason
        );

        // 3) act per config. Jail is the default for raid floods.
        let do_jail = matches!(cfg.action, FloodAction::Jail);
        if do_jail {
            if let Some(gid) = message.guild_id {
                let jailed = jail::try_jail(
                    ctx,
                    &self.store,
                    gid,
                    message.author.id,
                    &v.reason,
                    "flood-filter",
                )
                .await;
                if !jailed && !cfg.jail_role_id.is_empty() {
                    if let Ok(rid) = cfg.jail_role_id.parse::<u64>() {
                        if let Err(e) = ctx
                            .http
                            .add_member_role(
                                gid,
                                message.author.id,
                                RoleId::new(rid),
                                Some("flood-filter: trip"),
                            )
                            .await
                        {
                            eprintln!(
                                "❌ flood-filter: FAILED to add jail role {rid} to {author_id}: {e}"
                            );
                        }
                    }
                }
            }
        }
        // 4) DM notice.
        if cfg.warn_user {
            if let Ok(dm) = message.author.create_dm_channel(&ctx.http).await {
                let _ = dm
                    .say(
                        &ctx.http,
                        format!(
                            "🌊 Your messages were removed for posting too fast across \
                             channels.{}",
                            if do_jail {
                                " You have been restricted — contact a mod."
                            } else {
                                ""
                            }
                        ),
                    )
                    .await;
            }
        }
        true
    }
}
