<script lang="ts">
  import type { Snippet } from "svelte";
  import AppShell from "$lib/components/app/AppShell.svelte";
  import "$lib/styles/app.css";

  let { children }: { children: Snippet } = $props();
</script>

<svelte:head>
  <title>Aionforge Memory Console</title>
</svelte:head>

<AppShell>
  {@render children()}
</AppShell>
