//! Optional web dashboard — a small `axum` HTTP service run **inside the bot
//! process**, sharing the very same `redb` store as the gateway (so there is one
//! source of truth and no second database to keep in sync).
//!
//! It does what the slash commands do, in a browser: log in with Discord
//! (OAuth2), pick a server you manage, and view/edit every per-guild setting plus
//! the recent cases / strikes / jails. Writes go through the **same**
//! `core`-side `validate()` the slash commands use, so the two paths can't drift.
//!
//! Security model: Discord OAuth gives us the user's id + the guilds they're in
//! with their permission bitmask. A guild is editable only if the user has
//! **Manage Server** there (or is a configured bot owner) AND the bot is actually
//! in that guild. Sessions are opaque random tokens kept in memory (a restart
//! just means re-logging in). The dashboard stays OFF unless `[dashboard]` is
//! enabled with OAuth credentials.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use airforce_modbot_core::{
    AiModConfig, AntinukeConfig, AutomodConfig, FloodFilterConfig, JailConfig, LinkFilterConfig,
    ModConfig, RaidConfig,
};

use crate::config::BotConfig;
use crate::store::RedbStore;

const DISCORD_API: &str = "https://discord.com/api/v10";
/// Discord `MANAGE_GUILD` permission bit.
const MANAGE_GUILD: u64 = 1 << 5;
/// How long a login session stays valid. Kept short on purpose: the user's
/// Manage-Server permission is snapshotted at login and not re-checked until the
/// next one, so a short TTL bounds how long a just-revoked admin keeps access.
/// Re-login is silent (`prompt=none`), so the cost is ~one click.
const SESSION_TTL_SECS: i64 = 60 * 30;

// ── shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    store: Arc<RedbStore>,
    config: Arc<BotConfig>,
    bot_token: String,
    http: reqwest::Client,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

#[derive(Clone, Serialize)]
struct GuildInfo {
    id: String,
    name: String,
    icon: Option<String>,
}

#[derive(Clone)]
struct Session {
    user_id: String,
    username: String,
    guilds: Vec<GuildInfo>,
    expires_unix: i64,
}

impl Session {
    fn manages(&self, guild_id: &str) -> bool {
        self.guilds.iter().any(|g| g.id == guild_id)
    }
}

/// Start the dashboard HTTP server. Returns when the server stops (it normally
/// runs for the bot's lifetime). The caller spawns this alongside the gateway.
pub async fn serve(store: Arc<RedbStore>, config: Arc<BotConfig>, bot_token: String) {
    let state = AppState {
        store,
        config: config.clone(),
        bot_token,
        http: reqwest::Client::new(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/login", get(login))
        .route("/api/callback", get(callback))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/guilds/:id/config", get(get_config))
        .route("/api/guilds/:id/config/:section", put(put_config))
        .route("/api/guilds/:id/cases", get(list_cases))
        .route("/api/guilds/:id/strikes", get(list_strikes))
        .route("/api/guilds/:id/jails", get(list_jails))
        .with_state(state);

    let bind = &config.dashboard.bind;
    match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => {
            println!("🖥️  dashboard listening on http://{bind} (login at {}/api/login)", config.dashboard.base_url);
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("❌ dashboard server error: {e}");
            }
        }
        Err(e) => eprintln!("❌ dashboard: could not bind {bind}: {e}"),
    }
}

// ── static assets (embedded — single self-contained binary) ──────────────────

async fn index() -> Html<&'static str> {
    Html(include_str!("web/index.html"))
}
async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("web/app.js"))
}
async fn style_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("web/style.css"))
}

// ── OAuth login flow ─────────────────────────────────────────────────────────

