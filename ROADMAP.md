# api-airforce-modbot — Feature-Parity Roadmap

> Goal: grow the bot from a focused anti-spam/anti-raid + jail tool into a
> **serious, self-hostable, multi-server moderation & security suite** — closing
> the gaps the feature benchmark found against MEE6 / Dyno / Carl-bot / Wick /
> Discord AutoMod, **without** turning it into a bloated everything-bot.
>
> Operating model decided: **Hosted for many servers** → Multi-Guild is
> foundational (Plan 00) and lands first. Web dashboard is planned (Plan 06).
> Scope phase 1 = **Moderation-Vollausbau** (Plans 00–04). AI moderation over the
> owner's api.airforce account is an explicit later goal (Plan 05).

## How this roadmap works

- This file is the **master plan**. Each numbered **sub-plan** lives in
  [`docs/plans/`](docs/plans/) and owns one feature area: phases → tasks → tests →
  effort → status.
- We execute **one sub-plan at a time, in order**. A sub-plan is "done" only when
  its `core` logic is unit-tested green, the bot compiles, and (where it touches
  Discord) it's been verified on a staging/test guild.
- Status legend: ☐ todo · ◐ in progress · ☑ done · ⏸ parked.
- Work happens on the `feat/feature-parity` branch; we merge to `main` per
  feature area once it's green, on explicit go.

## Architecture guardrails (do not break)

1. **Ports & adapters stays intact.** All new moderation *logic* goes in pure
   `core/` (no `serenity`, no `redb`), fully unit-tested. The bot wires it.
2. **`core` is vendored read-only into the api.airforce backend.** Changing a
   `core` **port signature** (`ConfigStore`/`StrikeStore`/`JailStore`) ripples
   into the backend's own adapter. Prefer **additive** changes; for the
   multi-guild key change, scope it so the single-guild backend is unaffected
   (see Plan 00). Coordinate + re-sync `backend/scripts/sync-modbot-core.sh`
   before landing core changes that the backend consumes.
3. **Everything stays runtime-configurable** (slash commands today, dashboard
   later) and **per-guild** once Plan 00 lands. No recompile to change a setting.
4. **No feature ships without tests.** Pure logic → unit tests in `core`.
   Stateful adapters → redb round-trip tests. Discord glue → staging verify.

## Sub-plans & sequencing

| # | Sub-plan | Closes gap | Effort | Status |
| --- | --- | --- | --- | --- |
| **00** | [Multi-Guild Foundation](docs/plans/00-multi-guild-foundation.md) | Single-guild-only → hosted-for-many | ~3–5 d | ◐ code-complete, tests green; staging-verify pending |
| **01** | [Mod-Action Suite](docs/plans/01-mod-action-suite.md) | No ban/kick/timeout/warn, no mod-log/cases | ~2–3 d | ◐ code+review done; staging-verify pending |
| **02** | [Content Automod](docs/plans/02-content-automod.md) | No word/regex/caps/mention/zalgo/dup filters | ~3–4 d | ◐ code+review done; staging-verify pending |
| **03** | [Join-Raid / Anti-Nuke](docs/plans/03-join-raid-anti-nuke.md) | No join-gate/verification/lockdown/anti-nuke | ~4–6 d | ◐ code+review done (4 findings fixed); staging-verify pending |
| **04** | [Persistence & Hardening](docs/plans/04-persistence-hardening.md) | Flood window in-RAM; naive bulk-delete | ~1–2 d | ◐ code+review done (1 low fixed); staging-verify pending |
| **05** | [AI Moderation via api.airforce](docs/plans/05-ai-moderation-airforce.md) | No context-aware AI moderation (differentiator) | ~2–4 d | ◐ P1–3 code+review done (1 low fixed; mock-tested, no spend); live staging pending |
| **06** | [Web Dashboard](docs/plans/06-web-dashboard.md) | No dashboard (the "premium" of the big bots) | ~5–10 d | ✅ v1 done — review (mergebar, 0 blocker; 5 low/med fixed) **+** full OAuth-callback live test passed (real Discord app; login→edit→persist; auth gates 403/404/401). Caught + fixed one cookie bug (`AppendHeaders`) |
| **90** | [Community Breadth (backlog)](docs/plans/90-backlog-community.md) | Leveling/reaction-roles/welcome/tickets/… | ~10–14 d | ⏸ backlog |

