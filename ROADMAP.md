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
| **03** | [Join-Raid / Anti-Nuke](docs/plans/03-join-raid-anti-nuke.md) | No join-gate/verification/lockdown/anti-nuke | ~4–6 d | ☐ |
| **04** | [Persistence & Hardening](docs/plans/04-persistence-hardening.md) | Flood window in-RAM; naive bulk-delete | ~1–2 d | ☐ |
| **05** | [AI Moderation via api.airforce](docs/plans/05-ai-moderation-airforce.md) | No context-aware AI moderation (differentiator) | ~2–4 d | ☐ later |
| **06** | [Web Dashboard](docs/plans/06-web-dashboard.md) | No dashboard (the "premium" of the big bots) | ~5–10 d | ☐ later |
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
