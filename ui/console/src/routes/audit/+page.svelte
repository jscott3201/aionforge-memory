<script lang="ts">
  import { ScrollText } from "@lucide/svelte";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={ScrollText}
  {...routeSummaries.audit}
  detail="Cursor-backed audit history for visible subjects."
/>
