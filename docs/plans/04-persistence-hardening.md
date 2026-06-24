# Plan 04 — Persistence & Hardening

**Goal:** close the correctness gaps the benchmark flagged in what we *already*
ship, so the existing features are production-solid before/while we add more.

**Status:** ◐ code + adversarial review done (mergebar, 0 blocker; 1 low fixed —
flood-trip admin escape-hatch + bounded growth); staging-verify pending ·
**Effort:** ~1–2 d · **Depends on:** Plan 00 (guild-keyed flood state). Can
interleave early — it hardens shipped code.

> Implemented: (1) pure `core/bulk_delete.rs` `plan_deletions` partitions a burst
> into Discord bulk-delete batches (2..=100, <14 d) + single-delete stragglers;
> the bot uses it with a single-delete fallback on any bulk error. (2) opt-in
> `same_content_threshold` identical-content trigger in `flood_filter` (additive
> `record_and_check_content`; the original `record_and_check` is an unchanged
> delegating wrapper, so the single-guild backend is untouched). (3) persisted
> "recent-trip" memory (`record_flood_trip_in`/`recent_flood_trip_in` +
> `flood_penalty_active`), config-gated by `trip_cooldown_secs` (default off), so
> a restart mid-raid keeps deleting an in-progress raider. (4) README invite
> bitmask recomputed (`1099780140166`) to cover kick/ban/timeout/audit-log.

## Gaps (from benchmark — our own bot)

1. **Flood window is in-RAM only** → a restart mid-raid wipes the sliding-window
   state (strikes/jails persist; the live tracker doesn't).
2. **Bulk-deletes are naive** — flood deletes loop single `delete_message` calls,
   no Discord **bulk-delete** API, no rate-limit backoff.
3. **No identical-content signal** in the flood filter — it counts
   messages/channels but never compares text (same-text spam isn't recognized as
   such).
4. README inaccuracies (16 commands, not 14; `/allowinvite` `/allowserver`
   undocumented; invite-permission bitmask mismatch).

## Design / Phases

1. **Bulk-delete hardening** (`bot/handler.rs`): collect the burst's message ids
   per channel and use the channel **bulk-delete** endpoint (≤100, <14 days old)
   with a single-delete fallback for older stragglers; add rate-limit-aware
   backoff (serenity surfaces 429s — honor `retry_after`). **Test:** unit the
   "partition into bulk-deletable vs single" helper in `core`.
2. **Optional identical-content trigger** in `core/flood_filter.rs`: an opt-in
   `same_content_threshold` — N identical (normalized) messages in the window
   also trips, independent of channel spread. Pure + unit-tested. Default off
   (keeps current behavior).
3. **Flood-window persistence (optional, config-gated):** periodically checkpoint
   the per-(guild,user) window to redb, or at minimum **persist a short "recent
   trip" memory** so a restart doesn't immediately re-allow an in-progress
   raider. Decide cheapest sufficient option during build; document the
   trade-off. **Test:** round-trip of the checkpoint.
4. **README fix-up**: correct command count, document `/allowinvite`
   `/allowserver`, recompute the invite permission bitmask to match the listed
   perms (or list the perms the bitmask actually grants). Pure docs.

## Definition of done

- [x] Burst deletion uses bulk-delete + single-delete fallback (helper
      unit-tested); staging-verify of the live path still pending.
- [x] Identical-content trigger unit-tested, default-off.
- [x] Restart no longer instantly forgives an in-progress raider (persisted
      trip-memory + `flood_penalty_active`, config-gated; documented + tested).
- [x] README accurate (33 commands; `/allowinvite` `/allowserver` documented;
      permission bitmask recomputed to match the listed perms).

## Risks

- Bulk-delete API rejects messages >14 days old → must fall back to single
  delete for those (flood bursts are seconds old, so rarely an issue). Handled:
  `plan_deletions` routes >14-day ids to the single path by snowflake age.
