# Provenance signing

Every captured memory records who wrote it. Signed writes make that record provable: the
writer signs each capture with its own key, and the substrate verifies the signature against
the key it has on file before any memory is written. A write whose signature doesn't check
out, or whose timestamp is too far off, is refused and recorded — it never becomes memory.

This is off by default. A development or single-trusted-host deployment runs unsigned with
no overhead. Turning it on is a deliberate production decision.

## The substrate verifies, it never signs

The host holds the private key and signs; the substrate holds only the public key and
verifies. A private key never enters the process. The substrate stores each writer's public
key on its `Agent` record and the signature on the capture's provenance record, and it checks
one against the other — that's the whole of its role in the signing scheme.

Verification is strict: a malleable signature is rejected, not just an outright wrong one.

## What gets signed

The signature covers a fixed, versioned, canonical encoding of three things:

```
canonical( subject_id, writer_agent_id, ingested_at )
```

- `subject_id` — the episode the write creates,
- `writer_agent_id` — the writer doing the signing,
- `ingested_at` — when the write happened, to the millisecond.

The encoding is domain-separated and length-prefixed so two different field splits can never
produce the same bytes, and it carries a version byte so a signature made under one layout can
never validate under another. The writer signs these bytes; the substrate recomputes them from
the request and checks the signature — it never trusts payload bytes sent by the client.

### The host supplies the episode id on the signed path

`subject_id` is the episode's id, and a writer can't sign over an id it doesn't know. On the
unsigned path the substrate mints that id itself, after the request arrives — too late for the
writer to have signed it. So on the signed path the **host mints the episode id**, signs over
it, and ships both the id and the signature. The substrate adopts that id as the episode id, so
the id the writer signed is exactly the id that gets stored.

That moves id allocation, on the signed path, from the substrate to the host. The substrate
defends the one guarantee that move puts at risk — uniqueness — with a collision guard: a signed
write whose subject id already names an episode (live or soft-forgotten) is refused. The
ordinary content-hash de-duplication doesn't cover this, because it keys on content, not id, so
the guard is a separate, explicit check. The unsigned path is unchanged: it still mints a fresh,
time-ordered id server-side and never carries a signature.

## Writers enroll first

A writer must be registered before it can sign. Registration stores its public key on an
`Agent` record. When a signed write arrives, the substrate resolves the writer's key by id; if
there's no registered key, the write is **refused — fail-closed**. There is no lazy enrollment:
an unknown writer can't register itself by writing, which would let an attacker self-enroll a
key and forge from there.

## The clock-skew window

A signed write also has to be recent. The substrate compares the write's timestamp against its
own clock and refuses anything that deviates by more than a configured tolerance, in either
direction. This bounds replay and storm: a captured signed write replayed later falls outside
the window and is dropped. The wall-clock read is used only to accept or reject — it is never
stored, so it doesn't become a guessed timestamp on any record.

The window applies to **every** signed write when signing is on, with no carve-out. An untrusted
write that gets confined to its writer's private namespace is still gated: a replay flood into a
private namespace is still a flood, and exempting it would just reopen the channel the window
exists to close.

## Rejections are audited

A refused write writes no memory, but it doesn't vanish. The substrate commits a single audit
event in its own `System`-namespace transaction, then returns the error:

- a signature, enrollment, or unsigned-write failure records an `invalid_signature` audit,
- a timestamp outside the window records a `clock_skew_rejected` audit,
- a subject-id collision records an `invalid_signature` audit with a collision reason.

The audit payload carries the specific cause for forensics, but the error returned to the caller
is deliberately coarse: an unknown writer, a bad signature, an unsigned write, and a collision
all come back as one signature rejection, so the substrate is neither an enrollment oracle ("is
this agent registered?") nor a forge oracle ("which check failed?"). A clock-skew rejection is
reported on its own, so an honest client can tell it needs to resync its clock.

One failure is not an attack and is not audited as one: if the substrate can't read the writer's
key because a backend read failed, that's an availability fault, and it surfaces as an error
without a security audit — a transient outage should never be written down as a forged write.

## Derived memory stays unsigned

Signing covers the capture path. Memory the substrate authors itself during consolidation —
facts, entities, notes — is written under the substrate's own clock and carries no writer
signature, by design. It isn't an unsigned capture that slipped through; it has no external
writer to sign for it. Signing that with a substrate key would be a different thing entirely
(attestation), and it's out of scope here.

## Turning it on

Signing is controlled by two settings:

- `security.signed_writes` — off by default; set it on to gate every capture.
- `security.clock_skew_tolerance_ms` — the skew window in milliseconds. It defaults to one
  minute and is bounded to five minutes when signing is on, so a misconfiguration can't quietly
  widen the replay window to uselessness. A zero window with signing on is a configuration error,
  not a silent lockout, because it would reject every write.

The gate is built once, at construction, behind the same composition point as the rest of the
capture policy. When signing is off there is no gate at all: no key resolution, no clock read, no
crypto on the path — the unsigned capture path is byte-for-byte what it was before.
