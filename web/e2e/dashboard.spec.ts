import { type Page, expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";

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
  const cards = page.locator("#historical .card .chart");
  const n = await cards.count();
  expect(n).toBeGreaterThan(15);
  await expect(page.locator("#historical .card .chart canvas")).toHaveCount(n);
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

test("a range change during a pending refresh loads the newest range", async ({ page }) => {
  let release!: () => void;
  const pending = new Promise<void>((resolve) => { release = resolve; });
  let blocked = true;
  const spans: number[] = [];
  await page.route("**/api/query?**", async (route) => {
    const url = new URL(route.request().url());
    spans.push(Number(url.searchParams.get("to")) - Number(url.searchParams.get("from")));
    if (blocked) await pending;
    await route.continue();
  });
  await page.goto("/#range=1h");
  await page.getByRole("button", { name: "24h", exact: true }).click();
  await page.getByRole("button", { name: "7d", exact: true }).click();
  blocked = false;
  release();
  await expect.poll(() => spans.includes(7 * 86400)).toBe(true);
  await expect(page.locator("#status")).toContainText("Updated");
  await expect(page).toHaveURL(/#range=7d/);
});

test("live is opt-in and independent from historical ranges", async ({ page }) => {
  const errors = collectErrors(page);
  let liveRequests = 0;
  page.on("request", (r) => { if (r.url().endsWith("/api/live")) liveRequests++; });
  await page.addInitScript(() => {
    const NativeSource = window.EventSource;
    (window as any).liveEvents = { rows: [], groups: [], closed: 0 };
    window.EventSource = class extends NativeSource {
      constructor(url: string | URL, options?: EventSourceInit) {
        super(url, options);
        for (const kind of ["row", "groups"]) this.addEventListener(kind, (e) => {
          (window as any).liveEvents[kind === "row" ? "rows" : "groups"].push(JSON.parse((e as MessageEvent).data));
        });
      }
      close() { (window as any).liveEvents.closed++; super.close(); }
    } as typeof EventSource;
  });
  // Old Live bookmarks must not enable accelerated sampling either.
  await page.goto("/#range=live");
  await expect(page.locator("#live-toggle")).not.toBeChecked();
  await expect(page.locator("#live-status")).toContainText("Paused");
  await expect(page.locator("#status")).toContainText("Updated");
  expect(liveRequests).toBe(0);
  await page.locator("#live-toggle").check();
  await expect(page.locator("#live-status")).toContainText("processes ~5s");
  await expect(page.locator("#live-status")).toContainText("system 2s");
  await expect(page.locator("#live-value-mem")).not.toHaveText("–");
  await expect(page.locator("#live-section .proc-table tbody tr").first()).toBeVisible();
  await expect.poll(() => page.evaluate(() => (window as any).liveEvents.groups.length), { timeout: 20_000 }).toBeGreaterThanOrEqual(2);
  const events = await page.evaluate(() => (window as any).liveEvents);
  expect(events.rows.length).toBeGreaterThan(2);
  const tail = events.rows.slice(-3);
  expect(tail[2].t - tail[1].t).toBeLessThan(2800);
  expect(tail[2].t - tail[1].t).toBeGreaterThanOrEqual(1800);
  const scans = events.groups;
  expect(scans[scans.length - 1].ts - scans[scans.length - 2].ts).toBeLessThanOrEqual(7);
  expect(scans[scans.length - 1].groups.some((g: any) => g.mem > 0)).toBe(true);
  await page.screenshot({ path: "test-results/live-desktop.png" });
  await page.getByRole("button", { name: "24h", exact: true }).click();
  await expect(page).toHaveURL(/#range=24h/);
  await expect(page.locator("#live-toggle")).toBeChecked();
  expect(liveRequests).toBe(1);
  await page.getByLabel("Live chart window").selectOption("300");
  await expect(page.locator("#live-section .chart canvas")).toHaveCount(6);
  await page.locator("#live-toggle").uncheck();
  await expect(page.locator("#live-status")).toContainText("Paused");
  expect(await page.evaluate(() => (window as any).liveEvents.closed)).toBe(1);
  await expect(page.locator("#live-value-mem")).not.toHaveText("–");
  await expect(page.locator("#card-proc-table th", { hasText: "CPU avg" })).toBeVisible();
  await page.reload();
  await expect(page.locator("#live-toggle")).not.toBeChecked();
  expect(liveRequests).toBe(1);
  expect(errors).toEqual([]);
});

test("live snapshots replace exited groups; sorting, filters, pause and visibility work", async ({ page }) => {
  await page.addInitScript(() => {
    class FakeSource extends EventTarget {
      constructor() { super(); (window as any).fakeSource = this; }
      close() { (window as any).fakeClosedCount = ((window as any).fakeClosedCount ?? 0) + 1; }
    }
    (window as any).EventSource = FakeSource;
  });
  await page.goto("/");
  await page.locator("#live-toggle").check();
  const emit = (type: string, data: unknown) => page.evaluate(({ type, data }) => {
    (window as any).fakeSource.dispatchEvent(new MessageEvent(type, { data: JSON.stringify(data) }));
  }, { type, data });
  const app = (name: string, cpu: number, mem: number) => ({ name, cpu, mem, gpu: 0, power: 1, disk_read: 1024, disk_write: 0, count: 1 });
  await emit("open", {});
  await emit("init", { keys: ["mem.used", "cpu.active"], rows: [[Date.now(), [1024 ** 3, .25]]], sample_ms: 2000, process_sample_secs: 5, groups_ts: 100, groups: [app("First", .5, 200), app("Second", 1, 100)] });
  const table = page.locator("#card-live-procs");
  await expect(table.locator("tbody tr").first()).toContainText("Second");
  await expect(table.locator("tbody tr").first().locator("td:nth-child(2) .proc-data-bar")).toHaveCSS("--share", "1");
  await expect(table.locator("tbody tr").first().locator("td:nth-child(4) .proc-data-bar")).toHaveCount(0);
  await expect(table.locator("tbody tr").first().locator("td:last-child .proc-data-bar")).toHaveCount(0);
  await table.getByRole("button", { name: "Memory now" }).click();
  await expect(table.locator("tbody tr").first()).toContainText("First");
  await page.getByLabel("Filter live process groups").fill("second");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table.locator("tbody tr td:nth-child(3) .proc-data-bar")).toHaveCSS("--share", "0.5");
  await emit("groups", { ts: 105, groups: [app("First", .1, 200)] });
  await expect(table).toContainText("No groups match");
  await page.getByLabel("Filter live process groups").fill("");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await emit("error", {});
  await expect(page.locator("#live-status")).toContainText("reconnecting");
  await expect(table).toContainText("Disconnected");
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  expect(await page.evaluate(() => (window as any).fakeClosedCount)).toBe(1);
  await expect(page.locator("#live-status")).toContainText("hidden");
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await emit("open", {});
  await emit("init", { keys: [], rows: [], sample_ms: 2000, process_sample_secs: 5, groups_ts: 110, groups: [app("New", .2, 300)] });
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await expect(table.locator("tbody tr")).toContainText("New");
  await page.locator("#live-toggle").uncheck();
  await expect(table).toContainText("Paused");
});

test("live area charts retain CPU/GPU scales and allow RAM plus swap beyond physical RAM", async ({ page }) => {
  await page.addInitScript(() => {
    (window as any).EventSource = class extends EventTarget {
      constructor() { super(); (window as any).chartSource = this; }
      close() {}
    };
  });
  await page.goto("/");
  await page.locator("#live-toggle").check();
  await page.evaluate(() => {
    const source = (window as any).chartSource;
    const now = Date.now();
    source.dispatchEvent(new Event("open"));
    source.dispatchEvent(new MessageEvent("init", { data: JSON.stringify({
      keys: ["cpu.active", "gpu.active", "mem.used", "mem.total", "power.sys", "disk.read", "disk.write", "net.rx", "net.tx", "swap.used"],
      sample_ms: 2000, process_sample_secs: 5, groups: [], groups_ts: null,
      rows: Array.from({ length: 61 }, (_, i) => [now - (60 - i) * 2000,
        [.25 + Math.sin(i / 8) * .10, .125, 24 * 1024 ** 3, 32 * 1024 ** 3, 25 + Math.sin(i / 6) * 4, 1024 ** 2, 512 * 1024, 256 * 1024, 128 * 1024, 8 * 1024 ** 3]]),
    }) }));
  });
  const cpu = page.locator("#live-chart-cpu");
  const gpu = page.locator("#live-chart-gpu");
  const mem = page.locator("#live-chart-mem");
  await expect(cpu).toHaveAttribute("data-scale-min", "0");
  await expect(cpu).toHaveAttribute("data-scale-max", "1");
  await expect(gpu).toHaveAttribute("data-scale-max", "1");
  await expect(mem).toHaveAttribute("data-reference-value", String(32 * 1024 ** 3));
  expect(Number(await mem.getAttribute("data-scale-max"))).toBeGreaterThan(32 * 1024 ** 3);
  await expect(cpu).toHaveAttribute("aria-label", /100%.*capacity/);
  await expect(mem).toHaveAttribute("aria-label", /RAM \+ swap.*32 GB RAM reference/);
  await expect(page.locator("#live-value-mem")).toHaveText("32.0 GB");
  await expect(page.locator("#live-sub-mem")).toContainText("RAM 24.0 GB · Swap 8.00 GB");
  const memoryOver = page.locator("#live-chart-mem .u-over");
  const memoryBox = (await memoryOver.boundingBox())!;
  await page.mouse.move(memoryBox.x + memoryBox.width / 2, memoryBox.y + memoryBox.height / 2);
  await expect(page.locator("#live-chart-mem .tip")).toContainText("Physical RAM: 32.0 GB");
  await expect(page.locator("#live-chart-power")).toHaveAttribute("aria-label", /automatically scaled/);
  const initialOver = page.locator("#live-chart-disk .u-over");
  const initialBox = (await initialOver.boundingBox())!;
  await page.mouse.move(initialBox.x + initialBox.width / 2, initialBox.y + initialBox.height / 2);
  await expect(page.locator("#live-chart-disk .tip")).toContainText("512 KB/s");
  await expect(page.locator("#live-chart-disk .tip")).not.toContainText("-512");
  await page.mouse.move(0, 0);
  await page.screenshot({ path: "test-results/live-area-scales.png" });
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("aria-label", /Read · 6s average/);
  await expect(page.locator("#live-chart-net")).toHaveAttribute("aria-label", /Received · 6s average/);
  await page.evaluate(() => {
    const now = Date.now();
    (window as any).chartSource.dispatchEvent(new MessageEvent("init", { data: JSON.stringify({
      keys: ["cpu.active", "gpu.active", "mem.used", "mem.total", "power.sys", "disk.read", "disk.write", "net.rx", "net.tx", "swap.used"],
      sample_ms: 2000, process_sample_secs: 5, groups: [], groups_ts: null,
      rows: Array.from({ length: 61 }, (_, i) => [now - (60 - i) * 2000,
        [.25, .125, 24 * 1024 ** 3, 32 * 1024 ** 3, 25, i === 59 ? 90 * 1024 ** 2 : 0, 0, 0, 0, 8 * 1024 ** 3]]),
    }) }));
  });
  // A 90 MB/s spike becomes a 30 MB/s trend over three 2-second samples.
  // The prominent number remains the unsmoothed latest rate (zero).
  await expect(page.locator("#live-rate-disk-0")).toHaveText("0 B/s");
  await expect(page.locator("#live-rate-disk-1")).toHaveText("0 B/s");
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("aria-label", /Write · 6s average/);
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("data-direction-layout", "mirrored");
  const mirrorMax = Number(await page.locator("#live-chart-disk").getAttribute("data-scale-max"));
  expect(Number(await page.locator("#live-chart-disk").getAttribute("data-scale-min"))).toBe(-mirrorMax);
  await expect.poll(async () => Number(await page.locator("#live-chart-disk").getAttribute("data-scale-max"))).toBeLessThan(90 * 1024 ** 2);
  expect(Number(await page.locator("#live-chart-disk").getAttribute("data-scale-max"))).toBeGreaterThanOrEqual(30 * 1024 ** 2);
  const over = page.locator("#live-chart-disk .u-over");
  const box = (await over.boundingBox())!;
  await page.mouse.move(box.x + box.width - 2, box.y + box.height / 2);
  await expect(page.locator("#live-chart-disk .tip")).toContainText("30.0 MB/s");
  await expect(page.locator("#live-chart-disk .tip .tip-row")).toHaveCount(2);
  await expect(page.locator("#live-chart-disk .tip")).not.toContainText("-30");
  await page.getByLabel("Disk and network chart layout").selectOption("overlay");
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("data-direction-layout", "overlay");
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("data-scale-min", "0");
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("data-scale-max", String(mirrorMax));
  await page.screenshot({ path: "test-results/live-overlay.png" });
  await page.getByLabel("Disk and network chart layout").selectOption("mirrored");
  await expect(page.locator("#live-chart-disk")).toHaveAttribute("data-direction-layout", "mirrored");

  await page.evaluate(() => {
    (window as any).chartSource.dispatchEvent(new MessageEvent("row", { data: JSON.stringify({
      t: Date.now() + 1000, v: [.95, .9, 31 * 1024 ** 3, 32 * 1024 ** 3, 15, 0, 0, 0, 0, 6 * 1024 ** 3],
    }) }));
  });
  await expect(page.locator("#live-value-cpu")).toHaveText("95%");
  await expect(page.locator("#live-value-mem")).toHaveText("37.0 GB");
  await expect(page.locator("#live-sub-mem")).toContainText("RAM 31.0 GB · Swap 6.00 GB");
  expect(Number(await mem.getAttribute("data-scale-max"))).toBeGreaterThan(37 * 1024 ** 3);
  await page.mouse.move(0, 0);
  await page.screenshot({ path: "test-results/memory-over-physical.png" });
  await expect(cpu).toHaveAttribute("data-scale-max", "1");
  await expect(mem).toHaveAttribute("data-reference-value", String(32 * 1024 ** 3));
  expect(Number(await mem.getAttribute("data-scale-max"))).toBeGreaterThan(32 * 1024 ** 3);
  await page.locator("#theme").click();
  await expect(cpu).toHaveAttribute("data-scale-max", "1");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: "test-results/live-area-scales-mobile.png" });
  await page.locator("#live-toggle").uncheck();
});

