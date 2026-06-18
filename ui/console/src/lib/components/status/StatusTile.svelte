<script lang="ts">
  import type { StatusTileModel } from "$lib/api/contracts";

  let { tile }: { tile: StatusTileModel } = $props();
</script>

<section class={`status-tile tone-${tile.tone}`} aria-label={tile.label}>
  <span>{tile.label}</span>
  <strong>{tile.value}</strong>
  <small>{tile.detail}</small>
</section>
