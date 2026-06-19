import { expect, type Page } from "@playwright/test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type {
  AuditHistoryStructuredContent,
  ConsolidationStatusStructuredContent,
  SearchResultsStructuredContent,
  ServerStatusStructuredContent,
  ToolManifestStructuredContent,
} from "../../src/lib/api/contracts";

export const LIVE_AGENT_ID = "00000000-0000-4000-8000-000000000311";

interface ToolTextResult {
  content?: Array<{ type?: string; text?: string }>;
  isError?: boolean;
}

export function collectRuntimeErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      errors.push(message.text());
    }
  });
  page.on("pageerror", (error) => {
    errors.push(error.message);
  });
  return errors;
}

export async function seedLiveMemory(
  baseURL: string,
): Promise<ServerStatusStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    await captureWithClient(client, uniqueSeed("console live e2e seed"));

    const result = await client.callTool({
      name: "server_status",
      arguments: { verbose: true },
    });
    const structured = result.structuredContent;
    if (!isServerStatusStructuredContent(structured)) {
      throw new Error("server_status returned an unexpected payload.");
    }

    expect(structured.counts.memories).toBeGreaterThan(0);
    return structured;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function loadLiveServerStatus(
  baseURL: string,
): Promise<ServerStatusStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    const result = await client.callTool({
      name: "server_status",
      arguments: { verbose: true },
    });
    const structured = result.structuredContent;
    if (!isServerStatusStructuredContent(structured)) {
      throw new Error("server_status returned an unexpected payload.");
    }

    return structured;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function captureLiveMemory(
  baseURL: string,
  content: string,
): Promise<void> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    await captureWithClient(client, content);
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function searchLiveMemory(
  baseURL: string,
  query: string,
): Promise<SearchResultsStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    const result = await client.callTool({
      name: "search",
      arguments: {
        query,
        viewer: `agent:${LIVE_AGENT_ID}`,
        limit: 4,
        verbose: true,
        include_superseded: false,
      },
    });
    const structured = result.structuredContent;
    if (!isSearchResultsStructuredContent(structured)) {
      throw new Error("search returned an unexpected payload.");
    }

    expect(structured.summary.returned).toBeGreaterThan(0);
    return structured;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function readToolManifest(
  baseURL: string,
): Promise<ToolManifestStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    const result = await client.readResource({
      uri: "aionforge://manifest/tools.json",
    });
    const text = result.contents
      .map(textResourceContent)
      .find((content) => content !== undefined);
    if (!text) {
      throw new Error("tool manifest resource was not text.");
    }

    const manifest = JSON.parse(text) as unknown;
    if (!isToolManifestStructuredContent(manifest)) {
      throw new Error("tool manifest returned an unexpected payload.");
    }

    expect(manifest.tools.length).toBeGreaterThan(0);
    return manifest;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function seedConsolidationStatus(
  baseURL: string,
): Promise<ConsolidationStatusStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    await captureWithClient(
      client,
      uniqueSeed("console consolidation live e2e"),
    );

    const result = await client.callTool({
      name: "consolidation_status",
      arguments: { verbose: true },
    });
    const structured = result.structuredContent;
    if (!isConsolidationStatusStructuredContent(structured)) {
      throw new Error("consolidation_status returned an unexpected payload.");
    }

    expect(structured.pending).toBeGreaterThan(0);
    return structured;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function seedAuditHistory(
  baseURL: string,
): Promise<AuditHistoryStructuredContent> {
  const client = new Client({
    name: "aionforge-console-e2e",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL("/mcp", baseURL));

  try {
    await client.connect(transport);
    const memoryId = await captureWithClient(
      client,
      uniqueSeed("console audit live e2e"),
    );
    await pinWithClient(client, memoryId);

    const result = await client.callTool({
      name: "audit_history",
      arguments: {
        kind: "pin",
        viewer: `agent:${LIVE_AGENT_ID}`,
        limit: 12,
        verbose: true,
      },
    });
    const structured = result.structuredContent;
    if (!isAuditHistoryStructuredContent(structured)) {
      throw new Error("audit_history returned an unexpected payload.");
    }

    expect(structured.kind).toBe("pin");
    expect(structured.records.length).toBeGreaterThan(0);
    return structured;
  } finally {
    await client.close().catch(() => undefined);
  }
}

async function captureWithClient(
  client: Client,
  content: string,
): Promise<string> {
  const result = await client.callTool({
    name: "capture",
    arguments: {
      agent_id: LIVE_AGENT_ID,
      content,
      role: "event",
      trust: 0.8,
      model_family: "console-e2e",
    },
  });
  const text = toolTextResult(result);
  const memoryId = text.split(/\s+/)[1];
  if (!memoryId) {
    throw new Error(`capture returned an unexpected receipt: ${text}`);
  }
  return memoryId;
}

async function pinWithClient(client: Client, memoryId: string): Promise<void> {
  const result = await client.callTool({
    name: "pin",
    arguments: {
      memory_id: memoryId,
      viewer: `agent:${LIVE_AGENT_ID}`,
    },
  });
  const text = toolTextResult(result);
  if (toolIsError(result) || !text.startsWith("[pin] ")) {
    throw new Error(`pin returned an unexpected receipt: ${text}`);
  }
}

function toolTextResult(
  result: Awaited<ReturnType<Client["callTool"]>>,
): string {
  return (
    (result as ToolTextResult).content?.find((item) => item.type === "text")
      ?.text ?? ""
  );
}

function toolIsError(result: Awaited<ReturnType<Client["callTool"]>>): boolean {
  return Boolean((result as ToolTextResult).isError);
}

function isServerStatusStructuredContent(
  value: unknown,
): value is ServerStatusStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<ServerStatusStructuredContent>;
  return (
    candidate.schema === "aionforge.server_status.v1" &&
    typeof candidate.counts?.memories === "number" &&
    typeof candidate.surface?.tools === "number" &&
    typeof candidate.telemetry?.memory_traffic?.bytes_in_total === "number" &&
    typeof candidate.telemetry.memory_traffic.bytes_out_total === "number" &&
    typeof candidate.telemetry.memory_traffic.estimated_tokens_in_total ===
      "number" &&
    typeof candidate.telemetry.memory_traffic.estimated_tokens_out_total ===
      "number"
  );
}

