<script lang="ts">
  import { resolve } from "$app/paths";
  import { page } from "$app/state";
  import type { Component, Snippet } from "svelte";
  import { onMount } from "svelte";
  import { Moon, PanelLeft, Search, Sun } from "svelte-lucide";
  import { consoleRoutes, consoleSnapshot } from "$lib/api/status";
  import mark from "$lib/assets/mark.svg";
  import {
    setTheme,
    theme,
    toggleTheme,
    type ThemeMode,
  } from "$lib/stores/theme";

  let { children }: { children: Snippet } = $props();

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
      <span><strong>{consoleSnapshot.readLikeTools}</strong> read-like</span>
      <span><strong>{consoleSnapshot.mutatingTools}</strong> mutating</span>
    </div>
  </aside>

  <section class="workspace">
    <header class="topbar">
      <button
        class="icon-button mobile-only"
        type="button"
        aria-label="Open navigation"
      >
        <PanelLeft size="18" />
      </button>
      <div class="endpoint">
        <strong>{consoleSnapshot.transport}</strong>
        <span>{consoleSnapshot.endpoint}</span>
      </div>
      <label class="search-box">
        <Search size="16" />
        <input aria-label="Search memory" placeholder="Search memory" />
      </label>
      <button
        class="icon-button"
        type="button"
        aria-label="Toggle theme"
        onclick={() => toggleTheme($theme)}
      >
        {#if $theme === "dark"}
          <Moon size="17" />
        {:else}
          <Sun size="17" />
        {/if}
      </button>
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
