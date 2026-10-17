import "uplot/dist/uPlot.min.css";
import "./styles.css";

import { type LiveInit, type Meta, type MonitorEvent, type RangeData, type Summary, type Values, api } from "./api";
import { Chart, type Line, type Marker, Strip, css, sparkline } from "./chart";
import * as F from "./format";
import type { Unit } from "./format";

// ---------------------------------------------------------------------------------------------
// View state: a preset range ending now, or a fixed custom range (from zooming or the picker).

interface Preset {
  id: string;
  label: string;
  secs: number;
  title: string;
}

const PRESETS: Preset[] = [
  { id: "live", label: "Live", secs: 900, title: "Last 15 minutes, updated every second" },
  { id: "1h", label: "1h", secs: 3600, title: "Last hour" },
  { id: "6h", label: "6h", secs: 6 * 3600, title: "Last 6 hours" },
  { id: "24h", label: "24h", secs: 86400, title: "Last 24 hours" },
  { id: "7d", label: "7d", secs: 7 * 86400, title: "Last 7 days" },
  { id: "30d", label: "30d", secs: 30 * 86400, title: "Last 30 days" },
  { id: "90d", label: "90d", secs: 90 * 86400, title: "Last 90 days" },
  { id: "1y", label: "1y", secs: 365 * 86400, title: "Last year" },
];

interface ViewState {
  preset: string; // a preset id or "custom"
  from: number;
  to: number;
}

function readHash(): ViewState {
  const p = new URLSearchParams(location.hash.slice(1));
  const from = Number(p.get("from"));
  const to = Number(p.get("to"));
  if (from > 0 && to > from) return { preset: "custom", from, to };
  const preset = p.get("range");
  return { preset: PRESETS.some((x) => x.id === preset) ? preset! : "1h", from: 0, to: 0 };
}

function writeHash(): void {
  const h = state.preset === "custom" ? `from=${state.from}&to=${state.to}` : `range=${state.preset}`;
  history.replaceState(null, "", `#${h}`);
}

const state = readHash();
/** The window being shown; charts read it for their x range. */
const view = { from: 0, to: 0 };

const now = () => Math.floor(Date.now() / 1000);
const isLive = () => state.preset === "live";

function currentWindow(): [number, number] {
  if (state.preset === "custom") return [state.from, state.to];
  const p = PRESETS.find((x) => x.id === state.preset)!;
  const to = now();
  return [to - p.secs, to];
}

// ---------------------------------------------------------------------------------------------
// Colors: categorical slots in fixed order; process groups keep their slot across views.

const slot = (i: number) => css(`--s${(i % 8) + 1}`);
const otherColor = () => css("--other");

function groupSlots(groups: string[]): Map<string, number> {
  let saved: Record<string, number> = {};
  try {
    saved = JSON.parse(localStorage.getItem("mm.groupSlots") ?? "{}");
  } catch {
    saved = {};
  }
  const used = new Set<number>();
  const out = new Map<string, number>();
  for (const g of groups) {
    const s = saved[g];
    if (s != null && s >= 0 && s < 8 && !used.has(s)) {
      used.add(s);
      out.set(g, s);
    }
  }
  for (const g of groups) {
    if (out.has(g)) continue;
    const s = [0, 1, 2, 3, 4, 5, 6, 7].find((i) => !used.has(i)) ?? 0;
    used.add(s);
    out.set(g, s);
    saved[g] = s;
  }
  localStorage.setItem("mm.groupSlots", JSON.stringify(saved));
  return out;
}

// ---------------------------------------------------------------------------------------------
// DOM helpers. Untrusted strings (process names, event text) only go through textContent.

function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Record<string, string> = {},
  ...children: (Node | string | null | undefined)[]
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else e.setAttribute(k, v);
  }
  for (const c of children) if (c != null) e.append(c);
  return e;
}

function $(id: string): HTMLElement {
  return document.getElementById(id)!;
}

// ---------------------------------------------------------------------------------------------
// Panels

interface LineDef {
  key: string;
  label: string;
  band?: boolean;
}

interface PanelDef {
  id: string;
  title: string;
  help: string;
  unit: Unit;
  kind?: "line" | "stack";
  y?: "zero" | "auto" | "ratio";
  lines: LineDef[];
  wide?: boolean;
}

const TIER_NAMES: Record<string, string> = { E: "Efficiency", P: "Performance", S: "Super", _: "CPU" };

