# Plan 02 — Content Automod

**Goal:** a pluggable content-moderation engine: word/regex blocklist,
anti-caps, anti-mention-spam, anti-emoji-spam, anti-zalgo, and
identical/duplicate-content spam. Built with a **classifier seam** so AI
moderation (Plan 05) slots in as just another rule source — no rework.

**Status:** ☐ todo · **Effort:** ~3–4 d · **Depends on:** Plan 00 (per-guild
rules), shares strike/case/jail plumbing with Plans 00/01.

## Gap (from benchmark)
- Content moderation today = only link/invite + posting-rate. No word filter,
  no caps/mention/emoji/zalgo, no duplicate-content detection.

## Design — `core/automod.rs` (pure, the heart of this plan)
- `AutomodConfig { enabled, rules: Vec<Rule>, action, exempt_* }` (per guild).
- `Rule` variants, each pure + independently testable:
  - `Blocklist { patterns, match_mode: Substring|Word|Regex, case_insensitive }`
  - `Caps { min_len, max_ratio }`
  - `MentionSpam { max_mentions }` (users+roles, optional @everyone weight)
  - `EmojiSpam { max_emojis }`
  - `Zalgo { max_combining_ratio }`
  - `Duplicate { window_secs, max_repeats }` (same normalized text N× in window;
    needs a small per-(guild,user) recent-message ring — in `core`, in-memory,
    bounded, like `FloodTracker`).
- `evaluate(msg_ctx, cfg) -> Option<AutomodVerdict { rule, action, reason }>` —
  the single entry point. **Classifier seam:** `evaluate` runs a
  `Vec<Box<dyn RuleEval>>`; Plan 05's AI check is added as one more `RuleEval`
  with no engine change.
- Action reuses `FloodAction`-style enum (Warn|Delete|Timeout|Jail) + strike +
  case (Plan 01).

## Phases
1. `core/automod.rs` config + `Rule` enum + per-rule pure evaluators. **Unit
   tests per rule**: each rule's trip/no-trip + the abuse string it defends.
2. Duplicate-content tracker (bounded per-(guild,user) ring) with eviction +
   tests (restart-loses-window is acceptable; documented).
3. `evaluate()` orchestration + exemptions (channel/role/user, mirrored from the
   flood/link surface) + tests.
4. Bot wiring in `handler.rs`: run `evaluate()` after link/flood; on verdict →
   delete + strike + case + action. Slash commands: `/automod` (enable, action,
   per-rule toggles + thresholds), `/automod blocklist add|remove|list`,
   `/automod exempt …`.
5. README + command count; staging verify each rule.

## Definition of done
- [ ] Each rule unit-tested (trip + safe input).
- [ ] Regex blocklist is **catastrophic-backtracking-safe** (size/complexity
      guard or `regex` crate's linear engine — which is linear by design; cap
      pattern count/length anyway).
- [ ] Exemptions honored; disabled by default per guild.
- [ ] Verdict → delete + strike + case path verified on staging.
- [ ] Engine accepts an extra `RuleEval` without signature change (Plan 05 hook).

## Risks
- Regex DoS — `regex` crate is linear-time (no backtracking); still cap count +
  length + reject on compile error with a clear admin message.
- Unicode/zalgo false positives — make ratios tunable, default conservative.
