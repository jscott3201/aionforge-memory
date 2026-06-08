# Identifiers

Every node Aionforge stores — an episode, a fact, a note, a skill, an audit event —
carries a stable `id`. An id is a **UUID**, stored as selene-db's native 16-byte UUID
value (not a string), so it indexes and compares as a UUID at the storage layer.

There are two kinds, and the difference is deliberate.

## Generated ids are time-ordered (UUIDv7)

A fresh id from `Id::generate()` is a **UUIDv7**: its high bits are a millisecond
timestamp, so ids sort by creation time. Because the store indexes them as native
UUIDs, that ordering holds at the storage layer — a range scan over ids walks them in
the order they were minted. This is what an episode, a session, or any freshly captured
record gets. The timestamp is an opaque sort key; the substrate never reads it back as a
domain time (every stored time field — `captured_at`, `ingested_at`, `valid_from` — is
supplied explicitly, never inferred from an id).

## Content-addressed ids are deterministic (UUIDv8)

Some records have an id that is a pure function of their content rather than the clock.
A consolidated fact, a summary note, and a consolidation audit event each get an id
derived from a hash of the thing that identifies them (for a fact: its namespace,
subject, predicate, object, source episode, and the rule version that produced it).
`Id::from_content_hash()` packs the leading 128 bits of a BLAKE3 digest into a
**UUIDv8** — the version the UUID spec reserves for application-defined values.

The point is idempotency: re-running the same consolidation over the same episode
produces the same id, so the write is a no-op instead of a duplicate. This is what makes
a crash-and-replay or a deliberate cursor reset safe. A v8 id carries no timestamp, so
it is correctly *not* time-sortable — its ordering is meaningless, which is the honest
signal that it is content-derived.

## Why UUID

selene-db has a first-class UUID type with a typed index and native byte ordering.
Storing ids as that type rather than as encoded strings means smaller rows, a typed
index, and — for the time-ordered v7 ids — chronological ordering for free at the
storage layer. Generated ids read the system clock for their timestamp, the same as any
time-ordered id scheme; content-addressed ids never touch the clock.

Ids are safe to expose. A host that drives Aionforge over the MCP tools passes agent and
session ids as UUID strings (the canonical hyphenated form).