function hostPanels(meta: Meta): Record<string, PanelDef[]> {
  const tiers = meta.machine.cpu_tiers.length ? meta.machine.cpu_tiers : [{ label: "_", cores: 0, max_mhz: 0 }];
  const tierLabel = (t: { label: string; cores: number }) =>
    `${TIER_NAMES[t.label] ?? t.label}${t.cores ? ` ×${t.cores}` : ""}`;
  const fans = meta.keys.filter((k) => k.startsWith("fan.rpm:"));
  return {
    power: [
      {
        id: "power",
        title: "Power draw",
        help: "Whole-system power from the SMC, with CPU, GPU and Neural Engine shares. Shaded: min–max within each point.",
        unit: "W",
        lines: [
          { key: "power.sys", label: "System", band: true },
          { key: "power.cpu", label: "CPU" },
          { key: "power.gpu", label: "GPU" },
          { key: "power.ane", label: "Neural Engine" },
        ],
      },
    ],
    cpu: [
      {
        id: "cpu",
        title: "CPU usage",
        help: "Share of time cores were busy (active residency), all cores and per cluster.",
        unit: "ratio",
        y: "ratio",
        lines: [
          { key: "cpu.active", label: "All cores", band: true },
          ...tiers.map((t) => ({ key: `cpu.tier.active:${t.label}`, label: tierLabel(t) })),
        ],
      },
      {
        id: "cpu-freq",
        title: "CPU frequency",
        help: "Average clock of each cluster.",
        unit: "MHz",
        lines: tiers.map((t) => ({ key: `cpu.tier.freq:${t.label}`, label: tierLabel(t) })),
      },
      {
        id: "load",
        title: "Load average",
        help: "Runnable threads, 1-minute average. Above the core count means work is waiting.",
        unit: "load",
        lines: [{ key: "load.1", label: "1-min load", band: true }],
      },
      {
        id: "procs-count",
        title: "Processes",
        help: "Number of processes running, all users.",
        unit: "count",
        lines: [{ key: "procs.count", label: "Processes" }],
      },
    ],
    gpu: [
      {
        id: "gpu",
        title: "GPU usage",
        help: "Active: share of time the GPU was busy. Frequency-scaled: weighted by clock speed, closer to capacity used.",
        unit: "ratio",
        y: "ratio",
        lines: [
          { key: "gpu.active", label: "Active", band: true },
          { key: "gpu.scaled", label: "Frequency-scaled" },
        ],
      },
      {
        id: "gpu-freq",
        title: "GPU frequency",
        help: `Clock speed (max ${F.fmt("MHz", meta.machine.gpu_max_mhz)}).`,
        unit: "MHz",
        lines: [{ key: "gpu.freq", label: "GPU", band: true }],
      },
    ],
    memory: [
      {
        id: "mem",
        title: "Memory composition",
        help: "As in Activity Monitor: app, wired and compressed memory are in use; cached files can be reclaimed.",
        unit: "B",
        kind: "stack",
        lines: [
          { key: "mem.app", label: "App" },
          { key: "mem.wired", label: "Wired" },
          { key: "mem.compressed", label: "Compressed" },
          { key: "mem.cached", label: "Cached files" },
          { key: "mem.free", label: "Free" },
        ],
      },
      {
        id: "swap",
        title: "Swap",
        help: "Swap in use and allocated. macOS grows swap files on demand.",
        unit: "B",
        lines: [
          { key: "swap.used", label: "Used" },
          { key: "swap.total", label: "Allocated" },
        ],
      },
      {
        id: "swap-io",
        title: "Swap traffic",
        help: "Memory written to and read back from swap. Sustained swap-out means memory is short.",
        unit: "B/s",
        lines: [
          { key: "swap.out", label: "Swap out", band: true },
          { key: "swap.in", label: "Swap in", band: true },
        ],
      },
    ],
    thermal: [
      {
        id: "temp",
        title: "Temperature",
        help: "Average of the CPU and GPU die sensors.",
        unit: "C",
        y: "auto",
        lines: [
          { key: "temp.cpu", label: "CPU", band: true },
          { key: "temp.gpu", label: "GPU", band: true },
        ],
      },
      {
        id: "fans",
        title: "Fan speed",
        help: meta.machine.fans.length
          ? `Max ${meta.machine.fans.map(([n, m]) => `${n} ${m ?? "?"} rpm`).join(", ")}.`
          : "No fans reported.",
        unit: "rpm",
        lines: fans.map((k) => ({ key: k, label: k.split(":")[1], band: true })),
      },
    ],
    io: [
      {
        id: "disk",
        title: "Disk throughput",
        help: "Bytes read and written by all block devices.",
        unit: "B/s",
        lines: [
          { key: "disk.read", label: "Read", band: true },
          { key: "disk.write", label: "Write", band: true },
        ],
      },
      {
        id: "net",
        title: "Network throughput",
        help: "Ethernet and Wi-Fi (en*) interfaces; tunnels such as Tailscale are not double-counted.",
        unit: "B/s",
        lines: [
          { key: "net.rx", label: "Received", band: true },
          { key: "net.tx", label: "Sent", band: true },
        ],
      },
    ],
  };
}

const SECTIONS: { id: string; title: string }[] = [
  { id: "power", title: "Power & energy" },
  { id: "cpu", title: "CPU" },
  { id: "gpu", title: "GPU" },
  { id: "memory", title: "Memory" },
  { id: "processes", title: "Processes" },
  { id: "thermal", title: "Thermals" },
  { id: "io", title: "Disk & network" },
  { id: "events", title: "Events" },
  { id: "overhead", title: "Monitor overhead" },
];

// ---------------------------------------------------------------------------------------------
// App

let meta: Meta;
const charts = new Map<string, Chart>();
const panelDefs = new Map<string, PanelDef>();
let pressureStrip: Strip;
let thermalStrip: Strip;
let events: MonitorEvent[] = [];
let hostData: RangeData | null = null;
let hostSummary: Record<string, Summary> = {};
let price = Number(localStorage.getItem("mm.price") ?? "0.15") || 0.15;
let energyBars: { ts: number[]; wh: number[]; step: number } = { ts: [], wh: [], step: 3600 };
let timer: number | undefined;
let refreshing = false;
let lastError = "";

const markers = (): Marker[] =>
  events
    .filter((e) => e.kind === "boot" || e.kind === "monitor" || e.kind === "alert" || e.kind === "sleep")
    .map((e) => ({ ts: e.ts, end: e.end, level: e.level, title: e.title }));

const xRange = (): [number, number] => [view.from, view.to];

function onZoom(from: number, to: number): void {
  if (to - from < 60) return;
  state.preset = "custom";
  state.from = from;
  state.to = to;
  writeHash();
  renderControls();
  restart();
}

function card(id: string, title: string, help: string, opts: { wide?: boolean; table?: boolean } = {}): HTMLElement {
  const actions = h("div", { class: "card-actions" });
  if (opts.table !== false) {
    const tableBtn = h("button", { type: "button", class: "ghost", "aria-pressed": "false" }, "Table");
    tableBtn.addEventListener("click", () => {
      const shown = charts.get(id)?.toggleTable() ?? false;
      tableBtn.setAttribute("aria-pressed", String(shown));
      tableBtn.textContent = shown ? "Chart" : "Table";
    });
    const csvBtn = h("button", { type: "button", class: "ghost", title: "Download as CSV" }, "CSV");
    csvBtn.addEventListener("click", () => {
      const chart = charts.get(id);
      if (!chart) return;
      const a = h("a", {
        href: URL.createObjectURL(new Blob([chart.csv()], { type: "text/csv" })),
        download: `mac-monitor-${id}.csv`,
      });
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    });
    actions.append(tableBtn, csvBtn);
  }
  return h(
    "article",
    { class: `card${opts.wide ? " wide" : ""}`, id: `card-${id}` },
    h("header", { class: "card-head" }, h("div", {}, h("h3", {}, title), h("p", { class: "help" }, help)), actions),
    h("div", { class: "card-body", id: `body-${id}` }),
  );
}