test("disk selectors default to All, isolate devices and retain disconnected selections", async ({ page }) => {
  const errors = collectErrors(page);
  const disks = [
    { id: "internal", name: "Internal SSD", bsd: "disk0", kind: "Internal", connected: true },
    { id: "external", name: "Samsung T7", bsd: "disk6", kind: "External", connected: true },
  ];
  const queries: string[] = [];
  await page.route("**/api/meta", async (route) => {
    const response = await route.fetch();
    await route.fulfill({ json: { ...(await response.json()), disks } });
  });
  await page.route("**/api/query?**", async (route) => {
    const url = new URL(route.request().url());
    const keys = url.searchParams.get("keys") ?? "";
    queries.push(keys);
    if (!keys.includes("disk.device_read:")) return route.continue();
    const to = Number(url.searchParams.get("to"));
    const ts = [to - 30, to - 15, to];
    const selected = keys.includes("disk.device_read:external") ? "external" : "internal";
    const read = selected === "external" ? 2 * 1024 ** 2 : 1024 ** 2;
    await route.fulfill({ json: { from: to - 30, to, ts, step: 15, interval: 15, series: {
      [`disk.device_read:${selected}`]: { avg: ts.map(() => read), min: ts.map(() => 0), max: ts.map(() => read * 2) },
      [`disk.device_write:${selected}`]: { avg: ts.map(() => 1024), min: ts.map(() => 0), max: ts.map(() => 2048) },
    } } });
  });
  await page.addInitScript(() => {
    (window as any).EventSource = class extends EventTarget {
      constructor() { super(); (window as any).diskSource = this; }
      close() {}
    };
  });
  await page.goto("/");
  await expect(page.locator("#status")).toContainText("Updated");
  await expect(page.getByLabel("Live disk", { exact: true })).toHaveValue("");
  await expect(page.getByLabel("Historical disk", { exact: true })).toHaveValue("");
  await page.locator("#live-toggle").check();
  const emit = (type: string, data: unknown) => page.evaluate(({ type, data }) => {
    (window as any).diskSource.dispatchEvent(new MessageEvent(type, { data: JSON.stringify(data) }));
  }, { type, data });
  const keys = ["disk.read", "disk.write", "disk.device_read:internal", "disk.device_write:internal", "disk.device_read:external", "disk.device_write:external"];
  const values = [3 * 1024 ** 2, 1024 ** 2, 1024 ** 2, 256 * 1024, 2 * 1024 ** 2, 768 * 1024];
  await emit("init", { disks, keys, rows: [[Date.now() - 2000, values], [Date.now(), values]], groups: [], groups_ts: null, sample_ms: 2000, process_sample_secs: 5 });
  await expect(page.locator("#live-rate-disk-0")).toHaveText("3.00 MB/s");
  await page.getByLabel("Live disk", { exact: true }).selectOption("external");
  await expect(page.locator("#live-rate-disk-0")).toHaveText("2.00 MB/s");
  await expect(page.locator("#live-rate-disk-1")).toHaveText("768 KB/s");
  await page.getByLabel("Live disk", { exact: true }).selectOption("internal");
  await expect(page.locator("#live-rate-disk-0")).toHaveText("1.00 MB/s");
  await page.getByLabel("Live disk", { exact: true }).selectOption("external");
  await emit("disks", { disks: disks.map((d) => ({ ...d, connected: d.id === "internal" })) });
  await expect(page.getByLabel("Live disk", { exact: true })).toHaveValue("external");
  await expect(page.locator("#live-rate-disk-0")).toHaveText("–");
  await expect(page.locator("#live-sub-disk")).toContainText("Disconnected");
  await page.getByLabel("Live disk", { exact: true }).selectOption("");
  await expect(page.locator("#live-rate-disk-0")).toHaveText("3.00 MB/s");
  await emit("disks", { disks: disks.map((d) => ({ ...d, bsd: d.id === "external" ? "disk9" : d.bsd })) });
  await page.getByLabel("Live disk", { exact: true }).selectOption("external");
  await expect(page.locator("#live-disk option:checked")).toContainText("disk9");
  await page.getByLabel("Historical disk", { exact: true }).selectOption("external");
  await expect.poll(() => queries.some((keys) => keys.includes("disk.device_read:external"))).toBe(true);
  await expect(page.locator("#card-disk .legend-value").first()).toHaveText("2.00 MB/s");
  await expect(page.locator("#card-disk .help")).toContainText("history starts");
  await expect(page.getByLabel("Live disk", { exact: true })).toHaveValue("external");
  await page.getByLabel("Historical disk", { exact: true }).selectOption("internal");
  await expect(page.locator("#card-disk .legend-value").first()).toHaveText("1.00 MB/s");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.locator("#live-toggle").uncheck();
  expect(errors).toEqual([]);
});

