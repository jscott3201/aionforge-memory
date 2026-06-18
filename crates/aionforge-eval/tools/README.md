# aionforge-eval tools

On-demand tooling for the retrieval-quality eval harness. These are dev tools, not part
of any shipped artifact and not run in CI.

## `prepare_beam.py` — normalize a BEAM slice

Converts a slice of the [BEAM](https://github.com/mohammadtavakoli78/BEAM) long-term-memory
benchmark into the normalized JSONL the `beam_floor_recall` runner reads.

> **License / vendoring.** BEAM data is **CC BY-SA 4.0** and is **never committed to this
> repository**. The script reads it from the local HuggingFace cache (downloading the small
> `100K` split on first run) and writes the normalized JSONL to an **external** path under
> `~/.aionforge/beam-data/`. The repo `.gitignore` additionally guards against accidental
> in-repo copies. Only this script (code, no data) lives in the repo.

```bash
# 1. Normalize N conversations of the 100K split (downloads the parquet on first run).
python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 6
# -> ~/.aionforge/beam-data/normalized/beam-100k.jsonl

# 2. Run the source-recall-under-floor measurement against the REAL embedder.
source ~/.aionforge/aionforge-redeploy.env   # provides AIONFORGE_EMBEDDER_API_KEY
cargo test -p aionforge-eval --test beam_floor_recall -- --ignored --nocapture
```

Requires Python `pyarrow` and `huggingface_hub` (and an HF login for the dataset download).

## What the measurement reports

The `beam_floor_recall` runner seeds each BEAM conversation's messages as episodes, treats
each probing question as a query, and treats the probe's cited evidence messages
(`source_chat_ids`) as the retrieval gold. On real gemini-3072 embeddings it reports:

- a **uniform floor sweep** (recall@k / rejection / false-rejection vs. one floor applied to
  every query) — the abstract "how much would a dense gate cut" curve;
- the **production config** (`min_relevance: None`) — the shipped per-class floors, i.e. what
  actually ships;
- the **SingleHopFactual-only** slice — the only class the 0.60 floor touches, so the honest
  blast radius of the live floor;
- the **routed query-class distribution** — how representative the factual floor even is over
  real questions;
- a **per-ability** recall breakdown across the ten BEAM memory abilities.

BEAM's `abstention` probes (no resolvable evidence) are negatives: a healthy floor should
still reject them, so they measure rejection without a recall cost.