**Phase-1 commitment (Moderation-Vollausbau): Plans 00 → 04.** ≈ 3–4 focused
weeks. Plans 05/06 follow on go; Plan 90 is opt-in later.

> "Days" = focused work-days, not calendar. Pure-`core` work is fully
> auto-tested; Discord-facing work needs a staging guild + the running bot for
> true E2E (done together). *Music is intentionally out of scope —
> voice/Lavalink upkeep + ToS risk, poor ratio.*

## Definition of done per sub-plan

- [ ] `core` logic unit-tested (happy path + edges + the abuse case it defends).
- [ ] Adapter/store changes have redb round-trip tests.
- [ ] `cargo test --workspace` green; `cargo clippy` clean.
- [ ] Slash-command surface documented in `README.md` (+ command count correct).
- [ ] Verified on a staging guild for the Discord-facing path.
- [ ] Sub-plan status flipped to ☑ here and in its file.

## Progress log

- *2026-06-24* — Roadmap + sub-plan skeletons created on `feat/feature-parity`.
  Grounded in a read of `core/{lib,ports}.rs`, `bot/{config,store}.rs`.
- *2026-06-24* — **Local test loop verified** (`cargo test --workspace` builds the
  bot crate incl. serenity/redb/TLS on Windows + cmake 4.2.3; all green).
- *2026-06-24* — **Plan 00 brick 1: per-guild config layer.** Added
  `core::guild_blob_key` + `load_for_guild`/`save_for_guild` on
  `FloodFilterConfig`/`LinkFilterConfig`/`JailConfig` (additive; single-guild
  `load`/`save` untouched → api.airforce backend unaffected, no port-signature
  change). +2 isolation tests; core 38/38 green; no new clippy warnings (3 are
  pre-existing on `main`: 2× `too_many_arguments` on the existing `record_jail`
  trait — must NOT "fix" since it would change a vendored port signature; 1×
  collapsible-if in legacy code). Next: store composite-keying + handler cutover.
- *2026-06-24* — **Plan 00 brick 2: full multi-guild cutover (code-complete).**
  Store now keys `strikes`/`jails` per `(guild,user)` via composite keys +
  guild-scoped `*_in`/`*_for_guild` inherent methods (single-key trait methods
  KEPT for backend parity + as the regression guard); one-shot idempotent
  `migrate_legacy_to_guild_keys` upgrades an existing single-guild DB losslessly.
  `jail.rs` made concrete (`&RedbStore`) + guild-aware; `handler.rs` loads config
  per-guild, keys the flood window per `(guild,user)`, registers commands
  globally when no `guild_id` is set; every config command is guild-scoped and
  `load_for_guild` self-stamps `guild_id`. **bot 9/9 + core 38/38 green** (4
  original store tests untouched-green = extension-not-rewrite proof); clippy adds
  zero new warnings (5 pre-existing remain). Backend untouched (no port-signature
  change). Pending: adversarial diff review + live multi-guild staging test.
- *2026-06-24* — **Plan 00 DONE** (adversarial 3-lens review came back clean — no
  real bugs; isolation/migration/backend-safety/regression-guard all confirmed).
  Only the live multi-guild Discord staging test remains (needs the running bot).
- *2026-06-24* — **Plan 01 phase 1: `core/cases.rs`** (pure) — `Case`/`CaseAction`
  shapes, `WarnEscalation` policy + `warn_escalation()` decision + `next_case_number`.
  Additive, nothing existing touched. **core 45 tests green** (+7), clippy clean.
  Next: phase 2 `CaseStore` (redb, guild-scoped) → phase 3 `/ban /kick /timeout
  /warn /note /cases /case /setmodlog` + mod-log embeds + escalation wiring.
