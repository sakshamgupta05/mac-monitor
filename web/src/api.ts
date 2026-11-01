// Typed client for the mac-monitor HTTP API.

export type Values = (number | null)[];

export interface Machine {
  chip: string;
  model: string;
  memory_gb: number;
  cpu_tiers: { label: string; cores: number; max_mhz: number }[];
  gpu_cores: number;
  gpu_max_mhz: number;
  fans: [string, number | null][];
  boot_time: number | null;
  hostname: string;
}

export interface DiskInfo {
  id: string;
  name: string;
  bsd: string;
  kind: string;
  connected: boolean;
}

export interface Meta {
  disks: DiskInfo[];
  version: string;
  machine: Machine;
  now: number;
  started: number;
  sample_ms: number;
  live_sample_ms: number;
  raw_retention_days: number;
  retention_days: number;
  data_dir: string;
  db_bytes: number;
  keys: string[];
}

export interface SeriesData {
  avg: Values;
  min?: Values;
  max?: Values;
  cov?: number[];
}

export interface RangeData {
  from: number;
  to: number;
  interval: number;
  step: number;
  ts: number[];
  series: Record<string, SeriesData>;
}

export interface Summary {
  avg: number | null;
  min: number | null;
  max: number | null;
  integral: number;
  cov: number;
  last: number | null;
  last_ts: number | null;
}

export interface MonitorEvent {
  id: number;
  ts: number;
  end: number | null;
  kind: string;
  level: "info" | "warn" | "crit";
  title: string;
  detail: string;
}

export interface LiveGroup {
  name: string;
  cpu: number;
  mem: number;
  power: number;
  gpu: number;
  disk_read: number;
  disk_write: number;
  count: number;
}

export interface LiveGroups {
  groups: LiveGroup[];
  groups_ts: number | null;
}

export interface LiveInit extends LiveGroups {
  disks?: DiskInfo[];
  process_sample_secs: number;
  keys: string[];
  sample_ms: number;
  rows: [number, Values][];
}

async function get<T>(path: string, params: Record<string, string | number | undefined> = {}): Promise<T> {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") qs.set(k, String(v));
  }
  const url = qs.size ? `${path}?${qs}` : path;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status}`);
  return (await res.json()) as T;
}

export const api = {
  meta: () => get<Meta>("/api/meta"),
  range: (p: {
    keys?: string[];
    match?: string[];
    from: number;
    to: number;
    points?: number;
    step?: number;
    align?: number;
    cov?: boolean;
    minmax?: string[];
  }) =>
    get<RangeData>("/api/query", {
      keys: p.keys?.join(","),
      match: p.match?.join(","),
      from: p.from,
      to: p.to,
      points: p.points,
      step: p.step,
      align: p.align,
      cov: p.cov ? 1 : undefined,
      minmax: p.minmax ? p.minmax.join(",") || "-" : undefined,
    }),
  summary: (p: { keys?: string[]; match?: string[]; from: number; to: number }) =>
    get<{ series: Record<string, Summary> }>("/api/summary", {
      keys: p.keys?.join(","),
      match: p.match?.join(","),
      from: p.from,
      to: p.to,
    }).then((r) => r.series),
  events: (from: number, to: number) =>
    get<{ events: MonitorEvent[] }>("/api/events", { from, to, limit: 1000 }).then((r) => r.events),
};
