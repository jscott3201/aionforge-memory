<script lang="ts">
  import { onMount } from "svelte";
  import {
    Database,
    Layers,
    LoaderCircle,
    RefreshCcw,
    ShieldCheck,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadMemoryCensus,
    loadServerStatus,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type {
    MemoryCensusNamespace,
    MemoryCensusStructuredContent,
    ServerStatusStructuredContent,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  type NamespaceState =
    | { state: "offline" }
    | { state: "loading" }
    | {
        state: "ready";
        status: ServerStatusStructuredContent;
        census: MemoryCensusStructuredContent;
      }
    | { state: "error"; message: string };

  interface CountEntry {
    label: string;
    value: number;
  }

  const countFormat = new Intl.NumberFormat("en-US");
  const localViewer = "agent:00000000-0000-4000-8000-000000000311";
  let config: McpClientConfig | null = null;
  let viewer = localViewer;
  let namespaceState: NamespaceState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      namespaceState = { state: "offline" };
      return;
    }

    void refreshStatus();
  });

  async function refreshStatus(): Promise<void> {
    const activeConfig = config;
    if (!activeConfig || namespaceState.state === "loading") {
      return;
    }

    namespaceState = { state: "loading" };
    try {
      const [status, census] = await Promise.all([
        loadServerStatus(activeConfig),
        loadMemoryCensus(activeConfig, {
          viewer: viewer.trim() || undefined,
          mode: "counts",
        }),
      ]);
      namespaceState = {
        state: "ready",
        status,
        census,
      };
    } catch (error) {
      namespaceState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentStatus(
    state: NamespaceState,
  ): ServerStatusStructuredContent | null {
    return state.state === "ready" ? state.status : null;
  }

  function currentCensus(
    state: NamespaceState,
  ): MemoryCensusStructuredContent | null {
    return state.state === "ready" ? state.census : null;
  }

  function stateLabel(state: NamespaceState): string {
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

  function entriesFromRecord(
    record: Record<string, number> | undefined,
  ): CountEntry[] {
    return Object.entries(record ?? {})
      .map(([label, value]) => ({ label, value }))
      .sort(
        (left, right) =>
          right.value - left.value || left.label.localeCompare(right.label),
      );
  }

  function titleCase(value: string): string {
    return value
      .replaceAll("_", " ")
      .replace(/\b\w/g, (match) => match.toUpperCase());
  }

  function namespaceRows(
    census: MemoryCensusStructuredContent | null,
  ): MemoryCensusNamespace[] {
    return [...(census?.namespaces ?? [])].sort(
      (left, right) =>
        right.total - left.total ||
        left.namespace.localeCompare(right.namespace),
    );
  }

  function namespaceDetail(namespace: MemoryCensusNamespace): string {
    return `${countValue(memoryTotal(namespace))} memories / ${countValue(workTotal(namespace))} work`;
  }

  function memoryTotal(namespace: MemoryCensusNamespace): number {
    return Object.values(namespace.kinds).reduce(
      (sum, value) => sum + value,
      0,
    );
  }

  function workTotal(namespace: MemoryCensusNamespace): number {
    return Object.values(namespace.work_statuses).reduce(
      (sum, value) => sum + value,
      0,
    );
  }

  function compactEntries(record: Record<string, number>): string {
    return entriesFromRecord(record)
      .filter((entry) => entry.value > 0)
      .map((entry) => `${titleCase(entry.label)} ${countValue(entry.value)}`)
      .join(", ");
  }

  function authLabel(status: ServerStatusStructuredContent | null): string {
    if (!status) {
      return "n/a";
    }
    return status.auth.enabled ? "bearer" : "local";
  }

  function transportLabel(
    status: ServerStatusStructuredContent | null,
  ): string {
    return status?.transports.join(", ") || "none";
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="memory_census"
  title="Namespaces"
  detail="Viewer-scoped namespace counts from the MCP server."
/>

<section class="namespaces-workspace">
  <Card.Root class="panel namespaces-status-panel">
    <Card.Header class="panel-title">
      <Layers size="18" />
      <Card.Title>Visibility census</Card.Title>
      <Badge
        class="state-badge"
        data-testid="namespaces-state"
        variant="outline">{stateLabel(namespaceState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const status = currentStatus(namespaceState)}
      {@const census = currentCensus(namespaceState)}
      <div class="namespaces-summary-grid">
        <p>
          <strong data-testid="namespaces-memory-count"
            >{countValue(census?.totals.memories)}</strong
          >
          <span>memories</span>
        </p>
        <p>
          <strong data-testid="namespaces-work-count"
            >{countValue(census?.totals.work_items)}</strong
          >
          <span>work items</span>
        </p>
        <p>
          <strong data-testid="namespaces-kind-count"
            >{countValue(
              Object.keys(census?.totals.kinds ?? {}).length,
            )}</strong
          >
          <span>memory kinds</span>
        </p>
        <p>
          <strong data-testid="namespaces-auth-posture"
            >{authLabel(status)}</strong
          >
          <span>auth posture</span>
        </p>
      </div>

      {#if namespaceState.state === "offline"}
        <div class="namespaces-empty-state">
          <strong>Static preview</strong>
          <span>memory totals, work totals, and namespace rows</span>
        </div>
      {:else if namespaceState.state === "loading"}
        <div class="namespaces-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>memory_census</span>
        </div>
      {:else if namespaceState.state === "error"}
        <div class="namespaces-empty-state tone-danger">
          <strong>Status failed</strong>
          <span>{namespaceState.message}</span>
        </div>
      {:else}
        <div class="namespaces-meta-list">
          <p>
            <span>Transport</span>
            <strong>{transportLabel(status)}</strong>
          </p>
          <p>
            <span>Recall wrapper</span>
            <strong>{namespaceState.status.recall_wrapper}</strong>
          </p>
          <p>
            <span>Viewer</span>
            <strong>{viewer.trim() || "n/a"}</strong>
          </p>
          <p>
            <span>Read-like tools</span>
            <strong
              >{countValue(
                namespaceState.status.surface.read_like_tools.length,
              )}</strong
            >
          </p>
          <p>
            <span>Mutating tools</span>
            <strong
              >{countValue(
                namespaceState.status.surface.mutating_tools.length,
              )}</strong
            >
          </p>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel namespaces-boundary-panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Namespace census</Card.Title>
      <Badge variant="outline"
        >{countValue(namespaceRows(currentCensus(namespaceState)).length)}
        namespaces</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const namespaces = namespaceRows(currentCensus(namespaceState))}
      {#if namespaces.length > 0}
        <div class="namespaces-boundary-list">
          {#each namespaces as namespace (namespace.namespace)}
            <article>
              <header>
                <strong>{namespace.namespace}</strong>
                <Badge variant="secondary">{countValue(namespace.total)}</Badge>
              </header>
              <p>{namespaceDetail(namespace)}</p>
              <span>{compactEntries(namespace.kinds) || "no memories"}</span>
              <span
                >{compactEntries(namespace.work_statuses) ||
                  "no work items"}</span
              >
            </article>
          {/each}
        </div>
      {:else}
        <div class="namespaces-empty-state">
          <strong>No visible namespaces</strong>
          <span>{viewer.trim() || "n/a"}</span>
        </div>
      {/if}
      <Button
        class="namespaces-refresh"
        data-testid="namespaces-refresh"
        disabled={!config || namespaceState.state === "loading"}
        onclick={() => void refreshStatus()}
        variant="outline"
      >
        {#if namespaceState.state === "loading"}
          <LoaderCircle data-icon="inline-start" />
        {:else}
          <RefreshCcw data-icon="inline-start" />
        {/if}
        Refresh status
      </Button>
    </Card.Content>
  </Card.Root>
</section>

<section class="namespaces-bottom-grid">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Database size="18" />
      <Card.Title>Memory kinds</Card.Title>
      <Badge variant="outline"
        >{countValue(
          entriesFromRecord(currentCensus(namespaceState)?.totals.kinds).length,
        )}
        kinds</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const kindEntries = entriesFromRecord(
        currentCensus(namespaceState)?.totals.kinds,
      )}
      {#if kindEntries.length > 0}
        <div
          class="namespaces-census-list"
          data-testid="namespaces-kind-census"
        >
          {#each kindEntries as entry (entry.label)}
            <p data-testid="namespaces-kind-row">
              <strong>{titleCase(entry.label)}</strong>
              <span>{countValue(entry.value)}</span>
            </p>
          {/each}
        </div>
      {:else}
        <div class="namespaces-empty-state">
          <span>Memory kind counts follow the live server census.</span>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Layers size="18" />
      <Card.Title>Work statuses</Card.Title>
      <Badge variant="outline"
        >{countValue(
          entriesFromRecord(currentCensus(namespaceState)?.totals.work_statuses)
            .length,
        )}
        statuses</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const workEntries = entriesFromRecord(
        currentCensus(namespaceState)?.totals.work_statuses,
      )}
      {#if workEntries.length > 0}
        <div
          class="namespaces-census-list"
          data-testid="namespaces-work-census"
        >
          {#each workEntries as entry (entry.label)}
            <p data-testid="namespaces-work-row">
              <strong>{titleCase(entry.label)}</strong>
              <span>{countValue(entry.value)}</span>
            </p>
          {/each}
        </div>
      {:else}
        <div class="namespaces-empty-state">
          <span>Work status counts follow the live server census.</span>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>
</section>

<Card.Root class="panel namespaces-gap-panel">
  <Card.Header class="panel-title">
    <ShieldCheck size="18" />
    <Card.Title>Census payload</Card.Title>
    <Badge variant="outline"
      >{currentCensus(namespaceState)?.mode ?? "pending"}</Badge
    >
  </Card.Header>
  <Separator class="panel-separator" />
  <Card.Content class="panel-content">
    {@const census = currentCensus(namespaceState)}
    <div class="namespaces-gap-list" data-testid="namespaces-census-payload">
      <p>
        <strong>Schema</strong>
        <span>{census?.schema ?? "n/a"}</span>
      </p>
      <p>
        <strong>Namespaces</strong>
        <span>{countValue(census?.namespaces.length)}</span>
      </p>
      <p>
        <strong>Visible total</strong>
        <span
          >{countValue(census?.totals.memories)} memories / {countValue(
            census?.totals.work_items,
          )} work</span
        >
      </p>
    </div>
  </Card.Content>
</Card.Root>
