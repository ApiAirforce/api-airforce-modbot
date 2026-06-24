//! AI-moderation adapter — the api.airforce-backed [`AiClassifier`].
//!
//! The pure core ([`airforce_modbot_core::ai_mod`]) owns the config, the verdict
//! shape, the cost guards and the verdict→action mapping. This module is the
//! **host adapter**: it turns one message into a chat/completions call against
//! the owner's **api.airforce** account (an OpenAI-compatible endpoint) and parses
//! the model's reply back into an [`AiVerdict`].
//!
//! Two hard rules live here:
//!   * **Fail-open** — any transport error, non-2xx, timeout, or unparseable reply
//!     returns [`AiVerdict::allow`], so a down or slow API never blocks or
//!     punishes legitimate chat.
//!   * **Per-guild model + policy** — the model id and the policy prompt come from
//!     the guild's config (set via `/setai`), so each server (each hosted
//!     customer) classifies with its own model and its own rules on the shared
//!     account. A per-guild API key is a clean future extension on the same shape.
//!
//! The bug-prone, host-agnostic pieces (request body, reply parsing) are pure
//! functions, unit-tested below; the actual HTTP call is exercised at staging.

use std::time::Duration;

use serde::Deserialize;
use serenity::async_trait;

use airforce_modbot_core::AiVerdict;

/// Default OpenAI-compatible base URL (api.airforce). Overridable via
/// `AIRFORCE_BASE_URL`; the API key is never hardcoded (env `AIRFORCE_API_KEY`).
const DEFAULT_BASE_URL: &str = "https://api.airforce/v1";

/// Default per-guild policy used when an admin hasn't written one.
const DEFAULT_POLICY: &str = "Flag hate speech, harassment, threats, sexual content involving minors, \
scam/phishing, and raid/spam advertising. Do NOT flag ordinary disagreement, profanity, \
or on-topic mature discussion.";

/// The fixed classifier instruction prepended to every request.
const SYSTEM_PREAMBLE: &str = "You are a strict but fair Discord content-moderation classifier. \
Decide whether the USER message violates the server's policy below. \
Reply with ONLY a compact JSON object and nothing else — no prose, no code fence: \
{\"flagged\": <bool>, \"category\": <short tag>, \"confidence\": <0-100 int>, \"reason\": <short string>}. \
category is a short tag like \"toxicity\", \"harassment\", \"scam\", \"nsfw\", \"spam\", or \"none\". \
Be conservative: when a message is borderline or benign, set flagged=false.";

/// What the host needs from any classifier. Implemented here by
/// [`AirforceClassifier`]; tests inject a fake so the wiring is exercised with no
/// network. `model`/`policy` are per-guild (the caller passes the guild's config).
#[async_trait]
pub trait AiClassifier: Send + Sync {
    /// Classify one (already pre-filtered + truncated) message. MUST fail open:
    /// return [`AiVerdict::allow`] on any error rather than propagate it.
    async fn classify(&self, content: &str, policy: &str, model: &str) -> AiVerdict;
}

/// api.airforce-backed classifier (OpenAI-compatible chat/completions).
pub struct AirforceClassifier {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    timeout_secs: u64,
}

impl AirforceClassifier {
    /// Build from the environment: `AIRFORCE_API_KEY` (required — absent => AI
    /// moderation stays off) and `AIRFORCE_BASE_URL` (optional, defaults to
    /// api.airforce). Returns `None` when no key is set so the host can simply
    /// skip wiring the classifier.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("AIRFORCE_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
        let base_url = std::env::var("AIRFORCE_BASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let http = reqwest::Client::builder().build().ok()?;
        Some(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            timeout_secs: 8,
        })
    }
}

#[async_trait]
impl AiClassifier for AirforceClassifier {
    async fn classify(&self, content: &str, policy: &str, model: &str) -> AiVerdict {
        let body = build_request_body(model, policy, content);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(self.timeout_secs))
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("⚠️ ai-mod: request failed ({e}) — failing open");
                return AiVerdict::allow();
            }
        };
        if !resp.status().is_success() {
            eprintln!("⚠️ ai-mod: classifier returned {} — failing open", resp.status());
            return AiVerdict::allow();
        }
        let val: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("⚠️ ai-mod: bad response body ({e}) — failing open");
                return AiVerdict::allow();
            }
        };
        let text = val
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        parse_verdict(text)
    }
}

/// Build the chat/completions request body for a classification call. Pure +
/// testable. `policy` empty => the built-in [`DEFAULT_POLICY`].
pub fn build_request_body(model: &str, policy: &str, content: &str) -> serde_json::Value {
    let policy = if policy.trim().is_empty() { DEFAULT_POLICY } else { policy };
    let system = format!("{SYSTEM_PREAMBLE}\n\nSERVER POLICY:\n{policy}");
    serde_json::json!({
        "model": model,
        "temperature": 0,
        "max_tokens": 200,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": content},
        ],
    })
}

