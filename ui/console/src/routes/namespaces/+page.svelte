<script lang="ts">
  import { Layers } from "svelte-lucide";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={Layers}
  {...routeSummaries.namespaces}
  detail="Visibility boundaries for agent and team memory."
/>
