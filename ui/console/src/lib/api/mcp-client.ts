import { browser } from "$app/environment";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type {
  AuditCursor,
  AuditHistoryStructuredContent,
  ConsolidationStatusStructuredContent,
  McpResourceDescriptor,
  ReadMemoryStructuredContent,
  SearchResultsStructuredContent,
  ServerStatusStructuredContent,
  ToolManifestStructuredContent,
} from "./contracts";

export const TOOL_MANIFEST_URI = "aionforge://manifest/tools.json";

export interface McpClientConfig {
  endpoint: string;
  bearerToken?: string;
}

interface ConsoleRuntimeConfig {
  mcpEndpoint?: string;
}

declare global {
  interface Window {
    __AIONFORGE_CONSOLE_RUNTIME__?: ConsoleRuntimeConfig;
  }
}

export interface McpCallRequest<
  TParams extends Record<string, unknown> = Record<string, unknown>,
> {
  tool: string;
  params: TParams;
}

export interface McpTextResult<TStructured = unknown> {
  content: Array<{ type: "text"; text: string }>;
  structuredContent?: TStructured;
  isError?: boolean;
}

export interface McpToolCatalog {
  manifest: ToolManifestStructuredContent;
  resources: McpResourceDescriptor[];
}

export interface SearchMemoriesRequest {
  query: string;
  viewer?: string;
  teams?: string[];
  limit?: number;
  verbose?: boolean;
  includeSuperseded?: boolean;
}

export interface ReadMemoryRequest {
  memoryIds: string[];
  viewer?: string;
  teams?: string[];
  verbose?: boolean;
  full?: boolean;
}

export interface AuditHistoryRequest {
  subjectId?: string;
  kind?: string;
  viewer?: string;
  teams?: string[];
  after?: AuditCursor;
  limit?: number;
  verbose?: boolean;
}

export class McpClientError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "McpClientError";
  }
}

export function defaultMcpEndpoint(basePath = ""): string {
  return `${basePath}/mcp`.replace(/\/{2,}/g, "/");
}

export function createMcpClientConfig(endpoint = "/mcp"): McpClientConfig {
  return { endpoint };
}

export function createRuntimeMcpClientConfig(): McpClientConfig | null {
  if (!browser) {
    return null;
  }

  const endpoint = window.__AIONFORGE_CONSOLE_RUNTIME__?.mcpEndpoint;
  return endpoint ? createMcpClientConfig(endpoint) : null;
}

