# Plan 90 — Community Breadth (backlog)

**Goal:** the non-moderation "engagement" features the generalist bots lead with.
**Not** in the committed phase-1 scope (we chose Moderation-Vollausbau first);
parked here so the intent + sizing is captured. Pull individual items forward on
explicit go.

**Status:** ⏸ backlog · **Effort:** ~10–14 d total · **Depends on:** Plan 00
(everything per-guild).

## Candidate items (each independently shippable)
- **Leveling / XP** (~3–4 d): per-(guild,user) XP with anti-spam cooldown, rank
  card, leaderboard, role rewards. Pure XP math in `core`.
- **Reaction roles** (~2 d): message→emoji→role maps, button or reaction based.
- **Welcome / leave + autorole** (~1–2 d): templated join/leave messages, auto
  role on join (ties into Plan 03 verification).
- **Tickets** (~2–3 d): button-opened private channels/threads, transcript on
  close.
- **Starboard** (~1 d): N⭐ reactions → repost to a starboard channel.
- **Custom commands / tags** (~1–2 d): admin-defined text/embeds.

## Explicitly out of scope
- **Music** — voice/Lavalink upkeep + ToS risk, poor ratio. Skip unless a hard
  requirement appears.

## Notes
- These live mostly in `bot/` (not `core`'s moderation seam), so they don't touch
  the api.airforce vendored core — lower coordination risk.
- Each gets its own `docs/plans/NN-*.md` when pulled forward.
