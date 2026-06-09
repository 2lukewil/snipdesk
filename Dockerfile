# syntax=docker/dockerfile:1.7

# --- Builder stage ---
# Pull a recent Debian-based Rust image so we have OpenSSL + glibc paired
# with the runtime layer below. rust:1.88 matches rust-toolchain.toml.
FROM rust:1.88-bookworm AS builder

WORKDIR /src

# Pre-fetch dependencies for cache efficiency: copy only the Cargo
# manifests + lockfile, build a no-op binary, THEN copy the real sources.
# This way changes to source code don't bust the deps layer; only Cargo
# changes do. Saves minutes on every rebuild.
COPY Cargo.toml Cargo.lock ./
COPY crates/snipdesk-core/Cargo.toml crates/snipdesk-core/Cargo.toml
COPY crates/snipdesk-server/Cargo.toml crates/snipdesk-server/Cargo.toml
COPY crates/snipdesk-teams/Cargo.toml crates/snipdesk-teams/Cargo.toml
COPY src-tauri/Cargo.toml src-tauri/Cargo.toml

# Create stub source trees so cargo can resolve the workspace.
RUN mkdir -p crates/snipdesk-core/src crates/snipdesk-server/src \
             crates/snipdesk-teams/src src-tauri/src && \
    echo "fn main() {}" > crates/snipdesk-server/src/main.rs && \
    echo "" > crates/snipdesk-core/src/lib.rs && \
    echo "" > crates/snipdesk-teams/src/lib.rs && \
    echo "fn main() {}" > src-tauri/src/main.rs && \
    echo "" > src-tauri/src/lib.rs && \
    echo "fn main() {}" > src-tauri/build.rs && \
    cargo build --release --bin snipdesk-server || true

# Now copy the real source.
COPY . .

# Real build. The deps layer is already cached; only the server crate
# itself rebuilds from scratch in the typical "source change" loop.
RUN touch crates/snipdesk-server/src/main.rs && \
    cargo build --release --bin snipdesk-server

# --- Runtime stage ---
# debian:bookworm-slim is ~30MB; we add ca-certificates so outbound HTTPS
# (e.g. OIDC discovery later) works. Distroless or scratch would shave
# another 20MB but make debugging much harder; bookworm-slim is the
# sweet spot for an internal tool.
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Non-root user. UID 10001 is arbitrary; high enough to avoid colliding
# with system accounts in any base distro the operator might use.
RUN useradd --system --uid 10001 --user-group --shell /usr/sbin/nologin snipdesk && \
    mkdir -p /var/lib/snipdesk /etc/snipdesk && \
    chown -R snipdesk:snipdesk /var/lib/snipdesk

COPY --from=builder /src/target/release/snipdesk-server /usr/local/bin/snipdesk-server

# Ship the documented config schema inside the image so a confused
# operator can `docker cp <container>:/etc/snipdesk/config.toml.example .`
# without needing the repo. Doesn't replace the operator's real
# /etc/snipdesk/config.toml mount; sits alongside it as a reference.
COPY --from=builder /src/crates/snipdesk-server/snipdesk-server.example.toml /etc/snipdesk/config.toml.example

# Whitelabel build args. Defaults produce the vanilla "SnipDesk"
# image; per-customer image builds pass --build-arg to bake the
# customer's brand + deep-link scheme allowlist into the image.
# The server reads these at startup (see config::apply_env_overrides)
# with env > TOML precedence, so the customer's mounted TOML can
# focus on secrets + deployment knobs and brand fields stay baked
# in across `docker pull`.
ARG BRAND_NAME=SnipDesk
ARG DEEP_LINK_SCHEMES=snipdesk
ENV SNIPDESK_BRAND_NAME=$BRAND_NAME
ENV SNIPDESK_OIDC_ALLOWED_SCHEMES=$DEEP_LINK_SCHEMES

USER snipdesk
WORKDIR /var/lib/snipdesk
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/snipdesk-server"]
CMD ["--config", "/etc/snipdesk/config.toml"]
