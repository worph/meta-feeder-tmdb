# syntax=docker/dockerfile:1
# =============================================================================
# card-feeder — metadata-bridge feeder sidecar (the card tier). Multi-stage.
#
# The build context must be the REPO ROOT (the workspace) so the
# `meta-feeder-sdk` path dependency (../../crates/meta-feeder-sdk) resolves.
# Same BuildKit cache-mount strategy as the other feeders: the cargo
# registry/git and `target/` persist across builds; the release binary is
# copied OUT of the target cache mount within the same RUN.
# =============================================================================

FROM rust:1.89-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml rust-toolchain.toml ./
COPY Cargo.lock* ./
COPY crates/ crates/
COPY feeder-plugin/ feeder-plugin/

RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=meta-feeder-card-target,target=/build/target \
    cargo build --release --bin card-feeder \
    && cp target/release/card-feeder /card-feeder

# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /card-feeder /usr/local/bin/card-feeder

RUN useradd --system --uid 10001 --no-create-home metafeeder \
    && mkdir -p /data/meta-feeder \
    && chown metafeeder:metafeeder /data/meta-feeder

USER metafeeder
WORKDIR /data/meta-feeder

EXPOSE 8080

ENV META_FEEDER_HTTP_LISTEN=0.0.0.0:8080 \
    META_FEEDER_STATE_DIR=/data/meta-feeder \
    RUST_LOG=info

ENTRYPOINT ["/usr/local/bin/card-feeder"]
