# Sanitized project-memory retrieval corpus

These fixtures back `../project_corpus.rs`. They are small regression data, not a
benchmark claim.

## Source

The rows are hand-curated from recurring public engineering themes in this repository's
own memory workflow: retrieval ranking, doctor/WAL reporting, embedder dimensions,
multi-agent bearer tokens, token rotation, plugin cache visibility, release images,
container smoke tests, public-safe repository notes, and volume migration.

They are not a raw export from a memory store. Each row is rewritten as a generalized
engineering note before commit.

## Scrub rules

The curation pass removes or generalizes:

- person names, emails, tokens, UUIDs, request ids, and secret-shaped strings;
- host-specific paths, macOS temporary-directory labels, home-directory labels, and
  machine-local paths;
- local planning artifacts such as brief ids, handoff labels, stage labels, and
  private branch/worktree notes;
- customer data and private agent transcripts.

The test harness re-checks those invariants with regexes. A future refresh that adds
PII-shaped, secret-shaped, path-shaped, or planning-note-shaped content should fail
before it becomes a recall baseline.

## Format

`project_memory.jsonl` contains one generalized memory per line:

- `id`: stable fixture id;
- `text`: sanitized memory text;
- `embedding`: deterministic fake embedding topic, or `none`;
- `importance`, `trust`, `ingested_at`: ranking metadata used by the harness;
- `source`: fixed to `sanitized-project-memory-pattern`.

`project_queries.jsonl` contains one recall probe per line:

- `id`: stable query id;
- `query`: sanitized recall query;
- `embedding`: deterministic fake embedding topic for the query;
- `expected_top`: memory id expected as the first structured recall hit;
- `source`: fixed to `sanitized-project-memory-pattern`.
