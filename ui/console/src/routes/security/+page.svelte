<script lang="ts">
  import { onMount } from "svelte";
  import {
    FileText,
    KeyRound,
    LoaderCircle,
    RefreshCcw,
    ShieldCheck,
    ShieldOff,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadMcpToolCatalog,
    loadServerStatus,
    type McpClientConfig,
    type McpToolCatalog,
  } from "$lib/api/mcp-client";
  import type {
    ServerStatusStructuredContent,
    ToolManifestTool,
  } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  type SecurityState =
    | { state: "offline" }
    | { state: "loading" }
    | {
        state: "ready";
        status: ServerStatusStructuredContent;
        catalog: McpToolCatalog;
      }
    | { state: "error"; message: string };

  const lifecycleToolNames = [
    "capture",
    "batch_capture",
    "consolidate",
    "forget",
    "unforget",
    "pin",
    "unpin",
  ];
  const countFormat = new Intl.NumberFormat("en-US");
  let config: McpClientConfig | null = null;
  let securityState: SecurityState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      securityState = { state: "offline" };
      return;
    }

    void refreshSecurity();
  });

  async function refreshSecurity(): Promise<void> {
    const activeConfig = config;
    if (!activeConfig || securityState.state === "loading") {
      return;
    }

    securityState = { state: "loading" };
    try {
      const [status, catalog] = await Promise.all([
        loadServerStatus(activeConfig),
        loadMcpToolCatalog(activeConfig),
      ]);
      securityState = { state: "ready", status, catalog };
    } catch (error) {
      securityState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentStatus(
    state: SecurityState,
  ): ServerStatusStructuredContent | null {
    return state.state === "ready" ? state.status : null;
  }

  function currentCatalog(state: SecurityState): McpToolCatalog | null {
    return state.state === "ready" ? state.catalog : null;
  }

  function stateLabel(state: SecurityState): string {
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

  function authLabel(status: ServerStatusStructuredContent | null): string {
    if (!status) {
      return "pending";
    }
    return status.auth.enabled ? "bearer validation" : "local loopback";
  }

  function authMode(status: ServerStatusStructuredContent | null): string {
    if (!status) {
      return "pending";
    }
    return status.auth.enabled ? "enabled" : "default off";
  }

  function issuerLabel(status: ServerStatusStructuredContent | null): string {
    const issuers = status?.auth.issuers ?? [];
    return issuers.length === 0 ? "none" : countFormat.format(issuers.length);
  }

  function policyLabel(
    catalog: McpToolCatalog | null,
    policy: "read_like_approval" | "mutating_approval",
  ): string {
    return catalog?.manifest.policy[policy] ?? "pending";
  }

  function toolCount(catalog: McpToolCatalog | null, mutates: boolean): string {
    return countFormat.format(
      catalog?.manifest.tools.filter((tool) => tool.mutates === mutates)
        .length ?? 0,
    );
  }

  function lifecycleTools(catalog: McpToolCatalog | null): ToolManifestTool[] {
    return (catalog?.manifest.tools ?? [])
      .filter((tool) => lifecycleToolNames.includes(tool.name))
      .sort((left, right) => left.name.localeCompare(right.name));
  }

  function riskLabel(tool: ToolManifestTool): string {
    if (tool.destructive_hint) {
      return "destructive";
    }
    if (tool.idempotent_hint) {
      return "idempotent";
    }
    return tool.mutates ? "mutating" : "read-like";
  }

  function errorCount(tool: ToolManifestTool): string {
    return tool.errors.length === 0
      ? "none"
      : countFormat.format(tool.errors.length);
  }

  function issuerRows(status: ServerStatusStructuredContent | null): string[] {
    return status?.auth.issuers.length
      ? status.auth.issuers
      : ["not configured"];
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP request failed";
  }
</script>

<PageHeader
  eyebrow="auth and approval posture"
  title="Security"
  detail="Read-only OAuth resource-server and tool-safety posture."
/>

<section class="security-workspace">
  <Card.Root class="panel security-status-panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Auth posture</Card.Title>
      <Badge class="state-badge" data-testid="security-state" variant="outline"
        >{stateLabel(securityState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const status = currentStatus(securityState)}
      {@const catalog = currentCatalog(securityState)}
      <div class="security-summary-grid">
        <p>
          <strong data-testid="security-auth-state">{authMode(status)}</strong>
          <span>auth mode</span>
        </p>
        <p>
          <strong data-testid="security-issuer-count"
            >{issuerLabel(status)}</strong
          >
          <span>issuers</span>
        </p>
        <p>
          <strong data-testid="security-read-policy"
            >{policyLabel(catalog, "read_like_approval")}</strong
          >
          <span>read-like tools</span>
        </p>
        <p>
          <strong data-testid="security-mutation-policy"
            >{policyLabel(catalog, "mutating_approval")}</strong
          >
          <span>mutating tools</span>
        </p>
      </div>

      {#if securityState.state === "offline"}
        <div class="security-empty-state">
          <strong>Static preview</strong>
          <span>auth mode · approval policy · lifecycle safety</span>
        </div>
      {:else if securityState.state === "loading"}
        <div class="security-empty-state">
          <LoaderCircle size="18" />
          <strong>Loading</strong>
          <span>server_status and tool manifest</span>
        </div>
      {:else if securityState.state === "error"}
        <div class="security-empty-state tone-danger">
          <strong>Security read failed</strong>
          <span>{securityState.message}</span>
        </div>
      {:else}
        <div class="security-auth-detail">
          <p>
            <span>Resource server</span>
            <strong>{authLabel(status)}</strong>
          </p>
          <p>
            <span>Recall wrapper</span>
            <strong>{securityState.status.recall_wrapper}</strong>
          </p>
          <p>
            <span>Sampling</span>
            <strong
              >{securityState.status.sampling ? "enabled" : "disabled"}</strong
            >
          </p>
          <p>
            <span>Mutation rule</span>
            <strong
              >{securityState.catalog.manifest.policy.mutation_rule}</strong
            >
          </p>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel security-policy-panel">
    <Card.Header class="panel-title">
      <KeyRound size="18" />
      <Card.Title>Boundary</Card.Title>
      <Badge variant="outline">read-only</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="security-boundary-list">
        <p>
          <strong>OAuth role</strong>
          <span>validates bearer tokens when `[auth].enabled` is true</span>
        </p>
        <p>
          <strong>Login flow</strong>
          <span>not provided by the built-in console surface</span>
        </p>
        <p>
          <strong>Principal source</strong>
          <span>host-verified identity is resolved before MCP tools run</span>
        </p>
        <p>
          <strong>Open world tools</strong>
          <span>not advertised by the manifest</span>
        </p>
      </div>
      <Button
        class="security-refresh"
        data-testid="security-refresh"
        disabled={!config || securityState.state === "loading"}
        onclick={() => void refreshSecurity()}
        variant="outline"
      >
        {#if securityState.state === "loading"}
          <LoaderCircle data-icon="inline-start" />
        {:else}
          <RefreshCcw data-icon="inline-start" />
        {/if}
        Refresh posture
      </Button>
    </Card.Content>
  </Card.Root>
</section>

<section class="security-bottom-grid">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <ShieldOff size="18" />
      <Card.Title>Lifecycle tools</Card.Title>
      <Badge data-testid="security-mutating-count" variant="outline"
        >{toolCount(currentCatalog(securityState), true)} mutating</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const tools = lifecycleTools(currentCatalog(securityState))}
      {#if tools.length > 0}
        <div class="security-tool-list" data-testid="security-tool-list">
          {#each tools as tool (tool.name)}
            <article data-testid="security-tool-row">
              <header>
                <strong>{tool.name}</strong>
                <span>
                  <Badge variant="secondary">{riskLabel(tool)}</Badge>
                  <Badge variant="outline">{tool.approval}</Badge>
                </span>
              </header>
              <p>{tool.default_output}</p>
              <dl>
                <div>
                  <dt>Mutates</dt>
                  <dd>{tool.mutates ? "yes" : "no"}</dd>
                </div>
                <div>
                  <dt>Read-only hint</dt>
                  <dd>{tool.read_only_hint ? "yes" : "no"}</dd>
                </div>
                <div>
                  <dt>Errors</dt>
                  <dd>{errorCount(tool)}</dd>
                </div>
              </dl>
            </article>
          {/each}
        </div>
      {:else}
        <div class="security-empty-state">
          <span
            >Lifecycle tools render after the live manifest is available.</span
          >
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <FileText size="18" />
      <Card.Title>Signing and config</Card.Title>
      <Badge variant="outline">deferred</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="security-config-list" data-testid="security-config-list">
        <p>
          <strong>Audit verification</strong>
          <span>visible per audit record in the Audit log view</span>
        </p>
        <p>
          <strong>Signing key posture</strong>
          <span>not exposed by a sanitized console-readable config surface</span
          >
        </p>
        <p>
          <strong>Forgetting policy</strong>
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

<Card.Root class="panel security-issuers-panel">
  <Card.Header class="panel-title">
    <KeyRound size="18" />
    <Card.Title>Issuers</Card.Title>
    <Badge variant="outline">{issuerLabel(currentStatus(securityState))}</Badge>
  </Card.Header>
  <Separator class="panel-separator" />
  <Card.Content class="panel-content">
    <div class="security-issuer-list" data-testid="security-issuer-list">
      {#each issuerRows(currentStatus(securityState)) as issuer (issuer)}
        <p>{issuer}</p>
      {/each}
    </div>
  </Card.Content>
</Card.Root>
