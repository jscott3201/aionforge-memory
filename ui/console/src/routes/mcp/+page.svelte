<script lang="ts">
  import { Server } from "svelte-lucide";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries, toolSurface } from "$lib/api/status";
</script>

<RoutePanel
  icon={Server}
  {...routeSummaries.mcp}
  detail="Tool manifest, resources, and client posture."
/>

<section class="panel route-table">
  {#each toolSurface as tool (tool.name)}
    <p>
      <strong>{tool.name}</strong>
      <span>{tool.toolClass}</span>
      <em>{tool.approval}</em>
    </p>
  {/each}
</section>
