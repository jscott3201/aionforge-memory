<script lang="ts">
  import { ShieldCheck } from "@lucide/svelte";
  import RoutePanel from "$lib/components/app/RoutePanel.svelte";
  import { routeSummaries } from "$lib/api/status";
</script>

<RoutePanel
  icon={ShieldCheck}
  {...routeSummaries.security}
  detail="OAuth resource-server state and signing gates."
/>