/// Lenient view of the model's JSON reply (tolerates missing keys + a numeric or
/// fractional confidence).
#[derive(Deserialize)]
struct RawVerdict {
    #[serde(default)]
    flagged: bool,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    reason: Option<String>,
}

/// Parse a classifier reply into an [`AiVerdict`]. Robust to a code-fence or
/// surrounding prose (extracts the first `{`..last `}`); an unparseable reply
/// fails **open** (returns [`AiVerdict::allow`]) so a confused model never
/// punishes a user.
pub fn parse_verdict(assistant_content: &str) -> AiVerdict {
    let parsed = extract_json_object(assistant_content)
        .and_then(|slice| serde_json::from_str::<RawVerdict>(slice).ok());
    match parsed {
        Some(raw) => AiVerdict {
            flagged: raw.flagged,
            category: raw.category.unwrap_or_default(),
            confidence: raw.confidence.unwrap_or(0.0).clamp(0.0, 100.0) as u8,
            reason: raw.reason.unwrap_or_default(),
        }
        .sanitized(),
        None => AiVerdict::allow(),
    }
}

/// The first balanced-ish JSON object substring (`{`..last `}`), if any.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| &s[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use airforce_modbot_core::{AiModConfig, AutomodAction};

    #[test]
    fn request_body_has_model_policy_and_message() {
        let body = build_request_body("my-model", "no politics", "hello world");
        assert_eq!(body["model"], "my-model");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"], "hello world");
        assert!(msgs[0]["content"].as_str().unwrap().contains("no politics"));
        // empty policy falls back to the default
        let body2 = build_request_body("m", "  ", "hi");
        assert!(body2["messages"][0]["content"].as_str().unwrap().contains("Flag hate speech"));
    }

    #[test]
    fn parse_plain_json_verdict() {
        let v = parse_verdict(r#"{"flagged":true,"category":"scam","confidence":92,"reason":"phishing link"}"#);
        assert!(v.flagged);
        assert_eq!(v.category, "scam");
        assert_eq!(v.confidence, 92);
        assert_eq!(v.reason, "phishing link");
    }

    #[test]
    fn parse_json_in_code_fence_and_prose() {
        let fenced = "```json\n{\"flagged\":false,\"category\":\"none\",\"confidence\":10,\"reason\":\"\"}\n```";
        assert!(!parse_verdict(fenced).flagged);
        let prosed = "Sure, here is my judgment: {\"flagged\":true,\"category\":\"toxicity\",\"confidence\":80,\"reason\":\"slur\"} — hope that helps!";
        let v = parse_verdict(prosed);
        assert!(v.flagged);
        assert_eq!(v.confidence, 80);
    }

    #[test]
    fn parse_tolerates_missing_fields_and_float_confidence() {
        // only `flagged` present => category sanitized, confidence 0
        let v = parse_verdict(r#"{"flagged":true}"#);
        assert!(v.flagged);
        assert_eq!(v.category, "flagged");
        assert_eq!(v.confidence, 0);
        // fractional + over-range confidence clamps into u8 0..=100
        let f = parse_verdict(r#"{"flagged":true,"confidence":87.6,"category":"x"}"#);
        assert_eq!(f.confidence, 87);
        let over = parse_verdict(r#"{"flagged":true,"confidence":250,"category":"x"}"#);
        assert_eq!(over.confidence, 100);
    }

    #[test]
    fn unparseable_reply_fails_open() {
        assert!(!parse_verdict("the model rambled with no json").flagged);
        assert!(!parse_verdict("").flagged);
        assert!(!parse_verdict("{ not valid json").flagged);
    }

    // A deterministic fake classifier — exercises the trait + verdict→action path
    // with no network.
    struct FakeClassifier(AiVerdict);
    #[async_trait]
    impl AiClassifier for FakeClassifier {
        async fn classify(&self, _content: &str, _policy: &str, _model: &str) -> AiVerdict {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn fake_classifier_drives_the_action_mapping() {
        let cfg = AiModConfig {
            enabled: true,
            guild_id: "g".into(),
            model: "m".into(),
            confidence_threshold: 75,
            action: AutomodAction::Timeout,
            timeout_minutes: 10,
            ..Default::default()
        };
        // a confident flag maps to the configured action
        let flagged = FakeClassifier(AiVerdict {
            flagged: true,
            category: "scam".into(),
            confidence: 90,
            reason: "x".into(),
        });
        let v = flagged.classify("buy now", &cfg.policy, &cfg.model).await;
        assert_eq!(cfg.action_for(&v), Some(AutomodAction::Timeout));
        // a low-confidence flag does not act
        let unsure = FakeClassifier(AiVerdict {
            flagged: true,
            category: "scam".into(),
            confidence: 40,
            reason: "maybe".into(),
        });
        let v2 = unsure.classify("hmm", &cfg.policy, &cfg.model).await;
        assert_eq!(cfg.action_for(&v2), None);
    }
}
