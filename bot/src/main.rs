//! `airforce-modbot` — the self-hostable Discord moderation bot binary.
//!
//! Loads the bootstrap config, opens the embedded store, connects to the Discord
//! gateway and runs the moderation event handler (anti-ad link filter +
//! escape-proof jail + strikes). Runtime configuration is done via slash commands.

mod commands;
mod config;
mod handler;
mod invite_filter;
mod jail;
mod store;

use std::sync::Arc;

use serenity::all::{Client, GatewayIntents};

use config::BotConfig;
use handler::Handler;
use store::RedbStore;

#[tokio::main]
async fn main() {
    println!("airforce-modbot v{}", env!("CARGO_PKG_VERSION"));

    let cfg = match BotConfig::load("config.toml") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}\n→ copy config.example.toml to config.toml and fill it in.");
            std::process::exit(1);
        }
    };

    let token = match cfg.resolve_token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let store = match RedbStore::open(&cfg.db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("store error: {e}");
            std::process::exit(1);
        }
    };

    // GUILD_MEMBERS + MESSAGE_CONTENT are privileged — enable them for this app
    // in the Discord Developer Portal (Bot → Privileged Gateway Intents).
    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::MESSAGE_CONTENT;

    let handler = Handler::new(store, Arc::new(cfg));
    let mut client = match Client::builder(&token, intents).event_handler(handler).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build Discord client: {e}");
            std::process::exit(1);
        }
    };

    println!("airforce-modbot starting…");
    if let Err(e) = client.start().await {
        eprintln!("client error: {e}");
        std::process::exit(1);
    }
}