function isConsolidationStatusStructuredContent(
  value: unknown,
): value is ConsolidationStatusStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<ConsolidationStatusStructuredContent>;
  return (
    candidate.schema === "aionforge.consolidation_status.v1" &&
    typeof candidate.pending === "number" &&
    typeof candidate.failed === "number" &&
    typeof candidate.generation === "number" &&
    typeof candidate.oldest_pending_age_s === "number" &&
    (candidate.state === "idle" ||
      candidate.state === "backlog_pending" ||
      candidate.state === "attention_required")
  );
}

function isSearchResultsStructuredContent(
  value: unknown,
): value is SearchResultsStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<SearchResultsStructuredContent>;
  return (
    candidate.schema === "aionforge.search_results.v1" &&
    typeof candidate.summary?.returned === "number" &&
    typeof candidate.summary?.candidates_considered === "number" &&
    typeof candidate.summary?.embedder_available === "boolean" &&
    typeof candidate.explain?.route === "string" &&
    Array.isArray(candidate.explain?.signals_run) &&
    Array.isArray(candidate.memories)
  );
}

function isAuditHistoryStructuredContent(
  value: unknown,
): value is AuditHistoryStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<AuditHistoryStructuredContent>;
  return (
    candidate.schema === "aionforge.audit_history.v1" &&
    typeof candidate.subject === "string" &&
    typeof candidate.kind === "string" &&
    typeof candidate.count === "number" &&
    Array.isArray(candidate.records) &&
    candidate.records.every(
      (record) =>
        typeof record.id === "string" &&
        typeof record.subject_id === "string" &&
        typeof record.kind === "string" &&
        typeof record.occurred_at === "string" &&
        typeof record.actor === "string" &&
        typeof record.namespace === "string" &&
        typeof record.verification === "string" &&
        (record.payload_preview === null ||
          typeof record.payload_preview === "string"),
    )
  );
}

function isToolManifestStructuredContent(
  value: unknown,
): value is ToolManifestStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<ToolManifestStructuredContent>;
  return (
    candidate.schema === "aionforge.mcp_tools.v1" &&
    typeof candidate.server?.resource_count === "number" &&
    typeof candidate.resources?.tool_manifest === "string" &&
    Array.isArray(candidate.tools) &&
    candidate.tools.every(
      (tool) =>
        typeof tool.name === "string" &&
        (tool.class === "read_like" || tool.class === "mutating") &&
        typeof tool.approval === "string",
    )
  );
}

function textResourceContent(value: unknown): string | undefined {
  if (!value || typeof value !== "object" || !("text" in value)) {
    return undefined;
  }

  const text = (value as { text?: unknown }).text;
  return typeof text === "string" ? text : undefined;
}

export function uniqueSeed(prefix: string): string {
  const time = Date.now().toString(36);
  const nonce = Math.random().toString(36).slice(2, 8);
  return `${prefix} ${time} ${nonce}`;
}

export function titleCase(value: string): string {
  return value
    .replaceAll("_", " ")
    .replace(/\b\w/g, (match) => match.toUpperCase());
}
