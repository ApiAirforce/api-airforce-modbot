# syntax=docker/dockerfile:1

# ── builder ───────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

# The rustls crypto provider (aws-lc-rs) builds a small amount of C with cmake.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release --bin airforce-modbot

# ── runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# TLS root certificates for the Discord HTTPS/WSS connection.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The bot reads ./config.toml and writes its database (default modbot.redb) in
# the working directory — mount a volume at /data to persist both.
WORKDIR /data
COPY --from=builder /app/target/release/airforce-modbot /usr/local/bin/airforce-modbot

ENTRYPOINT ["airforce-modbot"]
