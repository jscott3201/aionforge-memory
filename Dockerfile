# syntax=docker/dockerfile:1.7

FROM rust:1.95.0-alpine3.23 AS builder

ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
ENV GIT_SSH_COMMAND="ssh -o StrictHostKeyChecking=accept-new"

WORKDIR /workspace

# hadolint ignore=DL3018
RUN apk add --no-cache ca-certificates git openssh-client

COPY . .

RUN --mount=type=ssh,required=true cargo build --locked --release -p aionforge-cli

FROM scratch AS binary-artifact

COPY --from=builder /workspace/target/release/aionforge /aionforge

FROM alpine:3.22 AS runtime

LABEL org.opencontainers.image.source="https://github.com/Aionforge-Labs/aionforge-memory"
LABEL org.opencontainers.image.description="Aionforge Memory single-binary MCP server"

ENV AIONFORGE_PERSISTENCE__DATA_DIR=/data

# hadolint ignore=DL3018
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 aionforge \
    && adduser -S -D -H -h /nonexistent -G aionforge -u 10001 aionforge \
    && mkdir -p /data \
    && chown -R 10001:10001 /data

COPY --from=builder /workspace/target/release/aionforge /usr/local/bin/aionforge

USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
EXPOSE 3918
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/aionforge"]
CMD ["serve", "http", "--listen", "0.0.0.0:3918", "--data-dir", "/data", "--bearer-token-env", "AIONFORGE_MCP_TOKEN"]
