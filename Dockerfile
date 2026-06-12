# syntax=docker/dockerfile:1.7

FROM rust:1.95.0-bookworm AS builder

ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

WORKDIR /workspace

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --locked --release -p aionforge-cli

FROM scratch AS binary-artifact

COPY --from=builder /workspace/target/release/aionforge /aionforge

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/Aionforge-Labs/aionforge-memory"
LABEL org.opencontainers.image.description="Aionforge Memory single-binary MCP server"

ENV AIONFORGE_PERSISTENCE__DATA_DIR=/data

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 aionforge \
    && useradd --system --no-create-home --home-dir /nonexistent --gid aionforge --uid 10001 aionforge \
    && mkdir -p /data \
    && chmod 700 /data \
    && chown -R 10001:10001 /data

COPY --from=builder /workspace/target/release/aionforge /usr/local/bin/aionforge

USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
EXPOSE 3918
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/aionforge"]
CMD ["serve", "http", "--listen", "0.0.0.0:3918", "--data-dir", "/data"]
