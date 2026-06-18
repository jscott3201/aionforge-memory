<script lang="ts">
  let {
    eyebrow,
    title,
    detail,
  }: {
    eyebrow: string;
    title: string;
    detail?: string;
  } = $props();
</script>

<header class="page-header">
  <span class="eyebrow">{eyebrow}</span>
  <h1>{title}</h1>
  {#if detail}
    <p>{detail}</p>
  {/if}
</header>
