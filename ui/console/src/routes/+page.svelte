<script lang="ts">
  import {
    Activity,
    Database,
    Flame,
    Server,
    ShieldCheck,
  } from "svelte-lucide";
  import PageHeader from "$lib/components/app/PageHeader.svelte";
  import StatusTile from "$lib/components/status/StatusTile.svelte";
  import {
    consoleSnapshot,
    dashboardActivity,
    statusTiles,
    toolSurface,
  } from "$lib/api/status";
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
  <article class="panel panel-large">
    <div class="panel-title">
      <Flame size="18" />
      <h2>Console foundation</h2>
      <span>static SPA</span>
    </div>
    <div class="foundation-list">
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
    </div>
  </article>

  <article class="panel">
    <div class="panel-title">
      <Server size="18" />
      <h2>MCP tool split</h2>
    </div>
    <div class="split-meter" aria-label="Tool split">
      <span style={`width: ${(consoleSnapshot.readLikeTools / 18) * 100}%`}
      ></span>
    </div>
    <div class="split-legend">
      <span
        ><i class="dot good"></i>{consoleSnapshot.readLikeTools} read-like</span
      >
      <span
        ><i class="dot warn"></i>{consoleSnapshot.mutatingTools} mutating</span
      >
    </div>
  </article>
</section>

<section class="dashboard-grid secondary">
  <article class="panel">
    <div class="panel-title">
      <Activity size="18" />
      <h2>Read surfaces</h2>
    </div>
    <div class="activity-list">
      {#each dashboardActivity as item (item.label)}
        <p>
          <svelte:component this={item.icon} size="16" />
          <strong>{item.label}</strong>
          <span>{item.value}</span>
        </p>
      {/each}
    </div>
  </article>

  <article class="panel panel-large">
    <div class="panel-title">
      <Database size="18" />
      <h2>Manifest preview</h2>
      <span>{toolSurface.length} tools</span>
    </div>
    <div class="tool-table">
      {#each toolSurface.slice(0, 8) as tool (tool.name)}
        <p>
          <strong>{tool.name}</strong>
          <span>{tool.toolClass}</span>
          <em>{tool.approval}</em>
        </p>
      {/each}
    </div>
  </article>

  <article class="panel">
    <div class="panel-title">
      <ShieldCheck size="18" />
      <h2>Security posture</h2>
    </div>
    <div class="security-list">
      <span>OAuth metadata</span>
      <span>Audit signer</span>
      <span>Principal gates</span>
    </div>
  </article>
</section>
