export interface McpClientConfig {
  endpoint: string;
  bearerToken?: string;
}

export interface McpCallRequest<
  TParams extends Record<string, unknown> = Record<string, unknown>,
> {
  tool: string;
  params: TParams;
}

export interface McpTextResult {
  content: Array<{ type: "text"; text: string }>;
  structuredContent?: unknown;
  isError?: boolean;
}

export function defaultMcpEndpoint(basePath = ""): string {
  return `${basePath}/mcp`.replace(/\/{2,}/g, "/");
}

export function createMcpClientConfig(endpoint = "/mcp"): McpClientConfig {
  return { endpoint };
}
