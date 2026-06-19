<script lang="ts">
  import {
    Activity,
    Database,
    Flame,
    Server,
    ShieldCheck,
  } from "@lucide/svelte";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import StatusTile from "$lib/components/status/StatusTile.svelte";
  import {
    consoleSnapshot,
    dashboardActivity,
    statusTiles,
    toolSurface,
  } from "$lib/api/status";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import * as Card from "$lib/components/ui/card/index.js";
  import { Separator } from "$lib/components/ui/separator/index.js";
</script>

<PageHeader
  eyebrow="Memory substrate"
  title="Operator dashboard"
  detail="Capture, consolidate, retrieve, and inspect the MCP memory surface from one console."
/>

<section class="status-grid" aria-label="Console status">
  {#each statusTiles as tile (tile.label)}
    <StatusTile {tile} />
  {/each}
</section>

<section class="dashboard-grid">
  <Card.Root class="panel panel-large">
    <Card.Header class="panel-title">
      <Flame size="18" />
      <Card.Title>Console foundation</Card.Title>
      <Badge variant="secondary">static SPA</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content foundation-list">
      <p>
        <strong>Base path</strong><span>{consoleSnapshot.releaseBase}</span>
      </p>
      <p><strong>MCP route</strong><span>{consoleSnapshot.endpoint}</span></p>
      <p><strong>Auth posture</strong><span>{consoleSnapshot.auth}</span></p>
      <p>
        <strong>DTO state</strong><span
          >{consoleSnapshot.structuredContent}</span
        >
      </p>
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Server size="18" />
      <Card.Title>MCP tool split</Card.Title>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content">
      <div class="split-meter" aria-label="Tool split">
        <span style={`width: ${(consoleSnapshot.readLikeTools / 18) * 100}%`}
        ></span>
      </div>
      <div class="split-legend">
        <Badge variant="secondary"
          ><i class="dot good"></i>{consoleSnapshot.readLikeTools} read-like</Badge
        >
        <Badge variant="outline"
          ><i class="dot warn"></i>{consoleSnapshot.mutatingTools} mutating</Badge
        >
      </div>
    </Card.Content>
  </Card.Root>
</section>

<section class="dashboard-grid secondary">
  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <Activity size="18" />
      <Card.Title>Read surfaces</Card.Title>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content activity-list">
      {#each dashboardActivity as item (item.label)}
        <p>
          <svelte:component this={item.icon} size="16" />
          <strong>{item.label}</strong>
          <span>{item.value}</span>
        </p>
      {/each}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel panel-large">
    <Card.Header class="panel-title">
      <Database size="18" />
      <Card.Title>Manifest preview</Card.Title>
      <Badge variant="outline">{toolSurface.length} tools</Badge>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content tool-table">
      {#each toolSurface.slice(0, 8) as tool (tool.name)}
        <p>
          <strong>{tool.name}</strong>
          <span>{tool.toolClass}</span>
          <Badge
            class="approval-badge"
            variant={tool.approval === "allow" ? "secondary" : "outline"}
            >{tool.approval}</Badge
          >
        </p>
      {/each}
    </Card.Content>
  </Card.Root>

  <Card.Root class="panel">
    <Card.Header class="panel-title">
      <ShieldCheck size="18" />
      <Card.Title>Security posture</Card.Title>
    </Card.Header>
    <Separator class="panel-separator" />
    <Card.Content class="panel-content security-list">
      <Badge variant="outline">OAuth metadata</Badge>
      <Badge variant="outline">Audit signer</Badge>
      <Badge variant="outline">Principal gates</Badge>
    </Card.Content>
  </Card.Root>
</section>
