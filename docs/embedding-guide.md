# Embedding and provider guide

Aionforge stores and retrieves embeddings, but it does not run an embedding model
itself. A deployment points the host at one provider/model pair and records that
model identity on stored vectors so the rest of the substrate can verify
dimensions, provenance, and cross-family boundaries.

## Embedder configuration

The embedder uses an OpenAI-compatible `/embeddings` endpoint:

```toml
[embedder]
enabled = true
endpoint = "https://api.example.com/v1"
model = "embedding-model-id"
dimension = 1536
api_key_env = "AIONFORGE_EMBEDDER_API_KEY"
timeout_ms = 30000
```

`endpoint` must use HTTPS unless it is loopback. `api_key_env` names the
environment variable holding the key; the key value itself is resolved at
runtime into a redacting secret type and is never stored in config.

The embedding `dimension` is a storage binding. The native vector indexes are
built for that dimension, and changing it after a store has data is a migration,
not a tuning change. `doctor` and `recover` fail loudly when the configured
dimension disagrees with the recovered store.

## Local and disabled modes

For local development, a loopback OpenAI-compatible server can run without a
secret:

```toml
[embedder]
enabled = true
endpoint = "http://127.0.0.1:1234/v1"
model = "codestral-embed-2505"
dimension = 1536
```

Embedding can also be disabled:

```toml
[embedder]
enabled = false
model = "disabled"
dimension = 1536
```

With embedding disabled, capture does not compute vectors and retrieval falls
back to lexical and graph signals. Calls that explicitly require new embeddings
return an unavailable error instead of silently fabricating vectors.

## Retrieval impact

Retrieval combines BM25 lexical search, dense vector search, graph expansion,
support expansion, trust, importance, and recency. Dense search is one signal in
that fusion, not a separate vector database. All search and graph work runs
inside selene-db native indexes/providers.

The important operator rules are:

- Keep one embedding model per store until you run a deliberate migration.
- Treat a model or dimension change as storage state, not config drift.
- Use `doctor` before exposing a service after provider changes.
- Do not log API keys; use `api_key_env` and a secret manager.

See [Retrieval](retrieval.md), [Graph signals](graph-signals.md), and
[Operations and recovery](operations-recovery.md) for the full mechanics.

## Optional completion provider

The chat/completion client is separate from embeddings and is off by default. It
exists for opt-in LLM distillation and link evolution:

```toml
[completer]
enabled = true
provider = "openai_chat"
endpoint = "https://api.example.com/v1"
model = "chat-model-id"
api_key_env = "AIONFORGE_COMPLETER_API_KEY"
timeout_ms = 60000
max_tokens = 4096
```

Supported provider wire formats are `openai_chat`, `openai_responses`, and
`anthropic`. A deployment declares one provider and one model; there is no
cost-first auto-routing. Optional LLM output is non-canonical, off the
consolidation cursor, and cannot perturb byte-identical capture or recall.

The distillation quality benchmark is deferred with M7. Until that lands, LLM
distillation is experimental and off by default. See
[Completion client](completion-client.md), [LLM distillation](distillation.md),
and [Honest scope and deferred work](honest-scope.md).