export async function callMcpTool<TStructured = unknown>(
  config: McpClientConfig,
  request: McpCallRequest,
): Promise<McpTextResult<TStructured>> {
  if (!browser) {
    throw new McpClientError("MCP calls require a browser runtime.");
  }

  const client = new Client({
    name: "aionforge-console",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(
    new URL(config.endpoint, window.location.origin),
    {
      requestInit: config.bearerToken
        ? { headers: { Authorization: `Bearer ${config.bearerToken}` } }
        : undefined,
    },
  );

  try {
    await client.connect(transport);
    return (await client.callTool({
      name: request.tool,
      arguments: request.params,
    })) as McpTextResult<TStructured>;
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function loadMcpToolCatalog(
  config = createMcpClientConfig(),
): Promise<McpToolCatalog> {
  if (!browser) {
    throw new McpClientError("MCP resource reads require a browser runtime.");
  }

  const client = new Client({
    name: "aionforge-console",
    version: "0.0.0",
  });
  const transport = new StreamableHTTPClientTransport(
    new URL(config.endpoint, window.location.origin),
    {
      requestInit: config.bearerToken
        ? { headers: { Authorization: `Bearer ${config.bearerToken}` } }
        : undefined,
    },
  );

  try {
    await client.connect(transport);
    const listed = await client.listResources();
    const resource = await client.readResource({ uri: TOOL_MANIFEST_URI });
    const text = resource.contents
      .map(textResourceContent)
      .find((content) => content !== undefined);

    if (!text) {
      throw new McpClientError("tool manifest resource was not text.");
    }

    const parsed = JSON.parse(text) as unknown;
    if (!isToolManifestStructuredContent(parsed)) {
      throw new McpClientError("tool manifest returned an unexpected payload.");
    }

    return {
      manifest: parsed,
      resources: listed.resources
        .map((resource) => ({
          uri: resource.uri,
          name: resource.name,
          title: resource.title,
          description: resource.description,
          mimeType: resource.mimeType,
          size: resource.size,
        }))
        .sort((left, right) => left.uri.localeCompare(right.uri)),
    };
  } finally {
    await client.close().catch(() => undefined);
  }
}

export async function loadServerStatus(
  config = createMcpClientConfig(),
): Promise<ServerStatusStructuredContent> {
  const result = await callMcpTool<ServerStatusStructuredContent>(config, {
    tool: "server_status",
    params: { verbose: true },
  });

  if (result.isError) {
    throw new McpClientError(
      textResultMessage(result) ?? "server_status failed",
    );
  }

  if (!isServerStatusStructuredContent(result.structuredContent)) {
    throw new McpClientError("server_status returned an unexpected payload.");
  }

  return result.structuredContent;
}

export async function loadConsolidationStatus(
  config = createMcpClientConfig(),
): Promise<ConsolidationStatusStructuredContent> {
  const result = await callMcpTool<ConsolidationStatusStructuredContent>(
    config,
    {
      tool: "consolidation_status",
      params: { verbose: true },
    },
  );

  if (result.isError) {
    throw new McpClientError(
      textResultMessage(result) ?? "consolidation_status failed",
    );
  }

  if (!isConsolidationStatusStructuredContent(result.structuredContent)) {
    throw new McpClientError(
      "consolidation_status returned an unexpected payload.",
    );
  }

  return result.structuredContent;
}

export async function loadAuditHistory(
  config: McpClientConfig,
  request: AuditHistoryRequest,
): Promise<AuditHistoryStructuredContent> {
  const subjectId = request.subjectId?.trim() || undefined;
  const kind = request.kind?.trim() || undefined;
  const result = await callMcpTool<AuditHistoryStructuredContent>(config, {
    tool: "audit_history",
    params: {
      subject_id: subjectId,
      kind,
      viewer: request.viewer?.trim() || undefined,
      teams: request.teams ?? [],
      after: request.after,
      limit: request.limit ?? 12,
      verbose: request.verbose ?? true,
    },
  });

  if (result.isError) {
    throw new McpClientError(
      textResultMessage(result) ?? "audit_history failed",
    );
  }

  if (!isAuditHistoryStructuredContent(result.structuredContent)) {
    throw new McpClientError("audit_history returned an unexpected payload.");
  }

  return result.structuredContent;
}

export async function searchMemories(
  config: McpClientConfig,
  request: SearchMemoriesRequest,
): Promise<SearchResultsStructuredContent> {
  const result = await callMcpTool<SearchResultsStructuredContent>(config, {
    tool: "search",
    params: {
      query: request.query,
      viewer: request.viewer,
      teams: request.teams ?? [],
      limit: request.limit ?? 8,
      verbose: request.verbose ?? true,
      include_superseded: request.includeSuperseded ?? false,
    },
  });

  if (result.isError) {
    throw new McpClientError(textResultMessage(result) ?? "search failed");
  }

  if (!isSearchResultsStructuredContent(result.structuredContent)) {
    throw new McpClientError("search returned an unexpected payload.");
  }

  return result.structuredContent;
}

export async function readMemory(
  config: McpClientConfig,
  request: ReadMemoryRequest,
): Promise<ReadMemoryStructuredContent> {
  const result = await callMcpTool<ReadMemoryStructuredContent>(config, {
    tool: "read_memory",
    params: {
      memory_ids: request.memoryIds,
      viewer: request.viewer,
      teams: request.teams ?? [],
      verbose: request.verbose ?? true,
      full: request.full ?? false,
    },
  });

  if (result.isError) {
    throw new McpClientError(textResultMessage(result) ?? "read_memory failed");
  }

  if (!isReadMemoryStructuredContent(result.structuredContent)) {
    throw new McpClientError("read_memory returned an unexpected payload.");
  }

  return result.structuredContent;
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
    typeof candidate.version === "string" &&
    typeof candidate.counts?.memories === "number" &&
    typeof candidate.counts?.work_items === "number" &&
    typeof candidate.surface?.tools === "number" &&
    Array.isArray(candidate.transports) &&
    typeof candidate.auth?.enabled === "boolean" &&
    typeof candidate.telemetry?.memory_traffic?.bytes_in_total === "number" &&
    typeof candidate.telemetry.memory_traffic.bytes_out_total === "number" &&
    typeof candidate.telemetry.memory_traffic.estimated_tokens_in_total ===
      "number" &&
    typeof candidate.telemetry.memory_traffic.estimated_tokens_out_total ===
      "number" &&
    candidate.telemetry.memory_traffic.token_estimate_kind ===
      "coarse_bytes_divisor"
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
    typeof candidate.oldest_pending_age_s === "number" &&
    typeof candidate.generation === "number" &&
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
    Array.isArray(candidate.memories)
  );
}

function isReadMemoryStructuredContent(
  value: unknown,
): value is ReadMemoryStructuredContent {
  if (!value || typeof value !== "object") {
    return false;
  }

  const candidate = value as Partial<ReadMemoryStructuredContent>;
  return (
    candidate.schema === "aionforge.read_memory.v1" &&
    typeof candidate.requested === "number" &&
    typeof candidate.found === "number" &&
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
    (candidate.next === null ||
      (typeof candidate.next?.occurred_at === "string" &&
        typeof candidate.next?.id === "string")) &&
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
    typeof candidate.server?.name === "string" &&
    typeof candidate.server?.version === "string" &&
    Array.isArray(candidate.server?.transports) &&
    typeof candidate.server?.resource_count === "number" &&
    typeof candidate.policy?.read_like_approval === "string" &&
    typeof candidate.policy?.mutating_approval === "string" &&
    !!candidate.resources &&
    Object.values(candidate.resources).every(
      (uri) => typeof uri === "string",
    ) &&
    Array.isArray(candidate.tools) &&
    candidate.tools.every(
      (tool) =>
        typeof tool.name === "string" &&
        (tool.class === "read_like" || tool.class === "mutating") &&
        typeof tool.approval === "string" &&
        typeof tool.mutates === "boolean" &&
        typeof tool.read_only_hint === "boolean" &&
        typeof tool.destructive_hint === "boolean" &&
        typeof tool.idempotent_hint === "boolean" &&
        typeof tool.open_world_hint === "boolean" &&
        typeof tool.default_output === "string" &&
        (tool.schema === undefined || typeof tool.schema === "string") &&
        typeof tool.verbose === "boolean" &&
        Array.isArray(tool.errors) &&
        tool.errors.every((error) => typeof error === "string"),
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

function textResultMessage(result: McpTextResult): string | undefined {
  return result.content.find((item) => item.type === "text")?.text;
}
