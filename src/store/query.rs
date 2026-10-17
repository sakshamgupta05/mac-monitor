//! Range queries: picks a tier and step for the requested resolution and folds stored slots into
//! buckets of avg / min / max and covered seconds.

use serde::Serialize;

use super::{RAW, ROLLUP, Store, Tier};
use crate::catalog;

/// Steps buckets snap to, so refreshing a view keeps its bucket boundaries.
const NICE_STEPS: &[i64] = &[
  15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
  pub tier: Tier,
  pub step: i64,
  /// Start of the first bucket.
  pub first: i64,
  pub buckets: usize,
}

/// Plans a query over `[from, to]` with about `points` buckets, or buckets of `step` seconds.
/// Bucket boundaries are aligned to `align` seconds (a timezone offset for daily buckets).
pub fn plan(
  now: i64,
  raw_secs: i64,
  from: i64,
  to: i64,
  points: usize,
  step: Option<i64>,
  align: i64,
) -> Plan {
  let range = (to - from).max(1);
  let desired = step.unwrap_or_else(|| range / points.max(1) as i64).max(1);
  let in_raw = from >= now - raw_secs;
  let tier = if desired >= ROLLUP.interval || !in_raw { ROLLUP } else { RAW };

  let step = match step {
    Some(s) => round_up(s.max(tier.interval), tier.interval),
    None => NICE_STEPS
      .iter()
      .copied()
      .find(|s| *s >= desired && *s >= tier.interval && s % tier.interval == 0)
      .unwrap_or_else(|| round_up(desired, 86400)),
  };

  let first = (from - align).div_euclid(step) * step + align;
  let buckets = ((to - first) / step + 1).clamp(1, 100_000) as usize;
  Plan { tier, step, first, buckets }
}

fn round_up(v: i64, m: i64) -> i64 {
  (v + m - 1) / m * m
}

#[derive(Debug, Serialize)]
pub struct Series {
  pub avg: Vec<Option<f32>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub min: Option<Vec<Option<f32>>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub max: Option<Vec<Option<f32>>>,
  /// Seconds of each bucket with data; `avg * cov` integrates the series.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cov: Option<Vec<u32>>,
}

struct Acc {
  sum: Vec<f64>,
  cnt: Vec<u32>,
  min: Vec<f32>,
  max: Vec<f32>,
  last: Option<(i64, f32)>,
}

impl Acc {
  fn new(n: usize) -> Self {
    Acc {
      sum: vec![0.0; n],
      cnt: vec![0; n],
      min: vec![f32::INFINITY; n],
      max: vec![f32::NEG_INFINITY; n],
      last: None,
    }
  }
}

fn accumulate(store: &Store, key: &str, plan: &Plan, from: i64, to: i64) -> Acc {
  let mut acc = Acc::new(plan.buckets);
  let tier = plan.tier;
  let _ = store.for_each_chunk(key, tier, from, to, |start, k, data| {
    let n = data.len() / k;
    for i in 0..n {
      let avg = data[i];
      if avg.is_nan() {
        continue;
      }
      let ts = start + i as i64 * tier.interval;
      if ts < from || ts > to {
        continue;
      }
      let b = ((ts - plan.first) / plan.step) as usize;
      if b >= plan.buckets {
        continue;
      }
      let (lo, hi) = if k == 3 { (data[n + i], data[2 * n + i]) } else { (avg, avg) };
      acc.sum[b] += avg as f64;
      acc.cnt[b] += 1;
      acc.min[b] = acc.min[b].min(lo);
      acc.max[b] = acc.max[b].max(hi);
      if acc.last.is_none_or(|(t, _)| ts > t) {
        acc.last = Some((ts, avg));
      }
    }
  });
  acc
}

