<script lang="ts">
  import { onMount } from "svelte";
  import { Flame, GitBranch, LoaderCircle, RefreshCcw } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadConsolidationStatus,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type { ConsolidationStatusStructuredContent } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  type ConsolidationState =
    | { state: "offline" }
    | { state: "loading" }
    | { state: "ready"; value: ConsolidationStatusStructuredContent }
    | { state: "error"; message: string };

  const countFormat = new Intl.NumberFormat("en-US");
  let config: McpClientConfig | null = null;
  let consolidationState: ConsolidationState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      consolidationState = { state: "offline" };
      return;
    }

    void refreshStatus();
  });

  async function refreshStatus(): Promise<void> {
    const activeConfig = config;
    if (!activeConfig || consolidationState.state === "loading") {
      return;
    }

    consolidationState = { state: "loading" };
    try {
      consolidationState = {
        state: "ready",
        value: await loadConsolidationStatus(activeConfig),
      };
    } catch (error) {
      consolidationState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentStatus(
    state: ConsolidationState,
  ): ConsolidationStatusStructuredContent | null {
    return state.state === "ready" ? state.value : null;
  }

  function statusLabel(state: ConsolidationState): string {
    if (state.state === "ready") {
      return state.value.state.replaceAll("_", " ");
    }
    if (state.state === "loading") {
      return "loading";
    }
    if (state.state === "offline") {
      return "offline";
    }
    return "error";
  }

  function stateTone(
    value: ConsolidationStatusStructuredContent | null,
  ): string {
    if (!value) {
      return "muted";
    }
    if (value.state === "attention_required") {
      return "danger";
    }
    if (value.state === "backlog_pending") {
      return "warn";
    }
    return "good";
  }

  function countValue(value: number | undefined): string {
    return value === undefined ? "0" : countFormat.format(value);
  }

  function formatAge(seconds: number | undefined): string {
    if (!seconds) {
      return "none";
    }

    const days = Math.floor(seconds / 86_400);
    const hours = Math.floor((seconds % 86_400) / 3_600);
    const minutes = Math.floor((seconds % 3_600) / 60);

    if (days > 0) {
      return `${days}d ${hours}h`;
    }
    if (hours > 0) {
      return `${hours}h ${minutes}m`;
    }
    if (minutes > 0) {
      return `${minutes}m`;
    }
    return `${seconds}s`;
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="consolidation status"
  title="Consolidation"
  detail="Backlog state, pending work, failures, and graph generation."
/>

<section class="consolidation-workspace">
  <Card.Root class="panel consolidation-status-panel">
    <Card.Header class="panel-title">
      <Flame size="18" />
      <Card.Title>Backlog status</Card.Title>
      <Badge
        class={`state-badge tone-${stateTone(currentStatus(consolidationState))}`}
        data-testid="consolidation-state"
        variant="outline">{statusLabel(consolidationState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="consolidation-summary-grid">
        <p>
          <strong data-testid="consolidation-pending"
            >{countValue(currentStatus(consolidationState)?.pending)}</strong
          >
          <span>pending</span>
        </p>
        <p>
          <strong data-testid="consolidation-failed"
            >{countValue(currentStatus(consolidationState)?.failed)}</strong
          >
          <span>failed</span>
        </p>
        <p>
          <strong data-testid="consolidation-oldest-age"
            >{formatAge(
              currentStatus(consolidationState)?.oldest_pending_age_s,
            )}</strong
          >
          <span>oldest age</span>
        </p>
        <p>
          <strong data-testid="consolidation-generation"
            >{countValue(currentStatus(consolidationState)?.generation)}</strong
          >
          <span>generation</span>
        </p>
      </div>

      {#if consolidationState.state === "offline"}
        <div class="consolidation-empty-state">
          <strong>Static preview</strong>
          <span>pending · failed · oldest age · generation</span>
        </div>
      {:else if consolidationState.state === "loading"}
        <div class="consolidation-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>consolidation_status</span>
        </div>
      {:else if consolidationState.state === "error"}
        <div class="consolidation-empty-state tone-danger">
          <strong>Status failed</strong>
          <span>{consolidationState.message}</span>
        </div>
      {:else}
        <div class="consolidation-ledger" data-testid="consolidation-ledger">
          <p>
            <span>State</span>
            <strong>{consolidationState.value.state}</strong>
          </p>
          <p>
            <span>Pending episodes</span>
            <strong>{countValue(consolidationState.value.pending)}</strong>
          </p>
          <p>
            <span>Failed episodes</span>
            <strong>{countValue(consolidationState.value.failed)}</strong>
          </p>
          <p>
            <span>Oldest pending lag</span>
            <strong
              >{formatAge(
                consolidationState.value.oldest_pending_age_s,
              )}</strong
            >
          </p>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel consolidation-scope-panel">
    <Card.Header class="panel-title">
      <GitBranch size="18" />
      <Card.Title>Run scope</Card.Title>
      <Badge variant="outline">read-only</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="consolidation-scope-list">
        <p>
          <strong>Progress</strong>
          <span>not exposed</span>
        </p>
        <p>
          <strong>ETA</strong>
          <span>not exposed</span>
        </p>
        <p>
          <strong>Foreground pass</strong>
          <span>deferred</span>
        </p>
      </div>
      <Button
        class="consolidation-refresh"
        data-testid="consolidation-refresh"
        disabled={!config || consolidationState.state === "loading"}
        onclick={() => void refreshStatus()}
        variant="outline"
      >
        {#if consolidationState.state === "loading"}
          <LoaderCircle size="16" />
        {:else}
          <RefreshCcw size="16" />
        {/if}
        Refresh
      </Button>
    </Card.Content>
  </Card.Root>
</section>
