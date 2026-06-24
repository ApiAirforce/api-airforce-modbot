# Plan 06 — Web Dashboard

**Goal:** a per-guild web config UI (what makes MEE6/Dyno feel "premium") —
Discord OAuth login, pick a server you manage, edit every per-guild setting that
today lives behind slash commands, view cases/strikes/jails.

**Status:** ☐ later · **Effort:** ~5–10 d · **Depends on:** Plan 00 (per-guild
config is the data the dashboard edits). Largest single item — effectively a
small app.

## Design (sketch — flesh out when we start)
- **Backend API:** expose the bot's per-guild config + case/strike/jail reads
  over an authenticated HTTP API. Either (a) a small `axum` service in the bot
  process reading the same `redb`, or (b) reuse the api.airforce backend pattern.
  Auth = **Discord OAuth2** (verify the user has Manage Server on the target
  guild before allowing edits).
- **Frontend:** a small SPA (or reuse the api.airforce frontend stack /
  components for visual consistency). Pages: guild picker → per-feature config
  forms (link/flood/automod/raid/mod-log/verify/ai) → cases/strikes/jails views.
- **Shared validation:** the dashboard writes the **same** config blobs the slash
  commands do (one source of truth); validation logic lives in `core` so both
  paths agree.

## Phases (provisional)
1. Read-only API + OAuth (guild picker + view config/cases). Lowest risk first.
2. Write paths (edit config) with `core`-shared validation + audit of who
   changed what.
3. Polish: per-feature forms, cases/strikes/jails UI, mobile.

## Open questions for when we start
- Host the dashboard where? (bot process vs api.airforce infra vs separate.)
- Reuse api.airforce's frontend components/design, or standalone for sellability?
- Multi-tenant auth model (operator vs per-guild admins).
