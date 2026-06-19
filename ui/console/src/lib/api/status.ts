import {
  Cpu,
  Database,
  Flame,
  Layers,
  LayoutDashboard,
  Radar,
  ScrollText,
  Server,
  ShieldCheck,
} from "@lucide/svelte";
import type { ConsoleRoute, ConsoleSnapshot } from "./contracts";

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
  structuredContent: "ready",
};

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
