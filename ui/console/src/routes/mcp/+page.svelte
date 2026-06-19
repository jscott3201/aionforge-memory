<script lang="ts">
  import { onMount } from "svelte";
  import {
    FileJson,
    LoaderCircle,
    RefreshCcw,
    Server,
    ShieldCheck,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import {
    createRuntimeMcpClientConfig,
    loadMcpToolCatalog,
    TOOL_MANIFEST_URI,
    type McpClientConfig,
    type McpToolCatalog,
  } from "$lib/api/mcp-client";
  import type { ToolManifestTool } from "$lib/api/contracts";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";

  type CatalogState =
    | { state: "offline" }
    | { state: "loading" }
    | { state: "ready"; value: McpToolCatalog }
    | { state: "error"; message: string };

  const countFormat = new Intl.NumberFormat("en-US");
  let config: McpClientConfig | null = null;
  let catalogState: CatalogState = { state: "offline" };

  onMount(() => {
    config = createRuntimeMcpClientConfig();
    if (!config) {
      catalogState = { state: "offline" };
      return;
    }

    void refreshCatalog();
  });

  async function refreshCatalog(): Promise<void> {
    const activeConfig = config;
    if (!activeConfig || catalogState.state === "loading") {
      return;
    }

    catalogState = { state: "loading" };
    try {
      catalogState = {
        state: "ready",
        value: await loadMcpToolCatalog(activeConfig),
      };
    } catch (error) {
      catalogState = { state: "error", message: errorMessage(error) };
    }
  }

  function currentCatalog(state: CatalogState): McpToolCatalog | null {
    return state.state === "ready" ? state.value : null;
  }

  function stateLabel(state: CatalogState): string {
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

  function toolClassLabel(tool: ToolManifestTool): string {
    return tool.class.replace("_", "-");
  }

  function countTools(
    catalog: McpToolCatalog | null,
    toolClass: ToolManifestTool["class"],
  ): string {
    return countFormat.format(
      catalog?.manifest.tools.filter((tool) => tool.class === toolClass)
        .length ?? 0,
    );
  }

  function toolCount(catalog: McpToolCatalog | null): string {
    return countFormat.format(catalog?.manifest.tools.length ?? 0);
  }

  function resourceCount(catalog: McpToolCatalog | null): string {
    return countFormat.format(catalog?.manifest.server.resource_count ?? 0);
  }

  function transportLabel(catalog: McpToolCatalog | null): string {
    return catalog?.manifest.server.transports.join(", ") ?? "none";
  }

  function schemaLabel(tool: ToolManifestTool): string {
    return tool.schema ?? "text receipt";
  }

  function errorLabel(tool: ToolManifestTool): string {
    return tool.errors.length === 0
      ? "none"
      : countFormat.format(tool.errors.length);
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP resource read failed";
  }
</script>

<PageHeader
  eyebrow={TOOL_MANIFEST_URI}
  title="MCP server"
  detail="Live tool manifest, resource catalog, and approval posture."
/>

<section class="mcp-workspace">
  <Card.Root class="panel mcp-manifest-panel">
    <Card.Header class="panel-title">
      <Server size="18" />
      <Card.Title>Manifest</Card.Title>
      <Badge class="state-badge" data-testid="mcp-state" variant="outline"
        >{stateLabel(catalogState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const catalog = currentCatalog(catalogState)}
      <div class="mcp-summary-grid">
        <p>
          <strong data-testid="mcp-tool-count">{toolCount(catalog)}</strong>
          <span>tools</span>
        </p>
        <p>
          <strong data-testid="mcp-read-like-count"
            >{countTools(catalog, "read_like")}</strong
          >
          <span>read-like</span>
        </p>
        <p>
          <strong data-testid="mcp-mutating-count"
            >{countTools(catalog, "mutating")}</strong
          >
          <span>mutating</span>
        </p>
        <p>
          <strong data-testid="mcp-resource-count"
            >{resourceCount(catalog)}</strong
          >
          <span>resources</span>
        </p>
      </div>

      {#if catalogState.state === "offline"}
        <div class="mcp-empty-state">
          <strong>Runtime MCP endpoint unavailable</strong>
          <span>Static preview cannot read MCP resources.</span>
        </div>
      {:else if catalogState.state === "loading"}
        <div class="mcp-empty-state">
          <LoaderCircle size="18" />
          <span>Reading manifest resource.</span>
        </div>
      {:else if catalogState.state === "error"}
        <div class="mcp-empty-state tone-danger">
          <strong>Manifest read failed</strong>
          <span>{catalogState.message}</span>
        </div>
      {:else}
        <div class="mcp-meta-list">
          <p>
            <span>Server</span>
            <strong>{catalogState.value.manifest.server.name}</strong>
          </p>
          <p>
            <span>Version</span>
            <strong>{catalogState.value.manifest.server.version}</strong>
          </p>
          <p>
            <span>Transport</span>
            <strong>{transportLabel(catalog)}</strong>
          </p>
          <p>
            <span>Recall wrapper</span>
            <strong>{catalogState.value.manifest.server.recall_wrapper}</strong>
          </p>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel mcp-policy-panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Approval policy</Card.Title>
      <Badge variant="outline">read-only</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const catalog = currentCatalog(catalogState)}
      <div class="mcp-policy-list">
        <p>
          <span>Read-like tools</span>
          <strong>{catalog?.manifest.policy.read_like_approval ?? "n/a"}</strong
          >
        </p>
        <p>
          <span>Mutating tools</span>
          <strong>{catalog?.manifest.policy.mutating_approval ?? "n/a"}</strong>
        </p>
        <p>
          <span>Mutation rule</span>
          <strong>{catalog?.manifest.policy.mutation_rule ?? "n/a"}</strong>
        </p>
        <p>
          <span>Open world</span>
          <strong>false for all advertised tools</strong>
        </p>
      </div>
      <Button
        class="mcp-refresh"
        variant="outline"
        onclick={() => void refreshCatalog()}
        disabled={!config || catalogState.state === "loading"}
      >
        {#if catalogState.state === "loading"}
          <LoaderCircle data-icon="inline-start" />
        {:else}
          <RefreshCcw data-icon="inline-start" />
        {/if}
        Refresh manifest
      </Button>
    </Card.Content>
  </Card.Root>
</section>

<Card.Root class="panel mcp-tools-panel">
  <Card.Header class="panel-title">
    <FileJson size="18" />
    <Card.Title>Tool catalog</Card.Title>
    <Badge variant="outline"
      >{toolCount(currentCatalog(catalogState))} tools</Badge
    >
  </Card.Header>
  <Separator class="panel-separator" />
  <Card.Content class="panel-content">
    {#if catalogState.state === "ready"}
      <div class="mcp-tool-list" aria-label="MCP tool catalog">
        {#each catalogState.value.manifest.tools as tool (tool.name)}
          <article data-testid="mcp-tool-row">
            <header>
              <strong>{tool.name}</strong>
              <span>
                <Badge variant="secondary">{toolClassLabel(tool)}</Badge>
                <Badge variant="outline">{tool.approval}</Badge>
              </span>
            </header>
            <p>{tool.default_output}</p>
            <dl>
              <div>
                <dt>Schema</dt>
                <dd>{schemaLabel(tool)}</dd>
              </div>
              <div>
                <dt>Verbose</dt>
                <dd>{tool.verbose ? "supported" : "none"}</dd>
              </div>
              <div>
                <dt>Errors</dt>
                <dd>{errorLabel(tool)}</dd>
              </div>
            </dl>
          </article>
        {/each}
      </div>
    {:else}
      <div class="mcp-empty-state">
        <strong>{stateLabel(catalogState)}</strong>
        <span>Tool rows render after the live manifest is available.</span>
      </div>
    {/if}
  </Card.Content>
</Card.Root>

<section class="mcp-bottom-grid">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <FileJson size="18" />
      <Card.Title>Resources</Card.Title>
      <Badge variant="outline"
        >{currentCatalog(catalogState)?.resources.length ?? 0} listed</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {#if catalogState.state === "ready"}
        <div class="mcp-resource-list" data-testid="mcp-resource-list">
          {#each catalogState.value.resources as resource (resource.uri)}
            <p>
              <strong>{resource.title ?? resource.name}</strong>
              <span>{resource.uri}</span>
            </p>
          {/each}
        </div>
      {:else}
        <div class="mcp-empty-state">
          <span>Resource metadata follows the live server capabilities.</span>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Server size="18" />
      <Card.Title>Telemetry</Card.Title>
      <Badge variant="outline">deferred</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="mcp-telemetry-gap" data-testid="mcp-telemetry-gap">
        <p>
          <strong>Call counts</strong>
          <span>not exposed by a console-readable MCP surface</span>
        </p>
        <p>
          <strong>Request log</strong>
          <span>not exposed until request telemetry lands</span>
        </p>
      </div>
    </Card.Content>
  </Card.Root>
</section>