- *2026-06-24* — **Plan 01 phases 2+3 (code-complete).** `CaseStore` (per-guild
  numbered cases, atomic counter, get/list per user+guild) + core `ModConfig`
  (mod-log channel + warn-escalation policy). 9 slash commands: `/ban /kick
  /timeout` (native, 28-day cap) `/warn` (auto-escalates to timeout/jail/ban)
  `/note /cases /case /setmodlog /setescalation` — each writes a numbered case +
  posts a mod-log embed. **bot 10 + core 45 green**, clippy zero new warnings.
  Pending: Plan 01 adversarial review + live staging test.
- *2026-06-24* — **Plan 01 adversarial review: clean architecture, 2 real findings
  fixed.** (A, medium) warn→Jail escalation used `try_jail`'s bool (true even when
  the Discord edit failed) → wrote a misleading "escalated to jail" case; now calls
  `jail_member` directly and surfaces the real error, case only on true success.
  (B, low) `do_ban/kick/timeout` swallowed an `add_case` write error as "case #0";
  now reported honestly via `case_ref()` + no "#0" mod-log. Review CONFIRMED:
  atomic per-guild numbering, no escalation loop, action-before-case ordering,
  auth-gating, guild isolation. bot 10 + core 45 green; clippy unchanged.
- *2026-06-24* — **Committed** `8a99584` (Plan 00 + 01, explicit paths, no AI
  attribution) on `feat/feature-parity`.
- *2026-06-24* — **Plan 02 phase 1: `core/automod.rs`** (pure rule engine):
  `AutomodConfig` + `evaluate()` for blocklist (substring/word/regex), caps,
  mention-spam, emoji-spam, zalgo, + `DuplicateTracker` (stateful sliding window).
  Pure counting helpers, AI-classifier layers on at the host (no engine change).
  **core 54 green** (+9; a test caught a real bug — lowercasing a regex pattern
  corrupted `\S`→`\s`, fixed via RegexBuilder.case_insensitive). clippy clean.
  Next: phase 2 (handler wiring + DuplicateTracker static + `/automod` commands).
- *2026-06-24* — **Plan 02 phase 2 (code-complete).** `handler.automod_check`
  runs after flood (per-guild config, `DuplicateTracker` keyed per (guild,user)),
  on a trip deletes + shared strike + numbered case + mod-log + the configured
  action (warn/delete/timeout/jail). 4 commands: `/automod` (flat config),
  `/blocklist` add|remove|list, `/automodexempt` `/automodunexempt`; `/modstatus`
  gained an automod section; `post_modlog` shared (pub(crate)). 29 commands total.
  **bot 10 + core 54 green**, clippy zero new warnings.
- *2026-06-24* — **Plan 02 adversarial review: 1 HIGH (blocker) + 1 low, both
  fixed.** (HIGH) blocklist regexes were recompiled PER MESSAGE — a pathological
  17-char pattern (`(\p{L}\p{M}*){50}`) passes validate but costs ~14.5ms to build
  ×1000 entries = ~14.5s CPU/message → shared-runtime cross-guild DoS. Fixed:
  `CompiledBlocklist` compiled once + cached in the handler (rebuilt only on config
  change) + `RegexBuilder.size_limit` so pathological patterns fail fast. (low)
  `DuplicateTracker` empty-deque eviction was dead code → opportunistic stale-user
  sweep past a size cap + accurate doc. Review CONFIRMED: case-folding, whole-word
  boundaries, exemptions, action-mapping, guild isolation, no await-across-lock.
  bot 10 + core 54 green; clippy unchanged (5 pre-existing).
- *2026-06-24* — **Plan 02 committed** `0dd913b` (explicit paths, no AI attribution).
- *2026-06-24* — **Plan 03 phase 1: `core/raid.rs` + `core/antinuke.rs`** (pure).
  raid: `RaidConfig` + `screen_join` (account-age / no-avatar gate → kick/ban/
  quarantine) + `JoinTracker` (join-velocity sliding window). antinuke:
  `AntinukeConfig` (trusted allowlist + dry-run) + `ActionTracker` (per-actor
  destructive-action window) + `DestructiveAction`. Additive, **core 64 green**
  (+10), clippy clean. Next: phase 2 — the bot wiring (member-join gate, lockdown,
  /verify flow, audit-log → anti-nuke role-strip + alert) — the high-stakes part
  (a false positive strips a real admin), built behind the dry-run mode first.
