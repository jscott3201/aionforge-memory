# Security model

Aionforge Memory treats memory as untrusted, multi-tenant state. The core
security posture is fail-closed writes, principal-scoped reads, signed provenance
when enabled, and prompt-injection-safe recall rendering.

## Namespace authorization

Every read and write is evaluated against a `Principal` and an authorization
policy. By default, an agent can write its private namespace and member team
namespaces. Reads are bounded by the visible set computed for that principal, so
a recall should not reveal the existence of hidden memories through result
content, counts, or ordinary denials.

The MCP server enforces the same boundary. Mutating tools require a viewer and
namespace context, and `forget`/`unforget` require write authority over the
target namespace. The `system` namespace and `system` role are excluded from
default recall; surfacing them is an admin/library path, not an MCP search flag.

## Untrusted recall

Recalled memory is third-party data, not instructions. Rendered recall bundles
are wrapped in `<recalled-memory-context>` and memory content is tag-escaped so a
stored string cannot close the wrapper or forge a higher-trust tag. The MCP
surface exposes the same rule in server instructions and in the
`recall_untrusted_data` prompt.

This is a boundary between the memory substrate and the caller's model. Hosts
must preserve the wrapper and prompt rule when splicing recall into a model
context.

## Capture-time hardening

Capture runs a conservative privacy and injection-marker filter before storage.
The marker set is precision-first and measured in CI against benign and hostile
corpora, but it is not a complete semantic injection detector. It blocks known
imperative override patterns at write time; recall-side untrusted-data tagging,
system-role exclusion, namespace authorization, and the red-team probes carry the
rest of the defense. A capture hollowed out by marker excision is refused outright
(`ResidueRejected` audit) rather than stored as a junk fragment, and the refusal
triggers only when a marker fired, so the measured benign false-positive ceiling
is unaffected.

## Signed writes and audit signing

Development defaults keep signed writes off so a local host can start without key
enrollment. Production deployments should make the signing posture explicit:

```toml
[security]
signed_writes = true
sign_audit_events = true
clock_skew_tolerance_ms = 60000
```

`signed_writes` requires hosts to enroll writers and attach Ed25519 signatures to
captures. Verification checks the signed payload, writer identity, host-supplied
episode id collision, and clock-skew window. Unsigned writes are refused while
the gate is on.

`sign_audit_events` lets the substrate sign audit rows it authors. By default it
self-custodies a seed under the locked-down data directory. Set
`security.audit_key_env` to move that seed into an operator secret manager.

## Promotion and trust

Quorum promotion and reliability scoring are off by default. When enabled, a
team fact promotes to global only after independent signed attestations satisfy
both a count gate and a reliability-weighted posterior threshold. Reliability is
folded from append-only audit events; cached trust can be recomputed from the
ledger.

Sensitive categories can raise promotion thresholds. Attestations bind to the
whole proposed transition, not just a stable subject id.

## MCP transport security

Stdio is for private process boundaries. Streamable HTTP binds to loopback by
default and is intended for local clients on the same host. HTTP auth is
default-off: in local mode, callers supply agent identity through an explicit
`principal` object or legacy tool parameters (`agent_id`, `viewer`, and
`teams`), and the engine applies namespace authorization from those values. When
`[auth].enabled=true`, `aionforge serve http` validates bearer tokens for
`/mcp`, maps verified claims to an authoritative principal, and refuses requests
that reach identity-bearing handlers without that validated identity. For remote
multi-user deployments, either enable that built-in HTTP OAuth posture or put an
OAuth-aware verifier/equivalent perimeter in front of the MCP service. Any
verifier must validate issuer, expiry, audience or resource binding, and scopes
before requests reach Aionforge, and it must not pass through access tokens that
were issued for another resource.

Client approval policy still matters. Read-like tools can usually be preapproved
for a trusted local agent; mutating tools (`capture`, `consolidate`, `forget`,
`unforget`) should stay prompt-gated unless the host supplies a stronger policy.

## Red-team gate

The M6 red-team suite runs as an explicit release-gate job. It covers query-only
injection, poisoned-RAG recall, malicious-skill promotion, subliminal trait
transfer, signature forgery, clock-skew replay, and cross-namespace extraction.
Reports use fixed thresholds in the `aionforge-redteam` crate; the release gate
must not relax them to get green.

## What remains outside the boundary

Aionforge does not make model outputs safe by itself. It can store, authorize,
rank, sign, audit, and render memory safely, but the host model can still ignore
instructions, mishandle untrusted data, or over-trust recall. Provider keys,
OAuth token validation, TLS termination, backups, and host-level policy remain
operator responsibilities.

See [Namespace authorization](namespace-authorization.md),
[Provenance signing](provenance-signing.md), [The audit subgraph](audit-subgraph.md),
[MCP client support](mcp-clients.md), and [Red-team suite](red-team.md).
