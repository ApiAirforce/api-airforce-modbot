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
use airforce_modbot_core::{JailConfig, JailStore, LinkFilterConfig, StrikeStore};

use crate::config::BotConfig;
use crate::store::RedbStore;
use crate::{commands, jail};

pub struct Handler {
    pub store: Arc<RedbStore>,
    pub config: Arc<BotConfig>,
    /// Guards the expiry-sweep task so a gateway reconnect (another `ready`)
    /// doesn't spawn a second one.
    sweep_started: AtomicBool,
}

impl Handler {
    pub fn new(store: Arc<RedbStore>, config: Arc<BotConfig>) -> Self {
        Self {
            store,
            config,
            sweep_started: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("✅ connected as {} ({})", ready.user.name, ready.user.id);

        // Register the admin slash commands for the configured guild.
        match self.config.guild_id.parse::<u64>() {
            Ok(gid) => commands::register(&ctx, GuildId::new(gid)).await,
            Err(_) if self.config.guild_id.is_empty() => {
                eprintln!("⚠️ no guild_id in config — slash commands not registered")
            }
            Err(_) => eprintln!(
                "⚠️ config guild_id `{}` is not a valid id — slash commands not registered",
                self.config.guild_id
            ),
        }

        // Seed the per-feature config `guild_id` from the bot's configured guild
        // if it is still unset. The monorepo populated this via its admin API;
        // the standalone bot is single-guild, so the filter and jail apply to the
        // guild in `config.toml`. Without this the message filter and the manual
        // jail-role watcher gate on an empty `guild_id` and never fire.
        if !self.config.guild_id.is_empty() {
            let mut lf = LinkFilterConfig::load(&*self.store);
            if lf.guild_id.is_empty() {
                lf.guild_id = self.config.guild_id.clone();
                if let Err(e) = lf.save(&*self.store) {
                    eprintln!("⚠️ could not seed link-filter guild_id: {e}");
                }
            }
            let mut jc = JailConfig::load(&*self.store);
            if jc.guild_id.is_empty() {
                jc.guild_id = self.config.guild_id.clone();
                if let Err(e) = jc.save(&*self.store) {
                    eprintln!("⚠️ could not seed jail guild_id: {e}");
                }
            }
        }

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
                            if let Err(e) = jail::unjail_member(&sweep_ctx, &*store, g, u, "expiry").await {
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
            &*self.store,
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
        let cfg = airforce_modbot_core::JailConfig::load(&*self.store);
        if !cfg.enabled || cfg.guild_id.is_empty() {
            return;
        }
        if event.guild_id.get().to_string() != cfg.guild_id {
            return;
        }
        let Some(jail_role) = cfg.jail_role_id.trim().parse::<u64>().ok().map(RoleId::new) else {
            return;
        };
        let user_id = event.user.id;
        let uid = user_id.to_string();
        let has_jail_role = event.roles.contains(&jail_role);
        let already_jailed = self.store_has_jail(&uid);

        if has_jail_role && !already_jailed {
            if let Err(e) = jail::jail_member(
                &ctx,
                &*self.store,
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
                jail::unjail_member(&ctx, &*self.store, event.guild_id, user_id, "manual-role").await
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
        let cfg = LinkFilterConfig::load(&*self.store);
        let Some(msg_guild) = message.guild_id.map(|g| g.get().to_string()) else {
            return;
        };
        let in_filter_guild =
            cfg.enabled && !cfg.guild_id.is_empty() && msg_guild == cfg.guild_id;
        if !in_filter_guild {
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
        if offenders.is_empty() {
            return;
        }

        // 1) delete the offending message (needs Manage Messages).
        let _ = message.delete(&ctx.http).await;

        // 2) record the strike (atomic + decay-aware).
        let reason = format!("ad link: {}", offenders.join(", "));
        let new_count = self
            .store
            .record_link_strike(&author_id, &cfg.guild_id, &reason, Utc::now().timestamp(), cfg.decay_days)
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
                    &*self.store,
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
    fn store_has_jail(&self, uid: &str) -> bool {
        self.store.get_jail(uid).is_some()
    }
}