async fn login(State(st): State<AppState>) -> Response {
    let d = &st.config.dashboard;
    let redirect = format!("{}/api/callback", d.base_url.trim_end_matches('/'));
    let state = random_token();
    let url = format!(
        "{DISCORD_API}/oauth2/authorize?client_id={}&response_type=code&scope=identify%20guilds&redirect_uri={}&state={}&prompt=none",
        d.oauth_client_id,
        urlencode(&redirect),
        state,
    );
    // Stash the CSRF state in a short-lived cookie, then bounce to Discord.
    let cookie = format!("oauth_state={state}; Max-Age=600; Path=/; HttpOnly; SameSite=Lax");
    ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return (StatusCode::BAD_REQUEST, "missing code/state").into_response();
    };
    if cookie_value(&headers, "oauth_state").as_deref() != Some(state.as_str()) {
        return (StatusCode::BAD_REQUEST, "state mismatch — please retry login").into_response();
    }

    let d = &st.config.dashboard;
    let redirect = format!("{}/api/callback", d.base_url.trim_end_matches('/'));

    // 1) exchange the code for a user access token.
    let token_res = st
        .http
        .post(format!("{DISCORD_API}/oauth2/token"))
        .form(&[
            ("client_id", d.oauth_client_id.as_str()),
            ("client_secret", d.oauth_client_secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
        ])
        .send()
        .await;
    let access_token = match token_res {
        Ok(r) if r.status().is_success() => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["access_token"].as_str().map(String::from)),
        Ok(r) => {
            eprintln!("⚠️ dashboard oauth: token exchange returned {}", r.status());
            None
        }
        Err(e) => {
            eprintln!("⚠️ dashboard oauth: token exchange failed: {e}");
            None
        }
    };
    let Some(access_token) = access_token else {
        return (StatusCode::BAD_GATEWAY, "Discord login failed").into_response();
    };

    // 2) who is this + which guilds do they manage?
    let user = discord_get(&st.http, "/users/@me", &format!("Bearer {access_token}")).await;
    let user_guilds = discord_get(&st.http, "/users/@me/guilds", &format!("Bearer {access_token}")).await;
    let (Some(user), Some(user_guilds)) = (user, user_guilds) else {
        return (StatusCode::BAD_GATEWAY, "could not read your Discord profile").into_response();
    };
    let user_id = user["id"].as_str().unwrap_or_default().to_string();
    let username = user["username"].as_str().unwrap_or("user").to_string();

    // 3) which guilds is the BOT in? (only those are editable).
    // NOTE: `/users/@me/guilds` returns one page (~200). For a bot/user in more
    // than that this list is truncated, so some manageable guilds may not appear
    // (under-grant only — never an over-grant). Pagination (`?after=`) is a TODO
    // for very large bots.
    let bot_guilds = discord_get(&st.http, "/users/@me/guilds", &format!("Bot {}", st.bot_token))
        .await
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let bot_guild_ids: std::collections::HashSet<String> = bot_guilds
        .iter()
        .filter_map(|g| g["id"].as_str().map(String::from))
        .collect();

    // editable = the user has Manage Server (or owns it, or is a bot owner) AND
    // the bot is in the guild.
    let is_bot_owner = st.config.is_owner(&user_id);
    let guilds: Vec<GuildInfo> = user_guilds
        .as_array()
        .map(|arr| arr.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|g| {
            let id = g["id"].as_str()?.to_string();
            if !bot_guild_ids.contains(&id) {
                return None;
            }
            let owner = g["owner"].as_bool().unwrap_or(false);
            let perms = g["permissions"].as_str().and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
            if owner || is_bot_owner || perms & MANAGE_GUILD != 0 {
                Some(GuildInfo {
                    id,
                    name: g["name"].as_str().unwrap_or("server").to_string(),
                    icon: g["icon"].as_str().map(String::from),
                })
            } else {
                None
            }
        })
        .collect();

    // 4) mint a session.
    let token = random_token();
    let session = Session {
        user_id,
        username,
        guilds,
        expires_unix: chrono::Utc::now().timestamp() + SESSION_TTL_SECS,
    };
    {
        let mut sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired(&mut sessions);
        sessions.insert(token.clone(), session);
    }
    let secure = if d.base_url.starts_with("https") { "; Secure" } else { "" };
    let session_cookie = format!("session={token}; Max-Age={SESSION_TTL_SECS}; Path=/; HttpOnly; SameSite=Lax{secure}");
    // The CSRF state nonce is single-use — clear it now that it has been consumed.
    let clear_state = "oauth_state=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax".to_string();
    // NOTE: must be `AppendHeaders`, not a plain `[(K, V); N]` array — the array
    // impl of `IntoResponseParts` *inserts* (overwrites) per key, so a second
    // `Set-Cookie` would clobber the session cookie. `AppendHeaders` appends, so
    // both the session cookie and the state-clearing cookie survive.
    (
        AppendHeaders([
            (header::SET_COOKIE, session_cookie),
            (header::SET_COOKIE, clear_state),
        ]),
        Redirect::to("/"),
    )
        .into_response()
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, "session") {
        st.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
    }
    let cookie = "session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax";
    ([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response()
}

async fn me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match session_of(&st, &headers) {
        Some(s) => Json(json!({
            "user": { "id": s.user_id, "username": s.username },
            "guilds": s.guilds,
        }))
        .into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({"error": "not logged in"}))).into_response(),
    }
}

// ── config read / write (the same blobs the slash commands manage) ───────────

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn deny(code: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg })))
}

/// Resolve the session and confirm it manages `guild`, else an error response.
fn authorize(st: &AppState, headers: &HeaderMap, guild: &str) -> Result<Session, (StatusCode, Json<Value>)> {
    let Some(s) = session_of(st, headers) else {
        return Err(deny(StatusCode::UNAUTHORIZED, "not logged in"));
    };
    if !s.manages(guild) {
        return Err(deny(StatusCode::FORBIDDEN, "you don't manage that server (or the bot isn't in it)"));
    }
    Ok(s)
}

