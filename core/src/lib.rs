//! `airforce-modbot-core` — the platform-agnostic core of the api.airforce
//! Discord moderation bot.
//!
//! This crate holds the **pure, fully unit-tested logic** (anti-advertising link
//! detection, domain whitelist matching, strike-decay math, and the config
//! shapes) plus the **storage/config ports** the bot needs. It depends on NO
//! Discord library and NO concrete database — the [`ports`] traits are the seam
//! a host implements (the bundled bot backs them with an embedded `redb` store).
//!
//! This is the same Ports-&-Adapters core that runs inside api.airforce; lifting
//! it into this standalone crate is what lets the bot be self-hosted by anyone.

pub mod jail;
pub mod link_filter;
pub mod ports;

pub use jail::JailConfig;
pub use link_filter::LinkFilterConfig;
pub use ports::{ConfigStore, JailRecord, JailStore, LinkStrike, StrikeStore};
