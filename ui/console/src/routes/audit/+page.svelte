<script lang="ts">
  import { onMount } from "svelte";
  import {
    LoaderCircle,
    RefreshCcw,
    ScrollText,
    ShieldCheck,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadAuditHistory,
    type McpClientConfig,
  } from "$lib/api/mcp-client";
  import type {
    AuditHistoryStructuredContent,
    AuditRecord,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Input } from "$lib/components/ui/input/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  const localViewer = "agent:00000000-0000-4000-8000-000000000311";

  type AuditState =
    | { state: "offline" }
    | { state: "loading" }
    | { state: "ready"; value: AuditHistoryStructuredContent }
    | { state: "empty"; value: AuditHistoryStructuredContent }
    | { state: "error"; message: string };

  let config: McpClientConfig | null = null;
  let kind = "pin";
  let subjectId = "";
  let viewer = localViewer;
  let auditState: AuditState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      auditState = { state: "offline" };
      return;
    }

    void refreshAudit();
  });

  async function refreshAudit(): Promise<void> {
    const activeConfig = config;
    if (!activeConfig || !hasScope() || auditState.state === "loading") {
      return;
    }

    auditState = { state: "loading" };
    try {
      const value = await loadAuditHistory(activeConfig, {
        subjectId,
        kind,
        viewer: viewer.trim() || undefined,
        limit: 12,
        verbose: true,
      });
      auditState =
        value.records.length > 0
          ? { state: "ready", value }
          : { state: "empty", value };
    } catch (error) {
      auditState = { state: "error", message: errorMessage(error) };
    }
  }

  function hasScope(): boolean {
    return Boolean(kind.trim() || subjectId.trim());
  }

  function currentAudit(
    state: AuditState,
  ): AuditHistoryStructuredContent | null {
    return state.state === "ready" || state.state === "empty"
      ? state.value
      : null;
  }

  function stateLabel(state: AuditState): string {
    if (state.state === "ready" || state.state === "empty") {
      return `${state.value.count} records`;
    }
    if (state.state === "loading") {
      return "loading";
    }
    if (state.state === "offline") {
      return "offline";
    }
    return "error";
  }

  function stateTone(state: AuditState): string {
    if (state.state === "ready") {
      return "good";
    }
    if (state.state === "error") {
      return "danger";
    }
    return "muted";
  }

  function nextCursorLabel(
    value: AuditHistoryStructuredContent | null,
  ): string {
    return value?.next ? "available" : "none";
  }

  function shortId(value: string): string {
    return value.length > 14
      ? `${value.slice(0, 8)}...${value.slice(-6)}`
      : value;
  }

  function recordPreview(record: AuditRecord): string {
    return record.payload_preview ?? "payload preview unavailable";
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="audit_history"
  title="Audit log"
  detail="Principal-scoped audit rows by subject, kind, or both."
/>

<section class="audit-workspace">
  <Card.Root class="panel audit-query-panel">
    <Card.Header class="panel-title">
      <ScrollText size="18" />
      <Card.Title>History query</Card.Title>
      <Badge
        class={`state-badge tone-${stateTone(auditState)}`}
        data-testid="audit-state"
        variant="outline">{stateLabel(auditState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <form
        class="audit-query-form"
        onsubmit={(event) => {
          event.preventDefault();
          void refreshAudit();
        }}
      >
        <label>
          <span>Kind</span>
          <Input
            data-testid="audit-kind-input"
            aria-label="Audit kind"
            bind:value={kind}
            disabled={!config}
          />
        </label>
        <label>
          <span>Subject</span>
          <Input
            data-testid="audit-subject-input"
            aria-label="Audit subject id"
            placeholder="optional"
            bind:value={subjectId}
            disabled={!config}
          />
        </label>
        <label>
          <span>Viewer</span>
          <Input
            data-testid="audit-viewer-input"
            aria-label="Audit viewer"
            bind:value={viewer}
            disabled={!config}
          />
        </label>
        <Button
          data-testid="audit-refresh"
          type="submit"
          disabled={!config || !hasScope() || auditState.state === "loading"}
          variant="outline"
        >
          {#if auditState.state === "loading"}
            <LoaderCircle size="16" />
          {:else}
            <RefreshCcw size="16" />
          {/if}
          Refresh
        </Button>
      </form>

      {@const audit = currentAudit(auditState)}
      <div class="audit-summary-grid">
        <p>
          <strong data-testid="audit-result-count"
            >{audit?.count.toString() ?? "0"}</strong
          >
          <span>records</span>
        </p>
        <p>
          <strong data-testid="audit-summary-kind">{audit?.kind ?? kind}</strong
          >
          <span>kind</span>
        </p>
        <p>
          <strong data-testid="audit-summary-subject"
            >{audit?.subject ?? "pending"}</strong
          >
          <span>subject</span>
        </p>
        <p>
          <strong data-testid="audit-next-cursor"
            >{nextCursorLabel(audit)}</strong
          >
          <span>next cursor</span>
        </p>
      </div>

      {#if auditState.state === "offline"}
        <div class="audit-empty-state">
          <strong>Static preview</strong>
          <span>pin | forget | unforget | governance rows</span>
        </div>
      {:else if auditState.state === "loading"}
        <div class="audit-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>audit_history structuredContent</span>
        </div>
      {:else if auditState.state === "error"}
        <div class="audit-empty-state tone-danger">
          <strong>Audit query failed</strong>
          <span>{auditState.message}</span>
        </div>
      {:else if auditState.state === "empty"}
        <div class="audit-empty-state">
          <strong>No audit rows</strong>
          <span>{auditState.value.subject} / {auditState.value.kind}</span>
        </div>
      {:else}
        <div class="audit-records" aria-label="Audit records">
          {#each auditState.value.records as record (record.id)}
            <article data-testid="audit-record-item">
              <header>
                <span>
                  <Badge variant="secondary">{record.kind}</Badge>
                  <Badge variant="outline">{record.verification}</Badge>
                </span>
                <strong>{shortId(record.subject_id)}</strong>
              </header>
              <dl>
                <div>
                  <dt>Actor</dt>
                  <dd>{shortId(record.actor)}</dd>
                </div>
                <div>
                  <dt>Namespace</dt>
                  <dd>{record.namespace}</dd>
                </div>
                <div>
                  <dt>Occurred</dt>
                  <dd>{record.occurred_at}</dd>
                </div>
              </dl>
              <code data-testid="audit-payload-preview"
                >{recordPreview(record)}</code
              >
            </article>
          {/each}
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel audit-scope-panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Read scope</Card.Title>
      <Badge variant="outline">read-only</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="audit-scope-list">
        <p>
          <strong>Principal</strong>
          <span>{viewer.trim() || "server default"}</span>
        </p>
        <p>
          <strong>Subject filter</strong>
          <span>{subjectId.trim() || "all visible subjects for kind"}</span>
        </p>
        <p>
          <strong>Payload</strong>
          <span>compact preview</span>
        </p>
      </div>
    </Card.Content>
  </Card.Root>
</section>