function tile(id: string, label: string): HTMLElement {
  return h(
    "div",
    { class: "tile", id: `tile-${id}` },
    h("div", { class: "tile-label" }, label),
    h("div", { class: "tile-value" }, "–"),
    h("div", { class: "tile-sub" }, ""),
    h("canvas", { class: "tile-spark", "aria-hidden": "true" }),
  );
}

function buildLayout(): void {
  const panels = hostPanels(meta);
  const main = $("app");
  main.replaceChildren();

  const tiles = h("section", { class: "tiles", "aria-label": "Summary" });
  for (const [id, label] of [
    ["power", "System power"],
    ["energy", "Energy"],
    ["cpu", "CPU"],
    ["gpu", "GPU"],
    ["mem", "Memory used"],
    ["swap", "Swap used"],
    ["temp", "CPU temperature"],
    ["disk", "Disk free"],
  ]) {
    tiles.append(tile(id, label));
  }
  main.append(tiles);

  const nav = h("nav", { class: "jump", "aria-label": "Sections" });
  for (const s of SECTIONS) nav.append(h("a", { href: `#sec-${s.id}`, "data-jump": s.id }, s.title));
  nav.addEventListener("click", (e) => {
    const a = (e.target as HTMLElement).closest("a");
    if (!a) return;
    e.preventDefault();
    $(`sec-${a.dataset.jump}`).scrollIntoView({ behavior: "smooth", block: "start" });
  });
  main.append(nav);

  for (const s of SECTIONS) {
    const grid = h("div", { class: "grid" });
    const section = h("section", { class: "section", id: `sec-${s.id}` }, h("h2", {}, s.title), grid);
    main.append(section);

    if (s.id === "power") {
      grid.append(card("energy", "Energy used", "Energy per period, from back-to-back power readings.", {}));
    }
    if (s.id === "memory") {
      grid.append(stripCard("pressure", "Memory pressure", "macOS memory pressure level over time."));
    }
    if (s.id === "thermal") {
      grid.append(stripCard("thermal", "Thermal pressure", "Throttling level reported by macOS."));
    }
    for (const p of panels[s.id] ?? []) {
      panelDefs.set(p.id, p);
      grid.append(card(p.id, p.title, p.help, { wide: p.wide }));
    }
    if (s.id === "processes") buildProcesses(grid);
    if (s.id === "events") buildEvents(grid);
    if (s.id === "overhead") buildOverhead(grid);
  }

  for (const [id, p] of panelDefs) {
    charts.set(id, new Chart($(`body-${id}`), { unit: p.unit, kind: p.kind, y: p.y, xRange, onZoom, markers, totalLabel: "Physical memory" }));
  }

  const priceInput = h("input", {
    type: "number",
    min: "0",
    step: "0.01",
    value: String(price),
    "aria-label": "Electricity price in USD per kWh",
    id: "price",
  });
  priceInput.addEventListener("change", () => {
    const v = Number(priceInput.value);
    if (Number.isFinite(v) && v >= 0) {
      price = v;
      localStorage.setItem("mm.price", String(v));
      renderTiles();
      renderEnergy();
    }
  });
  $("card-energy").querySelector(".card-actions")!.prepend(h("label", { class: "price" }, "$", priceInput, "/kWh"));
  charts.set(
    "energy",
    new Chart($("body-energy"), {
      unit: "Wh",
      kind: "bars",
      // whole bars, from the first bucket's start to the last one's end
      xRange: () => {
        const half = energyBars.step / 2;
        return energyBars.ts.length ? [energyBars.ts[0] - half, energyBars.ts[energyBars.ts.length - 1] + half] : xRange();
      },
      onZoom,
      tooltipExtra: (i) => {
        const wh = energyBars.wh[i];
        if (wh == null) return null;
        const avgW = wh / (energyBars.step / 3600);
        return `${F.money((wh / 1000) * price)} · avg ${F.fmt("W", avgW)}`;
      },
    }),
  );

  const statusColors = () => [css("--status-good-strip"), css("--status-warning"), css("--status-critical")];
  pressureStrip = new Strip($("body-pressure"), ["Normal", "Warning", "Critical"], statusColors, xRange);
  thermalStrip = new Strip(
    $("body-thermal"),
    ["Nominal", "Moderate", "Heavy", "Trapping", "Sleeping"],
    () => [css("--status-good-strip"), css("--status-warning"), css("--status-serious"), css("--status-critical"), css("--status-critical")],
    xRange,
  );
}

function stripCard(id: string, title: string, help: string): HTMLElement {
  const c = card(id, title, help, { wide: true, table: false });
  const names =
    id === "pressure"
      ? [
          ["good", "●", "Normal"],
          ["warning", "▲", "Warning"],
          ["critical", "◆", "Critical"],
        ]
      : [
          ["good", "●", "Nominal"],
          ["warning", "▲", "Moderate"],
          ["serious", "▲", "Heavy"],
          ["critical", "◆", "Trapping / sleeping"],
        ];
  const legend = h("div", { class: "strip-legend" });
  for (const [status, icon, label] of names) {
    legend.append(h("span", { class: `status status-${status}` }, h("span", { class: "status-icon", "aria-hidden": "true" }, icon), label));
  }
  c.querySelector(".card-actions")!.append(legend);
  return c;
}

// ---------------------------------------------------------------------------------------------
// Controls