async fn get_config(State(st): State<AppState>, headers: HeaderMap, Path(guild): Path<String>) -> ApiResult {
    authorize(&st, &headers, &guild)?;
    let s = &*st.store;
    Ok(Json(json!({
        "link": LinkFilterConfig::load_for_guild(s, &guild),
        "flood": FloodFilterConfig::load_for_guild(s, &guild),
        "automod": AutomodConfig::load_for_guild(s, &guild),
        "jail": JailConfig::load_for_guild(s, &guild),
        "raid": RaidConfig::load_for_guild(s, &guild),
        "antinuke": AntinukeConfig::load_for_guild(s, &guild),
        "ai": AiModConfig::load_for_guild(s, &guild),
        "mod": ModConfig::load_for_guild(s, &guild),
    })))
}

async fn put_config(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((guild, section)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult {
    authorize(&st, &headers, &guild)?;
    let s = &*st.store;

    // Deserialize into the right config type, stamp the guild, validate via the
    // SAME core logic the slash commands use, then persist. One source of truth.
    macro_rules! save {
        ($ty:ty, $set_guild:expr) => {{
            let mut cfg: $ty = serde_json::from_value(body).map_err(|e| deny(StatusCode::BAD_REQUEST, &format!("bad config: {e}")))?;
            $set_guild(&mut cfg);
            cfg.validate().map_err(|e| deny(StatusCode::BAD_REQUEST, &e))?;
            cfg.save_for_guild(s, &guild).map_err(|e| deny(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
        }};
    }
    match section.as_str() {
        "link" => save!(LinkFilterConfig, |c: &mut LinkFilterConfig| c.guild_id = guild.clone()),
        "flood" => save!(FloodFilterConfig, |c: &mut FloodFilterConfig| c.guild_id = guild.clone()),
        "automod" => save!(AutomodConfig, |c: &mut AutomodConfig| c.guild_id = guild.clone()),
        "jail" => save!(JailConfig, |c: &mut JailConfig| c.guild_id = guild.clone()),
        "raid" => save!(RaidConfig, |c: &mut RaidConfig| c.guild_id = guild.clone()),
        "antinuke" => save!(AntinukeConfig, |c: &mut AntinukeConfig| c.guild_id = guild.clone()),
        "ai" => save!(AiModConfig, |c: &mut AiModConfig| c.guild_id = guild.clone()),
        "mod" => save!(ModConfig, |c: &mut ModConfig| c.guild_id = guild.clone()),
        other => return Err(deny(StatusCode::NOT_FOUND, &format!("unknown section `{other}`"))),
    }
    println!("🖥️  dashboard: {section} config for guild {guild} updated");
    Ok(Json(json!({ "ok": true })))
}

// ── case / strike / jail reads ───────────────────────────────────────────────

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u32>,
}

async fn list_cases(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(guild): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    authorize(&st, &headers, &guild)?;
    Ok(Json(json!(st.store.list_cases_for_guild(&guild, q.limit.unwrap_or(100).min(1000)))))
}

async fn list_strikes(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(guild): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    authorize(&st, &headers, &guild)?;
    Ok(Json(json!(st.store.list_link_strikes_for_guild(&guild, q.limit.unwrap_or(100).min(1000)))))
}

async fn list_jails(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(guild): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult {
    authorize(&st, &headers, &guild)?;
    Ok(Json(json!(st.store.list_jails_for_guild(&guild, q.limit.unwrap_or(100).min(1000)))))
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn session_of(st: &AppState, headers: &HeaderMap) -> Option<Session> {
    let token = cookie_value(headers, "session")?;
    let mut sessions = st.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let now = chrono::Utc::now().timestamp();
    match sessions.get(&token) {
        Some(s) if s.expires_unix > now => Some(s.clone()),
        Some(_) => {
            sessions.remove(&token);
            None
        }
        None => None,
    }
}

fn prune_expired(sessions: &mut HashMap<String, Session>) {
    let now = chrono::Utc::now().timestamp();
    sessions.retain(|_, s| s.expires_unix > now);
}

async fn discord_get(http: &reqwest::Client, path: &str, auth: &str) -> Option<Value> {
    let res = http
        .get(format!("{DISCORD_API}{path}"))
        .header(header::AUTHORIZATION, auth)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json::<Value>().await.ok()
}

/// Read one cookie value out of the `Cookie` request header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// A 32-hex-char opaque token (sessions + OAuth CSRF state).
fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Minimal percent-encoding for the few values we put in the OAuth URL.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "session=abc123; oauth_state=xy".parse().unwrap());
        assert_eq!(cookie_value(&h, "session").as_deref(), Some("abc123"));
        assert_eq!(cookie_value(&h, "oauth_state").as_deref(), Some("xy"));
        assert_eq!(cookie_value(&h, "missing"), None);
    }

    #[test]
    fn urlencode_redirect() {
        assert_eq!(urlencode("https://x.io/api/callback"), "https%3A%2F%2Fx.io%2Fapi%2Fcallback");
    }

    #[test]
    fn random_token_is_32_hex() {
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token());
    }

    #[test]
    fn session_manage_gate() {
        let s = Session {
            user_id: "u".into(),
            username: "u".into(),
            guilds: vec![GuildInfo { id: "g1".into(), name: "G1".into(), icon: None }],
            expires_unix: i64::MAX,
        };
        assert!(s.manages("g1"));
        assert!(!s.manages("g2"));
    }
}
