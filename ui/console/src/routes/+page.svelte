<script lang="ts">
  import { resolve } from "$app/paths";
  import {
    Activity,
    Database,
    Layers,
    LoaderCircle,
    Server,
    ShieldCheck,
  } from "@lucide/svelte";
  import { onMount } from "svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import StatusTile from "$lib/components/status/StatusTile.svelte";
  import { consoleRoutes, consoleSnapshot } from "$lib/api/status";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";
  import {
    createRuntimeMcpClientConfig,
    loadMcpToolCatalog,
    loadServerStatus,
    type McpToolCatalog,
  } from "$lib/api/mcp-client";
  import type {
    ConsoleRoute,
    ConsoleRoutePath,
    ServerStatusStructuredContent,
    StatusTileModel,
    ToolManifestTool,
  } from "$lib/api/contracts";

  type DashboardState =
    | { state: "loading" }
    | {
        state: "ready";
        status: ServerStatusStructuredContent;
        catalog: McpToolCatalog;
      }
    | { state: "unavailable"; message: string };

  const countFormat = new Intl.NumberFormat("en-US");
  const operationPaths: ConsoleRoutePath[] = [
    "/records",
    "/retrieval",
    "/consolidation",
    "/audit",
  ];

  let dashboardState: DashboardState = { state: "loading" };

  onMount(() => {
    let mounted = true;
    const config = createRuntimeMcpClientConfig();

    if (!config) {
      dashboardState = {
        state: "unavailable",
        message: "Static preview",
      };
      return () => {
        mounted = false;
      };
    }

    Promise.all([loadServerStatus(config), loadMcpToolCatalog(config)])
      .then(([status, catalog]) => {
        if (mounted) {
          dashboardState = { state: "ready", status, catalog };
        }
      })
      .catch((error) => {
        if (mounted) {
          dashboardState = {
            state: "unavailable",
            message: errorMessage(error),
          };
        }
      });

    return () => {
      mounted = false;
    };
  });

  function dashboardTiles(state: DashboardState): StatusTileModel[] {
    if (state.state === "ready") {
      return [
        {
          label: "Memory records",
          value: formatCount(state.status.counts.memories),
          detail: `${kindCount(state.status)} kinds`,
          tone: "good",
          testId: "live-memory-count",
        },
        {
          label: "Work items",
          value: formatCount(state.status.counts.work_items),
          detail: `${workStatusCount(state.status)} statuses`,
          tone: "muted",
          testId: "live-work-count",
        },
        {
          label: "Tool surface",
          value: formatCount(state.catalog.manifest.tools.length),
          detail: `${readLikeCount(state.catalog)} read-like, ${mutatingCount(
            state.catalog,
          )} mutating`,
          tone: "muted",
          testId: "live-tool-count",
        },
        {
          label: "Resources",
          value: formatCount(state.catalog.manifest.server.resource_count),
          detail: "manifest resources",
          tone: "good",
          testId: "live-resource-count",
        },
      ];
    }

    if (state.state === "loading") {
      return [
        {
          label: "Memory records",
          value: "loading",
          detail: "server_status",
          tone: "muted",
          testId: "live-memory-count",
        },
        {
          label: "Work items",
          value: "loading",
          detail: "server_status",
          tone: "muted",
          testId: "live-work-count",
        },
        {
          label: "Tool surface",
          value: "loading",
          detail: "tool manifest",
          tone: "muted",
          testId: "live-tool-count",
        },
        {
          label: "Resources",
          value: "loading",
          detail: "tool manifest",
          tone: "muted",
          testId: "live-resource-count",
        },
      ];
    }

    return [
      {
        label: "MCP endpoint",
        value: consoleSnapshot.endpoint,
        detail: "not connected",
        tone: "warn",
      },
      {
        label: "Console base",
        value: consoleSnapshot.releaseBase,
        detail: "static SPA",
        tone: "good",
      },
      {
        label: "Tool surface",
        value: formatCount(
          consoleSnapshot.readLikeTools + consoleSnapshot.mutatingTools,
        ),
        detail: `${consoleSnapshot.readLikeTools} read-like, ${consoleSnapshot.mutatingTools} mutating`,
        tone: "muted",
      },
      {
        label: "DTO layer",
        value: consoleSnapshot.structuredContent,
        detail: "structuredContent",
        tone: "good",
      },
    ];
  }

  function currentStatus(
    state: DashboardState,
  ): ServerStatusStructuredContent | null {
    return state.state === "ready" ? state.status : null;
  }

  function currentCatalog(state: DashboardState): McpToolCatalog | null {
    return state.state === "ready" ? state.catalog : null;
  }

  function stateLabel(state: DashboardState): string {
    if (state.state === "ready") {
      return "live";
    }
    if (state.state === "loading") {
      return "connecting";
    }
    return "offline";
  }

  function formatCount(value: number): string {
    return countFormat.format(value);
  }

  function formatBytes(value: number): string {
    if (value < 1024) {
      return `${formatCount(value)} B`;
    }
    const units = ["KB", "MB", "GB", "TB"];
    let scaled = value / 1024;
    let unitIndex = 0;

    while (scaled >= 1024 && unitIndex < units.length - 1) {
      scaled /= 1024;
      unitIndex += 1;
    }

    return `${scaled.toFixed(scaled >= 10 ? 0 : 1)} ${units[unitIndex]}`;
  }

  function trafficLabel(status: ServerStatusStructuredContent | null): string {
    if (!status) {
      return "not exposed";
    }
    const traffic = status.telemetry.memory_traffic;
    return `${formatBytes(traffic.bytes_in_total)} in / ${formatBytes(
      traffic.bytes_out_total,
    )} out`;
  }

  function tokenEstimateLabel(
    status: ServerStatusStructuredContent | null,
  ): string {
    if (!status) {
      return "not exposed";
    }
    const traffic = status.telemetry.memory_traffic;
    return `${formatCount(
      traffic.estimated_tokens_in_total,
    )} in / ${formatCount(traffic.estimated_tokens_out_total)} out`;
  }

  function transportLabel(
    status: ServerStatusStructuredContent | null,
  ): string {
    return status ? status.transports.join(", ") : consoleSnapshot.transport;
  }

  function authLabel(status: ServerStatusStructuredContent | null): string {
    if (!status) {
      return consoleSnapshot.auth;
    }
    return status.auth.enabled ? "bearer validation" : "default off";
  }

  function buildLabel(status: ServerStatusStructuredContent | null): string {
    const sha = status?.build.sha;
    return sha ? sha.slice(0, 8) : "unknown";
  }

  function kindCount(status: ServerStatusStructuredContent): number {
    return Object.keys(status.counts.kinds).length;
  }

  function workStatusCount(status: ServerStatusStructuredContent): number {
    return Object.keys(status.counts.work_statuses).length;
  }

  function readLikeCount(catalog: McpToolCatalog | null): number {
    return (
      catalog?.manifest.tools.filter((tool) => tool.class === "read_like")
        .length ?? consoleSnapshot.readLikeTools
    );
  }

  function mutatingCount(catalog: McpToolCatalog | null): number {
    return (
      catalog?.manifest.tools.filter((tool) => tool.class === "mutating")
        .length ?? consoleSnapshot.mutatingTools
    );
  }

  function operationRoutes(): ConsoleRoute[] {
    return operationPaths
      .map((path) => consoleRoutes.find((route) => route.path === path))
      .filter((route): route is ConsoleRoute => route !== undefined);
  }

  function routeMetric(
    route: ConsoleRoute,
    status: ServerStatusStructuredContent | null,
  ): string {
    if (!status) {
      return "static";
    }

    switch (route.path) {
      case "/records":
        return countLabel(status.counts.memories, "memory", "memories");
      case "/retrieval":
        return `${formatCount(status.surface.read_like_tools.length)} read-like`;
      case "/consolidation":
        return countLabel(status.counts.work_items, "work item", "work items");
      case "/audit":
        return countLabel(status.surface.resources, "resource", "resources");
      default:
        return "open";
    }
  }

  function routeDetail(route: ConsoleRoute): string {
    switch (route.path) {
      case "/records":
        return "search and read_memory";
      case "/retrieval":
        return "search route and signals";
      case "/consolidation":
        return "backlog and generation";
      case "/audit":
        return "audit_history rows";
      default:
        return route.group;
    }
  }

  function kindRows(
    status: ServerStatusStructuredContent | null,
  ): Array<[string, number]> {
    return Object.entries(status?.counts.kinds ?? {})
      .sort(([, left], [, right]) => right - left)
      .slice(0, 6);
  }

  function workRows(
    status: ServerStatusStructuredContent | null,
  ): Array<[string, number]> {
    return Object.entries(status?.counts.work_statuses ?? {})
      .sort(([left], [right]) => left.localeCompare(right))
      .slice(0, 5);
  }

  function toolRows(catalog: McpToolCatalog | null): ToolManifestTool[] {
    return (catalog?.manifest.tools ?? [])
      .filter((tool) => tool.name !== "server_status")
      .sort((left, right) => {
        if (left.class !== right.class) {
          return left.class.localeCompare(right.class);
        }
        return left.name.localeCompare(right.name);
      })
      .slice(0, 6);
  }

  function titleCase(value: string): string {
    return value
      .replaceAll("_", " ")
      .replace(/\b\w/g, (match) => match.toUpperCase());
  }

  function countLabel(value: number, singular: string, plural: string): string {
    return `${formatCount(value)} ${value === 1 ? singular : plural}`;
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "MCP dashboard read failed";
  }
