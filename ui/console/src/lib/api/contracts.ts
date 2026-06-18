import type { Component } from "svelte";

export type ToolClass = "read-like" | "mutating";
export type StatusTone = "good" | "warn" | "muted" | "danger";
export type ConsoleRoutePath =
  | "/"
  | "/records"
  | "/retrieval"
  | "/consolidation"
  | "/audit"
  | "/mcp"
  | "/namespaces"
  | "/embedding"
  | "/security";

export interface ConsoleRoute {
  path: ConsoleRoutePath;
  label: string;
  group: "Operate" | "Configure";
  icon: Component;
}

export interface StatusTileModel {
  label: string;
  value: string;
  detail: string;
  tone: StatusTone;
}

export interface ToolSurfaceModel {
  name: string;
  toolClass: ToolClass;
  output: string;
  approval: "allow" | "ask";
}

export interface ConsoleSnapshot {
  endpoint: string;
  transport: string;
  auth: string;
  releaseBase: string;
  readLikeTools: number;
  mutatingTools: number;
  structuredContent: "pending" | "partial" | "ready";
}
