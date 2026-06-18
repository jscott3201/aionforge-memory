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

The corpus has three kinds of row, distinguished by the `source` tag:

- `aionforge-eval-synthetic` — three unrelated everyday topics (composting, bicycle
  maintenance, sourdough baking), three memories each. Clean, well-separated clusters.
- `aionforge-eval-project-sanitized` — **sanitized paraphrases of this project's own
  memory** about Aionforge Memory internals (rank fusion, the dense floor, the query
  router, the selene-db greenfield rule, episodes + facts, the embedder, consolidation,
  forgetting, provenance, supersession, work items, the operator console). These are
  hand-paraphrased to carry the technical meaning while removing every id, git sha, PR
  number, machine path, secret, and planning-note term — the scrub gate enforces this.
  They make the corpus homogeneous and technical, which is how the real store looks.
- `aionforge-eval-adjacent` — **domain-adjacent off-topic negatives** (Kubernetes,
  quicksort, CNN training, TCP/UDP, Bloom filters, Raft timeouts, kernel features,
  consistent hashing, Postgres replication, and transformer attention). They share
  vocabulary (vectors, databases, training, memory, recall-like false positives,
  consensus, time, and ranking/comparison language) with the project cluster but are
  NOT about the memory system, so they stress the floor harder than everyday negatives:
  the floor must still reject them.

Positive queries are on-topic for exactly one cluster and include single-hop factual,
multi-hop, and temporal-shaped prompts so the per-class shipped floors are represented.
Negatives (`expected_empty`) are off-topic to the whole corpus, so a healthy dense floor
rejects them while keeping the positives.

The broadened negative set was hand-authored in two bands:

- far off-topic everyday questions (history, cookware, watercolor paper, piano tuning);
- adjacent-but-off-topic technical questions that borrow project-adjacent vocabulary
  without becoming Aionforge Memory questions.

No supporting negative memories are added for those topics: the correct answer is empty
against the existing corpus by construction.

## Scrub

Every fixture string is gated by `aionforge_eval::scrub_violations` before the sweep runs:
no secrets, tokens, private keys, emails, UUIDs, machine-local paths, or planning notes.

## Regenerating / running

The sweep is on-demand and key-gated (never in CI):

```bash
source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
cargo test -p aionforge-eval --test floor_sweep -- --ignored --nocapture
```
