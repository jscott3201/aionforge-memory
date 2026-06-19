<script lang="ts">
  import { onMount } from "svelte";
  import {
    Activity,
    Cpu,
    LoaderCircle,
    Search,
    Settings2,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadServerStatus,
    searchMemories,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type {
    SearchMemoryRecord,
    SearchResultsStructuredContent,
    ServerStatusStructuredContent,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Input } from "$lib/components/ui/input/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  const localViewer = "agent:00000000-0000-4000-8000-000000000311";
  const previewSignals = ["lexical", "dense", "trust", "recency"];

  type EmbeddingState =
    | { state: "offline" }
    | { state: "loading" }
    | {
        state: "ready";
        status: ServerStatusStructuredContent;
        probe: SearchResultsStructuredContent;
      }
    | { state: "error"; message: string };

  interface SignalRow {
    name: string;
    weight: number | null;
    rank: number | null;
  }

  const countFormat = new Intl.NumberFormat("en-US");
  let config: McpClientConfig | null = null;
  let query = "embedding posture";
  let viewer = localViewer;
  let embeddingState: EmbeddingState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      embeddingState = { state: "offline" };
      return;
    }

    void refreshEmbedding();
  });

  async function refreshEmbedding(): Promise<void> {
    const activeConfig = config;
    const trimmedQuery = query.trim();
    if (!activeConfig || !trimmedQuery || embeddingState.state === "loading") {
      return;
    }

    embeddingState = { state: "loading" };
    try {
      const [status, probe] = await Promise.all([
        loadServerStatus(activeConfig),
        searchMemories(activeConfig, {
          query: trimmedQuery,
          viewer: viewer.trim() || undefined,
          limit: 4,
          verbose: true,
          includeSuperseded: false,
        }),
      ]);
      embeddingState = { state: "ready", status, probe };
    } catch (error) {
      embeddingState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentProbe(
    state: EmbeddingState,
  ): SearchResultsStructuredContent | null {
    return state.state === "ready" ? state.probe : null;
  }

  function currentStatus(
    state: EmbeddingState,
  ): ServerStatusStructuredContent | null {
    return state.state === "ready" ? state.status : null;
  }

  function stateLabel(state: EmbeddingState): string {
    if (state.state === "ready") {
      return "live";
    }
    if (state.state === "loading") {
      return "loading";
    }
    if (state.state === "offline") {
      return "offline";
    }
    return "error";
  }

  function countValue(value: number | undefined): string {
    return countFormat.format(value ?? 0);
  }

  function embedderLabel(probe: SearchResultsStructuredContent | null): string {
    if (!probe) {
      return "pending";
    }
    return probe.summary.embedder_available ? "available" : "disabled";
  }

  function denseLabel(probe: SearchResultsStructuredContent | null): string {
    if (!probe) {
      return "pending";
    }
    return hasSignal(probe, "dense") ? "ran" : "not run";
  }

  function transportLabel(
    status: ServerStatusStructuredContent | null,
  ): string {
    return status?.transports.join(", ") || "none";
  }

  function signalRows(
    probe: SearchResultsStructuredContent | null,
  ): SignalRow[] {
    if (!probe) {
      return previewSignals.map((name) => ({ name, weight: null, rank: null }));
    }

    const first = probe.memories[0];
    const names = [...probe.explain.signals_run];
    for (const signal of first?.signals ?? []) {
      if (!names.includes(signal.signal)) {
        names.push(signal.signal);
      }
    }
    if (names.length === 0) {
      names.push(...previewSignals);
    }

    return names.map((name) => {
      const contribution = first?.signals.find(
        (signal) => signal.signal === name,
      );
      return {
        name,
        weight: probe.explain.weights[name] ?? contribution?.weight ?? null,
        rank: contribution?.rank ?? null,
      };
    });
  }

  function signalWidth(weight: number | null): string {
    if (weight === null) {
      return "18%";
    }
    return `${Math.max(8, Math.min(100, Math.round(weight * 100)))}%`;
  }

  function scoreLabel(value: number | undefined): string {
    return value === undefined ? "n/a" : value.toFixed(3);
  }

  function resultTitle(memory: SearchMemoryRecord): string {
    return memory.snippet || memory.serialization_id;
  }

  function hasSignal(
    probe: SearchResultsStructuredContent,
    signalName: string,
  ): boolean {
    return (
      probe.explain.signals_run.includes(signalName) ||
      probe.memories.some((memory) =>
        memory.signals.some((signal) => signal.signal === signalName),
      )
    );
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="search embedder posture"
  title="Embedding"
  detail="Read-only retrieval embedding posture from live MCP search."
/>

<section class="embedding-workspace">
  <Card.Root class="panel embedding-probe-panel">
    <Card.Header class="panel-title">
      <Cpu size="18" />
      <Card.Title>Probe</Card.Title>
      <Badge class="state-badge" data-testid="embedding-state" variant="outline"
        >{stateLabel(embeddingState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <form
        class="embedding-probe-form"
        onsubmit={(event) => {
          event.preventDefault();
          void refreshEmbedding();
        }}
      >
        <label>
          <span>Query</span>
          <Input
            data-testid="embedding-query-input"
            aria-label="Embedding probe query"
            bind:value={query}
            disabled={!config}
          />
        </label>
        <label>
          <span>Viewer</span>
          <Input
            data-testid="embedding-viewer-input"
            aria-label="Embedding probe viewer"
            bind:value={viewer}
            disabled={!config}
          />
        </label>
        <Button
          data-testid="embedding-refresh"
          type="submit"
          disabled={!config ||
            !query.trim() ||
            embeddingState.state === "loading"}
          variant="outline"
        >
          {#if embeddingState.state === "loading"}
            <LoaderCircle data-icon="inline-start" />
          {:else}
            <Search data-icon="inline-start" />
          {/if}
          Probe
        </Button>
      </form>

      {@const probe = currentProbe(embeddingState)}
      <div class="embedding-summary-grid">
        <p>
          <strong data-testid="embedding-available"
            >{embedderLabel(probe)}</strong
          >
          <span>embedder</span>
        </p>
        <p>
          <strong data-testid="embedding-dense">{denseLabel(probe)}</strong>
          <span>dense signal</span>
        </p>
        <p>
          <strong data-testid="embedding-considered"
            >{countValue(probe?.summary.candidates_considered)}</strong
          >
          <span>considered</span>
        </p>
        <p>
          <strong data-testid="embedding-returned"
            >{countValue(probe?.summary.returned)}</strong
          >
          <span>returned</span>
        </p>
      </div>

      {#if embeddingState.state === "offline"}
        <div class="embedding-empty-state">
          <strong>Static preview</strong>
          <span>search summary · embedder flag · signal path</span>
        </div>
      {:else if embeddingState.state === "loading"}
        <div class="embedding-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>server_status and search</span>
        </div>
      {:else if embeddingState.state === "error"}
        <div class="embedding-empty-state tone-danger">
          <strong>Probe failed</strong>
          <span>{embeddingState.message}</span>
        </div>
      {:else if embeddingState.probe.memories.length === 0}
        <div class="embedding-empty-state">
          <strong>No matches</strong>
          <span>{embeddingState.probe.explain.route}</span>
        </div>
      {:else}
        <div class="embedding-results" aria-label="Embedding probe results">
          {#each embeddingState.probe.memories as memory (memory.id)}
            <article data-testid="embedding-result-item">
              <header>
                <span>
                  <Badge variant="secondary">{memory.kind}</Badge>
                  {#if memory.score_band}
                    <Badge variant="outline">{memory.score_band}</Badge>
                  {/if}
                </span>
                <strong>{resultTitle(memory)}</strong>
              </header>
              <p>{memory.snippet}</p>
              <dl>
                <div>
                  <dt>Score</dt>
                  <dd>{scoreLabel(memory.score)}</dd>
                </div>
                <div>
                  <dt>Dense</dt>
                  <dd>{scoreLabel(memory.dense_similarity)}</dd>
                </div>
                <div>
                  <dt>Namespace</dt>
                  <dd>{memory.namespace}</dd>
                </div>
              </dl>
            </article>
          {/each}
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel embedding-runtime-panel">
    <Card.Header class="panel-title">
      <Settings2 size="18" />
      <Card.Title>Runtime</Card.Title>
      <Badge data-testid="embedding-route" variant="outline"
        >{currentProbe(embeddingState)?.explain.route ?? "pending"}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const status = currentStatus(embeddingState)}
      <div class="embedding-runtime-list">
        <p>
          <span>Version</span>
          <strong>{status?.version ?? "n/a"}</strong>
        </p>
        <p>
          <span>Build profile</span>
          <strong>{status?.build.profile ?? "n/a"}</strong>
        </p>
        <p>
          <span>Transport</span>
          <strong>{transportLabel(status)}</strong>
        </p>
        <p>
          <span>Sampling</span>
          <strong>{status?.sampling ? "enabled" : "disabled"}</strong>
        </p>
      </div>
    </Card.Content>
  </Card.Root>
</section>

<section class="embedding-bottom-grid">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Activity size="18" />
      <Card.Title>Signal path</Card.Title>
      <Badge variant="outline"
        >{currentProbe(embeddingState)?.summary.query_class ?? "pending"}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="embedding-signal-list" data-testid="embedding-signals">
        {#each signalRows(currentProbe(embeddingState)) as signal (signal.name)}
          <div class="embedding-signal-row">
            <div>
              <strong>{signal.name}</strong>
              <span>
                {#if signal.rank !== null}
                  rank {signal.rank}
                {:else}
                  route
                {/if}
              </span>
            </div>
            <div class="embedding-signal-meter" aria-hidden="true">
              <span style:width={signalWidth(signal.weight)}></span>
            </div>
            <small>
              {#if signal.weight !== null}
                {signal.weight.toFixed(2)}
              {:else}
                pending
              {/if}
            </small>
          </div>
        {/each}
      </div>
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Cpu size="18" />
      <Card.Title>Config posture</Card.Title>
      <Badge variant="outline">read-only</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="embedding-config-list" data-testid="embedding-config-list">
        <p>
          <strong>Runtime controls</strong>
          <span>not exposed; embedding is configured before startup</span>
        </p>
        <p>
          <strong>Provider identity</strong>
          <span>not exposed by a sanitized console-readable config surface</span
          >
        </p>
        <p>
          <strong>Model dimensions</strong>
          <span>not exposed until config_status lands</span>
        </p>
        <p>
          <strong>Secrets</strong>
          <span>never rendered in the browser console</span>
        </p>
      </div>
    </Card.Content>
  </Card.Root>
</section>
