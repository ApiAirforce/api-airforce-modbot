# Plan 00 — Multi-Guild Foundation

**Goal:** one bot instance serves many servers, with **fully isolated** config,
strikes, jails, and (later) all per-guild feature state. Prerequisite for the
hosted-for-many model and for every later sub-plan's data.

**Status:** ☐ todo · **Effort:** ~3–5 d · **Depends on:** nothing (lands first).

## Why first

Retrofitting guild isolation after building features means re-touching every
feature's storage. The data model must be guild-keyed from the start.

## Current state (grounded)

- `bot/config.rs` `BotConfig` has a single mandatory `guild_id`; commands
  register to that one guild (`bot/src/main.rs`).
- `core/ports.rs`:
  - `StrikeStore::record_link_strike(user, guild, …)` **takes** `guild_id`, and
    `LinkStrike`/`JailRecord` **carry** `guild_id` — good.
  - **But** `bot/store.rs` keys the `strikes` and `jails` tables by
    **`user_id` alone** (`t.insert(discord_user_id, …)`), and
    `get_jail(user)` / `remove_jail(user)` / `reset_link_strikes(user)` /
    `jails_due_for_unjail(now)` have **no guild parameter**. → cross-server
    collisions: a user jailed in server A is "jailed" in server B's records too.
- Config blobs (`get/set_config_blob(key)`) are keyed by a bare string
  (`link_filter_config`, `jail_config`, `flood_filter_config`) — **global**, not
  per guild.

## Key design decision — backend blast-radius

`core`'s `StrikeStore`/`JailStore`/`ConfigStore` are **vendored read-only into
the api.airforce backend**, which implements them over its own DB and only ever
serves **one** guild. Two ways to add guild-scoping:

- **(A) Change the port signatures** (add `guild_id` to `get_jail`, `remove_jail`,
  `reset_link_strikes`, config keys). Cleanest in the bot, but **breaks the
  backend adapter** until re-synced + updated.
- **(B) Composite keys in the bot adapter only** — the **bot** forms the storage
  key as `"{guild_id}:{user_id}"` / `"{guild_id}:{blob_key}"` and the `core`
  port **signatures stay unchanged**. The single-guild backend is untouched;
  multi-guild lives entirely in the standalone bot's `redb` adapter.

**Chosen: (B), with a small additive extension.** Keep existing port signatures
stable. Where a guild-scoped *query* is genuinely needed (e.g. "all jails due in
**any** guild" for the expiry sweep — already global, fine; "list jails for guild
G" for an admin command), add **new** guild-aware methods rather than changing
old ones. This keeps the backend compiling and lets us re-sync `core` safely.

> If (B) gets ugly anywhere, revisit — but default to not breaking the backend.

## Phases

### Phase 0.1 — `BotConfig` goes multi-guild
- `guild_id` becomes **optional** (`Option<String>`): when set, register commands
  to that guild for **instant** dev iteration; when unset, register **globally**
  (propagates in ~1h, normal for multi-guild bots).
- Add optional `owner_ids` stays global (bot operators). Per-guild admin stays
  "Manage Server or owner".
- **Tests:** `config.rs` parse tests for present/absent `guild_id`; token
  resolution unchanged.

### Phase 0.2 — Guild-keyed store adapter (the core of this plan)
- In `bot/store.rs`, introduce a private `gkey(guild, id) -> String` =
  `format!("{guild}:{id}")` and route **strikes** + **jails** tables through it.
- Per-guild config blobs: the **bot** prefixes blob keys with the guild
  (`gkey(guild, "link_filter_config")`). A tiny `GuildConfig` helper in the bot
  centralizes "load/save the X config for guild G".
- `jails_due_for_unjail(now)` stays global (sweep runs across all guilds) — the
  `JailRecord.guild_id` field tells the handler which guild to act in.
- Add additive guild-scoped admin queries where needed:
  `list_jails_for_guild(guild, limit)`, `list_strikes_for_guild(guild, limit)`.
- **Migration:** on open, one-shot migrate legacy bare-keyed rows
  (`user_id` → `bootstrap_guild:user_id`) iff a bootstrap `guild_id` is set, so an
  existing single-guild self-hoster's data survives the upgrade. Guard it behind
  a `schema_version` config blob so it runs once.
- **Tests (redb round-trip):** two guilds, same user id → independent strike
  counts; jail in guild A absent in guild B; per-guild config isolation; the
  legacy-migration path; `jails_due` still returns cross-guild.

### Phase 0.3 — Thread guild through the handler & commands
- `bot/handler.rs` / `commands.rs`: every store call already runs inside a
  message/interaction that knows its `guild_id` — pass it into the new
  guild-keyed helpers. The `FloodTracker` static is **already** keyed by user; make
  its internal map keyed by `(guild, user)` so floods don't merge across servers.
- Command registration: global vs per-guild per Phase 0.1.
- On `guild_create` (bot added to a new server) make sure config defaults exist
  (everything starts disabled, same as today).
- **Tests:** `flood_filter` core test that two guilds with the same user id keep
  separate sliding windows (extend `core/flood_filter.rs` tests).

### Phase 0.4 — Re-sync core + backend safety
- Run `backend/scripts/sync-modbot-core.sh` mentally/locally: confirm **no core
  port signature changed**, so the backend adapter still compiles. (It only uses
  link/flood/jail single-guild.) Note in the backend that the new additive
  methods are bot-only.
- Broadcast to the mesh before touching anything the backend vendored copy reads.

## Definition of done
- [ ] Two-guild isolation proven by tests (strikes, jails, config, flood window).
- [ ] Legacy single-guild data migrates once, losslessly.
- [ ] No `core` port signature change → backend unaffected (verified).
- [ ] `cargo test --workspace` green, clippy clean.
- [ ] README note: bot now multi-guild; `guild_id` optional (dev-only fast
      registration).

## Risks
- Global command propagation latency (~1h) — document it; keep `guild_id` as the
  dev fast-path.
- Migration correctness — gate on `schema_version`, test both fresh + legacy DB.
