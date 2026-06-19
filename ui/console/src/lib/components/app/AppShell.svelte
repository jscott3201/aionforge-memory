<script lang="ts">
  import { resolve } from "$app/paths";
  import { page } from "$app/state";
  import type { Component, Snippet } from "svelte";
  import { onMount } from "svelte";
  import { Moon, PanelLeft, Search, Sun } from "@lucide/svelte";
  import { consoleRoutes, consoleSnapshot } from "$lib/api/status";
  import mark from "$lib/assets/mark.svg";
  import { Badge } from "$lib/components/ui/badge/index.js";
  import { Button } from "$lib/components/ui/button/index.js";
  import { Input } from "$lib/components/ui/input/index.js";
  import {
    setTheme,
    theme,
    toggleTheme,
    type ThemeMode,
  } from "$lib/stores/theme";

  let { children }: { children: Snippet } = $props();
  let globalSearch = $state("");

  const groupedRoutes = [
    {
      label: "Operate",
      routes: consoleRoutes.filter((route) => route.group === "Operate"),
    },
    {
      label: "Configure",
      routes: consoleRoutes.filter((route) => route.group === "Configure"),
    },
  ];

  function isActive(path: string): boolean {
    const current = page.route.id ?? "/";
    return current === path;
  }

  onMount(() => {
    let current: ThemeMode = "dark";
    const unsubscribe = theme.subscribe((value) => {
      current = value;
      document.documentElement.dataset.theme = value;
    });
    setTheme(current);
    return unsubscribe;
  });
</script>

<div class="app-shell">
  <aside class="sidebar" aria-label="Console navigation">
    <a class="brand" href={resolve("/")}>
      <img src={mark} alt="" width="34" height="34" />
      <span>AION<span>FORGE</span></span>
      <small>Memory Console</small>
    </a>

    {#each groupedRoutes as group (group.label)}
      <nav class="nav-group" aria-label={group.label}>
        <span>{group.label}</span>
        {#each group.routes as route (route.path)}
          {@const Icon = route.icon as Component}
          <a
            class:active={isActive(route.path)}
            href={resolve(route.path)}
            aria-current={isActive(route.path) ? "page" : undefined}
          >
            <Icon size="17" />
            {route.label}
          </a>
        {/each}
      </nav>
    {/each}

    <div class="sidebar-status">
      <Badge class="sidebar-badge" variant="outline"
        ><strong>{consoleSnapshot.readLikeTools}</strong> read-like</Badge
      >
      <Badge class="sidebar-badge" variant="outline"
        ><strong>{consoleSnapshot.mutatingTools}</strong> mutating</Badge
      >
    </div>
  </aside>

  <section class="workspace">
    <header class="topbar">
      <Button
        class="icon-button mobile-only"
        variant="outline"
        size="icon-sm"
        type="button"
        aria-label="Open navigation"
        title="Open navigation"
      >
        <PanelLeft />
      </Button>
      <div class="endpoint">
        <strong>{consoleSnapshot.transport}</strong>
        <span>{consoleSnapshot.endpoint}</span>
      </div>
      <form
        class="search-box"
        role="search"
        action={resolve("/records")}
        method="get"
        data-testid="global-search-form"
      >
        <Search size="16" />
        <Input
          class="search-input"
          aria-label="Search memory"
          data-testid="global-search-input"
          name="q"
          placeholder="Search memory"
          bind:value={globalSearch}
        />
      </form>
      <Button
        class="icon-button"
        variant="outline"
        size="icon-sm"
        type="button"
        aria-label="Toggle theme"
        title="Toggle theme"
        onclick={() => toggleTheme($theme)}
      >
        {#if $theme === "dark"}
          <Moon />
        {:else}
          <Sun />
        {/if}
      </Button>
      <div class="principal" aria-label="Current principal">
        <span>OP</span>
        <strong>operator</strong>
      </div>
    </header>

    <main>
      {@render children()}
    </main>
  </section>
</div>