pub fn range(
  store: &Store,
  keys: &[String],
  plan: Plan,
  from: i64,
  to: i64,
  with_cov: bool,
  minmax: Option<&[String]>,
) -> Vec<(String, Series)> {
  let mut series = Vec::with_capacity(keys.len());
  for key in keys {
    let acc = accumulate(store, key, &plan, from, to);
    let avg = (0..plan.buckets)
      .map(|b| (acc.cnt[b] > 0).then(|| round_sig((acc.sum[b] / acc.cnt[b] as f64) as f32)))
      .collect();
    let band = !catalog::is_single(key) && minmax.is_none_or(|m| m.contains(key));
    let pick = |v: &[f32]| {
      (0..plan.buckets).map(|b| (acc.cnt[b] > 0).then(|| round_sig(v[b]))).collect::<Vec<_>>()
    };
    let cov = with_cov
      .then(|| acc.cnt.iter().map(|c| c * plan.tier.interval as u32).collect::<Vec<_>>());
    series.push((
      key.clone(),
      Series {
        avg,
        min: band.then(|| pick(&acc.min)),
        max: band.then(|| pick(&acc.max)),
        cov,
      },
    ));
  }
  series
}

/// Rounds to 4 significant digits, plenty for charts and much shorter as JSON.
pub fn round_sig(v: f32) -> f32 {
  if v == 0.0 || !v.is_finite() {
    return v;
  }
  let mag = 10f64.powi(3 - (v.abs() as f64).log10().floor() as i32);
  ((v as f64 * mag).round() / mag) as f32
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Summary {
  pub avg: Option<f32>,
  pub min: Option<f32>,
  pub max: Option<f32>,
  /// `∫ value dt` over the range, in value-seconds (joules for watts).
  pub integral: f64,
  /// Seconds of the range with data.
  pub cov: u32,
  pub last: Option<f32>,
  pub last_ts: Option<i64>,
}

/// One summary per key over `[from, to]`, from the raw tier when it covers the range.
pub fn summary(store: &Store, keys: &[String], now: i64, from: i64, to: i64) -> Vec<(String, Summary)> {
  let tier = if from >= now - store.retention.raw_secs { RAW } else { ROLLUP };
  let step = (to - from).max(1) + tier.interval;
  let plan = Plan { tier, step, first: from, buckets: 1 };
  keys
    .iter()
    .map(|key| {
      let acc = accumulate(store, key, &plan, from, to);
      let has = acc.cnt[0] > 0;
      let summary = Summary {
        avg: has.then(|| (acc.sum[0] / acc.cnt[0] as f64) as f32),
        min: has.then_some(acc.min[0]),
        max: has.then_some(acc.max[0]),
        integral: acc.sum[0] * tier.interval as f64,
        cov: acc.cnt[0] * tier.interval as u32,
        last: acc.last.map(|(_, v)| v),
        last_ts: acc.last.map(|(t, _)| t),
      };
      (key.clone(), summary)
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::temp_store;

  const NOW: i64 = 1_800_000_000;

  #[test]
  fn plan_picks_tier_and_nice_step() {
    let raw = 14 * 86400;
    let p = plan(NOW, raw, NOW - 3600, NOW, 600, None, 0);
    assert_eq!((p.tier, p.step), (RAW, 15));
    let p = plan(NOW, raw, NOW - 86400, NOW, 600, None, 0);
    // 144 s per point snaps up to the next nice step, still from the raw tier
    assert_eq!((p.tier, p.step), (RAW, 300));
  }

  #[test]
  fn plan_long_ranges_use_rollup() {
    let raw = 14 * 86400;
    let p = plan(NOW, raw, NOW - 30 * 86400, NOW, 1000, None, 0);
    assert_eq!(p.tier, ROLLUP);
    assert_eq!(p.step, 3600);
    // range starting outside raw retention needs the rollup even at fine steps
    let p = plan(NOW, raw, NOW - 20 * 86400, NOW - 20 * 86400 + 600, 100, None, 0);
    assert_eq!((p.tier, p.step), (ROLLUP, 300));
  }

  #[test]
  fn plan_explicit_step_and_alignment() {
    let p = plan(NOW, 14 * 86400, NOW - 7 * 86400, NOW, 0, Some(86400), -19800);
    assert_eq!(p.step, 86400);
    assert_eq!((p.first + 19800) % 86400, 0);
    assert!(p.first <= NOW - 7 * 86400);
    assert_eq!(p.buckets, 8);
  }

  #[test]
  fn rounds_to_four_digits() {
    assert_eq!(round_sig(41.6376), 41.64);
    assert_eq!(round_sig(34359738368.0), 3.436e10);
    assert_eq!(round_sig(0.0012346), 0.001235);
    assert_eq!(round_sig(0.0), 0.0);
  }

  #[test]
  fn range_buckets_and_integrates() {
    let (store, _dir) = temp_store();
    let t0 = NOW - NOW % 3600;
    for i in 0..240 {
      // 10 W on even slots, 20 W on odd ones; min/max spread by 5
      let v = if i % 2 == 0 { 10.0 } else { 20.0 };
      store.write("power.sys", RAW, t0 + i * 15, &[v, v - 5.0, v + 5.0]).unwrap();
    }
    store.write("proc.cpu:claude", RAW, t0, &[0.5]).unwrap();

    let keys = vec!["power.sys".to_string(), "proc.cpu:claude".to_string()];
    let p = plan(t0 + 3600, 86400, t0, t0 + 3599, 0, Some(60), 0);
    let r = range(&store, &keys, p, t0, t0 + 3599, true, None);
    assert_eq!(p.buckets, 60);
    let (_, sys) = &r[0];
    assert_eq!(sys.avg[0], Some(15.0));
    assert_eq!(sys.min.as_ref().unwrap()[0], Some(5.0));
    assert_eq!(sys.max.as_ref().unwrap()[0], Some(25.0));
    assert_eq!(sys.cov.as_ref().unwrap()[0], 60);
    let (_, cpu) = &r[1];
    assert!(cpu.min.is_none());
    assert_eq!(cpu.avg[0], Some(0.5));
    assert_eq!(cpu.avg[1], None);

    let s = summary(&store, &keys, t0 + 3600, t0, t0 + 3599);
    let sys = &s[0].1;
    assert_eq!(sys.avg, Some(15.0));
    assert_eq!(sys.max, Some(25.0));
    assert_eq!(sys.integral, 15.0 * 3600.0); // 15 W for an hour = 54 kJ
    assert_eq!(sys.cov, 3600);
    assert_eq!(sys.last, Some(20.0));
    assert_eq!(sys.last_ts, Some(t0 + 239 * 15));
  }
}

#[cfg(test)]
mod bench {
  use super::*;
  use crate::store::temp_store;
  use std::time::Instant;

  /// A year of 5 min data plus 14 days of 15 s data for 60 series:
  /// `cargo test --release query_year -- --ignored --nocapture`
  #[test]
  #[ignore]
  fn query_year() {
    let (store, dir) = temp_store();
    let now = 1_800_000_000;
    let keys: Vec<String> = (0..30)
      .map(|i| format!("proc.cpu:g{i}"))
      .chain((0..30).map(|i| if i == 0 { "power.sys".into() } else { format!("cpu.tier.active:T{i}") }))
      .collect();
    let t0 = Instant::now();
    for (n, key) in keys.iter().enumerate() {
      let single = crate::catalog::is_single(key);
      let mut ts = now - 365 * 86400;
      while ts < now {
        let v = ((ts / 300 + n as i64) % 97) as f32;
        let vals = if single { vec![v] } else { vec![v, v - 1.0, v + 1.0] };
        store.write(key, ROLLUP, ts, &vals).unwrap();
        if ts >= now - 14 * 86400 {
          for k in 0..20 {
            store.write(key, RAW, ts + k * 15, &vals).unwrap();
          }
        }
        ts += 300;
      }
      store.flush(now).unwrap();
    }
    eprintln!("wrote a year for {} series in {:?}", keys.len(), t0.elapsed());
    let size: u64 = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum();
    eprintln!("database size {:.1} MB", size as f64 / 1e6);

    for (label, from, points) in [("1y", now - 365 * 86400, 1000), ("30d", now - 30 * 86400, 1000), ("7d", now - 7 * 86400, 1000), ("24h", now - 86400, 1000)] {
      let t = Instant::now();
      let plan = plan(now, 14 * 86400, from, now, points, None, 0);
      let r = range(&store, &keys, plan, from, now, false, None);
      assert_eq!(r.len(), keys.len());
      let q = t.elapsed();
      let t = Instant::now();
      summary(&store, &keys, now, from, now);
      eprintln!("{label:>4}: range ({} buckets of {} s, {} series) {:?}, summary {:?}", plan.buckets, plan.step, keys.len(), q, t.elapsed());
    }
  }
}
