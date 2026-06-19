import { expect, test } from "@playwright/test";
import {
  captureLiveMemory,
  collectRuntimeErrors,
  LIVE_AGENT_ID,
  loadLiveServerStatus,
  readToolManifest,
  searchLiveMemory,
  seedAuditHistory,
  seedConsolidationStatus,
  seedLiveMemory,
  titleCase,
  uniqueSeed,
} from "./live-helpers";

const countFormat = new Intl.NumberFormat("en-US");

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
    await expect(page.getByTestId("dashboard-traffic-bytes")).toContainText(
      formatBytes(status.telemetry.memory_traffic.bytes_in_total),
    );
    await expect(page.getByTestId("dashboard-traffic-bytes")).toContainText(
      formatBytes(status.telemetry.memory_traffic.bytes_out_total),
    );
    await expect(page.getByTestId("dashboard-traffic-tokens")).toContainText(
      countFormat.format(
        status.telemetry.memory_traffic.estimated_tokens_in_total,
      ),
    );
    await expect(errors).toEqual([]);
  });

  test("renders real MCP tool manifest resources", async ({
    page,
    baseURL,
  }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const seed = uniqueSeed("console mcp telemetry live e2e");
    await captureLiveMemory(baseURL, seed);
    await searchLiveMemory(baseURL, seed);
    const [manifest, status] = await Promise.all([
      readToolManifest(baseURL),
      loadLiveServerStatus(baseURL),
    ]);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/mcp");

    await expect(page.getByTestId("mcp-state")).toContainText("live");
    await expect(page.getByTestId("mcp-tool-count")).toContainText(
      manifest.tools.length.toString(),
    );
    await expect(page.getByTestId("mcp-resource-count")).toContainText(
      manifest.server.resource_count.toString(),
    );
    await expect(page.getByTestId("mcp-read-like-count")).toContainText(
      manifest.tools
        .filter((tool) => tool.class === "read_like")
        .length.toString(),
    );
    await expect(page.getByTestId("mcp-mutating-count")).toContainText(
      manifest.tools
        .filter((tool) => tool.class === "mutating")
        .length.toString(),
    );
    await expect(
      page.getByTestId("mcp-tool-row").filter({ hasText: "server_status" }),
    ).toContainText("aionforge.server_status.v1");
    await expect(
      page.getByTestId("mcp-tool-row").filter({ hasText: /^capture\b/ }),
    ).toContainText("ask_user");
    await expect(page.getByTestId("mcp-resource-list")).toContainText(
      manifest.resources.tool_manifest,
    );
    await expect(page.getByTestId("mcp-traffic-bytes")).toContainText(
      formatBytes(status.telemetry.memory_traffic.bytes_in_total),
    );
    await expect(page.getByTestId("mcp-traffic-bytes")).toContainText(
      formatBytes(status.telemetry.memory_traffic.bytes_out_total),
    );
    await expect(page.getByTestId("mcp-traffic-tokens")).toContainText(
      countFormat.format(
        status.telemetry.memory_traffic.estimated_tokens_in_total,
      ),
    );
    await expect(page.getByTestId("mcp-traffic-tokens")).toContainText(
      countFormat.format(
        status.telemetry.memory_traffic.estimated_tokens_out_total,
      ),
    );
    await expect(page.getByTestId("mcp-telemetry-rollup")).toContainText(
      "coarse bytes/4 estimate",
    );
    await expect(page.getByTestId("mcp-telemetry-followup")).toContainText(
      "queryable counters are next",
    );
    await expect(errors).toEqual([]);
  });

  test("renders real security approval posture", async ({ page, baseURL }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const [status, manifest] = await Promise.all([
      loadLiveServerStatus(baseURL),
      readToolManifest(baseURL),
    ]);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/security");

    await expect(page.getByTestId("security-state")).toContainText("live");
    await expect(page.getByTestId("security-auth-state")).toContainText(
      status.auth.enabled ? "enabled" : "default off",
    );
    await expect(page.getByTestId("security-issuer-count")).toContainText(
      status.auth.issuers.length === 0
        ? "none"
        : status.auth.issuers.length.toString(),
    );
    await expect(page.getByTestId("security-read-policy")).toContainText(
      manifest.policy.read_like_approval,
    );
    await expect(page.getByTestId("security-mutation-policy")).toContainText(
      manifest.policy.mutating_approval,
    );
    await expect(page.getByTestId("security-mutating-count")).toContainText(
      manifest.tools.filter((tool) => tool.mutates).length.toString(),
    );
    await expect(
      page.getByTestId("security-tool-row").filter({ hasText: /^capture\b/ }),
    ).toContainText(manifest.policy.mutating_approval);
    await expect(
      page.getByTestId("security-tool-row").filter({ hasText: /^forget\b/ }),
    ).toContainText("destructive");
    await expect(page.getByTestId("security-config-list")).toContainText(
      "not exposed",
    );
    await expect(page.getByTestId("security-issuer-list")).toContainText(
      status.auth.issuers[0] ?? "not configured",
    );
    await expect(errors).toEqual([]);
  });

  test("renders real namespace aggregate counts", async ({ page, baseURL }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const status = await seedLiveMemory(baseURL);
    const firstKind = Object.entries(status.counts.kinds).sort(
      ([left], [right]) => left.localeCompare(right),
    )[0];
    const firstWorkStatus = Object.entries(status.counts.work_statuses).sort(
      ([left], [right]) => left.localeCompare(right),
    )[0];
    if (!firstKind) {
      throw new Error("server_status did not return memory kind counts.");
    }
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/namespaces");

    await expect(page.getByTestId("namespaces-state")).toContainText("live");
    await expect(page.getByTestId("namespaces-memory-count")).toContainText(
      status.counts.memories.toString(),
    );
    await expect(page.getByTestId("namespaces-work-count")).toContainText(
      status.counts.work_items.toString(),
    );
    await expect(page.getByTestId("namespaces-kind-count")).toContainText(
      Object.keys(status.counts.kinds).length.toString(),
    );
    await expect(page.getByTestId("namespaces-kind-census")).toContainText(
      titleCase(firstKind[0]),
    );
    await expect(
      page
        .getByTestId("namespaces-kind-row")
        .filter({ hasText: titleCase(firstKind[0]) }),
    ).toContainText(firstKind[1].toString());
    if (firstWorkStatus) {
      await expect(page.getByTestId("namespaces-work-census")).toContainText(
        titleCase(firstWorkStatus[0]),
      );
    }
    await expect(page.getByTestId("namespaces-gap-list")).toContainText(
      "not exposed",
    );
    await expect(errors).toEqual([]);
  });

  test("renders real embedding search posture", async ({ page, baseURL }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const seed = uniqueSeed("console embedding live e2e");
    await captureLiveMemory(baseURL, seed);
    const probe = await searchLiveMemory(baseURL, seed);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console/embedding");
    await page.getByTestId("embedding-query-input").fill(seed);
    await page
      .getByTestId("embedding-viewer-input")
      .fill(`agent:${LIVE_AGENT_ID}`);
    await expect(page.getByTestId("embedding-refresh")).toBeEnabled();
    await page.getByTestId("embedding-refresh").click();

    await expect(page.getByTestId("embedding-state")).toContainText("live");
    await expect(page.getByTestId("embedding-available")).toContainText(
      probe.summary.embedder_available ? "available" : "disabled",
    );
    await expect(page.getByTestId("embedding-considered")).toContainText(
      probe.summary.candidates_considered.toString(),
    );
    await expect(page.getByTestId("embedding-returned")).toContainText(
      probe.summary.returned.toString(),
    );
    await expect(page.getByTestId("embedding-route")).toContainText(
      probe.explain.route,
    );
    await expect(page.getByTestId("embedding-config-list")).toContainText(
      "not exposed",
    );
    await expect(
      page.getByTestId("embedding-result-item").first(),
    ).toContainText(seed);
    if (probe.explain.signals_run[0]) {
      await expect(page.getByTestId("embedding-signals")).toContainText(
        probe.explain.signals_run[0],
      );
    }
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

  test("hands global search to the live records workflow", async ({
    page,
    baseURL,
  }) => {
    if (!baseURL) {
      throw new Error(
        "Playwright baseURL is required for live data-flow tests.",
      );
    }

    const seed = uniqueSeed("console global search live e2e");
    await captureLiveMemory(baseURL, seed);
    const errors = collectRuntimeErrors(page);

    await page.goto("/console");
    await expect(page.getByTestId("live-mcp-state")).toContainText("live");
    await page.getByTestId("global-search-input").fill(seed);
    await page.getByTestId("global-search-input").press("Enter");

    await expect(page).toHaveURL(/\/console\/records\?q=/);
    await expect(page.getByTestId("records-search-input")).toHaveValue(seed);
    await expect(page.getByTestId("records-result-count")).toContainText(
      "returned",
    );
    await expect(page.getByTestId("records-result-item").first()).toContainText(
      seed,
    );
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

function formatBytes(value: number): string {
  if (value < 1024) {
    return `${countFormat.format(value)} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let scaled = value / 1024;
  let unitIndex = 0;

  while (scaled >= 1024 && unitIndex < units.length - 1) {
    scaled /= 1024;
    unitIndex += 1;
  }

  return `${scaled.toFixed(scaled >= 10 ? 0 : 1)} ${units[unitIndex]}`;
}
