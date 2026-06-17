#!/usr/bin/env python3
"""Normalize a slice of the BEAM long-term-memory benchmark into JSONL for the
source-recall-under-floor measurement.

BEAM (https://github.com/mohammadtavakoli78/BEAM, dataset Mohammadta/BEAM) is
CC BY-SA 4.0. Its data is NEVER vendored into this repository. This tool reads
the dataset from the local HuggingFace cache (downloading the small 100K split
on first run) and writes a normalized JSONL to an EXTERNAL path under
~/.aionforge/beam-data, which the `#[ignore]` Rust runner reads. Both the data
and this output live outside the repo.

Each output line is one conversation:

    {
      "conversation_id": "1",
      "title": "...",
      "messages": [{"id": "msg-0", "text": "...", "role": "user", "time_anchor": "March-15-2024"}, ...],
      "probes":   [{"id": "1::information_extraction::0", "ability": "information_extraction",
                    "question": "...", "source_ids": ["msg-28"], "expected_empty": false}, ...]
    }

A probe with no source_chat_ids (the BEAM `abstention` ability) is a negative:
`expected_empty=true`, no gold — a healthy dense floor should still reject it.

Usage:
    python3 crates/aionforge-eval/tools/prepare_beam.py --conversations 6
    # writes ~/.aionforge/beam-data/normalized/beam-100k.jsonl
"""

import argparse
import ast
import json
import os
import sys
from pathlib import Path

REPO_SPLIT = "data/100K-00000-of-00001.parquet"
DATA_HOME = Path(os.path.expanduser("~/.aionforge/beam-data"))


def flatten_ids(x):
    """Flatten BEAM source_chat_ids (a flat list, or a dict of named event lists) to ints."""
    out = []
    if isinstance(x, (list, tuple)):
        for e in x:
            out += flatten_ids(e)
    elif isinstance(x, dict):
        for e in x.values():
            out += flatten_ids(e)
    elif isinstance(x, bool):
        pass
    elif isinstance(x, int):
        out = [x]
    return out


def load_parquet():
    import pyarrow.parquet as pq
    from huggingface_hub import hf_hub_download

    local = hf_hub_download(
        "Mohammadta/BEAM", REPO_SPLIT, repo_type="dataset",
        local_dir=str(DATA_HOME),
    )
    return pq.read_table(local).to_pylist()


def normalize_conversation(conv):
    # Flatten chat (list of turns, each a list of messages) into ordered messages.
    msgs = []
    seen = set()
    for turn in conv["chat"]:
        for m in turn:
            mid = m["id"]
            if mid in seen:
                continue
            seen.add(mid)
            msgs.append({
                "id": f"msg-{mid}",
                "text": m["content"],
                "role": m.get("role", "user"),
                "time_anchor": m.get("time_anchor"),
            })
    msg_ids = {m["id"] for m in msgs}

    probes = []
    pqs = ast.literal_eval(conv["probing_questions"])
    for ability, items in pqs.items():
        for i, pr in enumerate(items):
            question = pr.get("question") or pr.get("instruction") or ""
            if not question:
                continue
            gold_raw = flatten_ids(pr.get("source_chat_ids"))
            source_ids = [f"msg-{g}" for g in gold_raw if f"msg-{g}" in msg_ids]
            probes.append({
                "id": f"{conv['conversation_id']}::{ability}::{i}",
                "ability": ability,
                "question": question,
                "source_ids": source_ids,
                # A probe whose gold evidence is absent (BEAM abstention, or refs we
                # could not resolve) is a negative: a healthy floor rejects it.
                "expected_empty": len(source_ids) == 0,
            })
    return {
        "conversation_id": conv["conversation_id"],
        "title": conv["conversation_seed"]["title"],
        "messages": msgs,
        "probes": probes,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--conversations", type=int, default=6,
                    help="how many 100K conversations to normalize (default 6; max 20)")
    ap.add_argument("--out", default=str(DATA_HOME / "normalized" / "beam-100k.jsonl"))
    args = ap.parse_args()

    table = load_parquet()
    n = min(args.conversations, len(table))
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    total_msgs = total_probes = total_negatives = 0
    with out_path.open("w") as f:
        for conv in table[:n]:
            norm = normalize_conversation(conv)
            f.write(json.dumps(norm) + "\n")
            total_msgs += len(norm["messages"])
            total_probes += len(norm["probes"])
            total_negatives += sum(1 for p in norm["probes"] if p["expected_empty"])

    print(f"wrote {n} conversations to {out_path}")
    print(f"  messages: {total_msgs}")
    print(f"  probes:   {total_probes} ({total_probes - total_negatives} positive, "
          f"{total_negatives} negative/abstention)")
    print("  NOTE: BEAM data is CC BY-SA 4.0 and lives outside the repo; do not commit it.")


if __name__ == "__main__":
    sys.exit(main())
