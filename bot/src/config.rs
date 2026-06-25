//! Bootstrap configuration (`config.toml`) — only what's needed to START the
//! bot. The day-to-day moderation settings live in the store and are edited at
//! runtime via slash commands.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    /// Bot token. Prefer the `DISCORD_TOKEN` env var; optional in the file.
    #[serde(default)]
    pub token: Option<String>,
    /// The guild the bot operates in (slash commands register here).
    #[serde(default)]
    pub guild_id: String,
    /// User IDs allowed to run admin commands, in addition to anyone with the
    /// Manage Server permission.
    #[serde(default)]
    pub owner_ids: Vec<String>,
    /// Path to the embedded database file (created on first run).
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Optional web dashboard. Off unless `[dashboard]` is present and enabled.
    #[serde(default)]
    pub dashboard: DashboardConfig,
}

/// Web-dashboard bootstrap settings. The dashboard is a small HTTP service run
/// inside the bot process (sharing the same store); it stays OFF unless this is
/// enabled and the Discord OAuth credentials are filled in.
#[derive(Debug, Clone, Deserialize)]
pub struct DashboardConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Address to bind the HTTP server to.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Public base URL the dashboard is reached at (used to build the OAuth
    /// redirect, e.g. `https://mod.example.com`). No trailing slash.
    #[serde(default)]
    pub base_url: String,
    /// Discord application OAuth2 client id + secret (Dev Portal → OAuth2). The
    /// `<base_url>/api/callback` redirect must be registered there too.
    #[serde(default)]
    pub oauth_client_id: String,
    #[serde(default)]
    pub oauth_client_secret: String,
}

fn default_db_path() -> String {
    "modbot.redb".to_string()
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_bind(),
            base_url: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
        }
    }
}

impl DashboardConfig {
    /// True only when enabled AND the OAuth credentials + base URL are all set,
    /// so a half-configured dashboard never starts.
    pub fn is_ready(&self) -> bool {
        self.enabled
            && !self.base_url.trim().is_empty()
            && !self.oauth_client_id.trim().is_empty()
            && !self.oauth_client_secret.trim().is_empty()
    }
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            token: None,
            guild_id: String::new(),
            owner_ids: Vec::new(),
            db_path: default_db_path(),
            dashboard: DashboardConfig::default(),
        }
    }
}

impl BotConfig {
    /// Load and parse `config.toml` at `path`.
    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("parse {path}: {e}"))
    }

    /// Resolve the token: the `DISCORD_TOKEN` env var wins, else the config
    /// `token`. Errors if neither is set.
    pub fn resolve_token(&self) -> Result<String, String> {
        if let Ok(t) = std::env::var("DISCORD_TOKEN") {
            if !t.trim().is_empty() {
                return Ok(t);
            }
        }
        self.token
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "no bot token: set the DISCORD_TOKEN env var or `token` in config.toml".to_string()
            })
    }

    /// Is `user_id` an explicitly-listed owner? (Manage-Server holders are
    /// authorised separately, at the gateway, where the permission is known.)
    pub fn is_owner(&self, user_id: &str) -> bool {
        self.owner_ids.iter().any(|o| o == user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_toml_with_defaults() {
        let c: BotConfig = toml::from_str(r#"guild_id = "123""#).unwrap();
        assert_eq!(c.guild_id, "123");
        assert_eq!(c.db_path, "modbot.redb");
        assert!(c.owner_ids.is_empty());
        assert!(c.token.is_none());
    }

    #[test]
    fn owner_check() {
        let c = BotConfig {
            owner_ids: vec!["1".into()],
            ..Default::default()
        };
        assert!(c.is_owner("1"));
        assert!(!c.is_owner("2"));
    }
}