test("live layout fits a mobile viewport", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(page.locator("#live-section")).toBeVisible();
  await page.locator("#live-toggle").check();
  await expect(page.locator("#live-section .chart canvas")).toHaveCount(6);
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: "test-results/live-mobile.png" });
  await page.locator("#live-toggle").uncheck();
});

test("drag on a chart zooms to a custom range and can be reset", async ({ page }) => {
  await page.goto("/#range=1h");
  const over = page.locator("#card-power .u-over");
  await over.scrollIntoViewIfNeeded();
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
  await over.scrollIntoViewIfNeeded();
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

test("historical smoothing changes drawn trends but preserves raw tables, peaks and CSV", async ({ page }) => {
  const errors = collectErrors(page);
  await page.route("**/api/query?**", async (route) => {
    const url = new URL(route.request().url());
    if (!url.searchParams.get("keys")?.split(",").includes("cpu.active")) return route.continue();
    const to = Number(url.searchParams.get("to"));
    const ts = Array.from({ length: 61 }, (_, i) => to - 900 + i * 15);
    await route.fulfill({ json: {
      from: to - 900, to, interval: 15, step: 15, ts,
      series: { "cpu.active": {
        avg: ts.map((_, i) => i === 30 ? .9 : .1),
        min: ts.map(() => .05), max: ts.map((_, i) => i === 30 ? .98 : .15),
      } },
    } });
  });
  await page.goto("/#range=15m");
  const card = page.locator("#card-cpu");
  const chart = page.locator("#body-cpu");
  const smooth = page.getByLabel("Smooth historical trends");
  await expect(page.locator("#status")).toContainText("Updated");
  await expect(smooth).not.toBeChecked();
  await expect(chart).toHaveAttribute("data-smoothed", "false");
  const csv = async () => {
    const pending = page.waitForEvent("download");
    await card.getByRole("button", { name: "CSV", exact: true }).click();
    const download = await pending;
    return readFile((await download.path())!, "utf8");
  };
  const originalCsv = await csv();
  const originalCanvas = await card.locator("canvas").evaluate((el) => (el as HTMLCanvasElement).toDataURL());
  await smooth.check();
  await expect(chart).toHaveAttribute("data-smoothed", "true");
  await expect(page.locator("#body-proc-cpu")).toHaveAttribute("data-smoothed", "true");
  await expect(page.locator("#body-energy")).toHaveAttribute("data-smoothed", "false");
  const smoothedCanvas = await card.locator("canvas").evaluate((el) => (el as HTMLCanvasElement).toDataURL());
  expect(smoothedCanvas).not.toBe(originalCanvas);
  expect(await csv()).toBe(originalCsv);
  await card.getByRole("button", { name: "Table", exact: true }).click();
  await expect(card.locator(".table-view tbody tr").filter({ hasText: "90%" })).toHaveCount(1);
  await card.getByRole("button", { name: "Chart", exact: true }).click();
  const over = card.locator(".u-over");
  await over.scrollIntoViewIfNeeded();
  const box = (await over.boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await expect(card.locator(".tip")).toContainText("90%");
  await expect(card.locator(".tip")).toContainText("98%");
  await page.screenshot({ path: "test-results/history-area-smoothed.png" });
  await page.reload();
  await expect(smooth).toBeChecked();
  await expect(chart).toHaveAttribute("data-smoothed", "true");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(errors).toEqual([]);
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
  expect(meta.disks.some((d: any) => d.connected)).toBe(true);
  const disk = meta.disks.find((d: any) => d.connected);
  expect(meta.keys).toContain(`disk.device_read:${disk.id}`);
  const perDisk = await (await request.get(`/api/query?keys=disk.device_read:${disk.id}&from=${now - 600}&to=${now}`)).json();
  expect(perDisk.series[`disk.device_read:${disk.id}`].avg.some((v: any) => v != null)).toBe(true);
  expect(meta.sample_ms).toBe(2000);
  expect(meta.live_sample_ms).toBe(meta.sample_ms);
  const bad = await request.get("/api/query?keys=power.sys&step=-5");
  expect(bad.status()).toBe(400);
  const missing = await request.get("/nope.js");
  expect(missing.status()).toBe(404);
});
