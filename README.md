# api-airforce-modbot

A small, self-hostable **Discord moderation bot** focused on three things it does
well:

- **Anti-advertising link filter** — deletes messages linking to non-whitelisted
  domains, counts a strike against the author, DMs them a private notice, and
  auto-jails repeat offenders. The whitelist supports apex + wildcard (`*.`)
  matches; strikes decay on a configurable window; thresholds and exemptions are
  tunable down to an individual user or a single (user, channel) pair.
- **Cross-channel flood / raid filter** — catches an account blasting the same
  thing across many channels (or hammering one) in seconds: it bulk-deletes the
  whole burst, strikes the author, and (by default) jails them immediately. Both
  triggers — distinct-channels-per-window and messages-per-window — plus the
  action (warn / delete / jail), which messages count (all / attachments / links),
  and per-user/role/channel exemptions are all tunable at runtime.
- **A "real" jail** — instead of just adding a muted role, it **snapshots** the
  member's roles, **strips** them, and **restores** them on release. The jail
  survives bot restarts and is **escape-proof**: leaving and rejoining the server
  re-applies it. A moderator hand-assigning the jail role triggers the same
  snapshot/strip automatically.

Everything is configured at runtime through **slash commands** — no web panel, no
database to administer. It powers the [api.airforce](https://api.airforce)
community Discord and is open source so anyone can run it.

## Features

- Domain whitelist (`example.com` matches subdomains; `*.example.com` matches
  subdomains only), tolerant of pasted `https://`/`www.`/paths.
- Strike counting with optional decay; per-user and per-(user, channel)
  exemptions; whole-channel and whole-role exemptions; per-user strike limits.
- Cross-channel flood/raid detection with a per-user sliding window (bounded to
  active posters), three triggers (channel-spread, burst, and an optional
  identical-content rule), configurable action and scope, and the same
  exemption/per-user-override surface as the link filter. Bursts are removed with
  Discord's **bulk-delete** (single-delete fallback for stragglers), and a
  restart no longer instantly forgives an in-progress raider (optional persisted
  cooldown).
- Escape-proof role-snapshot jail with timed or indefinite sentences and an
  automatic expiry sweep.
- Standard mod toolkit: ban / kick / native timeout / warn with **auto-escalation**
  (N warns within a window → timeout, jail, or ban), a numbered **case log** per
  user, and a **mod-log channel** every action is posted to.
- Content automod: a **blocklist** (word / substring / regex), **anti-caps**,
  **mention-** and **emoji-spam**, **zalgo**, and **duplicate-message** detection —
  each with a configurable action (delete / strike / timeout / jail) and the same
  channel/role/user exemptions as the other filters.
- Anti-raid: a **join gate** (account-age / no-avatar → kick/ban/quarantine),
  join-velocity **auto-lockdown**, and **anti-nuke** — a rogue or compromised
  admin mass-deleting channels/roles or mass-banning gets their roles stripped +
  an alert (with a **dry-run** mode and a trusted-actor allowlist).
- Multi-guild: one instance moderates many servers, each with fully isolated
  config, strikes, jails, and cases.
- 33 admin slash commands, gated to bot owners or members with **Manage Server**.
- Single self-contained binary, an embedded [`redb`](https://github.com/cberner/redb)
  database (one file), and a tiny `config.toml`. No external services.

## Setup

### 1. Create the application & bot

1. Open the [Discord Developer Portal](https://discord.com/developers/applications)
   → **New Application**.
2. **Bot** tab → **Reset Token** → copy it (keep it secret).

### 2. Enable the gateway intents

In the **Bot** tab, under **Privileged Gateway Intents**, enable:

- ✅ **MESSAGE CONTENT INTENT** (to read messages for the link filter)
- ✅ **SERVER MEMBERS INTENT** (to re-apply the jail when a user rejoins)

### 3. Invite the bot

Use this URL (replace `YOUR_APP_ID` with your application's Client ID):

```text
https://discord.com/oauth2/authorize?client_id=YOUR_APP_ID&scope=bot+applications.commands&permissions=1099780140166
```

`1099780140166` grants the permissions the full toolkit needs: **View
Channels**, **Send Messages**, **Manage Messages** (delete ads + bulk-delete
floods), **Read Message History**, **Manage Roles** (the jail + anti-nuke
strip), **Kick Members**, **Ban Members**, **Timeout Members** (the mod
actions), and **View Audit Log** (anti-nuke). If you only want the link filter +
jail, the narrower `268512256` (the first five) is enough.

### 4. Set up the Jail role

1. Create a role (e.g. **Jailed**) and drag it **below** the bot's own role in
   the role list — Discord won't let the bot assign a role above its highest one.
2. Create a `#jail` channel.
3. Configure the jail role's permission **overwrites**: deny **View Channel** on
   your categories/channels, and allow **View Channel** only on `#jail`. (The
   bot strips a jailed member's roles down to just this one, so these overwrites
   are what actually hide the server from them.)

### 5. Configure & run

Copy [`config.example.toml`](config.example.toml) to `config.toml` and set your
`guild_id` (right-click your server → **Copy Server ID** with Developer Mode on)
and optional `owner_ids`. The **token** comes from the `DISCORD_TOKEN`
environment variable (preferred) or the `token` field in the file.

**With Docker (recommended):**

```sh
cp config.example.toml config.toml   # edit guild_id / owner_ids
echo "DISCORD_TOKEN=your-bot-token" > .env
docker compose up -d
```

**From source** (needs a Rust toolchain and, on first build, `cmake` for the TLS
layer):

```sh
cp config.example.toml config.toml   # edit guild_id / owner_ids
export DISCORD_TOKEN=your-bot-token
cargo run --release
```

On connect the bot registers its slash commands to your guild and prints
`✅ connected as …`.

### 6. Turn it on

The filter and jail start **disabled**. Configure them with slash commands
(needs Manage Server or a bot-owner id):

```text
/setjail enabled:true role:@Jailed channel:#jail
/setfilter enabled:true threshold:3 jail_role:@Jailed
/whitelist add domain:discord.com
/whitelist add domain:*.youtube.com
/modstatus
```

That's it — non-whitelisted links now get removed and repeat offenders jailed.

## Commands

| Command | What it does |
| --- | --- |
| `/modstatus` | Show the current filter & jail configuration and counts |
| `/setfilter` | Set `enabled`, `threshold`, `decay_days`, `jail_role`, `warn_user` (only what you pass) |
| `/whitelist add\|remove\|list` | Manage the domain whitelist |
| `/exempt channel\|role\|userchannel` | Add a filter exemption |
| `/unexempt channel\|role\|userchannel` | Remove a filter exemption |
| `/userlimit` | Set a per-user strike threshold (0 removes it) |
| `/strikes list\|reset` | View recent strikes or clear a user's |
| `/jail` | Restrict a member (`user`, optional `minutes`, `reason`) |
| `/unjail` | Release a member and restore their roles |
| `/setjail` | Set `enabled`, `role`, `channel`, `default_minutes` |
| `/setflood` | Configure the flood/raid filter: `enabled`, `channel_threshold`, `channel_window`, `msg_threshold`, `msg_window`, `same_content_threshold`, `trip_cooldown`, `action`, `scope`, `jail_role`, `decay_days`, `warn_user` (only what you pass) |
| `/floodexempt channel\|role\|userchannel` | Add a flood-filter exemption |
| `/floodunexempt channel\|role\|userchannel` | Remove a flood-filter exemption |
| `/floodlimit` | Set per-user flood thresholds (`channel_threshold`, `msg_threshold`; 0 inherits) |
| `/ban` \| `/kick` | Ban or kick a member (records a case + posts to the mod-log) |
| `/timeout` | Native Discord timeout, `minutes` (max 28 days) |
| `/warn` | Warn a member; auto-escalates per `/setescalation` |
| `/note` | Attach a moderator note to a member (logged as a case) |
| `/cases` \| `/case` | List a user's cases, or show one case by number |
| `/setmodlog` | Set the channel where moderation actions are posted |
| `/setescalation` | Configure warn auto-escalation (`threshold`, `window_days`, `action`, `timeout_minutes`) |
| `/allowinvite` add\|remove\|list | Manage the allowed Discord invite codes |
| `/allowserver` add\|remove\|list | Manage allowed partner server (guild) IDs |
| `/automod` | Configure content automod (caps/mentions/emoji/zalgo/duplicate + action) |
| `/blocklist` add\|remove\|list | Manage the automod blocklist (word / substring / regex) |
| `/automodexempt` \| `/automodunexempt` | Add/remove an automod exemption (channel/role/user-channel) |
| `/setraid` | Configure join-raid protection (join gate + join-velocity lockdown) |
| `/lockdown` on\|off | Engage or lift a raid lockdown |
| `/setantinuke` | Configure anti-nuke (mass-action protection; has a dry-run mode) |
| `/raidtrust` add\|remove\|list | Manage anti-nuke trusted actors |

The flood filter starts **disabled**; turn it on with e.g.
`/setflood enabled:true channel_threshold:3 channel_window:10 action:jail` and it
will quarantine accounts that post across 3+ channels within 10 seconds.

## How it works

A small Cargo workspace, split along a clean **ports & adapters** seam:

- [`core/`](core) — `airforce-modbot-core`: the **platform-agnostic** moderation
  logic (link detection, whitelist matching, strike-decay math, the config
  shapes) and the storage/config **ports**. Pure, fully unit-tested, depends on
  no Discord library and no concrete database.
- [`bot/`](bot) — `airforce-modbot`: the runnable bot. Implements the core's
  ports over an embedded `redb` store, loads `config.toml`, connects to the
  Discord gateway and exposes the slash commands.

This is the same core that runs inside api.airforce; the seam is what lets it be
lifted out and self-hosted.

## Development

```sh
# The pure core builds and tests with no system dependencies:
cargo test -p airforce-modbot-core

# The whole workspace (bot included) needs a C toolchain for the TLS stack
# (standard on Linux/macOS and in the Docker build):
cargo build --workspace
cargo test --workspace
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
