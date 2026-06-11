# Capture

Capture is the write path. When an agent produces a turn — a user message, an assistant reply, a tool result — capture is what turns that raw text into a stored episode. It runs on the hot path, in millisecond time, and it is deliberately thin: it filters the content, decides whether the turn is worth keeping, attaches just enough provenance to prove who wrote it, and commits. Everything that takes real thought — clustering, summarizing, recomputing importance, drawing links — is left to [consolidation](consolidation.md), which runs behind the path and never blocks it.

The path is `Capturer<F, E>`, generic over a `PrivacyFilter` and an `Embedder`. It names the contracts, not the concrete security filter or the HTTP embedder, so the same path runs against a stub embedder in tests and a real one in production without a code change.

## What capture writes

A capture produces an `Episode`, a `ProvenanceRecord`, and an `AuditEvent`, committed together. The episode is the memory; the provenance proves who wrote it and under what trust; the audit records that the write happened. The receipt the caller gets back, `CaptureReceipt`, says what was decided: the episode id, the dedup verdict, the namespace the write actually landed in, the redactions and injection flags, and whether the content was embedded.

## The filter, and the origin block

Before anything else, the raw content runs through the privacy and injection filter, `CaptureFilter`. It is local and synchronous, so it adds no network round-trip to the path. It does two things in one deterministic pass:

- **Redaction.** Configured patterns (email, US phone, payment-card numbers, `sk-` secret keys in the v1.0 default set) are matched and replaced with a typed `[redacted:<kind>]` placeholder. A redaction records the matched span as byte offsets into the *original* content. Some rules carry a structural check — the card pattern is gated on a Luhn checksum, so an ISBN-13 or an order id that happens to match the digit shape is left alone.
- **Injection stripping.** Known prompt-injection markers — anchored override phrases ("ignore/forget the previous instructions", "override your instructions"), spoofed headers ("system prompt:", "new instructions:"), exfiltration ("reveal your system prompt"), jailbreak role-swaps ("you are now DAN", "developer mode"), and spoofed prompt boundaries ("`</system>`") — are detected, stripped from the content, and their ids collected. The set is precision-first: each marker is a multi-token phrase, not a bare trigger word, so benign uses ("can I ignore this warning", "act as a translator") do not fire.

The pass is fail-closed by construction. Edits are applied earliest-start-first, longer-match-first on a tie; a later match fully covered by an applied one is dropped, but one that only partially overlaps still has its uncovered tail replaced. A matched sensitive byte is never copied out, even when two patterns overlap awkwardly.

What the filter did is recorded, not thrown away. The `FilterOutcome` it returns folds into the episode's `Origin` block: the list of `redactions` and the `injection_flags` are stored on the episode itself, alongside the writer context (`model_family`, `model_version`, `transport`, `request_id`). So an operator can look at any episode and see exactly what was scrubbed on the way in and which injection markers fired, without re-running the filter. The same two fields also ride out on the receipt. The outcome also carries `marker_hits` — a per-marker firing count for tuning the marker set — but that is observability metadata only: it is deliberately *not* folded into the content hash or the `Origin` block, so adding or tuning a marker never perturbs an episode's canonical stored bytes. The capturer emits those counts as a `capture_injection_marker_hits_total` counter labeled by marker id — the per-pattern hit log for tuning the set in production (07 §5). It is emitted at the capturer, the one consumer on the hot path, so the security crate stays free of any metrics dependency; the facade is a no-op when nothing scrapes it.

If the filter errors, the capture aborts. There is no fallback to writing the raw content. That is the point of failing closed here: the alternative — write the unfiltered turn and move on — is the one outcome the filter exists to prevent.

