<script lang="ts">
  import type { StatusTileModel } from "$lib/api/contracts";
  import * as Card from "$lib/components/ui/card/index.js";

  let { tile }: { tile: StatusTileModel } = $props();
</script>

<Card.Root
  class={`status-tile tone-${tile.tone}`}
  aria-label={tile.label}
  data-testid={tile.testId}
  size="sm"
>
  <span>{tile.label}</span>
  <strong>{tile.value}</strong>
  <small>{tile.detail}</small>
</Card.Root>
