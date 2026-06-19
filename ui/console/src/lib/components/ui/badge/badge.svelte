<script lang="ts" module>
  import { type VariantProps, tv } from "tailwind-variants";

  export const badgeVariants = tv({
    base: "h-5 gap-1 rounded-[var(--radius-sm)] border border-transparent bg-clip-padding px-2 py-0.5 text-xs font-medium shadow-none transition-colors has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 [&>svg]:size-3! focus-visible:border-ring focus-visible:ring-ring/50 aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 aria-invalid:border-destructive group/badge inline-flex w-fit shrink-0 items-center justify-center overflow-hidden whitespace-nowrap focus-visible:ring-[3px] [&>svg]:pointer-events-none",
    variants: {
      variant: {
        default:
          "border-border bg-transparent text-foreground [a]:hover:border-ring [a]:hover:bg-transparent",
        secondary:
          "border-border bg-transparent text-secondary-foreground [a]:hover:border-ring [a]:hover:bg-transparent [a]:hover:text-foreground",
        destructive:
          "border-destructive/30 bg-transparent text-destructive [a]:hover:bg-destructive/10 focus-visible:ring-destructive/20 dark:focus-visible:ring-destructive/40",
        outline:
          "border-border bg-transparent text-muted-foreground [a]:hover:border-ring [a]:hover:bg-transparent [a]:hover:text-foreground",
        ghost:
          "border-transparent bg-transparent hover:bg-transparent hover:text-muted-foreground",
        link: "text-primary underline-offset-4 hover:underline",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  });

  export type BadgeVariant = VariantProps<typeof badgeVariants>["variant"];
</script>

<script lang="ts">
  import type { HTMLAnchorAttributes } from "svelte/elements";
  import { cn, type WithElementRef } from "$lib/utils.js";

  let {
    ref = $bindable(null),
    href,
    class: className,
    variant = "default",
    children,
    ...restProps
  }: WithElementRef<HTMLAnchorAttributes> & {
    variant?: BadgeVariant;
  } = $props();
</script>

<svelte:element
  this={href ? "a" : "span"}
  bind:this={ref}
  data-slot="badge"
  {href}
  class={cn(badgeVariants({ variant }), className)}
  {...restProps}
>
  {@render children?.()}
</svelte:element>
