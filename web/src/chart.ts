// uPlot wrapper: line charts with min/max bands, stacked areas and bars, a legend that doubles
// as the value readout, a synced crosshair tooltip, drag-to-zoom, event markers and a table view.

import uPlot from "uplot";
import type { Values } from "./api";
import { type Unit, axis, fmt, time } from "./format";

export interface Line {
  label: string;
  color: string;
  values: Values;
  min?: Values;
  max?: Values;
}

export interface Marker {
  ts: number;
  end?: number | null;
  level: string;
  title: string;
}

export interface ChartOptions {
  unit: Unit;
  kind?: "line" | "stack" | "bars";
  y?: "zero" | "auto" | "ratio";
  height?: number;
  /** Fixed x range, so a view's empty edges show as gaps. */
  xRange?: () => [number, number];
  onZoom?: (from: number, to: number) => void;
  markers?: () => Marker[];
  /** Extra tooltip line for a data index (e.g. cost of an energy bar). */
  tooltipExtra?: (idx: number) => string | null;
  /** Label for the stacked total row. */
  totalLabel?: string;
}

/** Tick steps for byte axes: 1, 2, 4 … 512 of each binary unit. */
const BINARY_INCRS: number[] = [];
for (let k = 0; k < 6; k++) for (let m = 1; m < 1024; m *= 2) BINARY_INCRS.push(m * 1024 ** k);

