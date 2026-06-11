# Injection-filter measurement corpus — provenance

These vendored fixtures back the M6.T03 capture-filter measurement
(`../injection_corpus.rs`): block rate on a published injection corpus and
benign false-positive rate on a benign trigger-word corpus (07 §2, §5). They
are **test data only** — never compiled into the shipped library, never
embedded in `.rs` source. `check-thirdparty-current.sh` runs `cargo-about`
over `Cargo.lock` and therefore cannot see data fixtures, so the attribution
below is a manual discipline: re-verify it when refreshing a snapshot.

## Sources

| File | Upstream | Revision (snapshot 2026-06-10) | License | Rows |
|------|----------|--------------------------------|---------|------|
| `deepset_injections.jsonl` | [deepset/prompt-injections](https://huggingface.co/datasets/deepset/prompt-injections) | `4f61ecb038e9c3fb77e21034b22511b523772cdd` | Apache-2.0 | 662 (263 injection, 399 benign) |
| `notinject_benign.jsonl` | [leolee99/NotInject](https://huggingface.co/datasets/leolee99/NotInject) (InjecGuard, arXiv:2410.22770) | `847ae76cf8fea5ed325429e569ae8cfef022d2e0` | MIT | 339 (all benign) |

Both licenses are permissive and on the workspace `deny.toml` / `about.toml`
allow-list. Full license text and attribution: `LICENSE-deepset`,
`LICENSE-NotInject`. Why these two: deepset/prompt-injections is a labeled
published injection corpus (the block-rate set); NotInject is a benign corpus
built specifically from trigger words that over-trigger naive guards (the
false-positive set — the hardest test for a regex/marker filter). Both are
capture-relevant string corpora, not multi-turn model-jailbreak benchmarks.

## Curation transform (parquet → JSONL)

Downloaded the `refs/convert/parquet` exports, then:

- **deepset:** unioned the `train` (546) and `test` (116) splits in order;
  kept `text` and `label` verbatim; emitted `{"id":"deepset-NNNN","text":...,"label":0|1,"source":"deepset/prompt-injections"}`.
- **NotInject:** unioned the three splits (`NotInject_one`/`two`/`three`, 113
  each); **renamed the `prompt` column to `text`** (the schema gotcha — the
  upstream benign text lives in `prompt`, not `text`); dropped `word_list` /
  `category`; emitted `{"id":"notinject-NNNN","text":...,"source":"leolee99/NotInject"}`.
- One JSON object per line, UTF-8, keys in fixed order, compact separators.

## Secret scrub

Before commit, every substring matching one of the five `check-no-secrets.sh`
patterns (`AKIA[0-9A-Z]{16}`, `-----BEGIN (RSA|EC|OPENSSH|PGP) PRIVATE KEY-----`,
`xox[abpr]-[A-Za-z0-9-]{10,}`, `gh[pousr]_[A-Za-z0-9]{36,}`,
`sk-[A-Za-z0-9_-]{20,}`) was rewritten to the literal `[scrubbed-secret]`.
**0 substrings matched** in this snapshot (the corpora are natural-language
injection/benign text), but the transform is applied unconditionally and the
harness re-asserts the fixtures are secret-free, so a future refresh that
pulled in a secret-shaped row cannot silently red the whole-workspace gate.
Marker detection is unaffected: markers key on override phrases, not secrets.

## Refreshing a snapshot

Re-pull the parquet at a new revision, re-run the curation + scrub, update the
revision SHAs and row counts in this file, and re-check the observed
block/false-positive numbers recorded in `../injection_corpus.rs` against the
binding thresholds. Thresholds are never relaxed to make a refreshed corpus
pass (07 §5).
