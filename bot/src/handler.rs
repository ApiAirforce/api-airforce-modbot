//! The serenity gateway event handler: the anti-ad link filter, the manual
//! jail-role watcher, escape-proof re-apply on rejoin, and the timed-jail
//! expiry sweep. Slash commands (runtime configuration) are added on top of
//! this in `commands.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serenity::all::{
    Context, EventHandler, GuildId, GuildMemberUpdateEvent, Interaction, Member, Message, Ready,
    RoleId, UserId,
};
use serenity::async_trait;

use airforce_modbot_core::link_filter::offending_hosts;
use airforce_modbot_core::{
    FloodAction, FloodFilterConfig, FloodTracker, JailConfig, JailStore, LinkFilterConfig,
};

use crate::config::BotConfig;
use crate::store::RedbStore;
use crate::{commands, jail};

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
}

impl Handler {
    pub fn new(store: Arc<RedbStore>, config: Arc<BotConfig>) -> Self {
        Self {
            store,
            config,
            sweep_started: AtomicBool::new(false),
            invite_cache: crate::invite_filter::InviteCache::default(),
            flood_tracker: std::sync::Mutex::new(FloodTracker::new()),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("✅ connected as {} ({})", ready.user.name, ready.user.id);

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
        jail::reapply_if_jailed(
            &ctx,
            &self.store,
            new_member.guild_id,
            new_member.user.id,
        )
        .await;
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