export function css(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function hexToRgb(hex: string): [number, number, number] {
  const h = hex.replace("#", "");
  const n = parseInt(h.length === 3 ? h.replace(/./g, "$&$&") : h, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

export function alpha(hex: string, a: number): string {
  const [r, g, b] = hexToRgb(hex);
  return `rgba(${r},${g},${b},${a})`;
}

/** Opaque mix of `hex` over `surface`, for stacked fills that must not compound. */
function mix(hex: string, surface: string, a: number): string {
  const c = hexToRgb(hex);
  const s = hexToRgb(surface);
  const m = c.map((v, i) => Math.round(v * a + s[i] * (1 - a)));
  return `rgb(${m[0]},${m[1]},${m[2]})`;
}

function niceCeil(v: number): number {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) {
    if (m * p >= v) return m * p;
  }
  return 10 * p;
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

export class Chart {
  readonly root: HTMLElement;
  private readonly legendEl: HTMLDivElement;
  private readonly plotEl: HTMLDivElement;
  private readonly emptyEl: HTMLDivElement;
  private readonly tableEl: HTMLDivElement;
  private readonly tipEl: HTMLDivElement;
  private u: uPlot | null = null;
  private ts: number[] = [];
  private lines: Line[] = [];
  /** Plot column of each visible line, for the readout. */
  private columns: { line: Line; col: number }[] = [];
  private signature = "";
  private hidden = new Set<string>();
  private tableShown = false;
  private readonly ro: ResizeObserver;

  constructor(root: HTMLElement, private readonly o: ChartOptions) {
    this.root = root;
    root.classList.add("chart");
    this.legendEl = el("div", "legend");
    this.plotEl = el("div", "plot");
    this.emptyEl = el("div", "empty", "No data in this range");
    this.tableEl = el("div", "table-view");
    this.tipEl = el("div", "tip");
    this.plotEl.append(this.emptyEl);
    root.append(this.legendEl, this.plotEl, this.tableEl);
    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(this.plotEl);
  }

  get height(): number {
    return this.o.height ?? 200;
  }

  setData(ts: number[], lines: Line[]): void {
    this.ts = ts;
    this.lines = lines;
    const visible = lines.filter((l) => !this.hidden.has(l.label));
    const sig =
      `${this.o.kind}|${[...this.hidden].join(",")}|` +
      visible.map((l) => `${l.label}:${l.color}:${l.min ? 1 : 0}`).join(";");
    const data = this.buildData(visible);
    if (!this.u || sig !== this.signature) {
      this.signature = sig;
      this.build(visible, data);
    } else {
      this.u.setData(data);
    }
    const hasData = lines.some((l) => l.values.some((v) => v != null));
    this.emptyEl.style.display = hasData ? "none" : "";
    this.renderLegend();
    this.updateReadout(null);
    if (this.tableShown) this.renderTable();
  }

  /** Re-creates the plot, e.g. after a theme change. */
  rebuild(): void {
    this.signature = "";
    this.setData(this.ts, this.lines);
  }

  toggleTable(): boolean {
    this.tableShown = !this.tableShown;
    this.root.classList.toggle("show-table", this.tableShown);
    if (this.tableShown) this.renderTable();
    return this.tableShown;
  }

  csv(): string {
    const head = ["time", ...this.lines.map((l) => l.label)];
    const rows = this.ts.map((t, i) => [new Date(t * 1000).toISOString(), ...this.lines.map((l) => l.values[i] ?? "")]);
    return [head, ...rows].map((r) => r.map((c) => `"${String(c).replace(/"/g, '""')}"`).join(",")).join("\n");
  }

  destroy(): void {
    this.ro.disconnect();
    this.u?.destroy();
  }

  private buildData(visible: Line[]): uPlot.AlignedData {
    const cols: Values[] = [];
    this.columns = [];
    if (this.o.kind === "stack") {
      // cumulative from the bottom line up; drawn top-first so each fill covers the one below
      const sums: Values[] = [];
      let acc: Values = this.ts.map(() => null);
      for (const l of visible) {
        acc = acc.map((a, i) => {
          const v = l.values[i];
          return v == null && a == null ? null : (a ?? 0) + (v ?? 0);
        });
        sums.push(acc);
      }
      for (let i = visible.length - 1; i >= 0; i--) cols.push(sums[i]);
      visible.forEach((line, i) => this.columns.push({ line, col: visible.length - i }));
    } else {
      for (const line of visible) {
        cols.push(line.values);
        this.columns.push({ line, col: cols.length });
        if (line.min && line.max) cols.push(line.min, line.max);
      }
    }
    return [this.ts, ...cols] as uPlot.AlignedData;
  }

  private build(visible: Line[], data: uPlot.AlignedData): void {
    this.u?.destroy();
    const surface = css("--chart-surface");
    const muted = css("--ink-muted");
    const grid = css("--grid");
    const font = `11px ${css("--font")}`;
    const series: uPlot.Series[] = [{}];
    const bands: uPlot.Band[] = [];
    const kind = this.o.kind ?? "line";

    if (kind === "stack") {
      for (let i = visible.length - 1; i >= 0; i--) {
        const l = visible[i];
        series.push({ label: l.label, stroke: l.color, width: 1.5, fill: mix(l.color, surface, 0.42), points: { show: false } });
      }
    } else if (kind === "bars") {
      for (const l of visible) {
        series.push({
          label: l.label,
          stroke: l.color,
          fill: l.color,
          width: 0,
          paths: uPlot.paths.bars!({ size: [0.7, 24], radius: 0.15 }),
          points: { show: false },
        });
      }
    } else {
      for (const l of visible) {
        const idx = series.length;
        series.push({ label: l.label, stroke: l.color, width: 2, points: { show: false } });
        if (l.min && l.max) {
          series.push({ label: `${l.label} min`, stroke: "transparent", width: 0, points: { show: false } });
          series.push({ label: `${l.label} max`, stroke: "transparent", width: 0, points: { show: false } });
          bands.push({ series: [idx + 2, idx + 1], fill: alpha(l.color, 0.14) });
        }
      }
    }

    const yMode = this.o.y ?? "zero";
    const yRange: uPlot.Range.Function = (_u, min, max) => {
      if (yMode === "ratio") return [0, Math.max(1, max ?? 1)];
      if (min == null || max == null || !Number.isFinite(min) || !Number.isFinite(max)) return [0, 1];
      if (yMode === "auto") {
        const pad = Math.max((max - min) * 0.15, 1);
        return [Math.floor(min - pad), Math.ceil(max + pad)];
      }
      return [Math.min(0, min), niceCeil(max * 1.05)];
    };

    const opts: uPlot.Options = {
      width: Math.max(200, this.plotEl.clientWidth),
      height: this.height,
      series,
      bands,
      legend: { show: false },
      padding: [8, 8, 0, 0],
      scales: {
        x: { time: true, range: this.o.xRange ? () => this.o.xRange!() : undefined },
        y: { range: yRange },
      },
      axes: [
        { stroke: muted, font, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid, width: 1, size: 4 }, space: 70 },
        {
          stroke: muted,
          font,
          size: 58,
          incrs: this.o.unit === "B" || this.o.unit === "B/s" ? BINARY_INCRS : undefined,
          space: 28,
          grid: { stroke: grid, width: 1 },
          ticks: { show: false },
          values: (_u, splits) => splits.map((v) => axis(this.o.unit, v)),
        },
      ],
      cursor: {
        sync: { key: "mm" },
        drag: { x: !!this.o.onZoom, y: false, setScale: false },
        points: { show: false },
      },
      hooks: {
        setCursor: [(u) => this.updateReadout(u.cursor.idx ?? null)],
        setSelect: [
          (u) => {
            if (u.select.width > 8 && this.o.onZoom) {
              const from = u.posToVal(u.select.left, "x");
              const to = u.posToVal(u.select.left + u.select.width, "x");
              u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
              this.o.onZoom(Math.floor(from), Math.ceil(to));
            }
          },
        ],
        draw: [(u) => this.drawMarkers(u)],
      },
    };
    this.u = new uPlot(opts, data, this.plotEl);
    this.u.over.append(this.tipEl);
    this.u.over.addEventListener("mouseleave", () => this.updateReadout(null));
  }

  private drawMarkers(u: uPlot): void {
    const markers = this.o.markers?.() ?? [];
    if (!markers.length) return;
    const ctx = u.ctx;
    const { top, height, left, width } = u.bbox;
    const colors: Record<string, string> = { warn: css("--status-warning"), crit: css("--status-critical") };
    ctx.save();
    for (const m of markers) {
      const x = u.valToPos(m.ts, "x", true);
      if (m.end && m.end > m.ts) {
        const x2 = u.valToPos(m.end, "x", true);
        const a = Math.max(left, x);
        const b = Math.min(left + width, x2);
        if (b > a) {
          ctx.fillStyle = alpha(css("--ink-muted").startsWith("#") ? css("--ink-muted") : "#898781", 0.12);
          ctx.fillRect(a, top, b - a, height);
        }
        continue;
      }
      if (x < left || x > left + width) continue;
      ctx.strokeStyle = colors[m.level] ?? css("--axis");
      ctx.lineWidth = Math.max(1, devicePixelRatio);
      ctx.beginPath();
      ctx.moveTo(Math.round(x) + 0.5, top);
      ctx.lineTo(Math.round(x) + 0.5, top + height);
      ctx.stroke();
    }
    ctx.restore();
  }

  private renderLegend(): void {
    const frag = document.createDocumentFragment();
    for (const line of this.lines) {
      const item = el("button", "legend-item");
      item.type = "button";
      const off = this.hidden.has(line.label);
      item.classList.toggle("off", off);
      item.setAttribute("aria-pressed", String(!off));
      item.title = off ? `Show ${line.label}` : `Hide ${line.label}`;
      const key = el("span", this.o.kind === "line" || !this.o.kind ? "key-line" : "key-box");
      key.style.background = line.color;
      const label = el("span", "legend-label", line.label);
      const value = el("span", "legend-value");
      value.dataset.label = line.label;
      item.append(key, label, value);
      item.addEventListener("click", () => {
        if (this.hidden.has(line.label)) this.hidden.delete(line.label);
        else if (this.hidden.size < this.lines.length - 1) this.hidden.add(line.label);
        this.setData(this.ts, this.lines);
      });
      frag.append(item);
    }
    this.legendEl.replaceChildren(frag);
  }

  /** Legend values at the cursor (or the latest values) and the tooltip. */
  private updateReadout(idx: number | null): void {
    const unit = this.o.unit;
    const at = (vals: Values) => {
      if (idx != null) return vals[idx];
      for (let i = vals.length - 1; i >= 0; i--) if (vals[i] != null) return vals[i];
      return null;
    };
    for (const v of this.legendEl.querySelectorAll<HTMLElement>(".legend-value")) {
      const line = this.lines.find((l) => l.label === v.dataset.label);
      v.textContent = line && !this.hidden.has(line.label) ? fmt(unit, at(line.values)) : "";
    }

    const u = this.u;
    if (idx == null || !u || u.cursor.left == null || u.cursor.left < 0) {
      this.tipEl.style.display = "none";
      return;
    }
    const span = (this.ts[this.ts.length - 1] ?? 0) - (this.ts[0] ?? 0);
    const tip = document.createDocumentFragment();
    tip.append(el("div", "tip-time", time(this.ts[idx], span)));
    const rows = this.o.kind === "stack" ? [...this.columns].reverse() : this.columns;
    let total = 0;
    for (const { line } of rows) {
      const v = line.values[idx];
      total += v ?? 0;
      const row = el("div", "tip-row");
      const key = el("span", "key-line");
      key.style.background = line.color;
      row.append(key, el("strong", "", fmt(unit, v)), el("span", "tip-label", line.label));
      if (line.min && line.max && line.min[idx] != null && line.max[idx] != null && line.min[idx] !== line.max[idx]) {
        row.append(el("span", "tip-range", `${fmt(unit, line.min[idx])} – ${fmt(unit, line.max[idx])}`));
      }
      tip.append(row);
    }
    if (this.o.kind === "stack" && rows.length > 1) {
      const row = el("div", "tip-row tip-total");
      row.append(el("span", "key-none"), el("strong", "", fmt(unit, total)), el("span", "tip-label", this.o.totalLabel ?? "Total"));
      tip.append(row);
    }
    const extra = this.o.tooltipExtra?.(idx);
    if (extra) tip.append(el("div", "tip-extra", extra));
    this.tipEl.replaceChildren(tip);
    this.tipEl.style.display = "block";

    const w = this.tipEl.offsetWidth;
    const h = this.tipEl.offsetHeight;
    const overW = u.over.clientWidth;
    const left = u.cursor.left + 14 + w > overW ? u.cursor.left - w - 14 : u.cursor.left + 14;
    const top = Math.min(Math.max(0, (u.cursor.top ?? 0) - h / 2), Math.max(0, u.over.clientHeight - h));
    this.tipEl.style.transform = `translate(${Math.max(0, left)}px, ${top}px)`;
  }

  private renderTable(): void {
    const table = el("table");
    const head = el("tr");
    head.append(el("th", "", "Time"));
    for (const l of this.lines) head.append(el("th", "", l.label));
    table.append(el("thead"), el("tbody"));
    table.tHead!.append(head);
    const span = (this.ts[this.ts.length - 1] ?? 0) - (this.ts[0] ?? 0);
    for (let i = this.ts.length - 1; i >= 0; i--) {
      if (this.lines.every((l) => l.values[i] == null)) continue;
      const tr = el("tr");
      tr.append(el("td", "", time(this.ts[i], span)));
      for (const l of this.lines) tr.append(el("td", "num", fmt(this.o.unit, l.values[i])));
      table.tBodies[0].append(tr);
    }
    this.tableEl.replaceChildren(table);
  }

  private resize(): void {
    if (!this.u) return;
    const width = Math.max(200, this.plotEl.clientWidth);
    if (width !== this.u.width) this.u.setSize({ width, height: this.height });
  }
}

/** Level timeline (memory / thermal pressure): one colored segment per bucket. */
export class Strip {
  private readonly canvas: HTMLCanvasElement;
  private readonly tip: HTMLDivElement;
  private ts: number[] = [];
  private levels: Values = [];
  private step = 15;

  constructor(
    root: HTMLElement,
    private readonly names: string[],
    private readonly colors: () => string[],
    private readonly xRange: () => [number, number],
  ) {
    root.classList.add("strip");
    this.canvas = el("canvas");
    this.canvas.setAttribute("role", "img");
    this.tip = el("div", "tip");
    root.append(this.canvas, this.tip);
    new ResizeObserver(() => this.draw()).observe(root);
    this.canvas.addEventListener("mousemove", (e) => this.hover(e));
    this.canvas.addEventListener("mouseleave", () => (this.tip.style.display = "none"));
  }

  setData(ts: number[], levels: Values, step: number): void {
    this.ts = ts;
    this.levels = levels;
    this.step = step;
    this.draw();
  }

  draw(): void {
    const c = this.canvas;
    const w = c.parentElement!.clientWidth;
    const h = 14;
    const dpr = devicePixelRatio;
    c.width = w * dpr;
    c.height = h * dpr;
    c.style.width = `${w}px`;
    c.style.height = `${h}px`;
    const ctx = c.getContext("2d")!;
    ctx.scale(dpr, dpr);
    const [from, to] = this.xRange();
    const x = (t: number) => ((t - from) / (to - from)) * w;
    ctx.fillStyle = css("--grid");
    ctx.beginPath();
    ctx.roundRect(0, 0, w, h, 4);
    ctx.fill();
    const colors = this.colors();
    let worst = 0;
    ctx.save();
    ctx.beginPath();
    ctx.roundRect(0, 0, w, h, 4);
    ctx.clip();
    this.ts.forEach((t, i) => {
      const lv = this.levels[i];
      if (lv == null) return;
      const level = Math.round(lv);
      worst = Math.max(worst, level);
      ctx.fillStyle = colors[Math.min(level, colors.length - 1)];
      const a = x(t);
      ctx.fillRect(a, 0, Math.max(1, x(t + this.step) - a), h);
    });
    ctx.restore();
    c.setAttribute("aria-label", `Worst level in range: ${this.names[worst] ?? "unknown"}`);
  }

  private hover(e: MouseEvent): void {
    const [from, to] = this.xRange();
    const rect = this.canvas.getBoundingClientRect();
    const t = from + ((e.clientX - rect.left) / rect.width) * (to - from);
    const i = this.ts.findIndex((ts) => t >= ts && t < ts + this.step);
    const lv = i >= 0 ? this.levels[i] : null;
    if (lv == null) {
      this.tip.style.display = "none";
      return;
    }
    this.tip.replaceChildren(el("div", "tip-time", time(this.ts[i], to - from)), el("strong", "", this.names[Math.round(lv)] ?? "?"));
    this.tip.style.display = "block";
    const left = Math.min(e.clientX - rect.left + 12, rect.width - this.tip.offsetWidth);
    this.tip.style.transform = `translate(${Math.max(0, left)}px, 18px)`;
  }
}

/** Minimal sparkline for stat tiles. */
export function sparkline(canvas: HTMLCanvasElement, values: Values, color: string): void {
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (!w || !h) return;
  const dpr = devicePixelRatio;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const ctx = canvas.getContext("2d")!;
  ctx.scale(dpr, dpr);
  const nums = values.filter((v): v is number => v != null);
  if (nums.length < 2) return;
  let lo = Math.min(...nums);
  let hi = Math.max(...nums);
  if (hi - lo < 1e-9) {
    lo -= 1;
    hi += 1;
  }
  const n = values.length;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.beginPath();
  let pen = false;
  values.forEach((v, i) => {
    if (v == null) {
      pen = false;
      return;
    }
    const x = (i / (n - 1)) * (w - 2) + 1;
    const y = h - 2 - ((v - lo) / (hi - lo)) * (h - 4);
    if (pen) ctx.lineTo(x, y);
    else ctx.moveTo(x, y);
    pen = true;
  });
  ctx.stroke();
}
