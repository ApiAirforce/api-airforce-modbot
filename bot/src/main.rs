//! `airforce-modbot` — the self-hostable Discord moderation bot binary.
//!
//! Current state: loads the bootstrap config and opens the embedded store. The
//! Discord gateway connection, the jail mechanics, the event handler and the
//! admin slash commands are wired in the next build steps.

mod config;
mod store;

use config::BotConfig;
use store::RedbStore;

fn main() {
    println!("airforce-modbot v{}", env!("CARGO_PKG_VERSION"));

    let cfg = match BotConfig::load("config.toml") {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "no usable config.toml ({e}).\n\
                 → copy config.example.toml to config.toml and fill it in.\n\
                 (the gateway is not wired yet, so the bot does not connect.)"
            );
            return;
        }
    };

    match RedbStore::open(&cfg.db_path) {
        Ok(_store) => println!(
            "config OK — guild {}, store at {}. Gateway wiring lands next.",
            if cfg.guild_id.is_empty() { "<unset>" } else { &cfg.guild_id },
            cfg.db_path
        ),
        Err(e) => eprintln!("failed to open store at {}: {e}", cfg.db_path),
    }
}