function renderControls(): void {
  const presets = $("presets");
  presets.replaceChildren();
  for (const p of PRESETS) {
    const b = h("button", { type: "button", title: p.title, "aria-pressed": String(state.preset === p.id) }, p.label);
    if (p.id === "live") b.prepend(h("span", { class: "live-dot", "aria-hidden": "true" }));
    b.addEventListener("click", () => {
      state.preset = p.id;
      writeHash();
      renderControls();
      restart();
    });
    presets.append(b);
  }
  const custom = $("custom-label");
  if (state.preset === "custom") {
    custom.textContent = `${F.time(state.from, 2 * 86400)} → ${F.time(state.to, 2 * 86400)}`;
    $("reset-zoom").hidden = false;
  } else {
    custom.textContent = "";
    $("reset-zoom").hidden = true;
  }
}

function toLocalInput(ts: number): string {
  const d = new Date(ts * 1000);
  d.setMinutes(d.getMinutes() - d.getTimezoneOffset());
  return d.toISOString().slice(0, 16);
}

function setupControls(): void {
  $("reset-zoom").addEventListener("click", () => {
    state.preset = "1h";
    writeHash();
    renderControls();
    restart();
  });

  const dialog = $("range-dialog") as HTMLDialogElement;
  $("custom-btn").addEventListener("click", () => {
    const [from, to] = currentWindow();
    ($("range-from") as HTMLInputElement).value = toLocalInput(from);
    ($("range-to") as HTMLInputElement).value = toLocalInput(to);
    dialog.showModal();
  });
  $("range-form").addEventListener("submit", (e) => {
    const submitter = (e as SubmitEvent).submitter as HTMLButtonElement | null;
    if (submitter?.value !== "apply") return;
    const from = Math.floor(new Date(($("range-from") as HTMLInputElement).value).getTime() / 1000);
    const to = Math.floor(new Date(($("range-to") as HTMLInputElement).value).getTime() / 1000);
    if (to - from >= 60) onZoom(from, to);
  });

  const themeBtn = $("theme");
  const themes = ["auto", "light", "dark"];
  const applyTheme = (t: string) => {
    if (t === "auto") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = t;
    themeBtn.textContent = `Theme: ${t}`;
  };
  applyTheme(localStorage.getItem("mm.theme") ?? "auto");
  themeBtn.addEventListener("click", () => {
    const cur = localStorage.getItem("mm.theme") ?? "auto";
    const next = themes[(themes.indexOf(cur) + 1) % themes.length];
    localStorage.setItem("mm.theme", next);
    applyTheme(next);
    rerenderAll();
  });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", rerenderAll);
}

function rerenderAll(): void {
  for (const c of charts.values()) c.rebuild();
  pressureStrip.draw();
  thermalStrip.draw();
  renderTiles();
}

function setStatus(text: string, kind: "ok" | "live" | "error" | "loading" = "ok"): void {
  const s = $("status");
  s.textContent = text;
  s.dataset.kind = kind;
}

// ---------------------------------------------------------------------------------------------
// Host metrics

function hostKeys(): { keys: string[]; minmax: string[] } {
  const keys = new Set<string>(["mem.pressure", "thermal.pressure", "mem.used", "mem.total", "disk.free", "disk.total"]);
  const minmax = new Set<string>(["mem.pressure", "thermal.pressure"]);
  for (const p of panelDefs.values()) {
    for (const l of p.lines) {
      keys.add(l.key);
      if (l.band) minmax.add(l.key);
    }
  }
  return { keys: [...keys], minmax: [...minmax] };
}

function plotPoints(): number {
  const w = document.querySelector<HTMLElement>(".card .plot")?.clientWidth ?? 600;
  return Math.max(150, Math.min(1000, Math.round(w)));
}

function renderHost(data: RangeData): void {
  hostData = data;
  for (const [id, p] of panelDefs) {
    const lines: Line[] = [];
    p.lines.forEach((l, i) => {
      const s = data.series[l.key];
      if (!s && !meta.keys.includes(l.key)) return;
      lines.push({
        label: l.label,
        color: slot(i),
        values: s?.avg ?? data.ts.map(() => null),
        min: l.band ? s?.min : undefined,
        max: l.band ? s?.max : undefined,
      });
    });
    charts.get(id)!.setData(data.ts, lines);
  }
  const step = data.step;
  const level = (k: string) => data.series[k]?.max ?? data.series[k]?.avg ?? [];
  pressureStrip.setData(data.ts, level("mem.pressure"), step);
  thermalStrip.setData(data.ts, level("thermal.pressure"), step);
  renderTiles();
}

async function loadHost(from: number, to: number): Promise<void> {
  const { keys, minmax } = hostKeys();
  const data = await api.range({ keys, from, to, points: plotPoints(), minmax });
  if (!isLive()) renderHost(data);
}

// ---------------------------------------------------------------------------------------------
// Live mode: 1-second rows over Server-Sent Events.

let source: EventSource | null = null;
let liveKeys: string[] = [];
let liveRows: [number, Values][] = [];
let liveFrame = 0;

function startLive(): void {
  stopLive();
  source = new EventSource("/api/live");
  source.addEventListener("init", (e) => {
    const init = JSON.parse((e as MessageEvent).data) as LiveInit;
    liveKeys = init.keys;
    liveRows = init.rows;
    scheduleLive();
  });
  source.addEventListener("keys", (e) => {
    liveKeys = JSON.parse((e as MessageEvent).data).keys;
  });
  source.addEventListener("row", (e) => {
    const row = JSON.parse((e as MessageEvent).data) as { t: number; v: Values };
    liveRows.push([row.t, row.v]);
    const cutoff = row.t - 900_000;
    while (liveRows.length && liveRows[0][0] < cutoff) liveRows.shift();
    scheduleLive();
  });
  source.addEventListener("error", () => setStatus("Reconnecting…", "error"));
  source.addEventListener("open", () => setStatus("Live", "live"));
}

function stopLive(): void {
  source?.close();
  source = null;
}

function scheduleLive(): void {
  if (liveFrame) return;
  liveFrame = requestAnimationFrame(() => {
    liveFrame = 0;
    renderLive();
  });
}

