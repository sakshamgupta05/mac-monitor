import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { defineConfig } from "@playwright/test";

// Runs a real mac-monitor (release build by default) on a scratch database.
const port = Number(process.env.E2E_PORT ?? 9399);
const bin = process.env.MAC_MONITOR_BIN ?? "../target/release/mac-monitor";
const dataDir = process.env.E2E_DATA_DIR ?? mkdtempSync(join(tmpdir(), "mac-monitor-e2e-"));

export default defineConfig({
  testDir: "e2e",
  timeout: 90_000,
  workers: 1,
  reporter: [["list"]],
  use: { baseURL: `http://127.0.0.1:${port}`, viewport: { width: 1440, height: 900 }, trace: "retain-on-failure" },
  webServer: {
    command: `${bin} --no-config --listen 127.0.0.1:${port} --data-dir ${dataDir}`,
    url: `http://127.0.0.1:${port}/api/health`,
    reuseExistingServer: false,
    timeout: 30_000,
  },
});
