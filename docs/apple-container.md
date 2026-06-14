# Apple container

Apple's `container` runtime can run the OCI image published for Aionforge
Memory. It is a local macOS path for Apple silicon machines; release publishing
still uses GHCR and the existing Docker/buildx workflow.

## Prerequisites

- Apple silicon Mac running macOS 26.
- Apple `container` 1.0.0 or newer, installed from
  <https://github.com/apple/container/releases>.
- The container service started with `container system start`.

Check the local install:

```bash
container system version
container system status
```

## Run the published image

Published GHCR images are OCI-compatible. On Apple silicon, pull or run the
`linux/arm64` image:

```bash
scripts/container-dev.sh pull
```

Run the server on loopback:

```bash
scripts/container-dev.sh run
```

The script prints the MCP endpoint. By default the endpoint is:

```text
http://127.0.0.1:3918/mcp
```

Equivalent raw commands:

```bash
container system start
container image pull --platform linux/arm64 ghcr.io/jscott3201/aionforge-memory:0.1.0
container run -d \
  --name aionforge-memory \
  --platform linux/arm64 \
  --publish 127.0.0.1:3918:3918 \
  ghcr.io/jscott3201/aionforge-memory:0.1.0
```

The runtime image keeps the same default command as Docker:

```bash
aionforge serve http --listen 0.0.0.0:3918 --data-dir /data
```

Publish the container port to host loopback unless an external verifier is
protecting the endpoint.

## Operate the local container

```bash
scripts/container-dev.sh status
scripts/container-dev.sh logs
scripts/container-dev.sh stop
scripts/container-dev.sh start
```

The helper uses a named container, `aionforge-memory`, and leaves `/data` inside
that container by default. Stop/start keeps the state. Deleting the container
removes that state:

```bash
scripts/container-dev.sh delete
```

Export the stopped container before deleting it if you need a host-side backup:

```bash
container stop aionforge-memory
container export -o aionforge-memory.tar aionforge-memory
```

## Notes

- Apple `container` runs Linux containers in lightweight VMs. It does not build
  native macOS containers.
- The release workflow continues to publish Linux `amd64` and `arm64` OCI
  images to GHCR.
- Local source builds remain on the existing Docker/buildx path. The supported
  Apple `container` path is running the published OCI image.
- Docker named-volume commands in other docs are not drop-in Apple `container`
  commands. Use the named-container flow above unless you have tested a bind
  mount ownership model for your host.