function renderLive(): void {
  if (!isLive() || !liveRows.length) return;
  const t = now();
  view.from = t - 900;
  view.to = t;
  const ts = liveRows.map((r) => r[0] / 1000);
  const series: RangeData["series"] = {};
  liveKeys.forEach((k, i) => {
    series[k] = { avg: liveRows.map((r) => r[1][i] ?? null) };
  });
  renderHost({ from: view.from, to: view.to, interval: 1, step: meta.live_sample_ms / 1000, ts, series });
  setStatus("Live", "live");
}

// ---------------------------------------------------------------------------------------------
// Stat tiles

const SUMMARY_KEYS = [
  "power.sys",
  "cpu.active",
  "gpu.active",
  "mem.used",
  "mem.total",
  "mem.pressure",
  "swap.used",
  "swap.total",
  "temp.cpu",
  "temp.gpu",
  "disk.free",
  "disk.total",
];

function latest(key: string): number | null {
  if (isLive() && liveRows.length) {
    const i = liveKeys.indexOf(key);
    for (let r = liveRows.length - 1; r >= 0 && i >= 0; r--) {
      const v = liveRows[r][1][i];
      if (v != null) return v;
    }
  }
  return hostSummary[key]?.last ?? null;
}

function setTile(id: string, value: string, sub: (string | Node)[], spark?: { key: string; color: string }): void {
  const t = $(`tile-${id}`);
  t.querySelector(".tile-value")!.textContent = value;
  t.querySelector(".tile-sub")!.replaceChildren(...sub);
  const canvas = t.querySelector<HTMLCanvasElement>(".tile-spark")!;
  if (spark && hostData?.series[spark.key]) sparkline(canvas, hostData.series[spark.key].avg, spark.color);
  else canvas.getContext("2d")?.clearRect(0, 0, canvas.width, canvas.height);
}

/** "avg 41.6 · peak 69.5 W": the unit once when both values share it. */
function avgPeak(key: string, unit: Unit): string {
  const s = hostSummary[key];
  if (!s || s.avg == null) return "";
  const avg = F.fmt(unit, s.avg);
  const peak = F.fmt(unit, s.max);
  const suffix = (v: string) => v.replace(/^[-\d.,]+/, "");
  const short = suffix(avg) === suffix(peak) ? avg.slice(0, avg.length - suffix(avg).length) : avg;
  return `avg ${short} · peak ${peak}`;
}

function statusChip(level: number | null): HTMLElement {
  const [cls, icon, label] =
    level == null ? ["muted", "", "–"] : level >= 2 ? ["critical", "◆", "Critical"] : level >= 1 ? ["warning", "▲", "Warning"] : ["good", "●", "Normal"];
  return h("span", { class: `status status-${cls}` }, h("span", { class: "status-icon", "aria-hidden": "true" }, icon), `${label} pressure`);
}

function renderTiles(): void {
  if (!meta) return;
  const spark = slot(0);
  setTile("power", F.fmt("W", latest("power.sys")), [avgPeak("power.sys", "W")], { key: "power.sys", color: spark });

  const sys = hostSummary["power.sys"];
  const wh = sys ? sys.integral / 3600 : 0;
  const covered = sys?.cov ?? 0;
  setTile("energy", sys ? F.energy(wh) : "–", [sys ? `${F.money((wh / 1000) * price)} at $${price}/kWh` : ""]);
  $("tile-energy").title = covered ? `${F.duration(covered)} of the range has power data` : "";

  setTile("cpu", F.fmt("ratio", latest("cpu.active")), [avgPeak("cpu.active", "ratio")], { key: "cpu.active", color: spark });
  setTile("gpu", F.fmt("ratio", latest("gpu.active")), [avgPeak("gpu.active", "ratio")], { key: "gpu.active", color: spark });

  const total = latest("mem.total");
  const pressure = latest("mem.pressure");
  setTile("mem", F.fmt("B", latest("mem.used")), [total ? `of ${F.bytes(total)} · ` : "", statusChip(pressure)], {
    key: "mem.used",
    color: spark,
  });
  setTile("swap", F.fmt("B", latest("swap.used")), [`${F.bytes(latest("swap.total") ?? 0)} allocated`], { key: "swap.used", color: spark });
  const gpuT = latest("temp.gpu");
  setTile("temp", F.fmt("C", latest("temp.cpu")), [gpuT != null ? `GPU ${F.fmt("C", gpuT)}` : ""], {
    key: "temp.cpu",
    color: spark,
  });
  const diskTotal = latest("disk.total");
  setTile("disk", F.fmt("B", latest("disk.free")), [diskTotal ? `of ${F.bytes(diskTotal)}` : ""]);
}

async function loadSummary(from: number, to: number): Promise<void> {
  hostSummary = await api.summary({ keys: SUMMARY_KEYS, from, to });
  renderTiles();
}

// ---------------------------------------------------------------------------------------------
// Energy bars

function energyStep(span: number): number {
  if (span <= 1800) return 60;
  if (span <= 3 * 3600) return 300;
  if (span <= 12 * 3600) return 900;
  if (span <= 2 * 86400) return 3600;
  if (span <= 120 * 86400) return 86400;
  return 7 * 86400;
}

async function loadEnergy(from: number, to: number): Promise<void> {
  const step = energyStep(to - from);
  const align = new Date().getTimezoneOffset() * 60;
  const r = await api.range({ keys: ["power.sys"], from, to, step, align, cov: true, minmax: [] });
  const s = r.series["power.sys"];
  energyBars = {
    step: r.step,
    ts: r.ts.map((t) => t + r.step / 2),
    wh: r.ts.map((_, i) => (s && s.avg[i] != null && s.cov ? (s.avg[i]! * s.cov[i]) / 3600 : (null as unknown as number))),
  };
  renderEnergy();
}

function renderEnergy(): void {
  const chart = charts.get("energy");
  if (!chart) return;
  const label = `Energy per ${F.stepName(energyBars.step)}`;
  chart.setData(energyBars.ts, [{ label, color: slot(0), values: energyBars.wh }]);
}

// ---------------------------------------------------------------------------------------------
// Processes