- *2026-06-24* — **Plan 03 phase 2 (code-complete).** `handler.raid_check`
  (member-join gate via snowflake account age + avatar; `JoinTracker` velocity →
  latched lockdown that gate-actions every join) + `handler.antinuke_check` (the
  `guild_audit_log_entry_create` event → `ActionTracker` per actor → strip
  non-managed roles + alert, with bot-self/owner/trusted exemptions + **dry-run**).
  Quarantine reuses the jail; verification = quarantine + existing /unjail (no
  separate verify flow). 4 commands: `/setraid` `/lockdown` `/setantinuke`
  `/raidtrust`; `/modstatus` raid+anti-nuke section. 33 commands total. **bot 10 +
  core 64 green**, clippy zero new. Pending: Plan 03 review + staging test.
- *2026-06-24* — **Plan 03 adversarial review (2 lenses + synthesis) → 4 findings,
  all fixed.** (1) **High** — anti-nuke owner-exemption failed *open*: if the owner
  lookup errored (likely during the API flood of a real nuke) the owner's roles
  got stripped. Now **fail-closed**, cache-first (`to_guild_cached`, populated even
  while HTTP is rate-limited) → HTTP fallback → if still unresolved, alert + take no
  action. (2) **Med** — role-strip fell back to an empty keep-set on a fetch error
  (would try to remove *all* roles incl. managed → Discord rejects the whole edit →
  silent no-op); now alerts + skips the strip, detection still logged. (3) **Med** —
  raid gate wrote a "kicked/banned/jailed" case + mod-log even when enforcement
  failed; now logs only on success (`jail_member` directly, not `try_jail`).
  (4) **Med** — false-positives: dropped non-destructive `WebhookCreate` from the
  burst counter + raised the default threshold 5→10; `/setantinuke` now nudges to
  dry-run first when going live. **bot 10 + core 64 green**, clippy still the 5
  pre-existing warnings (zero new). Pending: staging test.
- *2026-06-24* — **Plan 03 committed** (`cd629d1`, explicit paths, no AI
  attribution).
- *2026-06-24* — **Plan 04 (Persistence & Hardening), code-complete.**
  (1) New pure `core/bulk_delete.rs` — `plan_deletions` partitions a burst into
  Discord **bulk-delete** batches (per channel, 2..=100, <14 d by snowflake age)
  plus single-delete stragglers; the flood path now bulk-deletes with a
  single-delete fallback on any error (replaces the per-message delete loop).
  (2) **Identical-content** trigger in `flood_filter` (`same_content_threshold`,
  opt-in/default-off) via an additive `record_and_check_content`; the original
  `record_and_check` is an unchanged delegating wrapper → the single-guild
  api.airforce backend stays byte-compatible. (3) **Persisted trip-memory**:
  `record_flood_trip_in`/`recent_flood_trip_in` (redb CONFIG, guild-scoped) +
  pure `flood_penalty_active`, config-gated by `trip_cooldown_secs` (default
  off) — a restart mid-raid keeps deleting an in-progress raider without
  re-strike spam. (4) **README** invite bitmask recomputed `268512256` →
  `1099780140166` (adds kick/ban/timeout/audit-log for Plans 01/03); `/setflood`
  gains the two new knobs + `/modstatus` shows them. **bot 11 + core 74 green**
  (+1 store, +10 core tests), clippy still the 5 pre-existing warnings (zero
  new). Pending: Plan 04 review + staging test.
