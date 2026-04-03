# ─── Stage 1: Builder ────────────────────────────────────────────────────────
FROM rust:slim-bookworm AS builder

WORKDIR /app

# System deps for native-tls
RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Cache dependency build (dummy main speeds up layer caching)
COPY Cargo.toml ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Build real source
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ─── Stage 2: Minimal runtime ────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ws-latency-monitor /usr/local/bin/

# ── Defaults (override via -e or cloud env panel) ─────────────────────────────
ENV RUST_LOG=warn
ENV PING_INTERVAL_MS=1000
ENV REPORT_INTERVAL_SEC=10
ENV WINDOW_SIZE=100
ENV PING_TIMEOUT_MS=5000
ENV MAX_RECONNECTS=0

ENTRYPOINT ["ws-latency-monitor"]