interface GroupRow {
  name: string;
  cpuAvg: number;
  cpuPeak: number;
  cpuSecs: number;
  memAvg: number;
  memPeak: number;
  energyWh: number;
  powerAvg: number;
  gpuAvg: number;
  readBytes: number;
  writeBytes: number;
  count: number;
}

const PROC_COLUMNS: { id: keyof GroupRow; label: string; title: string; fmt: (r: GroupRow) => string }[] = [
  { id: "name", label: "Group", title: "App bundle or process name", fmt: (r) => r.name },
  { id: "cpuAvg", label: "CPU avg", title: "Average CPU, 100% = one core", fmt: (r) => F.pct(r.cpuAvg) },
  { id: "cpuPeak", label: "CPU peak", title: "Highest 15-second CPU in the range", fmt: (r) => F.pct(r.cpuPeak) },
  { id: "cpuSecs", label: "CPU time", title: "Total CPU time used in the range", fmt: (r) => F.duration(r.cpuSecs) },
  { id: "memAvg", label: "Mem avg", title: "Average memory footprint", fmt: (r) => F.bytes(r.memAvg) },
  { id: "memPeak", label: "Mem peak", title: "Highest memory footprint", fmt: (r) => F.bytes(r.memPeak) },
  { id: "energyWh", label: "CPU energy", title: "CPU energy macOS attributes to these processes (GPU energy is not attributed)", fmt: (r) => F.energy(r.energyWh) },
  { id: "powerAvg", label: "CPU power", title: "Average attributed CPU power", fmt: (r) => F.fmt("W", r.powerAvg) },
  { id: "gpuAvg", label: "GPU avg", title: "Average share of GPU time", fmt: (r) => F.pct(r.gpuAvg) },
  { id: "readBytes", label: "Disk read", title: "Bytes read in the range", fmt: (r) => F.bytes(r.readBytes) },
  { id: "writeBytes", label: "Disk write", title: "Bytes written in the range", fmt: (r) => F.bytes(r.writeBytes) },
  { id: "count", label: "Procs", title: "Processes in the group (latest)", fmt: (r) => String(r.count) },
];

let procRows: GroupRow[] = [];
let procSort: { col: keyof GroupRow; desc: boolean } = { col: "cpuSecs", desc: true };
let procFilter = "";
let selfSummary: Record<string, Summary> = {};

const PROC_STACKS: { id: string; fam: string; title: string; help: string; unit: Unit; rank: keyof GroupRow }[] = [
  { id: "proc-cpu", fam: "proc.cpu", title: "CPU by process group", help: "100% = one core. Top groups in the range; the rest are Other.", unit: "cores", rank: "cpuSecs" },
  { id: "proc-mem", fam: "proc.mem", title: "Memory by process group", help: "Memory footprint, as Activity Monitor's Memory column.", unit: "B", rank: "memAvg" },
  { id: "proc-gpu", fam: "proc.gpu", title: "GPU by process group", help: "Share of GPU time used by each group's processes.", unit: "ratio", rank: "gpuAvg" },
  { id: "proc-power", fam: "proc.power", title: "CPU power by process group", help: "CPU energy macOS attributes to processes. GPU energy is not attributed per process.", unit: "W", rank: "energyWh" },
];

function buildProcesses(grid: HTMLElement): void {
  for (const p of PROC_STACKS) {
    grid.append(card(p.id, p.title, p.help));
  }
  const filter = h("input", { type: "search", placeholder: "Filter groups", "aria-label": "Filter process groups" });
  filter.addEventListener("input", () => {
    procFilter = filter.value.toLowerCase();
    renderProcTable();
  });
  const c = card("proc-table", "Process groups", "Your user's processes grouped by app. Click a column to sort. Other users' processes need root and are not included.", {
    wide: true,
    table: false,
  });
  c.querySelector(".card-actions")!.append(filter);
  grid.append(c);
  for (const p of PROC_STACKS) {
    charts.set(p.id, new Chart($(`body-${p.id}`), { unit: p.unit, kind: "stack", xRange, onZoom, markers, totalLabel: "All groups" }));
  }
}

function procName(key: string): string {
  return key.slice(key.indexOf(":") + 1);
}

async function loadProcs(from: number, to: number): Promise<void> {
  const [summary, series] = await Promise.all([
    api.summary({ keys: ["self.cpu", "self.mem"], match: ["proc.*"], from, to }),
    api.range({ keys: ["self.cpu", "self.mem"], match: PROC_STACKS.map((p) => `${p.fam}:*`), from, to, points: Math.min(400, plotPoints()), minmax: [] }),
  ]);
  selfSummary = summary;

  const rows = new Map<string, GroupRow>();
  const row = (name: string) => {
    let r = rows.get(name);
    if (!r) {
      r = { name, cpuAvg: 0, cpuPeak: 0, cpuSecs: 0, memAvg: 0, memPeak: 0, energyWh: 0, powerAvg: 0, gpuAvg: 0, readBytes: 0, writeBytes: 0, count: 0 };
      rows.set(name, r);
    }
    return r;
  };
  for (const [key, s] of Object.entries(summary)) {
    if (!key.startsWith("proc.")) continue;
    const fam = key.slice(0, key.indexOf(":"));
    const r = row(procName(key));
    switch (fam) {
      case "proc.cpu":
        r.cpuAvg = s.avg ?? 0;
        r.cpuPeak = s.max ?? 0;
        r.cpuSecs = s.integral;
        break;
      case "proc.mem":
        r.memAvg = s.avg ?? 0;
        r.memPeak = s.max ?? 0;
        break;
      case "proc.power":
        r.energyWh = s.integral / 3600;
        r.powerAvg = s.avg ?? 0;
        break;
      case "proc.gpu":
        r.gpuAvg = s.avg ?? 0;
        break;
      case "proc.disk_read":
        r.readBytes = s.integral;
        break;
      case "proc.disk_write":
        r.writeBytes = s.integral;
        break;
      case "proc.count":
        r.count = s.last ?? 0;
        break;
    }
  }
  procRows = [...rows.values()].filter((r) => r.cpuSecs > 0 || r.memAvg > 0 || r.count > 0);
  renderProcTable();

  // The same featured groups (and colors) in every process chart: round-robin over the rankings.
  const named = procRows.filter((r) => r.name !== "other");
  const rankings = PROC_STACKS.map((p) => [...named].sort((a, b) => (b[p.rank] as number) - (a[p.rank] as number)));
  const featured: string[] = [];
  for (let i = 0; featured.length < 7 && i < named.length; i++) {
    for (const ranking of rankings) {
      const g = ranking[i]?.name;
      if (g && !featured.includes(g) && featured.length < 7) featured.push(g);
    }
  }
  const slots = groupSlots(featured);
  const order = [...featured].sort((a, b) => slots.get(a)! - slots.get(b)!);

  for (const p of PROC_STACKS) {
    const lines: Line[] = order.map((g) => ({
      label: g,
      color: slot(slots.get(g)!),
      values: series.series[`${p.fam}:${g}`]?.avg ?? series.ts.map(() => null),
    }));
    const other: Values = series.ts.map(() => null);
    for (const [key, s] of Object.entries(series.series)) {
      if (!key.startsWith(`${p.fam}:`) || featured.includes(procName(key))) continue;
      s.avg.forEach((v, i) => {
        if (v != null) other[i] = (other[i] ?? 0) + v;
      });
    }
    lines.push({ label: "Other", color: otherColor(), values: other });
    charts.get(p.id)!.setData(series.ts, lines);
  }

  for (const k of ["self.cpu", "self.mem"]) {
    charts.get(k)?.setData(series.ts, [{ label: "mac-monitor", color: slot(0), values: series.series[k]?.avg ?? [] }]);
  }
  renderOverhead();
}

