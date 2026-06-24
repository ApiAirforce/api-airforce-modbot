# Plan 03 — Join-Raid / Anti-Nuke

**Goal:** the security tier that makes us a real Wick alternative: join-velocity
raid detection, account-age / no-avatar join gates, a verification gate,
lockdown automation, and anti-nuke (catch a rogue/compromised mod mass-deleting
channels or mass-banning).

**Status:** ☐ todo · **Effort:** ~4–6 d · **Depends on:** Plan 00 (per-guild
state), Plan 01 (cases for nuke/raid actions).

## Gap (from benchmark)
- Our "raid filter" is only **message**-flood. No mass-**join** detection, no
  account-age/avatar gate, no verification, no lockdown, no anti-nuke.

## Design
- **`core/raid.rs` (pure):**
  - `JoinTracker` — sliding window of joins per guild; trips on
    `max_joins / window_secs` (raid) → returns a `RaidVerdict` (lockdown /
    auto-action on the wave). Bounded, like `FloodTracker`.
  - `join_gate(member_age, has_avatar, cfg) -> GateDecision(Allow|Kick|Ban|
    Quarantine)` — account-age threshold + optional no-avatar rule. Pure.
- **`core/antinuke.rs` (pure):** given a stream of recent **admin actions** (from
  the audit log: channel/role deletes, bans, kicks, webhook creates) per actor,
  trip when one actor exceeds `max_destructive_actions / window` → verdict =
  strip the actor's dangerous roles + alert. Pure decision; the bot feeds it
  audit-log events.
- **Bot:**
  - `guild_member_addition` → `join_gate` + `JoinTracker`; on raid → set guild
    **lockdown** (raise verification level / auto-deny new joins / enable slow
    mode) and act on the wave.
  - `/verify` flow: new members get a holding role; a button/command grants the
    member role (config: `/setverify role:@unverified grant_on:button`).
  - `/lockdown on|off` manual + auto; `/setraid` (join thresholds, gate rules);
    `/setantinuke` (per-actor destructive-action thresholds, whitelist trusted
    admins/bots).
  - Audit-log consumption for anti-nuke (requires `View Audit Log`).

## Phases
1. `core/raid.rs`: `JoinTracker` + `join_gate`. **Unit tests**: burst of joins
   trips; age/avatar gate decisions; window eviction.
2. `core/antinuke.rs`: per-actor destructive-action window + verdict. **Unit
   tests**: rogue mass-delete trips; whitelisted actor doesn't; normal activity
   doesn't.
3. Bot: member-join gate + join-raid → lockdown automation. Staging verify with
   simulated joins.
4. Verification flow (holding role + grant button) + `/setverify`.
5. Anti-nuke: subscribe to audit-log events, feed `antinuke.rs`, strip roles +
   alert on trip. `/setantinuke` + trusted whitelist. Staging verify carefully
   (dangerous actions — test on a throwaway guild).
6. README + command count.

## Definition of done
- [ ] Join-raid + gate + anti-nuke decisions unit-tested in `core`.
- [ ] Lockdown raises verification / blocks the wave; reversible via `/lockdown
      off`.
- [ ] Anti-nuke strips a rogue actor's dangerous roles and alerts; trusted
      whitelist respected; **never** trips on the bot's own actions.
- [ ] Staging-verified on a throwaway guild.

## Risks
- **Anti-nuke is high-stakes** — a false positive strips a real admin. Default
  thresholds conservative, whitelist owner+trusted, dry-run/alert-only mode
  first, and **never** act on the bot or the guild owner.
- Audit-log events are slightly delayed/rate-limited — tune windows accordingly.
- Verification UX — don't lock out legit users; provide a mod override.
