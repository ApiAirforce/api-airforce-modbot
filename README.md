# api-airforce-modbot

A small, self-hostable **Discord moderation bot** focused on two things it does
well:

- **Anti-advertising link filter** — deletes messages linking to non-whitelisted
  domains, counts a strike against the author, DMs them a private notice, and
  auto-jails repeat offenders. Whitelist supports apex + wildcard (`*.`) matches;
  strikes decay on a configurable window; thresholds and exemptions are tunable
  down to the individual user or a single (user, channel) pair.
- **A "real" jail** — instead of just adding a muted role, it **snapshots** the
  member's roles, **strips** them, and **restores** them on release. The jail
  survives bot restarts and is **escape-proof**: leaving and rejoining the server
  re-applies it. Manual jail-role assignment by a moderator triggers the same
  snapshot/strip automatically.

It powers the [api.airforce](https://api.airforce) community Discord and is open
source so anyone can run it.

## Status

🚧 **Early development.** The moderation core (link-filter logic, jail rules,
strike accounting) is implemented and unit-tested; the Discord gateway wiring,
the embedded store, and the admin slash commands are being added next. Not yet
ready to run.

## Architecture

A small Cargo workspace, split along a clean **ports & adapters** seam:

- [`core/`](core) — `airforce-modbot-core`: the **platform-agnostic** moderation
  logic. Pure, fully unit-tested, depends on no Discord library and no concrete
  database. Storage and config are abstracted behind the traits in
  [`core/src/ports.rs`](core/src/ports.rs).
- [`bot/`](bot) — `airforce-modbot`: the runnable bot. Implements the core's
  ports over an embedded [`redb`](https://github.com/cberner/redb) store, loads a
  small `config.toml`, connects to the Discord gateway, and exposes the admin
  slash commands.

This is the same core that runs inside api.airforce; the seam is what lets it be
lifted out and self-hosted.

## Configuration

Only start-up settings live in a file; everything else is managed at runtime via
slash commands. Copy [`config.example.toml`](config.example.toml) to
`config.toml` and fill in your bot token (or set `DISCORD_TOKEN`), the guild id,
and optional owner ids. See the example file for details. (Full setup — invite
URL, gateway intents, required permissions — lands with the gateway wiring.)

## Development

```sh
# The pure core builds and tests with no system dependencies:
cargo test -p airforce-modbot-core

# The full workspace (bot included) needs a C toolchain for the TLS stack
# (standard on Linux/macOS CI and in Docker):
cargo build --workspace
cargo test --workspace
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
