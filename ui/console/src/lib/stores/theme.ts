import { browser } from "$app/environment";
import { writable } from "svelte/store";

export type ThemeMode = "dark" | "light";

const STORAGE_KEY = "aionforge-console-theme";

function initialTheme(): ThemeMode {
  if (!browser) return "dark";
  const stored = window.localStorage.getItem(STORAGE_KEY);
  if (stored === "dark" || stored === "light") return stored;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export const theme = writable<ThemeMode>(initialTheme());

export function setTheme(next: ThemeMode): void {
  theme.set(next);
  if (browser) {
    window.localStorage.setItem(STORAGE_KEY, next);
    document.documentElement.dataset.theme = next;
  }
}

export function toggleTheme(current: ThemeMode): void {
  setTheme(current === "dark" ? "light" : "dark");
}
