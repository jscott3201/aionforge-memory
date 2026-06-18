<script lang="ts">
  import { Flame } from "svelte-lucide";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={Flame}
  {...routeSummaries.consolidation}
  detail="Backlog status and bounded foreground run state."
/>
