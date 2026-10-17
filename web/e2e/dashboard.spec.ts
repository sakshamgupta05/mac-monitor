import { type Page, expect, test } from "@playwright/test";

test.describe.configure({ mode: "serial" });

// A fresh database has no stored slot until the first 15 s boundary passes.
test.beforeAll(async ({ request }) => {
  await expect
    .poll(
      async () => {
        const r = await request.get("/api/summary?keys=power.sys,mem.used&from=0");
        const s = (await r.json()).series;
        return s["mem.used"]?.last != null;
      },
      { timeout: 45_000, intervals: [1000] },
    )
    .toBe(true);
});

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") errors.push(m.text());
  });
  page.on("pageerror", (e) => errors.push(String(e)));
  return errors;
}

test("dashboard renders every section without errors", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/#range=1h");
  await expect(page.locator("#host")).not.toHaveText("mac-monitor");
  await expect(page.locator("#tile-mem .tile-value")).not.toHaveText("–");
  await expect(page.locator("#status")).toContainText("Updated");

  for (const title of ["Power & energy", "CPU", "GPU", "Memory", "Processes", "Thermals", "Disk & network", "Events", "Monitor overhead"]) {
    await expect(page.getByRole("heading", { level: 2, name: title, exact: true })).toBeVisible();
  }
  // every chart card draws a uPlot canvas
  const cards = page.locator(".card .chart");
  const n = await cards.count();
  expect(n).toBeGreaterThan(15);
  await expect(page.locator(".card .chart canvas")).toHaveCount(n);
  await expect(page.locator(".proc-table tbody tr").first()).toBeVisible();
  await expect(page.locator(".events li").first()).toContainText("started");
  expect(errors).toEqual([]);
});

test("range presets update the URL and pressed state", async ({ page }) => {
  await page.goto("/#range=1h");
  await page.getByRole("button", { name: "24h", exact: true }).click();
  await expect(page).toHaveURL(/#range=24h/);
  await expect(page.getByRole("button", { name: "24h", exact: true })).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByRole("button", { name: "1h", exact: true })).toHaveAttribute("aria-pressed", "false");
});

test("live mode streams 1-second rows", async ({ page }) => {
  await page.goto("/#range=live");
  await expect(page.locator("#status")).toHaveText("Live");
  const value = page.locator("#card-power .legend-value").first();
  await expect(value).not.toHaveText("–");
  const before = await page.evaluate(() => document.querySelector("#card-power .uplot") !== null);
  expect(before).toBe(true);
  // rows keep arriving: the tooltip-free legend readout changes within a few seconds
  const first = await page.locator("#card-cpu .legend-value").first().textContent();
  await expect.poll(async () => page.locator("#card-cpu .legend-value").first().textContent(), { timeout: 15_000 }).not.toBe(first);
});

test("drag on a chart zooms to a custom range and can be reset", async ({ page }) => {
  await page.goto("/#range=1h");
  const over = page.locator("#card-power .u-over");
  await expect(over).toBeVisible();
  const box = (await over.boundingBox())!;
  await page.mouse.move(box.x + box.width * 0.5, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.9, box.y + box.height / 2, { steps: 8 });
  await page.mouse.up();
  await expect(page).toHaveURL(/#from=\d+&to=\d+/);
  await expect(page.getByRole("button", { name: "Reset zoom" })).toBeVisible();
  await page.getByRole("button", { name: "Reset zoom" }).click();
  await expect(page).toHaveURL(/#range=1h/);
});

test("hovering a chart shows a tooltip with every series", async ({ page }) => {
  await page.goto("/#range=1h");
  const over = page.locator("#card-power .u-over");
  const box = (await over.boundingBox())!;
  await page.mouse.move(box.x + box.width - 4, box.y + box.height / 2);
  const tip = page.locator("#card-power .tip");
  await expect(tip).toBeVisible();
  await expect(tip.locator(".tip-row")).toHaveCount(4);
  await expect(tip).toContainText("System");
});

test("table view and CSV export", async ({ page }) => {
  await page.goto("/#range=1h");
  const card = page.locator("#card-temp");
  await card.getByRole("button", { name: "Table" }).click();
  await expect(card.locator(".table-view tbody tr").first()).toBeVisible();
  await expect(card.getByRole("button", { name: "Chart" })).toHaveAttribute("aria-pressed", "true");
  const download = page.waitForEvent("download");
  await card.getByRole("button", { name: "CSV" }).click();
  const file = await download;
  expect(file.suggestedFilename()).toBe("mac-monitor-temp.csv");
});

test("electricity price persists and drives the cost", async ({ page }) => {
  await page.goto("/#range=1h");
  const price = page.getByLabel("Electricity price in USD per kWh");
  await price.fill("0.42");
  await price.dispatchEvent("change");
  await expect(page.locator("#tile-energy .tile-sub")).toContainText("$0.42/kWh");
  await page.reload();
  await expect(page.getByLabel("Electricity price in USD per kWh")).toHaveValue("0.42");
});

test("process table sorts by column", async ({ page }) => {
  await page.goto("/#range=1h");
  const header = page.locator(".proc-table th", { hasText: "Mem avg" });
  await header.getByRole("button").click();
  await expect(header).toHaveAttribute("aria-sort", "descending");
  const values = await page.locator(".proc-table tbody tr td:nth-child(5)").allTextContents();
  expect(values.length).toBeGreaterThan(0);
});

test("theme toggle switches and re-renders charts", async ({ page }) => {
  const errors = collectErrors(page);
  await page.goto("/#range=1h");
  const btn = page.locator("#theme");
  await btn.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await btn.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.locator("#card-power canvas")).toHaveCount(1);
  expect(errors).toEqual([]);
});

test("API: query, summary, events and Prometheus export", async ({ request }) => {
  const now = Math.floor(Date.now() / 1000);
  const q = await (await request.get(`/api/query?keys=power.sys,mem.used&from=${now - 600}&to=${now}&points=40&cov=1`)).json();
  expect(q.ts.length).toBeGreaterThan(0);
  expect(q.series["power.sys"].avg).toHaveLength(q.ts.length);
  expect(q.series["power.sys"].cov).toHaveLength(q.ts.length);
  const metrics = await (await request.get("/metrics")).text();
  expect(metrics).toContain("# TYPE mac_monitor_mem_used_bytes gauge");
  const meta = await (await request.get("/api/meta")).json();
  expect(meta.keys).toContain("mem.used");
  const bad = await request.get("/api/query?keys=power.sys&step=-5");
  expect(bad.status()).toBe(400);
  const missing = await request.get("/nope.js");
  expect(missing.status()).toBe(404);
});
