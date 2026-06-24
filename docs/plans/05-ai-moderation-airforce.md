# Plan 05 — AI Moderation via api.airforce

**Goal:** context-aware AI moderation — the bot calls an LLM (over the owner's
**api.airforce** account/API key) to judge messages that rules can't
(subtle toxicity, scams, context-dependent rule-breaking, multilingual abuse).
A genuine differentiator *and* dogfooding of api.airforce.

**Status:** ◐ Phases 1–3 code + adversarial review done (mergebar, 0 blocker;
1 low fixed — daily cap is now a hard atomic cap; mock-tested, **no real spend**);
live staging pending · **Effort:** ~2–4 d · **Depends on:**
Plan 02 (slots in as one more check feeding the same `AutomodVerdict` → action
path — that seam is built in Plan 02).

> Decisions taken with anes: sensible defaults (base URL + key via env, never
> hardcoded), **model + policy per guild** via `/setai` (so each hosted customer
> classifies with their own model + prompt on the shared account; per-guild keys
> are a clean future add on the same shape); built mock-tested (no live calls).
>
> Implemented: (1) pure `core/ai_mod.rs` — config, verdict, the cost guards
> (`should_classify` / `within_budget` / `truncate_for_call`) and `action_for`; no
> new core deps. (2) `bot/ai.rs` — an `AiClassifier` trait, the fail-open
> `AirforceClassifier` (reqwest chat/completions), and pure
> `build_request_body` / `parse_verdict`, fake-tested. (3) a persisted
> self-resetting daily budget; `apply_content_action` shared by automod and AI;
> `ai_check` wired after automod; `/setai` and `/aiexempt`. Live verify (a real
> api.airforce call) is Phase 4 — done together when the bot runs.

## Design (sketch — flesh out when we start)

- **`core` stays LLM-agnostic:** add an `AiClassifier` **port** —
  `classify(text, policy) -> AiVerdict { flagged, category, confidence, reason }`.
  Pure core only defines the trait + how a verdict maps to an action; it never
  imports an HTTP client.
- **Bot adapter:** an `AirforceClassifier` implementing the port via an HTTP call
  to api.airforce's OpenAI-compatible endpoint (chat/completions or a
  moderation-style prompt), configured with `AIRFORCE_API_KEY`, a base URL, a
  model, and a per-guild policy prompt.
- **Cost & safety guards (critical):** only call the LLM for messages that pass a
  cheap pre-filter (length/heuristic), per-guild rate cap + monthly budget cap,
  timeout + fail-open (never block legit chat if the API is down), cache repeated
  strings. Surfaced in `/setai` (enable, model, budget, sensitivity).
- **Action:** AI verdict feeds the same automod action path (warn/delete/timeout/
  jail + case), with confidence threshold gating.

## Phases (provisional)

1. `AiClassifier` port + verdict→action mapping in `core` (+ unit tests with a
   fake classifier).
2. `AirforceClassifier` HTTP adapter (api.airforce key/model/base-url) + a
   pre-filter + budget/rate guard.
3. `/setai` config + wire as a `RuleEval` into the Plan-02 engine.
4. Staging verify (toxic/scam samples flagged; benign passes; API-down = fail
   open); cost dashboards.

## Open questions for when we start

- Which api.airforce model + prompt shape (chat vs a dedicated moderation route)?
- Per-guild budget model (the owner's account pays — needs hard caps + visibility).
- Privacy note for self-hosters: enabling AI mod sends message text to the
  configured api.airforce endpoint — document clearly (opt-in, off by default).
