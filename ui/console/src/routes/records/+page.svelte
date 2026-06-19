<script lang="ts">
  import { Database } from "@lucide/svelte";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={Database}
  {...routeSummaries.records}
  detail="Search-backed records with read_memory detail panes."
/>
