// Value formatting. Units match the server catalog, plus a few display-only ones.

export type Unit =
  | "W" // watts
  | "ratio" // 0..1, shown as %
  | "cores" // 1 = one core, shown as % like Activity Monitor
  | "MHz"
  | "C"
  | "rpm"
  | "B"
  | "B/s"
  | "load"
  | "count"
  | "Wh";

const KB = 1024;
const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB"];

function trim(v: number, digits: number): string {
  return v.toFixed(digits).replace(/\.0+$/, "").replace(/(\.\d*?)0+$/, "$1");
}

/** Up to 3 significant digits: 4.25, 42.5, 425. */
function sig3(v: number): string {
  const a = Math.abs(v);
  if (a >= 100) return v.toFixed(0);
  if (a >= 10) return v.toFixed(1);
  return v.toFixed(2);
}

export function bytes(v: number): string {
  let i = 0;
  let x = Math.abs(v);
  while (x >= KB && i < BYTE_UNITS.length - 1) {
    x /= KB;
    i++;
  }
  const s = i === 0 ? x.toFixed(0) : sig3(x);
  return `${v < 0 ? "-" : ""}${s} ${BYTE_UNITS[i]}`;
}

export function pct(ratio: number): string {
  const p = ratio * 100;
  if (p !== 0 && Math.abs(p) < 1) return `${p.toFixed(1)}%`;
  return `${p.toFixed(p >= 100 ? 0 : p >= 10 ? 0 : 1)}%`;
}

export function energy(wh: number): string {
  if (Math.abs(wh) >= 1000) return `${sig3(wh / 1000)} kWh`;
  if (Math.abs(wh) >= 1) return `${sig3(wh)} Wh`;
  return `${trim(wh * 1000, 0)} mWh`;
}

export function money(usd: number): string {
  if (usd === 0) return "$0";
  if (Math.abs(usd) < 0.01) return "<$0.01";
  if (Math.abs(usd) < 100) return `$${usd.toFixed(2)}`;
  return `$${usd.toFixed(0)}`;
}

export function fmt(unit: Unit, v: number | null | undefined): string {
  if (v == null || !Number.isFinite(v)) return "–";
  switch (unit) {
    case "W":
      return `${sig3(v)} W`;
    case "ratio":
      return pct(v);
    case "cores":
      return pct(v);
    case "MHz":
      return v >= 1000 ? `${(v / 1000).toFixed(2)} GHz` : `${v.toFixed(0)} MHz`;
    case "C":
      return `${v.toFixed(1)} °C`;
    case "rpm":
      return `${v.toFixed(0)} rpm`;
    case "B":
      return bytes(v);
    case "B/s":
      return `${bytes(v)}/s`;
    case "load":
      return v.toFixed(2);
    case "count":
      return v.toFixed(0);
    case "Wh":
      return energy(v);
  }
}

/** Compact axis tick labels. */
export function axis(unit: Unit, v: number): string {
  if (v === 0) return "0";
  switch (unit) {
    case "ratio":
    case "cores":
      return `${trim(v * 100, 0)}%`;
    case "B":
      return bytes(v).replace(/\.0+ /, " ");
    case "B/s":
      return `${bytes(v).replace(/\.0+ /, " ")}/s`;
    case "MHz":
      return v >= 1000 ? `${trim(v / 1000, 1)}G` : `${trim(v, 0)}M`;
    case "W":
      return `${trim(v, 1)}W`;
    case "C":
      return `${trim(v, 0)}°`;
    case "Wh":
      return energy(v);
    default:
      return Math.abs(v) >= 1000 ? `${trim(v / 1000, 1)}K` : trim(v, 2);
  }
}

export function duration(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
const dateTimeFmt = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
});
const dateFmt = new Intl.DateTimeFormat(undefined, { weekday: "short", month: "short", day: "numeric" });

/** A timestamp (unix seconds), with the date when the range spans days. */
export function time(ts: number, spanSecs = 0): string {
  const d = new Date(ts * 1000);
  return spanSecs > 86400 ? dateTimeFmt.format(d) : timeFmt.format(d);
}

export function date(ts: number): string {
  return dateFmt.format(new Date(ts * 1000));
}

export function ago(ts: number, now = Date.now() / 1000): string {
  return `${duration(now - ts)} ago`;
}

/** Human step size: "minute", "5 min", "hour", "day", "week". */
export function stepName(secs: number): string {
  if (secs === 60) return "minute";
  if (secs === 3600) return "hour";
  if (secs === 86400) return "day";
  if (secs === 604800) return "week";
  if (secs < 3600) return `${secs / 60} min`;
  if (secs < 86400) return `${secs / 3600} hours`;
  return `${secs / 86400} days`;
}
