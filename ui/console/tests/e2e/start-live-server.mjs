import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../../../..");
const consoleRoot = path.join(repoRoot, "ui/console");
const port = Number(process.env.AIONFORGE_CONSOLE_E2E_PORT ?? 4183);
const listen = `127.0.0.1:${port}`;
const origin = `http://127.0.0.1:${port}`;
const dataDir =
  process.env.AIONFORGE_CONSOLE_LIVE_DATA_DIR ??
  mkdtempSync(path.join(tmpdir(), "aionforge-console-live-"));

const binary = process.env.AIONFORGE_CONSOLE_E2E_BIN;
const command = binary ?? "cargo";
const args = binary
  ? [
      "--data-dir",
      dataDir,
      "serve",
      "http",
      "--listen",
      listen,
      "--allowed-host",
      listen,
      "--allowed-origin",
      origin,
    ]
  : [
      "run",
      "-p",
      "aionforge-cli",
      "--",
      "--data-dir",
      dataDir,
      "serve",
      "http",
      "--listen",
      listen,
      "--allowed-host",
      listen,
      "--allowed-origin",
      origin,
    ];

const child = spawn(command, args, {
  cwd: repoRoot,
  env: {
    ...process.env,
    AIONFORGE_CONSOLE_DIST_DIR: path.join(consoleRoot, "build"),
    AIONFORGE_EMBEDDER__ENABLED: "false",
    AIONFORGE_TRAFFIC_HEARTBEAT_SECS: "0",
    RUST_LOG: process.env.RUST_LOG ?? "warn",
  },
  stdio: "inherit",
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    child.kill(signal);
  });
}

child.on("exit", (code, signal) => {
  if (signal) {
    process.exit(0);
  }
  process.exit(code ?? 1);
});
