import { expect, test, type Page } from "@playwright/test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { ServerStatusStructuredContent } from "../../src/lib/api/contracts";

const LIVE_AGENT_ID = "00000000-0000-4000-8000-000000000311";

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
    await client.callTool({
      name: "capture",
      arguments: {
        agent_id: LIVE_AGENT_ID,
        content: `console live e2e seed ${Date.now()}`,
        role: "event",
        trust: 0.8,
        model_family: "console-e2e",
      },
    });

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
