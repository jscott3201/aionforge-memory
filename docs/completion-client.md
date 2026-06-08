# Completion client

The completion client is how Aionforge talks to a chat model. It is the chat counterpart to
the embedding client: one provider-agnostic seam, several provider adapters behind it, and a
deployment that declares exactly one provider and model.

> **Optional and off by default.** Nothing in the core memory path calls a chat model. The
> client exists for the opt-in LLM work (distillation and the like); with it unconfigured, the
> substrate runs entirely on its deterministic, rule-based path.

## The seam

The contract is `aionforge_domain::contracts::Completer`: give it a `CompletionRequest` (a list
of `ChatMessage`s and an optional token cap), get back a `Completion` (the assistant text, the
model that actually responded, and a normalized stop reason). Callers depend on this trait, not
on any provider — so the distiller, a future reflect pass, or a test double all speak the same
shape.

```rust
fn complete(&self, request: &CompletionRequest)
    -> impl Future<Output = Result<Completion, Self::Error>> + Send;
fn model(&self) -> &CompleterModel;
```

`aionforge-chat`'s `HttpCompleter` implements it over three wire formats.

## Providers

A deployment picks one `provider`:

- **`openai_chat`** — OpenAI Chat Completions (`POST {base}/chat/completions`). This is also the
  open de-facto standard, so the same setting drives any OpenAI-compatible local or self-hosted
  server: vLLM, Ollama, LM Studio, llama.cpp. Bearer auth. The token cap is sent as `max_tokens`
  (the field every OpenAI-compatible server understands).
- **`openai_responses`** — OpenAI's Responses API (`POST {base}/responses`), used **statelessly**:
  `store` is `false` and `previous_response_id` is never sent, so no server-side conversation
  state is created or carried. Bearer auth. The token cap is `max_output_tokens`.
- **`anthropic`** — Anthropic's Messages API (`POST {base}/messages`). Auth is the `x-api-key`
  header plus the required `anthropic-version` header. The system prompt is a top-level field, so
  any system-role messages are lifted out of the turn list. `max_tokens` is required.

The base URL carries the version segment the provider expects (e.g. `.../v1`); the client appends
the resource path, exactly as the embedding client appends `/embeddings`.

### One declared provider, never auto-routed

There is deliberately **no cost-first multi-provider auto-routing** (the kind that silently swaps
model families between calls). A deployment declares one provider and one model, and every
`Completion` records the model the endpoint *actually* responded with — so a silent swap is
detectable and the consolidating model family stays verifiable. This is what keeps a downstream
cross-family safety guard meaningful.

## Determinism

Sampling is **pinned by the client**, not chosen per call: temperature is `0.0` everywhere, and a
fixed `seed` is sent on the one provider that supports one (OpenAI Chat Completions). Responses
and Anthropic expose no seed, so there temperature `0.0` is the only lever. Across every provider,
determinism is **best-effort, never guaranteed** — providers can still vary outputs run to run.
That is exactly why LLM output is treated as non-canonical: the byte-deterministic path is the
rule-based canonical tier, and a chat model never feeds it.

A `Completion` carries a `finish_reason` normalized across providers to one small vocabulary so a
caller need not know who answered:

| meaning | OpenAI Chat | OpenAI Responses | Anthropic |
| --- | --- | --- | --- |
| `stop` | `stop` | `status: completed` | `end_turn`, `stop_sequence` |
| `length` (truncated) | `length` | `incomplete` + `max_output_tokens` | `max_tokens` |
| `filter` | `content_filter` | `incomplete` + `content_filter` | — |
| `refusal` | — | — | `refusal` |

`Some("length")` is the truncation sentinel a distiller's detail-retention guard uses to reject a
lossy completion rather than store it.

## Degrade, don't fail

When the endpoint is unreachable, times out, is overloaded (HTTP 429 or 529), or returns a 5xx,
the error is `CompleteError::Unavailable` and `is_unavailable()` is true — the signal to fall back
to the deterministic canonical tier instead of failing the operation. A 4xx (other than 429) is a
hard `Status` error, and a malformed or empty body is a `Decode` error.

## Configuration

`CompleterConfig` (off by default):

```toml
[completer]
enabled = true
provider = "anthropic"            # openai_chat | openai_responses | anthropic
endpoint = "https://api.anthropic.com/v1"
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY" # the NAME of the env var; the key never lives in config
timeout_ms = 60000
max_tokens = 4096                 # required by Anthropic; an upper bound for the OpenAI providers
```

Like the embedder, the API key is resolved from the named environment variable into a
`SecretString` that redacts in logs and zeroizes on drop; the key is never stored in the config.
`https://` is required for the endpoint unless the host is localhost — enforced both at config
validation and at client construction.

## Where it sits

`aionforge-chat` is a layer-2 client. It depends only on the domain seam and the config crate,
names no storage, and runs no inference itself — it is the boundary to a provider. Its consumer is
the optional LLM distiller, which is gated behind a quality benchmark and remains off until it
clears; until then the completion client is a standalone, tested capability with no caller on the
core path.
