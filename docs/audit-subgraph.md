# The audit subgraph

Every governance operation the substrate performs — promotions, demotions, attestations,
reliability updates, consolidation decisions, refused writes — leaves an `AuditEvent` row.
Together those rows are the audit subgraph: the forensic record of what the system did and
why, queryable by subject, by kind, and in time order.

This document covers how audit rows are written, how the substrate signs the events it
authors, how signatures are verified on the way back out, and what an operator needs to know
to run it.

## One door for every writer

Audit event ids are content-addressed over everything *except* the signature (a signature
can't sign itself), and the id is unique. That pairing has a sharp edge: whichever copy of an
event lands first owns the row. Before the write funnel existed, anyone who could write the
store could pre-place a blank-signature copy of a predictable id, and the legitimately signed
re-emit of the same event would dedup into silence — the blank copy shadowed the signed one
forever, reading back as harmless "legacy unsigned."

The fix is structural. Every audit author goes through one funnel (`ensure_event`), and the
funnel's dedup probe is private to its module — nothing in the codebase *can* probe-and-create
around it. On a dedup hit the funnel reconciles the stored signature under a one-way latch:

| stored | incoming | result |
|---|---|---|
| same as incoming | same | no-op (deterministic replay) |
| blank | signed | **upgrade in place** — the shadow heal |
| signed | blank | keep — a signature is never stripped |
| signed | different signed | keep, with a warning trace |

Blank → signed is the only transition that ever writes. The keep-on-conflict arm is
load-bearing: the verifier binds each row to the key whose validity window contains the row's
stored ingest time, and a dedup hit never re-stamps that time — replacing the signature with
one made by a newer key would flip the row from valid to invalid.

To make the upgrade legal, `AuditEvent.signature` is the one signature in the schema that is
not declared immutable (the DDL can't express "blank to signed exactly once"; the funnel
enforces it). `ProvenanceRecord` and `ATTESTED_BY` signatures stay immutable — those are
host-signed at capture and never reconciled.

**Operator note:** a store created before this carve-out still binds the old immutable
declaration — recovery replays the persisted schema, and the engine has no `ALTER TYPE`. Such
a store is refused loudly at open with a recreate-the-store message rather than failing at
some later commit. Pre-1.0, recreate from a fresh migration.

## Substrate audit signing

Off by default. Enabled with one boolean, `security.sign_audit_events`, plus a data
directory. When it's on, the substrate stamps every blank-signature audit event at commit
time, inside the funnel — no author site opts in, none can forget.

This is the one place the substrate holds a private key of its own. The writer channel rule
("the host signs, the substrate only verifies") is untouched: the audit key signs only events
the substrate itself authors, never a host's writes. Key generation stays confined to a
single function and is enforced in CI.

### Custody

Two modes, resolved at startup:

- **File custody (default):** a 32-byte seed at `<data_dir>/audit/audit_seed`, minted on
  first enable. The directory is created `0700` and the file `0600` before any bytes land;
  pre-existing looser permissions are refused, not silently tightened. Relative or unsafe
  data directories are refused — a key never lands wherever the process happened to start.
- **Env custody (opt-in):** the config names an environment variable
  (`security.audit_key_env`) holding the seed as base64. The host resolves it; the seed never
  touches disk. The keyring anchor below still does.

### The keyring anchor

Trust in the audit key is anchored *out of band*, in a file —
`<data_dir>/audit/audit_keyring.json` — never in the store itself. An in-store announcement
can be forged by anyone who can write the store; the keyring file can't be extended except by
the custody holding it. The keyring records each key's public half and its validity window;
the file is the sole authority, and a missing keyring with signing enabled fails closed.

On first enable the substrate anchors genesis: it saves the keyring first, then echoes a
self-signed `key_rotation` event into the audit trail. The echo is content-addressed, so a
crash between the two steps heals on the next start — the rebuilt event dedups if the row
exists and fills the gap if it doesn't. A resolved seed that doesn't match the anchor's
active key is refused at startup; the substrate never silently re-anchors to a new identity.

Losing the seed does not invalidate history: verifiability rides on the stored signature and
the published public key, so past rows verify forever. New events simply fall back to
unsigned until a new anchor is deliberately established.

### Reading it back

The audit read surface returns each row with a verification verdict, computed per row — a
tampered row reads as invalid, it never makes the query fail:

- `valid` — the signature checks against the key whose window covers the row's ingest time
- `unsigned` / `downgraded` — blank before the must-sign cutover / blank after it
- `invalid` — a signature that doesn't check
- `untrusted` — signed by a key the anchor doesn't vouch for

When signing is off, rows read back as *not checked* — distinct, on purpose, from a checked
"unsigned." The substrate never fabricates a verdict it didn't compute.

Agent-facing reads (`audit_history`, `audit_by_kind`, `audit_by_subject_kind` on the memory
facade) are namespace-scoped by the same visibility rule as every other read: an agent sees
global rows and its own, never the `system` namespace where governance events live. Full
forensic access — including system rows — is the host's path, through the store-level
readers. Counts and pages stay honest across hidden rows: pages refill rather than shorten.

The MCP `audit_history` tool exposes those same agent-facing axes: pass `subject_id` for a
subject's full history, pass both `subject_id` and `kind` for subject+kind, or omit
`subject_id` and pass `kind` to page every visible event of that kind. The compact kind-only
header uses `subject=*` and each row includes its concrete subject id.

## Runbook

- **Enable:** set `security.sign_audit_events = true` and a data directory. First start
  mints the seed (file mode), anchors the keyring, and signs from there on.
- **Export the seed:** the load-only custody primitive (`load_audit_seed`) reads the seed
  without ever minting — it is the backup path. A CLI wrapper (`audit key export`) ships
  with the binary surface; until then it is a one-line library call for the host.
- **Pin the anchor:** back up `audit_keyring.json` out of band. It holds only public keys —
  it is integrity-sensitive, not secret.
- **Key loss:** past rows stay verifiable. Remove the seed deliberately and let the next
  enable mint a fresh anchor — the mismatch refusal exists precisely so this never happens
  by accident.
- **Retention:** unlimited by default; a compliance-window prune is the forward path and
  arrives with the erasure milestone.
