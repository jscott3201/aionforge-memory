# Security Policy

Aionforge Memory is a memory substrate for AI agents: it handles bearer tokens,
multi-tenant namespace boundaries, and recalled content that must never be
treated as executable instructions. Security reports are taken seriously.

## Supported versions

This project is pre-1.0 and ships from the `development` → `main` line. Security
fixes land on the latest published `0.1.x` release; expect schema and API changes
before 1.0. There is no long-term-support branch yet.

| Version | Supported |
| ------- | --------- |
| latest `0.1.x` | ✅ |
| older pre-releases | ❌ (upgrade to the latest) |

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Report it privately
through GitHub's **"Report a vulnerability"** button on the repository's
**Security** tab (GitHub Private Vulnerability Reporting), which opens a private
advisory visible only to the maintainers.

> Maintainer note: Private Vulnerability Reporting must be enabled under
> *Settings → Security → Advanced Security* for the private-report button to be
> available. If it is not yet enabled, contact the maintainer through the
> repository's listed channel before disclosing details publicly.

Please include, as far as you can: affected version or commit, the impacted
surface (CLI, MCP server, store, capture/retrieval/consolidation, auth), a
minimal reproduction, and the impact you observed. **Do not include real bearer
tokens, private keys, or customer data** — redact before sharing.

## What to expect

We aim to acknowledge a report, confirm the issue, and agree on a coordinated
disclosure timeline with the reporter. Fixes are prioritized by severity. We do
not currently commit to a fixed response-time SLA, but we will keep reporters
informed of progress.

## Scope

In scope: the Rust crates, the `aionforge` CLI, the MCP server surface, the
published Docker image, and the configuration and authorization paths. The
threat model and the substrate's security invariants — namespace isolation,
recalled-memory-as-untrusted-data, the no-raw-GQL and audit-keygen-confinement
gates, and the default-off network posture — are documented in
[`docs/security-model.md`](docs/security-model.md), and the repository's
red-team acceptance gates exercise them.

Out of scope: vulnerabilities in third-party dependencies (report those upstream;
we track advisories via `cargo deny`), and issues that require a
already-compromised host or a misconfiguration the documentation warns against
(for example exposing the built-in HTTP server to an untrusted network without an
OAuth-aware perimeter).
