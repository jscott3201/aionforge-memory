<script lang="ts">
  import { onMount } from "svelte";
  import { Activity, LoaderCircle, Radar, Search } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    searchMemories,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type {
    SearchMemoryRecord,
    SearchResultsStructuredContent,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Input } from "$lib/components/ui/input/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  const localViewer = "agent:00000000-0000-4000-8000-000000000311";
  const previewSignals = ["lexical", "lexical_anchor", "trust", "recency"];

  type RetrievalState =
    | { state: "offline" }
    | { state: "idle" }
    | { state: "loading" }
    | { state: "ready"; result: SearchResultsStructuredContent }
    | { state: "empty"; result: SearchResultsStructuredContent }
    | { state: "error"; message: string };

  interface SignalRow {
    name: string;
    weight: number | null;
    rank: number | null;
  }

  let config: McpClientConfig | null = null;
  let query = "console retrieval";
  let viewer = localViewer;
  let retrievalState: RetrievalState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    retrievalState = config ? { state: "idle" } : { state: "offline" };
  });

  async function runRetrieval(): Promise<void> {
    const activeConfig = config;
    const trimmedQuery = query.trim();
    if (!activeConfig || !trimmedQuery || retrievalState.state === "loading") {
      return;
    }

    retrievalState = { state: "loading" };

    try {
      const result = await searchMemories(activeConfig, {
        query: trimmedQuery,
        viewer: viewer.trim() || undefined,
        limit: 6,
        verbose: true,
        includeSuperseded: false,
      });
      retrievalState =
        result.memories.length > 0
          ? { state: "ready", result }
          : { state: "empty", result };
    } catch (error) {
      retrievalState = { state: "error", message: errorMessage(error) };
    }
  }

  function resultCount(state: RetrievalState): string {
    if (state.state === "ready" || state.state === "empty") {
      return `${state.result.summary.returned} returned`;
    }
    if (state.state === "loading") {
      return "searching";
    }
    if (state.state === "offline") {
      return "offline";
    }
    return "ready";
  }

  function stateResult(
    state: RetrievalState,
  ): SearchResultsStructuredContent | null {
    return state.state === "ready" || state.state === "empty"
      ? state.result
      : null;
  }

  function routeLabel(result: SearchResultsStructuredContent | null): string {
    return result?.explain.route ?? "offline";
  }

  function embedderLabel(
    result: SearchResultsStructuredContent | null,
  ): string {
    if (!result) {
      return "pending";
    }
    return result.summary.embedder_available ? "available" : "disabled";
  }

  function summaryValue(
    result: SearchResultsStructuredContent | null,
    key: keyof SearchResultsStructuredContent["summary"],
  ): string {
    const value = result?.summary[key];
    return typeof value === "number" ? value.toString() : "0";
  }

  function signalRows(
    result: SearchResultsStructuredContent | null,
  ): SignalRow[] {
    if (!result) {
      return previewSignals.map((name) => ({ name, weight: null, rank: null }));
    }

    const first = result.memories[0];
    const names = [...result.explain.signals_run];
    for (const signal of first?.signals ?? []) {
      if (!names.includes(signal.signal)) {
        names.push(signal.signal);
      }
    }

    return names.map((name) => {
      const contribution = first?.signals.find(
        (signal) => signal.signal === name,
      );
      return {
        name,
        weight: result.explain.weights[name] ?? contribution?.weight ?? null,
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

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="search results"
  title="Retrieval"
  detail="Live recall route, signal mix, and result inspection."
/>

<section class="retrieval-workspace">
  <Card.Root class="panel retrieval-query-panel">
    <Card.Header class="panel-title">
      <Radar size="18" />
      <Card.Title>Recall query</Card.Title>
      <Badge
        class="count-badge"
        data-testid="retrieval-result-count"
        variant="outline">{resultCount(retrievalState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <form
        class="retrieval-search-form"
        onsubmit={(event) => {
          event.preventDefault();
          void runRetrieval();
        }}
      >
        <label>
          <span>Query</span>
          <Input
            data-testid="retrieval-search-input"
            aria-label="Retrieval query"
            bind:value={query}
            disabled={!config}
          />
        </label>
        <label>
          <span>Viewer</span>
          <Input
            data-testid="retrieval-viewer-input"
            aria-label="Retrieval viewer"
            bind:value={viewer}
            disabled={!config}
          />
        </label>
        <Button
          data-testid="retrieval-search-submit"
          type="submit"
          disabled={!config ||
            !query.trim() ||
            retrievalState.state === "loading"}
          variant="outline"
        >
          {#if retrievalState.state === "loading"}
            <LoaderCircle size="16" />
          {:else}
            <Search size="16" />
          {/if}
          Search
        </Button>
      </form>

      <div class="retrieval-summary-grid">
        <p>
          <strong
            >{summaryValue(stateResult(retrievalState), "returned")}</strong
          >
          <span>returned</span>
        </p>
        <p>
          <strong
            >{summaryValue(
              stateResult(retrievalState),
              "candidates_considered",
            )}</strong
          >
          <span>considered</span>
        </p>
        <p>
          <strong
            >{summaryValue(
              stateResult(retrievalState),
              "filtered_or_hidden",
            )}</strong
          >
          <span>hidden</span>
        </p>
        <p>
          <strong>{embedderLabel(stateResult(retrievalState))}</strong>
          <span>embedder</span>
        </p>
      </div>

      {#if retrievalState.state === "offline"}
        <div class="retrieval-empty-state">
          <strong>Static preview</strong>
          <span>search · route explain · signal weights</span>
        </div>
      {:else if retrievalState.state === "idle"}
        <div class="retrieval-empty-state">
          <strong>Ready</strong>
          <span>search results payload</span>
        </div>
      {:else if retrievalState.state === "error"}
        <div class="retrieval-empty-state tone-danger">
          <strong>Search failed</strong>
          <span>{retrievalState.message}</span>
        </div>
      {:else if retrievalState.state === "empty"}
        <div class="retrieval-empty-state">
          <strong>No matches</strong>
          <span>{retrievalState.result.explain.route}</span>
        </div>
      {:else if retrievalState.state === "ready"}
        <div class="retrieval-results" aria-label="Retrieval results">
          {#each retrievalState.result.memories as memory (memory.id)}
            <article data-testid="retrieval-result-item">
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
                  <dt>Trust</dt>
                  <dd>{memory.trust.toFixed(2)}</dd>
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

  <Card.Root class="panel retrieval-signals-panel">
    <Card.Header class="panel-title">
      <Activity size="18" />
      <Card.Title>Signal mix</Card.Title>
      <Badge data-testid="retrieval-route" variant="outline"
        >{routeLabel(stateResult(retrievalState))}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="retrieval-route-card">
        <span>Query class</span>
        <strong
          >{stateResult(retrievalState)?.summary.query_class ??
            "pending"}</strong
        >
      </div>

      <div class="retrieval-signal-list" data-testid="retrieval-signals">
        {#each signalRows(stateResult(retrievalState)) as signal (signal.name)}
          <div class="retrieval-signal-row">
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
            <div class="retrieval-signal-meter" aria-hidden="true">
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
</section>
