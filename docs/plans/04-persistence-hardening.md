# Plan 04 — Persistence & Hardening

**Goal:** close the correctness gaps the benchmark flagged in what we *already*
ship, so the existing features are production-solid before/while we add more.

**Status:** ☐ todo · **Effort:** ~1–2 d · **Depends on:** Plan 00 (guild-keyed
flood state). Can interleave early — it hardens shipped code.

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
- [ ] Burst deletion uses bulk-delete + backoff; verified on staging (no 429
      storm, whole burst removed).
- [ ] Identical-content trigger unit-tested, default-off.
- [ ] Restart no longer instantly forgives an in-progress raider (chosen
      mechanism documented + tested).
- [ ] README accurate (command count, undocumented commands, permission bitmask).

## Risks
- Bulk-delete API rejects messages >14 days old → must fall back to single
  delete for those (flood bursts are seconds old, so rarely an issue).
