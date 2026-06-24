# Plan 05 — AI Moderation via api.airforce

**Goal:** context-aware AI moderation — the bot calls an LLM (over the owner's
**api.airforce** account/API key) to judge messages that rules can't
(subtle toxicity, scams, context-dependent rule-breaking, multilingual abuse).
A genuine differentiator *and* dogfooding of api.airforce.

**Status:** ☐ later · **Effort:** ~2–4 d · **Depends on:** Plan 02 (slots in as
one more `RuleEval` on the automod engine — that seam is built in Plan 02).

## Design (sketch — flesh out when we start)
- **`core` stays LLM-agnostic:** add an `AiClassifier` **port** —
  `classify(text, policy) -> AiVerdict { flagged, category, confidence, reason }`.
  Pure core only defines the trait + how a verdict maps to an action; it never
  imports an HTTP client.
- **Bot adapter:** an `AirforceClassifier` implementing the port via an HTTP call
  to api.airforce's OpenAI-compatible endpoint (chat/completions or a
  moderation-style prompt), configured with `AIRFORCE_API_KEY` + base URL + model
  + a per-guild policy prompt.
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