A capture can also be *hollowed out* by excision: when the markers consume the substance of the content, what is left ("and immediately.") is not a memory, just the connective tissue around a stripped injection. The funnel refuses such a residue-only write — `FilterOutcome::is_residue_only` decides, the capturer writes a `ResidueRejected` audit recording the firing markers and the original/cleaned lengths (never the residue text itself), and the write returns `CaptureError::ResidueOnly` with no episode landed. The predicate fires only when at least one injection marker fired, so benign short captures ("ok.", "done") are untouched and the measured false-positive ceiling below is unaffected; and it never fires while substance survives, so a long legitimate message that quoted one injection phrase keeps its episode. Concretely: refusal requires both fewer than 24 alphanumeric characters remaining *and* the excision having consumed more than half the content's alphanumeric substance.

The default marker set is **precision-first and measured**, not a content classifier. Each marker is an anchored multi-token override / exfiltration / role-swap phrase tuned against a published corpus, and a CI gate holds it to two binding numbers: it fires on **no** benign trigger-word row (zero false positives on the `leolee99/NotInject` set) while clearing a block-rate floor on `deepset/prompt-injections`. Be honest about what that floor measures. A capture-time string filter only sees the imperative-override family, so it blocks the slice of corpus injections that carry a known override phrase (~12%) and **cannot** reach the rest (~88%), which are semantic, role-play, or obfuscated and carry no marker. Those are the job of the recall-side untrusted-data tagging and system-role exclusion (see [retrieval](retrieval.md)) and the red-team probes — not this filter. The zero-false-positive figure is measured on that benign corpus, not a guarantee over all benign production text; and the filter operates only at capture on stored content, so it does not address injection-*steering* of the optional distiller, a separate limit noted in the honest-scope. It raises the bar; it is not a complete injection defense. A host that needs more supplies its own rules through `CaptureFilter::new`.

## Dedup: exact, then near

A turn that is already stored should not be stored again, and a turn that is nearly a copy of an existing one should be flagged so consolidation can reconcile the two. Capture does both halves.

The **exact** half hashes the *cleaned* content into a `ContentHash`. Hashing the cleaned form, not the raw input, means the redacted shape is the dedup key and the embedder never sees the secrets the filter just removed. If that hash already names an episode, capture writes nothing: the receipt comes back `ExactDuplicate`, pointing at the existing episode, with no audit id.

Because the dedup key is the cleaned form, hardening the filter is forward-only by design. If a future marker strips more bytes, two raw inputs that differ only in the stripped marker can clean to the same bytes and dedup onto one episode. That affects only new captures — stored episodes are immutable and their hashes are never recomputed — so a filter change can never retroactively alter history or a stored dedup decision.

The **near** half runs after embedding. The episode's vector is checked against the nearest *active* episode (a small window of `NEAR_DUPLICATE_CANDIDATES`, eight, so it can skip a few soft-forgotten ones and still find the nearest live one without scanning deeply on the hot path). If the cosine similarity clears `near_duplicate_threshold` (0.95 by default), the verdict is `NearDuplicate`, carrying the id it resembles and the distance.

A near-duplicate is **still written**. Episodes are immutable and append-only, so capture does not merge or drop it. The similarity is surfaced on the receipt and recorded in the audit so consolidation can cluster or summarize the pair later. Without a vector — when embedding is off or failed — similarity cannot be judged, so the verdict is `New`.

## The add / update / supersede decision

Capture's decision is intentionally lightweight: it is ADD or nothing. The three outcomes are `New` (commit a fresh episode), `ExactDuplicate` (commit nothing, return the existing id), and `NearDuplicate` (commit a fresh episode and flag the resemblance). Capture never edits an existing episode and never supersedes one. Reconciling overlapping memories, choosing a current value among competing facts, retiring stale state — that is [merge](concurrent-merge.md) and [consolidation](consolidation.md), off the hot path. Keeping the path append-only is what keeps it fast and what makes it safe to retry: a capture either lands a new immutable node or recognizes one already there.

## Embedding and provenance

