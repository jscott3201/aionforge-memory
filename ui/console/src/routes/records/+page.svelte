<script lang="ts">
  import { onMount } from "svelte";
  import { Database, FileText, LoaderCircle, Search } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    readMemory,
    searchMemories,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type {
    MemoryRecord,
    ReadMemoryStructuredContent,
    SearchMemoryRecord,
    SearchResultsStructuredContent,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Input } from "$lib/components/ui/input/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  const localViewer = "agent:00000000-0000-4000-8000-000000000311";

  type SearchState =
    | { state: "offline" }
    | { state: "idle" }
    | { state: "loading" }
    | { state: "ready"; result: SearchResultsStructuredContent }
    | { state: "empty"; result: SearchResultsStructuredContent }
    | { state: "error"; message: string };

  type DetailState =
    | { state: "empty" }
    | { state: "loading"; id: string }
    | { state: "ready"; result: ReadMemoryStructuredContent }
    | { state: "error"; message: string };

  let config: McpClientConfig | null = null;
  let query = "console";
  let viewer = localViewer;
  let selectedId: string | null = null;
  let searchState: SearchState = { state: "offline" };
  let detailState: DetailState = { state: "empty" };

  onMount(() => {
    const routeQuery = new URLSearchParams(window.location.search)
      .get("q")
      ?.trim();
    if (routeQuery) {
      query = routeQuery;
    }

    config = createRuntimeMcpClientConfig();
    searchState = config ? { state: "idle" } : { state: "offline" };
    if (config && routeQuery) {
      void runSearch();
    }
  });

  async function runSearch(): Promise<void> {
    const activeConfig = config;
    const trimmedQuery = query.trim();
    if (!activeConfig || !trimmedQuery || searchState.state === "loading") {
      return;
    }

    searchState = { state: "loading" };
    detailState = { state: "empty" };
    selectedId = null;

    try {
      const result = await searchMemories(activeConfig, {
        query: trimmedQuery,
        viewer: viewer.trim() || undefined,
        limit: 8,
        verbose: true,
        includeSuperseded: false,
      });
      searchState =
        result.memories.length > 0
          ? { state: "ready", result }
          : { state: "empty", result };

      const first = result.memories[0];
      if (first) {
        await selectMemory(first);
      }
    } catch (error) {
      searchState = { state: "error", message: errorMessage(error) };
    }
  }

  async function selectMemory(memory: SearchMemoryRecord): Promise<void> {
    const activeConfig = config;
    if (!activeConfig) {
      return;
    }

    selectedId = memory.id;
    detailState = { state: "loading", id: memory.id };

    try {
      const result = await readMemory(activeConfig, {
        memoryIds: [memory.id],
        viewer: viewer.trim() || undefined,
        verbose: true,
      });
      detailState =
        result.memories.length > 0
          ? { state: "ready", result }
          : { state: "error", message: "Memory not visible" };
    } catch (error) {
      detailState = { state: "error", message: errorMessage(error) };
    }
  }

  function resultCount(state: SearchState): string {
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

  function memoryTitle(memory: SearchMemoryRecord): string {
    return memory.snippet || memory.id;
  }

  function memoryBody(memory: MemoryRecord): string {
    switch (memory.kind) {
      case "episode":
        return memory.body;
      case "fact":
        return memory.statement;
      case "entity":
        return memory.body;
      case "note":
        return memory.content;
      case "skill":
        return memory.description;
      case "bad_pattern":
        return memory.description;
      case "core":
        return memory.content;
      case "work_item":
        return memory.display;
      case "tag":
        return memory.display;
    }
  }

  function detailMemory(state: DetailState): MemoryRecord | null {
    return state.state === "ready" ? (state.result.memories[0] ?? null) : null;
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="search + read_memory"
  title="Memory records"
  detail="Search-backed records with read_memory detail panes."
/>

<section class="records-workspace">
  <Card.Root class="panel records-search-panel">
    <Card.Header class="panel-title">
      <Database size="18" />
      <Card.Title>Record search</Card.Title>
      <Badge
        class="count-badge"
        data-testid="records-result-count"
        variant="outline">{resultCount(searchState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <form
        class="records-search-form"
        onsubmit={(event) => {
          event.preventDefault();
          void runSearch();
        }}
      >
        <label>
          <span>Query</span>
          <Input
            data-testid="records-search-input"
            aria-label="Records query"
            bind:value={query}
            disabled={!config}
          />
        </label>
        <label>
          <span>Viewer</span>
          <Input
            data-testid="records-viewer-input"
            aria-label="Records viewer"
            bind:value={viewer}
            disabled={!config}
          />
        </label>
        <Button
          data-testid="records-search-submit"
          type="submit"
          disabled={!config || !query.trim() || searchState.state === "loading"}
          variant="outline"
        >
          {#if searchState.state === "loading"}
            <LoaderCircle size="16" />
          {:else}
            <Search size="16" />
          {/if}
          Search
        </Button>
      </form>

      {#if searchState.state === "offline"}
        <div class="records-empty-state">
          <strong>Static preview</strong>
          <span>search · read_memory · session visibility</span>
        </div>
      {:else if searchState.state === "idle"}
        <div class="records-empty-state">
          <strong>Ready</strong>
          <span>search_results structuredContent</span>
        </div>
      {:else if searchState.state === "error"}
        <div class="records-empty-state tone-danger">
          <strong>Search failed</strong>
          <span>{searchState.message}</span>
        </div>
      {:else if searchState.state === "empty"}
        <div class="records-empty-state">
          <strong>No records</strong>
          <span>{searchState.result.explain.route}</span>
        </div>
      {:else if searchState.state === "ready"}
        <div class="records-list" aria-label="Search results">
          {#each searchState.result.memories as memory (memory.id)}
            <button
              class:active={selectedId === memory.id}
              data-testid="records-result-item"
              type="button"
              onclick={() => void selectMemory(memory)}
            >
              <span>
                <Badge variant="secondary">{memory.kind}</Badge>
                <strong>{memoryTitle(memory)}</strong>
              </span>
              <small>{memory.namespace}</small>
            </button>
          {/each}
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel records-detail-panel">
    <Card.Header class="panel-title">
      <FileText size="18" />
      <Card.Title>Record detail</Card.Title>
      {#if detailState.state === "ready"}
        <Badge variant="secondary">{detailState.result.found} found</Badge>
      {:else if detailState.state === "loading"}
        <Badge variant="outline">loading</Badge>
      {:else}
        <Badge variant="outline">read_memory</Badge>
      {/if}
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const memory = detailMemory(detailState)}
      {#if memory}
        <div class="record-detail">
          <div class="record-detail-meta">
            <Badge variant="secondary">{memory.kind}</Badge>
            <span>{memory.namespace}</span>
            <span>{memory.ingested_at}</span>
          </div>
          <p data-testid="records-detail-body">{memoryBody(memory)}</p>
        </div>
      {:else if detailState.state === "loading"}
        <div class="records-empty-state">
          <strong>Loading</strong>
          <span>{detailState.id}</span>
        </div>
      {:else if detailState.state === "error"}
        <div class="records-empty-state tone-danger">
          <strong>Read failed</strong>
          <span>{detailState.message}</span>
        </div>
      {:else}
        <div class="records-empty-state">
          <strong>Select a record</strong>
          <span>read_memory structuredContent</span>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>
</section>
