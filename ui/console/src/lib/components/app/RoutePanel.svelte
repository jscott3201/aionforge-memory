<script lang="ts">
  import type { Component } from "svelte";
  import PageHeader from "./PageHeader.svelte";
  import * as Card from "$lib/components/ui/card/index.js";

  let {
    eyebrow,
    title,
    detail,
    items,
    icon: Icon,
  }: {
    eyebrow: string;
    title: string;
    detail?: string;
    items: string[];
    icon: Component;
  } = $props();
</script>

<PageHeader {eyebrow} {title} {detail} />

<Card.Root class="route-panel" size="sm">
  <div class="route-panel-icon">
    <Icon size="22" />
  </div>
  <div class="route-panel-grid">
    {#each items as item (item)}
      <span>{item}</span>
    {/each}
  </div>
</Card.Root>