If `embed_on_capture` is set, the cleaned content is embedded and the vector is stored on the episode along with the model that produced it. Embedding is the one step that may fail without aborting the capture. An embedder error degrades to `EmbeddingOutcome::Skipped` with the reason kept for observability: the episode is committed **without a vector**, marked `ConsolidationState::Raw`, and consolidation embeds it later. Capture never blocks on the embedder. That split is deliberate — the filter and the store are correctness-critical and fail closed, but a slow or down embedder is an availability problem the path routes around rather than a reason to drop the turn.

Every committed episode gets a `ProvenanceRecord` proving the write: the subject episode id, the writer agent, the writer's model family and version, and `trust_at_write` (the writer's trust, clamped to `[0, 1]`). On an unsigned deployment the signature is empty. On a [signed-write deployment](provenance-signing.md) the record carries the host signature the gate verified.

The episode lands with a default `importance` of 0.5; consolidation recomputes the real value. Stored timestamps all come from the request's `captured_at`, not the system clock — event time and transaction time coincide on the fast path. Record ids are minted as sortable UUIDv7s (see [identifiers](identifiers.md)).

## Durable before visible: the single funnel

The episode, its provenance, and the audit event are committed as **one atomic transaction** through `Store::commit_capture`. That call wires `Episode -HAS_PROVENANCE-> ProvenanceRecord` and `AuditEvent -AUDIT-> Episode`, and commits. If translation or any node or edge mutation or the commit fails, nothing is published. A reader never sees an episode without its provenance, or a memory with no audit trail. Durable before visible: the write is on disk before any reader can observe it, and a partial capture is never observable at all.

## The audit event

Each committed capture writes an `AuditEvent` of kind `Capture`, in the **System** namespace, subject the episode and actor the writing agent. Its payload records the dedup verdict tag (`new` or `near_duplicate` — an exact duplicate never reaches the audit write), the count of redactions, and the injection flags. An exact duplicate writes no new memory and no audit; its receipt carries `audit_id: None`.

A *rejected* write is audited too, in its own transaction through `Store::commit_audit`, and produces no memory node. A namespace denial writes a `NamespaceDenied` event whose subject is the agent itself (there is no episode to point an `AUDIT` edge at). A signed-write rejection writes an `InvalidSignature` or `ClockSkewRejected` event. A residue-only refusal writes a `ResidueRejected` event, likewise subject-on-the-agent. These standalone audits are content-addressed on their own id, so a deterministic retry of the same rejected attempt is a no-op rather than a spurious constraint error.

## Untrusted writes stay private

A `CaptureRequest` carries a `trusted` flag the host sets when it vouches for the namespace the caller asked for. The resolved namespace is decided before any authorization check:

- A **trusted** write goes to its requested namespace, defaulting to the agent's own private space.
- An **untrusted** write is forced into `agent:<agent_id>`, the writer's private namespace, **regardless of what it requested**. Untrusted content never lands in a team or global space on the caller's say-so.

That resolved target is then checked against the writer's `Principal` by the `Authorizer`. Because an untrusted write was already confined to the agent's own space, it always passes. A trusted write to a team the agent does not belong to, or to global or system, is **refused** — the denial writes a `NamespaceDenied` audit and returns, and no memory lands. This is the same policy [namespace authorization](namespace-authorization.md) describes, applied at the one point every write passes through. A remote MCP capture is untrusted, so it is confined to the writer's private namespace; team-shared writes are a trusted-host path, not a remote-client one.

## What it does not do

Capture is a funnel, not a thinker. It does not compute final importance, surprise, or any reliability signal — those start at neutral defaults and are filled in later. It does not merge, summarize, update, or supersede existing memories. It does not draw edges between episodes beyond the provenance and audit wiring. It does not promote anything to a shared namespace. It does not execute or interpret content; an injection marker is stripped and flagged, never acted on. And the default privacy filter is a conservative first scrub, not a complete data-loss-prevention layer. All of that deferred work is consolidation's, run behind the path so the write stays fast.
