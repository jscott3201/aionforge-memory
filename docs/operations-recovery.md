# Operations and recovery

This guide covers the operator-facing binary path: how a host loads config, starts the MCP
server, and validates a durable store after a restart or incident.

## Config and data directory

The binary loads config in this order:

1. compiled defaults
2. the TOML file at `~/.aionforge/config.toml`, or `--config <path>`
3. environment variables prefixed with `AIONFORGE_`, nested on `__`
4. command-line flags such as `--data-dir`

The data directory defaults to `~/.aionforge` and can be set with
`persistence.data_dir`, `AIONFORGE_PERSISTENCE__DATA_DIR`, or `--data-dir`. On Unix, durable
stores require the final directory to be owner-only. Fresh directories are created as
`0700`; pre-existing directories with group or other access, and symlink final paths, are
refused.

For production, set the write and audit posture explicitly:

```toml
[security]
signed_writes = true
sign_audit_events = true
clock_skew_tolerance_ms = 60000
```

`signed_writes` requires hosts to enroll writers and attach Ed25519 provenance signatures to
capture requests. `sign_audit_events` lets the substrate sign the audit rows it authors; it
creates or loads its key material under the data directory unless `security.audit_key_env`
names an environment-held seed.

## Fresh deploy

Use `doctor` before exposing the MCP server:

```bash
aionforge --config /etc/aionforge/config.toml doctor
```

`doctor` opens the configured store with `open_or_recover`: if no WAL exists yet, it creates
a fresh migrated store; if one exists, it replays it. It reports schema version, index and
provider inventory, embedder dimension consistency, consolidation lag, and graph capacity.

Start the MCP server from the same single binary:

```bash
AIONFORGE_MCP_TOKEN=change-me \
  aionforge --config /etc/aionforge/config.toml \
  serve http --listen 127.0.0.1:3918 \
  --bearer-token-env AIONFORGE_MCP_TOKEN
```

## Recovery validation

Use `recover` when you specifically need to validate an existing WAL-backed store:

```bash
aionforge --config /etc/aionforge/config.toml recover
aionforge --config /etc/aionforge/config.toml recover --json
```

Unlike `doctor`, `recover` calls `Store::recover` directly. It refuses a missing data
directory or missing WAL instead of creating a new store, then wires the replayed store through
the same engine host config used by `serve` and emits the doctor snapshot. A healthy result
means the configured embedder dimension matches the recovered store, the schema is current,
native indexes and maintained candidate-state providers rebuilt, and consolidation lag can be
read. Because `recover` builds the normal engine facade, startup hooks that `serve` would run
also run here; with audit signing enabled, that can provision or heal the audit-key genesis
event.

Run `recover` with the same config and environment the service uses. A dimension change after
the store was created is a hard recovery failure; vector index dimensions are binding storage
state, not a runtime preference.

## Backup boundary

Current v1 persistence is WAL-backed. Recovery replays the WAL into a closed graph and rebuilds
schema, indexes, and candidate-state providers from primary graph values. The store does not yet
drive scheduled snapshot publication or WAL truncation, so backups must include the whole data
directory. This also means hard-erased values can remain in the WAL until snapshot publication is
wired; see [Erasure](erasure.md) for that residency boundary.
