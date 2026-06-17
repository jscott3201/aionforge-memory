# Eval fixture provenance

These fixtures are **fully synthetic**, hand-authored for the off-topic-rejection floor
sweep. They are not derived from any real memory export, user data, or operational
corpus. Every row carries `"source": "aionforge-eval-synthetic"`.

## Schema

`corpus_memories.jsonl` — one memory per line:

```json
{"id": "m-0001", "text": "...", "importance": 0.5, "trust": 0.85, "source": "aionforge-eval-synthetic"}
```

`corpus_queries.jsonl` — one query per line. A **positive** carries graded relevance
labels; a **negative** (off-topic, correct answer empty) carries `"expected_empty": true`:

```json
{"id": "q-0001", "query": "...", "source": "...", "expected": [{"id": "m-0001", "grade": 3}, {"id": "m-0002", "grade": 1}]}
{"id": "q-0101", "query": "...", "source": "...", "expected_empty": true}
```

Grades are `0..=3` (gain `2^grade - 1`); only `grade > 0` ids count as gold.

## Design

Three unrelated everyday topics — composting, bicycle maintenance, sourdough baking —
each with three memories. The three positive queries are clearly on-topic for one cluster
each (so dense relevance is high); the three negative queries are about wholly unrelated
subjects (cloud autoscaling, French history, OAuth) so a healthy dense floor should reject
them while keeping the positives. Everyday topics, not technical ones, keep the negatives
genuinely off-topic relative to the corpus.

## Scrub

Every fixture string is gated by `aionforge_eval::scrub_violations` before the sweep runs:
no secrets, tokens, private keys, emails, UUIDs, machine-local paths, or planning notes.

## Regenerating / running

The sweep is on-demand and key-gated (never in CI):

```bash
source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
cargo test -p aionforge-eval --test floor_sweep -- --ignored --nocapture
```
