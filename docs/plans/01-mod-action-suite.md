# Plan 01 — Mod-Action Suite

**Goal:** the classic moderator toolkit the big bots have and we lack: ban /
kick / native timeout / warn, plus a **mod-log channel** and a **case system**
(every action recorded, listable, note-able). Our jail stays the special-sauce;
this adds the standard verbs around it.

**Status:** ☐ todo · **Effort:** ~2–3 d · **Depends on:** Plan 00 (guild-keyed
case/warn storage).

## Gap (from benchmark)
- No `/ban` `/kick` `/timeout`; only the custom jail as punishment.
- No mod-log channel; actions go to stdout + native `audit_log_reason` only.
- No cases/notes/history.

## Design
- **`core`**: a pure `cases` module — `Case { id, guild_id, user_id, mod_id,
  action: CaseAction(Ban|Kick|Timeout|Warn|Jail|Unjail|Note), reason, created_unix,
  duration_secs: Option<u64> }` + the case-numbering/decay/warn-escalation logic
  (e.g. "N warns within window → auto-escalate to timeout/jail", configurable).
  Pure + unit-tested.
- **Port (additive):** `CaseStore` — `add_case(...) -> u64` (returns case #),
  `list_cases_for_user(guild, user, limit)`, `list_cases_for_guild(guild, limit)`,
  `add_note(...)`, `get_case(guild, id)`. New table in `bot/store.rs`,
  guild-keyed. **No change to existing ports** (backend-safe).
- **Bot commands:** `/ban` `/kick` `/timeout` (uses Discord native
  `communication_disabled_until`, max 28 d) `/warn` `/note` `/cases` (list for a
  user) `/case` (show one) `/reason` (edit a case reason). Each writes a case and
  posts an embed to the configured **mod-log channel**.
- **Config:** `/setmodlog channel:#mod-log`; warn-escalation thresholds in a
  `mod_config` blob (per guild). Mod-log channel id per guild.
- **Escalation:** warn handler reads recent warn-cases (decay-windowed, reuses
  the strike-decay math style) and auto-applies the configured next step.

## Phases
1. `core/cases.rs`: types + case-numbering + warn-escalation decision fn. **Unit
   tests**: numbering is monotonic per guild; escalation fires at threshold;
   decay drops old warns.
2. `CaseStore` redb table + impl. **Round-trip tests**: per-guild case numbers,
   list-by-user, notes.
3. Commands + mod-log embeds: `/ban /kick /timeout /warn /note /cases /case
   /reason /setmodlog`. Permission-gated (Ban Members / Kick Members / Moderate
   Members as appropriate, plus owner override).
4. Wire jail/flood/link actions to **also** write a case → unified history.
5. README + command-count update; staging verify (ban/kick/timeout/warn + log
   embed render).

## Definition of done
- [ ] Every mod action produces a numbered, listable case in the right guild.
- [ ] Mod-log embeds post to the configured channel; missing channel = silent
      no-op, not a crash.
- [ ] Native timeout respects Discord's 28-day cap (validated).
- [ ] Warn-escalation tested in `core`.
- [ ] Permissions enforced; clippy + tests green.

## Risks
- Permission/role-hierarchy: bot can't ban someone above its top role — handle
  the Discord error gracefully with a clear reply.
- Timeout cap (28 d) — clamp + inform.
