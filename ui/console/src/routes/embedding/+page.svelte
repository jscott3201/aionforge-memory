<script lang="ts">
  import { Cpu } from "svelte-lucide";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={Cpu}
  {...routeSummaries.embedding}
  detail="Embedding provider and retrieval posture."
/>
