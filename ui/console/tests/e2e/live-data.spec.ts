import { expect, test, type Page } from "@playwright/test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type {
  AuditHistoryStructuredContent,
  ConsolidationStatusStructuredContent,
  ServerStatusStructuredContent,
} from "../../src/lib/api/contracts";

const LIVE_AGENT_ID = "00000000-0000-4000-8000-000000000311";

interface ToolTextResult {
  content?: Array<{ type?: string; text?: string }>;
  isError?: boolean;
}

function collectRuntimeErrors(page: Page): string[] {
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

test.describe("live data flow", () => {
  test.skip(
    process.env.AIONFORGE_CONSOLE_E2E_LIVE !== "1",
    "live MCP data-flow tests run with pnpm e2e:live",
  );

  test("renders real MCP census data after capture", async ({
    page,
    baseURL,
  }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const status = await seedLiveMemory(baseURL);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console");

    await expect(page.getByTestId("live-mcp-state")).toContainText("live");
    await expect(page.getByTestId("live-memory-count")).toContainText(
      status.counts.memories.toString(),
    );
    await expect(page.getByTestId("live-tool-count")).toContainText(
      status.surface.tools.toString(),
    );
    await expect(errors).toEqual([]);
  });

  test("searches and reads real memory records", async ({ page, baseURL }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const seed = uniqueSeed("console records live e2e");
    await captureLiveMemory(baseURL, seed);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/records");
    await page.getByTestId("records-search-input").fill(seed);
    await page
      .getByTestId("records-viewer-input")
      .fill(`agent:${LIVE_AGENT_ID}`);
    await page.getByTestId("records-search-submit").click();

    await expect(page.getByTestId("records-result-count")).toContainText(
      "returned",
    );
    await expect(page.getByTestId("records-result-item").first()).toContainText(
      seed,
    );
    await page.getByTestId("records-result-item").first().click();
    await expect(page.getByTestId("records-detail-body")).toContainText(seed);
    await expect(errors).toEqual([]);
  });

  test("runs retrieval search against real memory", async ({
    page,
    baseURL,
  }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const seed = uniqueSeed("console retrieval live e2e");
    await captureLiveMemory(baseURL, seed);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/retrieval");
    await page.getByTestId("retrieval-search-input").fill(seed);
    await page
      .getByTestId("retrieval-viewer-input")
      .fill(`agent:${LIVE_AGENT_ID}`);
    await page.getByTestId("retrieval-search-submit").click();

    await expect(page.getByTestId("retrieval-result-count")).toContainText(
      "returned",
    );
    await expect(
      page.getByTestId("retrieval-result-item").first(),
    ).toContainText(seed);
    await expect(page.getByTestId("retrieval-route")).not.toContainText(
      "offline",
    );
    await expect(page.getByTestId("retrieval-signals")).toContainText(
      /lexical|trust|recency/i,
    );
    await expect(errors).toEqual([]);
  });

  test("renders real consolidation backlog status", async ({
    page,
    baseURL,
  }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const status = await seedConsolidationStatus(baseURL);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/consolidation");

    await expect(page.getByTestId("consolidation-state")).toContainText(
      status.state.replaceAll("_", " "),
    );
    await expect(page.getByTestId("consolidation-pending")).toContainText(
      status.pending.toString(),
    );
    await expect(page.getByTestId("consolidation-failed")).toContainText(
      status.failed.toString(),
    );
    await expect(page.getByTestId("consolidation-generation")).toContainText(
      status.generation.toString(),
    );
    await expect(page.getByTestId("consolidation-ledger")).toContainText(
      status.state,
    );
    await expect(errors).toEqual([]);
  });

  test("renders real lifecycle audit history", async ({ page, baseURL }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const audit = await seedAuditHistory(baseURL);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/audit");
    await page.getByTestId("audit-kind-input").fill("pin");
    await page.getByTestId("audit-viewer-input").fill(`agent:${LIVE_AGENT_ID}`);
    await page.getByTestId("audit-refresh").click();

    await expect(page.getByTestId("audit-state")).toContainText("records");
    await expect(page.getByTestId("audit-result-count")).toContainText(
      audit.count.toString(),
    );
    await expect(page.getByTestId("audit-summary-kind")).toContainText("pin");
    await expect(page.getByTestId("audit-summary-subject")).toContainText("*");
    await expect(page.getByTestId("audit-record-item").first()).toContainText(
      "pin",
    );
    await expect(
      page.getByTestId("audit-payload-preview").first(),
    ).not.toContainText("unavailable");
    await expect(errors).toEqual([]);
  });
});

async function seedLiveMemory(
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

async function captureLiveMemory(
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

async function seedConsolidationStatus(
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

async function seedAuditHistory(
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
    typeof candidate.surface?.tools === "number"
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

function uniqueSeed(prefix: string): string {
  const time = Date.now().toString(36);
  const nonce = Math.random().toString(36).slice(2, 8);
  return `${prefix} ${time} ${nonce}`;
}
