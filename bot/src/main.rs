//! `airforce-modbot` — the self-hostable Discord moderation bot binary.
//!
//! Wiring lands here: load `config.toml`, open the embedded store, connect to
//! the Discord gateway, and dispatch events to the moderation core. The gateway
//! handler, the `redb` store adapter, the TOML bootstrap config, and the admin
//! slash commands are added in the next build steps.

fn main() {
    // Sanity check that the core links and is reachable from the binary; the
    // real gateway loop replaces this in the next step.
    let cfg = airforce_modbot_core::LinkFilterConfig::default();
    println!(
        "airforce-modbot v{} — core linked (filter enabled by default: {}). Gateway not wired yet.",
        env!("CARGO_PKG_VERSION"),
        cfg.enabled
    );
}