</script>

<PageHeader
  eyebrow="Memory substrate"
  title="Operator dashboard"
  detail="Live MCP census, route handoffs, and runtime posture."
/>

<section class="status-grid" aria-label="Console status">
  {#each dashboardTiles(dashboardState) as tile (tile.label)}
    <StatusTile {tile} />
  {/each}
</section>

<section class="dashboard-grid">
  <Card.Root class="panel panel-large">
    <Card.Header class="panel-title">
      <Server size="18" />
      <Card.Title>Runtime surface</Card.Title>
      <Badge data-testid="live-mcp-state" variant="outline"
        >{stateLabel(dashboardState)}</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      {@const status = currentStatus(dashboardState)}
      <div class="dashboard-runtime-list">
        <p>
          <strong>Transport</strong>
          <span data-testid="dashboard-transport">{transportLabel(status)}</span
          >
        </p>
        <p>
          <strong>MCP route</strong>
          <span>{consoleSnapshot.endpoint}</span>
        </p>
        <p>
          <strong>Auth posture</strong>
          <span data-testid="dashboard-auth-state">{authLabel(status)}</span>
        </p>
        <p>
          <strong>Build</strong>
          <span data-testid="dashboard-build-sha">{buildLabel(status)}</span>
        </p>
        <p>
          <strong>DTO state</strong>
          <span>{consoleSnapshot.structuredContent}</span>
        </p>
        <p>
          <strong>Memory traffic</strong>
          <span data-testid="dashboard-traffic-bytes"
            >{trafficLabel(status)}</span
          >
        </p>
        <p>
          <strong>Token estimate</strong>
          <span data-testid="dashboard-traffic-tokens"
            >{tokenEstimateLabel(status)}</span
          >
        </p>
      </div>
      {#if dashboardState.state === "loading"}
        <div class="dashboard-inline-state">
          <LoaderCircle size="16" />
          <span>server_status + tool manifest</span>
        </div>
      {:else if dashboardState.state === "unavailable"}
        <div class="dashboard-inline-state tone-warn">
          <span>{dashboardState.message}</span>
        </div>
      {/if}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Activity size="18" />
      <Card.Title>Operate flow</Card.Title>
      <Badge variant="outline">live routes</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content dashboard-flow-list">
      {@const status = currentStatus(dashboardState)}
      {#each operationRoutes() as route (route.path)}
        {@const Icon = route.icon}
        <a
          class="dashboard-link-row"
          data-testid="dashboard-route-link"
          href={resolve(route.path)}
        >
          <span class="dashboard-link-icon"><Icon size="16" /></span>
          <span>
            <strong>{route.label}</strong>
            <small>{routeDetail(route)}</small>
          </span>
          <Badge variant="outline">{routeMetric(route, status)}</Badge>
        </a>
      {/each}
    </Card.Content>
  </Card.Root>
</section>

<section class="dashboard-grid secondary">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Layers size="18" />
      <Card.Title>Memory census</Card.Title>
      <Badge variant="outline"
        >{kindRows(currentStatus(dashboardState)).length} kinds</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content dashboard-census-list">
      {#each kindRows(currentStatus(dashboardState)) as [kind, count] (kind)}
        <p data-testid="dashboard-kind-row">
          <strong>{titleCase(kind)}</strong>
          <span>{formatCount(count)}</span>
        </p>
      {:else}
        <p>
          <strong>Unavailable</strong>
          <span>server_status</span>
        </p>
      {/each}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel panel-large">
    <Card.Header class="panel-title">
      <Database size="18" />
      <Card.Title>Tool approvals</Card.Title>
      <Badge data-testid="dashboard-mutating-count" variant="outline"
        >{mutatingCount(currentCatalog(dashboardState))} mutating</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content dashboard-tool-list">
      {#each toolRows(currentCatalog(dashboardState)) as tool (tool.name)}
        <p data-testid="dashboard-tool-row">
          <strong>{tool.name}</strong>
          <span>{tool.class.replace("_", "-")}</span>
          <Badge variant={tool.mutates ? "outline" : "secondary"}
            >{tool.approval}</Badge
          >
        </p>
      {:else}
        <p>
          <strong>Manifest</strong>
          <span>not connected</span>
          <Badge variant="outline">offline</Badge>
        </p>
      {/each}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Work states</Card.Title>
      <Badge variant="outline"
        >{workRows(currentStatus(dashboardState)).length} statuses</Badge
      >
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content dashboard-census-list">
      {#each workRows(currentStatus(dashboardState)) as [status, count] (status)}
        <p data-testid="dashboard-work-row">
          <strong>{titleCase(status)}</strong>
          <span>{formatCount(count)}</span>
        </p>
      {:else}
        <p>
          <strong>Unavailable</strong>
          <span>server_status</span>
        </p>
      {/each}
    </Card.Content>
  </Card.Root>
</section>