- *2026-06-24* — **Plan 04 adversarial review (2 lenses + synthesis) → mergebar,
  0 blocker.** Regression/extension guarantee confirmed: the old `record_and_check`
  (content=None) can never reach the same-content branch, so the single-guild
  backend path is observably unchanged; `plan_deletions` batches strictly 2..=100,
  14-day boundary `<`, no message dropped; no await across the tracker lock; the
  `flood_trip:` key can't collide with config/`case_seq:` keys and CONFIG is never
  iterated by migration/`all_values`. **1 low fixed**: the penalty box had no
  admin escape-hatch and its rows grew write-only — added
  `RedbStore::clear_flood_trip_in`, called from `/unjail` + `/strikes reset`
  (release lifts the box) and opportunistically when a trip has expired (bounds
  growth). (The 2nd low — Discord-side `min/max_int_value` on the new `/setflood`
  ints — left for parity with the existing 8 int options that also rely on
  `validate()`.) **bot 11 + core 74 still green**, clippy zero new. Pending:
  staging test.
- *2026-06-24* — **Plan 04 committed** (`94c897b`, explicit paths, no AI
  attribution).
- *2026-06-24* — **Plan 05 (AI moderation), Phases 1–3 code-complete
  (mock-tested, NO real spend).** anes-Entscheidungen: sensible Defaults
  (base URL + Key via env, **Modell + Policy pro Guild** via `/setai`), Scope =
  mock-getestet. (1) New pure `core/ai_mod.rs` (no new core deps → backend stays
  clean): `AiModConfig` (model/policy/action/confidence + Kostenwächter
  min_chars/max_chars/daily_call_cap), `AiVerdict` + `allow()` (fail-open),
  `should_classify`/`within_budget`/`action_for`/`truncate_for_call`; `validate`
  erzwingt Modell + cap>0 wenn enabled. (2) `bot/ai.rs`: `AiClassifier`-Trait +
  `AirforceClassifier` (reqwest chat/completions, **fail-open** auf jeden Fehler),
  pure `build_request_body`/`parse_verdict` (robust gg fences/prose/garbage), gg
  Fake-Classifier getestet. (3) Persisted Tagesbudget (`ai_calls_today`/`incr`,
  EIN self-resetting Row pro Guild — kein Key-Growth). (4) handler: automod-Tail
  in shared `apply_content_action` extrahiert (byte-identisch für automod) → AI
  speist denselben Pfad; `ai_check` (Exemptions → Pre-Filter → Budget → classify
  → action_for). `/setai` + `/aiexempt`/`/aiunexempt`, `/modstatus` zeigt AI +
  Tages-Calls. **36 commands. bot 18 + core 80 green** (+7 bot, +6 core tests),
  clippy still the 5 pre-existing warnings (zero new). Pending: Plan 05 review +
  live staging (echter Spend, mit anes).
- *2026-06-24* — **Plan 05 adversarial review (2 lenses + synthesis) → mergebar,
  0 blocker.** Verified: the automod refactor is **byte-identical** (the shared
  `apply_content_action` with `tag="automod"` reproduces `automod [rule]: reason`
  and every audit/DM/case string → no regression); core stayed dependency-clean
  (empty `core/Cargo.toml` diff → backend untouched); fail-open on every classify
  error path; **no key leak** (key only in `bearer_auth`, never logged); cost
  guards + exemptions run before any spend; budget row self-resets (no key
  growth). **1 low fixed**: the daily cap was a soft cap under concurrency
  (`within_budget` read + `incr` were 2 txns) → replaced with an atomic
  `try_incr_ai_calls_today(cap)` compare-and-increment in ONE txn = a true hard
  cap. (2nd low — a reserved slot isn't refunded when the call then fails open —
  kept by design: counting attempts also circuit-breaks a sustained outage;
  documented in `ai_check`.) **bot 18 + core 80 still green**, clippy zero new.
  Pending: live staging (real api.airforce call, with anes).
- *2026-06-25* — **Plan 05 committed** (`e2104c9`).
- *2026-06-25* — **Live staging test (Browser MCP, real test Discord) → PASSED;
  2 fixes committed.** Created a throwaway bot app + server, ran the binary:
  gateway connect, **36 commands registered**, `/modstatus`, `/setfilter`, a
  posted link **deleted + struck**, and **AI moderation live** (gpt-4o-mini
  returned `{flagged,category,confidence}`, message deleted; an invalid model id
  failed open → no false-delete). Found + fixed `/allowinvite` description >100
  chars (Discord rejected the WHOLE command batch → no commands registered on any
  real guild) — committed `211872f`. Added AI verdict logging — committed
  `9dff4a8`. The "compiled + reviewed" Discord-glue layer is now real-world
  proven.
