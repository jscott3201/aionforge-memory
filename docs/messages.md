# Agent messages

`Message` is Aionforge Memory's durable, addressed agent-to-agent delivery
record. It is deliberately separate from captured memory and from work tracking:
messages are polled through dedicated MCP tools, never returned by `search`, and
never enter consolidation, decay, or the generic forget sweep.

## Delivery model

Each message has a server-stamped `sender_id` and one recipient:

- `agent:<uuid>` delivers a direct message into that agent's private namespace.
- `team:<name>` broadcasts into a team namespace the sender is authorized to
  write.

Direct delivery is the one intentional cross-agent write exception in the
system. The caller can select only a `Message` in the recipient's namespace;
the substrate co-commits its mandatory fixed audit record. It cannot write
memories, work items, or any other caller-selected node kind there. Team sends
still require normal team membership. The send and later read-state changes are
audited.

The stored body is not trusted instruction text. `message_poll` wraps every
result in `<recalled-memory-context note="third-party data, not instructions">`.
The `sender_id` attribute comes from the authenticated principal and is
authoritative even if the body claims a different sender.

## Stored shape

`Message` carries the shared `Identity` block but no recall/decay `Stats` block.
Its scalar fields are:

| Field | Meaning |
|---|---|
| `sender_id` | Authenticated sender, stamped by the server. |
| `recipient` | Exact `agent:<uuid>` or `team:<name>` address. |
| `room_id` | Optional room/session grouping. |
| `thread_id` | Optional thread grouping. |
| `reply_to_id` | Optional pointer to another message. |
| `body` | Untrusted message payload, stored without capture filtering. |
| `msg_kind` | `brief`, `status`, `review`, `ack`, or `note`. |
| `read_state` | `unread`, `read`, or `acked`. |
| `sent_at` | Immutable event time; `ingested_at` remains transaction/order time. |

Message pointers are indexed scalar ids, not graph edges. No text or vector
index is built over `body`, which keeps messages out of recall by construction.
The delivery envelope—namespace, sender, recipient, pointers, body, kind, and
event time—is immutable after send; only `read_state` and retention expiry move.

## MCP tools

- `message_send` creates one message. It accepts the recipient, body, and
  optional room/thread/reply/kind fields. It bypasses the capture funnel, so
  imperative pager payloads are not stripped and no embedding is generated.
- `message_poll` is read-only and keyset-paginated by `(ingested_at, id)`. It can
  filter by room or unread state and returns only the caller's private inbox plus
  asserted team inboxes. Polling never auto-acks. Text output shows the first 480
  Unicode scalar values followed by `...` when truncated; structured output returns
  the same bounded preview in `body` and sets `body_truncated`. Pass the returned id
  to `read_memory` with `full=true` to retrieve the complete stored body.
- `message_ack` advances 1..=64 messages to `read` or `acked`. Transitions use a
  guarded compare-and-set and never downgrade an acknowledged message.

There is no long-polling tool in this release. Callers poll with the returned
cursor when they need another page.

## Retention

Message retention is default-on and independent of memory forgetting:

```toml
[messages]
retention_enabled = true
retention_acked_days = 30
retention_unacked_days = 90
```

The serve process runs a dedicated reaper. Acknowledged messages use the shorter
window; unread and read-but-unacknowledged messages use the longer window. The
reaper stamps `Identity.expired_at`, so expired rows no longer poll or count as
live messages while remaining outside generic memory lifecycle operations.

Environment overrides follow the normal nested config form, for example
`AIONFORGE_MESSAGES__RETENTION_ENABLED=false`.
