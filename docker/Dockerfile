# syntax=docker/dockerfile:1.7
#
# Build once, ship three binaries: the daemon, the MCP bridge and the CLI.
#
# INTENT: the runtime stage carries no toolchain and no source. `rusqlite` is
# built with the bundled SQLite (static, no system package) and `reqwest` uses
# rustls, so the final image needs nothing but CA certificates.

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Dependencies first, so editing our own sources does not re-download the world.
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --workspace --locked || cargo build --release --workspace

# The cache mount is not part of the image layer, so the artefacts are copied out.
RUN --mount=type=cache,target=/build/target \
    mkdir -p /out && \
    cp target/release/finderyd /out/ && \
    cp target/release/findery-mcp /out/ && \
    cp target/release/findery /out/

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# Never root: the daemon only ever reads the mounted workspaces.
RUN useradd --system --create-home --uid 10001 embedder

COPY --from=builder /out/finderyd    /usr/local/bin/finderyd
COPY --from=builder /out/findery-mcp /usr/local/bin/findery-mcp
COPY --from=builder /out/findery     /usr/local/bin/findery

RUN mkdir -p /data /etc/findery && chown -R embedder:embedder /data

USER embedder
WORKDIR /data

ENV FINDERY_DATA_DIR=/data \
    FINDERY_EXPORT_DIR=/data/exports \
    FINDERY_BIND=0.0.0.0:8077 \
    FINDERY_LOG=info,tower_http=warn

EXPOSE 8077

# `/healthz` is liveness only and needs no token, which is exactly what a
# container health check should probe.
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8077/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/finderyd"]
