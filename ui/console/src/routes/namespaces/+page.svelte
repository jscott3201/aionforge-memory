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
    loadServerStatus,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type { ServerStatusStructuredContent } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  type NamespaceState =
    | { state: "offline" }
    | { state: "loading" }
    | { state: "ready"; value: ServerStatusStructuredContent }
    | { state: "error"; message: string };

  interface CountEntry {
    label: string;
    value: number;
  }

  const countFormat = new Intl.NumberFormat("en-US");
  const namespaceBoundaries = [
    {
      name: "Agent private",
      scope: "agent:<id>",
      owner: "default capture owner",
      posture: "isolated by writer identity",
    },
    {
      name: "Team",
      scope: "team:<name>",
      owner: "asserted team membership",
      posture: "shared with authorized teammates",
    },
    {
      name: "Global",
      scope: "global",
      owner: "promotion policy",
      posture: "separate trust surface",
    },
    {
      name: "System",
      scope: "system",
      owner: "host-controlled",
      posture: "excluded from default recall",
    },
  ];
  let config: McpClientConfig | null = null;
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
      namespaceState = {
        state: "ready",
        value: await loadServerStatus(activeConfig),
      };
    } catch (error) {
      namespaceState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentStatus(
    state: NamespaceState,
  ): ServerStatusStructuredContent | null {
    return state.state === "ready" ? state.value : null;
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
  eyebrow="server_status"
  title="Namespaces"
  detail="Live aggregate visibility posture from the MCP server."
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
      <div class="namespaces-summary-grid">
        <p>
          <strong data-testid="namespaces-memory-count"
            >{countValue(status?.counts.memories)}</strong
          >
          <span>memories</span>
        </p>
        <p>
          <strong data-testid="namespaces-work-count"
            >{countValue(status?.counts.work_items)}</strong
          >
          <span>work items</span>
        </p>
        <p>
          <strong data-testid="namespaces-kind-count"
            >{countValue(
              Object.keys(status?.counts.kinds ?? {}).length,
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
          <span>memory totals, work totals, and namespace gaps</span>
        </div>
      {:else if namespaceState.state === "loading"}
        <div class="namespaces-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>server_status</span>
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
            <strong>{namespaceState.value.recall_wrapper}</strong>
          </p>
          <p>
            <span>Read-like tools</span>
            <strong
              >{countValue(
                namespaceState.value.surface.read_like_tools.length,
              )}</strong
            >
          </p>
          <p>
            <span>Mutating tools</span>
            <strong
              >{countValue(
                namespaceState.value.surface.mutating_tools.length,
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
      <Card.Title>Boundaries</Card.Title>
      <Badge variant="outline">policy</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="namespaces-boundary-list">
        {#each namespaceBoundaries as boundary (boundary.name)}
          <article>
            <header>
              <strong>{boundary.name}</strong>
              <Badge variant="secondary">{boundary.scope}</Badge>
            </header>
            <p>{boundary.owner}</p>
            <span>{boundary.posture}</span>
          </article>
        {/each}
      </div>
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
          entriesFromRecord(currentStatus(namespaceState)?.counts.kinds).length,
        )}
        kinds</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const kindEntries = entriesFromRecord(
        currentStatus(namespaceState)?.counts.kinds,
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
          entriesFromRecord(currentStatus(namespaceState)?.counts.work_statuses)
            .length,
        )}
        statuses</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const workEntries = entriesFromRecord(
        currentStatus(namespaceState)?.counts.work_statuses,
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
    <Card.Title>Backend gaps</Card.Title>
    <Badge variant="outline">deferred</Badge>
  </Card.Header>
  <Separator class="panel-separator" />
  <Card.Content class="panel-content">
    <div class="namespaces-gap-list" data-testid="namespaces-gap-list">
      <p>
        <strong>Namespace inventory</strong>
        <span>not exposed by a console-readable MCP surface</span>
      </p>
      <p>
        <strong>Principal access listing</strong>
        <span>not exposed until a namespace census reader lands</span>
      </p>
      <p>
        <strong>Session visibility map</strong>
        <span>not exposed until session access metadata is modeled</span>
      </p>
    </div>
  </Card.Content>
</Card.Root>
