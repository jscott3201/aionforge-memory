import {
  Activity,
  Cpu,
  Database,
  Flame,
  Layers,
  LayoutDashboard,
  Radar,
  ScrollText,
  Server,
  ShieldCheck,
} from "svelte-lucide";
import type {
  ConsoleRoute,
  ConsoleSnapshot,
  StatusTileModel,
  ToolSurfaceModel,
} from "./contracts";

export const consoleRoutes: ConsoleRoute[] = [
  { path: "/", label: "Dashboard", group: "Operate", icon: LayoutDashboard },
  {
    path: "/records",
    label: "Memory records",
    group: "Operate",
    icon: Database,
  },
  { path: "/retrieval", label: "Retrieval", group: "Operate", icon: Radar },
  {
    path: "/consolidation",
    label: "Consolidation",
    group: "Operate",
    icon: Flame,
  },
  { path: "/audit", label: "Audit log", group: "Operate", icon: ScrollText },
  { path: "/mcp", label: "MCP server", group: "Configure", icon: Server },
  {
    path: "/namespaces",
    label: "Namespaces",
    group: "Configure",
    icon: Layers,
  },
  { path: "/embedding", label: "Embedding", group: "Configure", icon: Cpu },
  {
    path: "/security",
    label: "Security",
    group: "Configure",
    icon: ShieldCheck,
  },
];

export const consoleSnapshot: ConsoleSnapshot = {
  endpoint: "/mcp",
  transport: "Streamable HTTP",
  auth: "default-off local; bearer validation when enabled",
  releaseBase: "/console",
  readLikeTools: 8,
  mutatingTools: 10,
  structuredContent: "pending",
};

export const statusTiles: StatusTileModel[] = [
  {
    label: "MCP endpoint",
    value: "/mcp",
    detail: "explicit route",
    tone: "good",
  },
  {
    label: "Console base",
    value: "/console",
    detail: "static SPA",
    tone: "good",
  },
  {
    label: "Tool surface",
    value: "18",
    detail: "8 read-like, 10 mutating",
    tone: "muted",
  },
  {
    label: "DTO layer",
    value: "pending",
    detail: "structuredContent PR next",
    tone: "warn",
  },
];

export const toolSurface: ToolSurfaceModel[] = [
  ...[
    "server_status",
    "search",
    "read_memory",
    "session_manifest",
    "consolidation_status",
    "audit_history",
    "work_tree",
    "work_query",
  ].map((name) => ({
    name,
    toolClass: "read-like" as const,
    output: "recalled-memory-context or compact status text",
    approval: "allow" as const,
  })),
  ...[
    "capture",
    "batch_capture",
    "consolidate",
    "forget",
    "unforget",
    "pin",
    "unpin",
    "work_create",
    "work_advance",
    "work_link",
  ].map((name) => ({
    name,
    toolClass: "mutating" as const,
    output: "compact receipt text",
    approval: "ask" as const,
  })),
];

export const routeSummaries = {
  records: {
    title: "Memory records",
    eyebrow: "read_memory",
    items: ["Episode", "Fact", "Note", "Work item"],
  },
  retrieval: {
    title: "Retrieval",
    eyebrow: "search",
    items: ["Lexical anchors", "Vector search", "Graph signals", "Trust"],
  },
  consolidation: {
    title: "Consolidation",
    eyebrow: "consolidation_status",
    items: ["Pending", "Retried", "Failed", "Generation"],
  },
  audit: {
    title: "Audit log",
    eyebrow: "audit_history",
    items: ["Subject", "Kind", "Cursor", "Signature"],
  },
  mcp: {
    title: "MCP server",
    eyebrow: "aionforge://manifest/tools.json",
    items: ["Tools", "Resources", "Approvals", "Clients"],
  },
  namespaces: {
    title: "Namespaces",
    eyebrow: "principal teams",
    items: ["Agent private", "Team", "Global", "System"],
  },
  embedding: {
    title: "Embedding",
    eyebrow: "embedder posture",
    items: ["Provider", "Dimensions", "Rerank", "Backoff"],
  },
  security: {
    title: "Security",
    eyebrow: "auth and signing",
    items: ["OAuth", "Audit signer", "Redaction", "Guards"],
  },
};

export const dashboardActivity = [
  {
    icon: Activity,
    label: "Server status",
    value: "waiting for live MCP session",
  },
  {
    icon: Database,
    label: "Records",
    value: "search-backed list route staged",
  },
  { icon: Flame, label: "Consolidation", value: "status route staged" },
];
