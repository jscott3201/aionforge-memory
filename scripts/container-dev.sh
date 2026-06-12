#!/usr/bin/env bash
# Run the Aionforge Memory OCI image with Apple's `container` runtime.

set -euo pipefail

IMAGE="${AIONFORGE_CONTAINER_IMAGE:-ghcr.io/aionforge-labs/aionforge-memory:0.1.0}"
NAME="${AIONFORGE_CONTAINER_NAME:-aionforge-memory}"
ARCH="${AIONFORGE_CONTAINER_ARCH:-arm64}"
PLATFORM="${AIONFORGE_CONTAINER_PLATFORM:-linux/$ARCH}"
HOST="${AIONFORGE_CONTAINER_HOST:-127.0.0.1}"
PORT="${AIONFORGE_CONTAINER_PORT:-3918}"

usage() {
  cat <<'USAGE'
Usage: scripts/container-dev.sh <command>

Commands:
  pull     Pull the configured image for the configured platform
  run      Run a named container on 127.0.0.1:3918
  start    Start the named container
  stop     Stop the named container
  logs     Print logs for the named container
  status   List all containers
  delete   Delete the named container and its internal /data state

Environment:
  AIONFORGE_CONTAINER_IMAGE   Image to run (default: ghcr.io/aionforge-labs/aionforge-memory:0.1.0)
  AIONFORGE_CONTAINER_NAME    Container name (default: aionforge-memory)
  AIONFORGE_CONTAINER_ARCH    Image architecture shorthand (default: arm64)
  AIONFORGE_CONTAINER_PLATFORM OCI platform (default: linux/$AIONFORGE_CONTAINER_ARCH)
  AIONFORGE_CONTAINER_HOST    Host bind address (default: 127.0.0.1)
  AIONFORGE_CONTAINER_PORT    Host port (default: 3918)
USAGE
}

require_container() {
  if ! command -v container >/dev/null 2>&1; then
    echo "Apple container CLI not found. Install it from https://github.com/apple/container/releases." >&2
    exit 127
  fi
}

ensure_system() {
  if ! container system status >/dev/null 2>&1; then
    container system start
  fi
}

cmd="${1:-}"
case "$cmd" in
  pull)
    require_container
    ensure_system
    container image pull --platform "$PLATFORM" "$IMAGE"
    ;;
  run)
    require_container
    ensure_system
    container run -d \
      --name "$NAME" \
      --platform "$PLATFORM" \
      --publish "$HOST:$PORT:3918" \
      "$IMAGE"
    echo "MCP endpoint: http://$HOST:$PORT/mcp"
    ;;
  start)
    require_container
    ensure_system
    container start "$NAME"
    ;;
  stop)
    require_container
    container stop "$NAME"
    ;;
  logs)
    require_container
    container logs "$NAME"
    ;;
  status)
    require_container
    container list --all
    ;;
  delete)
    require_container
    container delete "$NAME"
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
