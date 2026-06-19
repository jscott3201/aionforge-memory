import { browser } from "$app/environment";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type {
  ReadMemoryStructuredContent,
  SearchResultsStructuredContent,
  ServerStatusStructuredContent,
} from "./contracts";

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
    typeof candidate.auth?.enabled === "boolean"
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

function textResultMessage(result: McpTextResult): string | undefined {
  return result.content.find((item) => item.type === "text")?.text;
}
