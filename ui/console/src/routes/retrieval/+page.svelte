<script lang="ts">
  import { Radar } from "svelte-lucide";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={Radar}
  {...routeSummaries.retrieval}
  detail="Recall query surface for visible namespaces."
/>