function renderProcTable(): void {
  const body = $("body-proc-table");
  const rows = procRows
    .filter((r) => !procFilter || r.name.toLowerCase().includes(procFilter))
    .sort((a, b) => {
      const x = a[procSort.col];
      const y = b[procSort.col];
      const c = typeof x === "string" ? x.localeCompare(y as string) : (x as number) - (y as number);
      return procSort.desc ? -c : c;
    });
  const table = h("table", { class: "proc-table" });
  const head = h("tr");
  for (const col of PROC_COLUMNS) {
    const active = procSort.col === col.id;
    const th = h("th", {
      title: col.title,
      class: col.id === "name" ? "" : "num",
      "aria-sort": active ? (procSort.desc ? "descending" : "ascending") : "none",
    });
    const btn = h("button", { type: "button" }, col.label, active ? (procSort.desc ? " ↓" : " ↑") : "");
    btn.addEventListener("click", () => {
      procSort = { col: col.id, desc: procSort.col === col.id ? !procSort.desc : col.id !== "name" };
      renderProcTable();
    });
    th.append(btn);
    head.append(th);
  }
  table.append(h("thead", {}, head));
  const tbody = h("tbody");
  for (const r of rows) {
    const tr = h("tr", r.name === "other" ? { class: "other-row" } : {});
    for (const col of PROC_COLUMNS) {
      tr.append(h("td", { class: col.id === "name" ? "name" : "num" }, col.id === "name" && r.name === "other" ? "Other (small groups)" : col.fmt(r)));
    }
    tbody.append(tr);
  }
  table.append(tbody);
  const wrap = h("div", { class: "table-scroll" }, table);
  body.replaceChildren(rows.length ? wrap : h("p", { class: "muted pad" }, "No process data in this range."));
}

// ---------------------------------------------------------------------------------------------
// Events

let eventFilter = "all";
const EVENT_FILTERS: [string, string, (e: MonitorEvent) => boolean][] = [
  ["all", "All", () => true],
  ["alert", "Alerts", (e) => e.kind === "alert" || e.level !== "info"],
  ["pressure", "Pressure", (e) => e.kind === "pressure" || e.kind === "thermal"],
  ["process", "Processes", (e) => e.kind === "process"],
  ["system", "System", (e) => ["boot", "sleep", "monitor"].includes(e.kind)],
];

function buildEvents(grid: HTMLElement): void {
  const c = card("events", "Event log", "Boots, sleep, restarts of mac-monitor, pressure changes, long-running processes that exited, and alerts.", {
    wide: true,
    table: false,
  });
  const chips = h("div", { class: "chips", role: "group", "aria-label": "Filter events" });
  for (const [id, label] of EVENT_FILTERS) {
    const b = h("button", { type: "button", "aria-pressed": String(eventFilter === id), "data-filter": id }, label);
    b.addEventListener("click", () => {
      eventFilter = id;
      chips.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x.dataset.filter === id)));
      renderEvents();
    });
    chips.append(b);
  }
  c.querySelector(".card-actions")!.append(chips);
  grid.append(c);
}

const EVENT_ICONS: Record<string, string> = { boot: "⏻", sleep: "☾", monitor: "↻", pressure: "▤", thermal: "≋", process: "■", alert: "▲" };

function renderEvents(): void {
  const body = $("body-events");
  const pred = EVENT_FILTERS.find((f) => f[0] === eventFilter)![2];
  const list = events.filter(pred);
  if (!list.length) {
    body.replaceChildren(h("p", { class: "muted pad" }, "No events in this range."));
    return;
  }
  const ul = h("ul", { class: "events" });
  const span = view.to - view.from;
  for (const e of list.slice(0, 300)) {
    const lvl = e.level === "crit" ? "critical" : e.level === "warn" ? "warning" : "info";
    ul.append(
      h(
        "li",
        { class: `event event-${lvl}` },
        h("span", { class: "event-icon", "aria-hidden": "true" }, EVENT_ICONS[e.kind] ?? "•"),
        h("time", { datetime: new Date(e.ts * 1000).toISOString() }, F.time(e.ts, Math.max(span, 86401))),
        h("span", { class: "event-title" }, e.title, lvl !== "info" ? h("span", { class: `badge badge-${lvl}` }, lvl) : null),
        h("span", { class: "event-detail" }, e.detail),
      ),
    );
  }
  if (list.length > 300) ul.append(h("li", { class: "muted" }, `${list.length - 300} more not shown`));
  body.replaceChildren(ul);
}