- *2026-06-25* — **Plan 06 (Web Dashboard) v1 code-complete.** `bot/dashboard.rs`
  — an `axum` HTTP service spawned alongside the gateway, sharing the same
  `Arc<RedbStore>` (one source of truth, no second DB). Discord **OAuth2** login
  (identify+guilds), opaque in-memory sessions, and a guild gate (Manage-Server
  **and** bot-in-guild). API: `GET config` (all 8 sections) + `PUT
  config/:section` through the **same `core` `validate()`** the slash commands
  use; `cases`/`strikes`/`jails` reads. Self-contained vanilla SPA
  (`web/{index.html,app.js,style.css}`, embedded via `include_str!`) with
  auto-generated per-section forms. Off unless `[dashboard]` is enabled with
  OAuth creds. New deps: `axum`, `rand` (bot only — core untouched). **Live-smoke
  verified**: server starts with the bot, serves the SPA, `/api/login` 303→Discord,
  `/api/me` + `/api/guilds/:id/config` 401 without a session. **bot 22 + core 80
  green** (+4 dashboard tests), clippy still the 5 pre-existing (zero new).
  Pending: review + full OAuth-callback live test (needs real OAuth creds).
- *2026-06-25* — **Plan 06 adversarial security review (2 lenses + synthesis) →
  mergebar, 0 blocker.** Verified clean: no cross-guild access (`authorize()` is
  the first statement in every guild endpoint, exact-match against the login
  guild snapshot — a crafted `:id` can't slip the gate), no secret leak
  (client-secret only in the token POST, bot token only as a `Bot` auth header,
  no config blob holds a key, errors echo only serde/validate strings),
  login-CSRF blocked (HttpOnly `oauth_state` cookie checked on callback), no
  await-across-lock, no SSRF (fixed Discord URLs), body-limit + capped list
  limits. **5 low/medium all fixed**: (M) permission staleness — TTL 8h→30 min so
  a revoked admin loses access fast (silent re-auth keeps it one click); (L)
  clear the single-use `oauth_state` cookie after the callback; (L) recover a
  poisoned session mutex (`unwrap_or_else(into_inner)`); (L) documented the
  ~200-guild single-page fetch (under-grant only); (L) `esc()` two app.js sinks
  (defense-in-depth). **bot 22 + core 80 still green**, clippy zero new. Pending:
  full OAuth-callback live test (needs real OAuth creds).
- *2026-06-25* — **Plan 06 committed** (`4269ee3`, explicit paths, no AI
  attribution).
- *2026-06-25* — **Plan 06 full OAuth-callback live test (Browser MCP, real
  Discord app) → PASSED.** Registered the `http://127.0.0.1:8099/api/callback`
  redirect + reset the OAuth client secret in the Dev Portal, ran the bot with
  `[dashboard]` enabled, and drove the whole flow: login page → `/api/login` →
  Discord consent (exactly `identify`+`guilds`) → callback → session →
  logged-in SPA → edited Link-filter strike threshold 3→5 → **persisted**
  (verified via re-GET + the `dashboard: link config … updated` server log, same
  store the slash commands write). Auth gates confirmed live: foreign-guild
  GET **and** PUT → 403, unknown section → 404, no session → 401. The store is
  shared bidirectionally (AI config set earlier via `/setai` showed up in the
  dashboard GET). **One real bug caught by the live run** (invisible to unit
  tests + review): the callback returned two `Set-Cookie` headers via a
  `[(K,V); N]` array, whose `IntoResponseParts` impl *inserts* (overwrites) per
  header name, so the state-clearing cookie clobbered the session cookie →
  `/api/me` 401. **Fixed with `AppendHeaders`** (appends both). Re-verified:
  bot 22 + core 80 green, clippy zero new. **Roadmap 00–06 complete.**
