<script lang="ts">
  import type { Component } from "svelte";
  import PageHeader from "./PageHeader.svelte";

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

<section class="route-panel">
  <div class="route-panel-icon">
    <Icon size="22" />
  </div>
  <div class="route-panel-grid">
    {#each items as item (item)}
      <span>{item}</span>
    {/each}
  </div>
</section>