async function loadEvents(from: number, to: number): Promise<void> {
  events = await api.events(from, to);
  renderEvents();
}

// ---------------------------------------------------------------------------------------------
// Monitor overhead

const MONITORS = /^(mac-monitor|macmon|victoria-metrics|vmagent|grafana|grafana-server|prometheus|node_exporter|Activity Monitor|telegraf|netdata)$/;

function buildOverhead(grid: HTMLElement): void {
  grid.append(card("self.cpu", "mac-monitor CPU", "CPU used by this monitor (100% = one core)."));
  grid.append(card("self.mem", "mac-monitor memory", "Memory footprint of this monitor."));
  grid.append(card("monitors", "Monitoring tools on this Mac", "CPU and memory of monitoring tools seen in the selected range.", { wide: true, table: false }));
  charts.set("self.cpu", new Chart($("body-self.cpu"), { unit: "cores", xRange, onZoom, height: 140 }));
  charts.set("self.mem", new Chart($("body-self.mem"), { unit: "B", xRange, onZoom, height: 140 }));
}

function renderOverhead(): void {
  const rows = procRows.filter((r) => MONITORS.test(r.name));
  const cpu = selfSummary["self.cpu"];
  const mem = selfSummary["self.mem"];
  const facts = h(
    "dl",
    { class: "facts" },
    h("dt", {}, "mac-monitor CPU (avg)"),
    h("dd", {}, cpu?.avg != null ? F.pct(cpu.avg) : "–"),
    h("dt", {}, "mac-monitor memory (latest)"),
    h("dd", {}, mem?.last != null ? F.bytes(mem.last) : "–"),
    h("dt", {}, "Database size"),
    h("dd", {}, F.bytes(meta.db_bytes)),
    h("dt", {}, "Sampling"),
    h("dd", {}, `every ${meta.sample_ms / 1000} s (${meta.live_sample_ms / 1000} s while live view is open)`),
    h("dt", {}, "Retention"),
    h("dd", {}, `15 s points for ${meta.raw_retention_days} days, 5 min points for ${meta.retention_days} days`),
    h("dt", {}, "Data directory"),
    h("dd", { class: "mono" }, meta.data_dir),
  );
  const table = h("table", { class: "proc-table" });
  table.append(
    h("thead", {}, h("tr", {}, h("th", {}, "Process"), h("th", { class: "num" }, "CPU avg"), h("th", { class: "num" }, "CPU peak"), h("th", { class: "num" }, "Memory avg"), h("th", { class: "num" }, "Energy"))),
  );
  const tbody = h("tbody");
  for (const r of rows.sort((a, b) => b.cpuAvg - a.cpuAvg)) {
    tbody.append(h("tr", {}, h("td", { class: "name" }, r.name), h("td", { class: "num" }, F.pct(r.cpuAvg)), h("td", { class: "num" }, F.pct(r.cpuPeak)), h("td", { class: "num" }, F.bytes(r.memAvg)), h("td", { class: "num" }, F.energy(r.energyWh))));
  }
  table.append(tbody);
  $("body-monitors").replaceChildren(h("div", { class: "overhead" }, facts, rows.length ? h("div", { class: "table-scroll" }, table) : h("p", { class: "muted" }, "No monitoring tools above the reporting threshold.")));
}

// ---------------------------------------------------------------------------------------------
// Refresh loop

function refreshInterval(): number | null {
  const [from, to] = currentWindow();
  if (state.preset === "custom" && to < now() - 60) return null;
  const span = to - from;
  if (span <= 6 * 3600) return 15_000;
  if (span <= 86400) return 30_000;
  if (span <= 7 * 86400) return 120_000;
  return 300_000;
}

async function refresh(): Promise<void> {
  if (refreshing || document.hidden) return;
  refreshing = true;
  const [from, to] = currentWindow();
  if (!isLive()) {
    view.from = from;
    view.to = to;
    setStatus("Updating…", "loading");
  }
  document.body.classList.add("loading");
  try {
    const metaP = api.meta().then((m) => (meta = m));
    await Promise.all([
      isLive() ? Promise.resolve() : loadHost(from, to),
      loadSummary(from, to),
      loadEnergy(from, to),
      loadProcs(from, to),
      loadEvents(from, to),
      metaP,
    ]);
    renderHeader();
    lastError = "";
    if (!isLive()) setStatus(`Updated ${F.time(now())}`, "ok");
  } catch (e) {
    lastError = e instanceof Error ? e.message : String(e);
    setStatus(`Error: ${lastError}`, "error");
  } finally {
    refreshing = false;
    document.body.classList.remove("loading");
  }
}

function schedule(): void {
  clearTimeout(timer);
  const every = refreshInterval();
  if (every == null || document.hidden) return;
  timer = window.setTimeout(async () => {
    await refresh();
    schedule();
  }, every);
}

function restart(): void {
  clearTimeout(timer);
  if (isLive()) {
    const t = now();
    view.from = t - 900;
    view.to = t;
    startLive();
  } else {
    stopLive();
  }
  void refresh().then(schedule);
}

function renderHeader(): void {
  const m = meta.machine;
  const uptime = m.boot_time ? `up ${F.duration(now() - m.boot_time)}` : "";
  $("host").textContent = m.hostname || "Mac";
  $("machine").textContent = [m.chip, m.memory_gb ? `${m.memory_gb} GB` : "", uptime].filter(Boolean).join(" · ");
  $("version").textContent = `v${meta.version}`;
}

async function main(): Promise<void> {
  setupControls();
  renderControls();
  try {
    meta = await api.meta();
  } catch (e) {
    setStatus(`Cannot reach mac-monitor: ${e instanceof Error ? e.message : e}`, "error");
    setTimeout(main, 5000);
    return;
  }
  renderHeader();
  buildLayout();
  restart();

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      clearTimeout(timer);
      stopLive();
    } else {
      restart();
    }
  });
  window.addEventListener("hashchange", () => {
    Object.assign(state, readHash());
    renderControls();
    restart();
  });
}

void main();
