//! The sampling loop: macmon and host metrics every `sample_ms` in all modes, folded into
//! 15 s slots (avg/min/max) and 5 min rollups; process groups once
//! per slot (every 5 s with live viewers); live rows for the dashboard.

pub mod alerts;
pub mod host;
mod iokit;
pub mod procs;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::catalog::key;
use crate::store::{RAW, ROLLUP, Store};
use alerts::{AlertConfig, Alerts};
use procs::{GroupStat, Procs, Scan};

pub const LIVE_PROC_SECS: u64 = 5;

const FLUSH_SECS: i64 = 60;
const PRUNE_SECS: i64 = 3600;
/// Wall time running ahead of monotonic time by more than this means the Mac slept.
const SLEEP_GAP_SECS: f64 = 5.0;

pub fn now_secs() -> i64 {
  now_f64() as i64
}

pub fn now_f64() -> f64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TierInfo {
  pub label: String,
  pub cores: u8,
  pub max_mhz: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MachineInfo {
  pub chip: String,
  pub model: String,
  pub memory_gb: u16,
  pub cpu_tiers: Vec<TierInfo>,
  pub gpu_cores: u8,
  pub gpu_max_mhz: u32,
  pub fans: Vec<(String, Option<u32>)>,
  pub boot_time: Option<i64>,
  pub hostname: String,
}

#[derive(Clone, Debug)]
pub enum LiveMsg {
  Keys(Arc<Vec<String>>),
  Row(i64, Arc<Vec<f32>>),
  Groups(i64, Arc<Vec<GroupStat>>),
}

/// Recent system samples for the live view, in a fixed key order that only grows.
#[derive(Default)]
pub struct Live {
  pub keys: Vec<String>,
  index: HashMap<String, usize>,
  pub rows: VecDeque<(i64, Arc<Vec<f32>>)>,
  pub groups: Vec<GroupStat>,
  pub groups_ts: Option<i64>,
  /// Milliseconds of rows to keep.
  keep_ms: i64,
}

impl Live {
  pub fn latest(&self) -> impl Iterator<Item = (&str, f32)> {
    let row = self.rows.back().map(|(_, r)| r.clone());
    self.keys.iter().enumerate().filter_map(move |(i, k)| {
      let v = *row.as_ref()?.get(i)?;
      (!v.is_nan()).then_some((k.as_str(), v))
    })
  }

  fn push(&mut self, ts_ms: i64, values: &[(String, f32)]) -> (Option<Arc<Vec<String>>>, Arc<Vec<f32>>) {
    let mut grew = false;
    for (k, _) in values {
      if !self.index.contains_key(k) {
        self.index.insert(k.clone(), self.keys.len());
        self.keys.push(k.clone());
        grew = true;
      }
    }
    let mut row = vec![f32::NAN; self.keys.len()];
    for (k, v) in values {
      row[self.index[k]] = *v;
    }
    let row = Arc::new(row);
    self.rows.push_back((ts_ms, row.clone()));
    while self.rows.front().is_some_and(|(t, _)| *t < ts_ms - self.keep_ms) {
      self.rows.pop_front();
    }
    (grew.then(|| Arc::new(self.keys.clone())), row)
  }
}

pub struct Shared {
  pub live: Mutex<Live>,
  pub tx: broadcast::Sender<LiveMsg>,
  pub machine: Mutex<MachineInfo>,
  pub started: i64,
  pub last_sample: AtomicI64,
  pub shutdown: AtomicBool,
  /// Open live views; only process scans accelerate while there are any.
  pub live_clients: AtomicUsize,
}

impl Shared {
  pub fn new(live_secs: u64) -> Arc<Self> {
    let (tx, _) = broadcast::channel(64);
    Arc::new(Shared {
      live: Mutex::new(Live { keep_ms: live_secs as i64 * 1000, ..Default::default() }),
      tx,
      machine: Mutex::new(MachineInfo::default()),
      started: now_secs(),
      last_sample: AtomicI64::new(0),
      shutdown: AtomicBool::new(false),
      live_clients: AtomicUsize::new(0),
    })
  }
}

/// Counts an open live view for as long as it is alive.
pub struct LiveClient(Arc<Shared>);

impl LiveClient {
  pub fn new(shared: Arc<Shared>) -> Self {
    shared.live_clients.fetch_add(1, Ordering::Relaxed);
    LiveClient(shared)
  }
}

impl Drop for LiveClient {
  fn drop(&mut self) {
    self.0.live_clients.fetch_sub(1, Ordering::Relaxed);
  }
}

#[derive(Debug, Clone)]
pub struct CollectorConfig {
  pub sample_ms: u64,
  pub alerts: AlertConfig,
}

#[derive(Default)]
struct BandAcc {
  sum: f64,
  n: u32,
  min: f32,
  max: f32,
}

impl BandAcc {
  fn add(&mut self, avg: f32, min: f32, max: f32) {
    if self.n == 0 {
      (self.min, self.max) = (min, max);
    } else {
      self.min = self.min.min(min);
      self.max = self.max.max(max);
    }
    self.sum += avg as f64;
    self.n += 1;
  }

  fn values(&self) -> [f32; 3] {
    [(self.sum / self.n.max(1) as f64) as f32, self.min, self.max]
  }
}

/// Preserve a full slot's usage when live scans split the counter deltas into shorter pieces.
/// Missing groups contribute zero; counts are from the most recent scan.
#[derive(Default)]
struct ProcessWindow {
  seconds: f64,
  groups: BTreeMap<String, ([f64; 6], u32)>,
  self_cpu_seconds: f64,
}

impl ProcessWindow {
  fn add(&mut self, scan: &Scan) {
    let dt = scan.elapsed_secs;
    self.seconds += dt;
    self.self_cpu_seconds += scan.self_cpu as f64 * dt;
    for (_, count) in self.groups.values_mut() {
      *count = 0;
    }
    for g in &scan.groups {
      let (sums, count) = self.groups.entry(g.name.clone()).or_default();
      for (sum, value) in sums.iter_mut().zip([g.cpu, g.mem, g.power, g.gpu, g.disk_read, g.disk_write]) {
        *sum += value as f64 * dt;
      }
      *count = g.count;
    }
  }

  fn finish(self, mut latest: Scan) -> Scan {
    let seconds = self.seconds.max(f64::EPSILON);
    latest.self_cpu = (self.self_cpu_seconds / seconds) as f32;
    latest.groups = self
      .groups
      .into_iter()
      .map(|(name, (sums, count))| {
        let [cpu, mem, power, gpu, disk_read, disk_write] = sums.map(|v| (v / seconds) as f32);
        GroupStat { name, cpu, mem, power, gpu, disk_read, disk_write, count }
      })
      .collect();
    latest
  }
}

/// Requires the level to hold for two slots before reporting a change.
#[derive(Default)]
struct Debounced {
  level: Option<i32>,
  candidate: Option<(i32, u8)>,
}

impl Debounced {
  fn update(&mut self, level: i32) -> Option<(Option<i32>, i32)> {
    if self.level == Some(level) {
      self.candidate = None;
      return None;
    }
    if self.level.is_none() {
      self.level = Some(level);
      return None;
    }
    let seen = match self.candidate {
      Some((l, n)) if l == level => n + 1,
      _ => 1,
    };
    self.candidate = Some((level, seen));
    if seen >= 2 {
      let prev = self.level.replace(level);
      self.candidate = None;
      return Some((prev, level));
    }
    None
  }
}

pub struct Collector {
  store: Arc<Store>,
  shared: Arc<Shared>,
  cfg: CollectorConfig,
  host: host::Host,
  procs: Procs,
  proc_window: ProcessWindow,
  last_proc_scan: Instant,
  slot: Option<i64>,
  acc: BTreeMap<String, BandAcc>,
  roll_slot: i64,
  roll: BTreeMap<String, BandAcc>,
  alerts: Alerts,
  pressure: Debounced,
  thermal: Debounced,
  last_flush: i64,
  last_prune: i64,
}

impl Collector {
  pub fn new(store: Arc<Store>, shared: Arc<Shared>, cfg: CollectorConfig) -> Self {
    let alerts = Alerts::new(cfg.alerts.clone());
    let now = now_secs();
    Collector {
      store,
      shared,
      cfg,
      host: host::Host::new(),
      procs: Procs::new(),
      proc_window: ProcessWindow::default(),
      last_proc_scan: Instant::now(),
      slot: None,
      acc: BTreeMap::new(),
      roll_slot: 0,
      roll: BTreeMap::new(),
      alerts,
      pressure: Debounced::default(),
      thermal: Debounced::default(),
      last_flush: now,
      last_prune: 0,
    }
  }

  pub fn run(mut self) {
    let mut sampler = match macmon::Sampler::new() {
      Ok(s) => Some(s),
      Err(e) => {
        eprintln!("mac-monitor: macmon sampler unavailable ({e}); recording host metrics only");
        None
      }
    };
    self.record_machine(sampler.as_ref());
    self.record_startup();
    // Baseline for process counters, so the first slot has rates.
    self.procs.scan(now_secs());
    self.last_proc_scan = Instant::now();

    let mut values = Vec::with_capacity(96);
    while !self.shared.shutdown.load(Ordering::Relaxed) {
      let sample_ms = self.cfg.sample_ms;
      let wall0 = now_f64();
      let mono0 = Instant::now();
      values.clear();
      match sampler.as_mut().map(|s| s.get_metrics(sample_ms as u32)) {
        Some(Ok(m)) => {
          macmon_values(&m, &mut values);
          let mut machine = self.shared.machine.lock().unwrap();
          if machine.fans.len() != m.fans.len() {
            machine.fans = m.fans.iter().map(|f| (f.name.clone(), f.max_rpm)).collect();
          }
        }
        Some(Err(e)) => {
          eprintln!("mac-monitor: macmon sample failed: {e}");
          std::thread::sleep(Duration::from_millis(sample_ms));
        }
        None => std::thread::sleep(Duration::from_millis(sample_ms)),
      }
      let wall = now_f64();
      let slept = (wall - wall0) - mono0.elapsed().as_secs_f64();
      if slept > SLEEP_GAP_SECS {
        // The sample spans the sleep; drop it.
        self.on_sleep(wall0 as i64, wall as i64);
        continue;
      }

      self.host.sample(&mut values);
      self.shared.last_sample.store(wall as i64, Ordering::Relaxed);
      self.publish_live((wall * 1000.0) as i64, &values);

      let mid = (wall - sample_ms as f64 / 2000.0) as i64;
      let slot = mid - mid.rem_euclid(RAW.interval);
      if let Some(prev) = self.slot
        && prev != slot
      {
        self.close_slot(prev);
      } else if self.shared.live_clients.load(Ordering::Relaxed) > 0
        && self.last_proc_scan.elapsed() >= Duration::from_secs(LIVE_PROC_SECS)
      {
        self.scan_procs();
      }
      self.slot = Some(slot);
      for (k, v) in &values {
        if v.is_finite() {
          self.acc.entry(k.clone()).or_default().add(*v, *v, *v);
        }
      }
      self.housekeeping(wall as i64);
    }

    if let Some(slot) = self.slot.take() {
      self.close_slot(slot);
    }
    if let Err(e) = self.store.flush(now_secs()) {
      eprintln!("mac-monitor: final flush failed: {e}");
    }
  }

  fn record_machine(&self, sampler: Option<&macmon::Sampler>) {
    let mut info = MachineInfo { boot_time: host::boot_time(), hostname: hostname(), ..Default::default() };
    if let Some(s) = sampler {
      let soc = s.get_soc_info();
      info.chip = soc.chip_name.clone();
      info.model = soc.mac_model.clone();
      info.memory_gb = soc.memory_gb;
      info.gpu_cores = soc.gpu_cores;
      info.gpu_max_mhz = soc.gpu_freqs.iter().copied().max().unwrap_or(0);
      info.cpu_tiers = soc
        .cpu_tiers
        .iter()
        .map(|t| TierInfo {
          label: t.label.clone(),
          cores: t.cores,
          max_mhz: t.freqs.iter().copied().max().unwrap_or(0),
        })
        .collect();
    }
    *self.shared.machine.lock().unwrap() = info;
  }

  fn record_startup(&self) {
    let now = now_secs();
    let version = env!("CARGO_PKG_VERSION");
    let _ = self.store.add_event(now, None, "monitor", "info", &format!("mac-monitor {version} started"), "");
    if let Some(boot) = host::boot_time() {
      let known = self.store.get_meta("boot_time").ok().flatten();
      if known.as_deref() != Some(&boot.to_string()) {
        let _ = self.store.add_event(boot, None, "boot", "info", "System booted", "");
        let _ = self.store.set_meta("boot_time", &boot.to_string());
      }
    }
  }

  fn on_sleep(&mut self, from: i64, to: i64) {
    if let Some(slot) = self.slot.take() {
      self.close_slot(slot);
    }
    let title = format!("Slept for {}", human_duration(to - from));
    let _ = self.store.add_event(from, Some(to), "sleep", "info", &title, "");
    // Rates across the sleep would be meaningless.
    self.host = host::Host::new();
    self.procs.scan(to);
    self.last_proc_scan = Instant::now();
    self.proc_window = ProcessWindow::default();
  }

  fn publish_live(&self, ts_ms: i64, values: &[(String, f32)]) {
    let (keys, row) = self.shared.live.lock().unwrap().push(ts_ms, values);
    if let Some(keys) = keys {
      let _ = self.shared.tx.send(LiveMsg::Keys(keys));
    }
    let _ = self.shared.tx.send(LiveMsg::Row(ts_ms, row));
  }

  /// Writes the finished 15 s slot, samples process groups, updates the 5 min rollup and
  /// evaluates events and alerts.
  fn close_slot(&mut self, slot: i64) {
    let mut slow = Vec::new();
    self.host.sample_slow(&mut slow);
    for (k, v) in slow {
      self.acc.entry(k).or_default().add(v, v, v);
    }

    let scan = self.scan_procs();
    let scan = std::mem::take(&mut self.proc_window).finish(scan);
    let total = scan.total as f32;
    self.acc.entry("procs.count".into()).or_default().add(total, total, total);

    let roll_slot = slot - slot.rem_euclid(ROLLUP.interval);
    if roll_slot != self.roll_slot {
      self.roll.clear();
      self.roll_slot = roll_slot;
    }

    let mut summary: HashMap<String, (f32, f32)> = HashMap::new();
    let acc = std::mem::take(&mut self.acc);
    for (k, a) in &acc {
      let v = a.values();
      self.write(k, RAW, slot, &v);
      self.roll.entry(k.clone()).or_default().add(v[0], v[1], v[2]);
      summary.insert(k.clone(), (v[0], v[2]));
    }

    let mut singles: Vec<(String, f32)> = vec![
      ("self.cpu".into(), scan.self_cpu),
      ("self.mem".into(), scan.self_mem),
    ];
    for g in &scan.groups {
      for (fam, v) in [
        ("proc.cpu", g.cpu),
        ("proc.mem", g.mem),
        ("proc.power", g.power),
        ("proc.gpu", g.gpu),
        ("proc.disk_read", g.disk_read),
        ("proc.disk_write", g.disk_write),
        ("proc.count", g.count as f32),
      ] {
        singles.push((key(fam, &g.name), v));
      }
    }
    for (k, v) in &singles {
      self.write(k, RAW, slot, &[*v]);
      self.roll.entry(k.clone()).or_default().add(*v, *v, *v);
    }

    let roll = std::mem::take(&mut self.roll);
    for (k, a) in &roll {
      let v = a.values();
      if crate::catalog::is_single(k) {
        self.write(k, ROLLUP, roll_slot, &v[..1]);
      } else {
        self.write(k, ROLLUP, roll_slot, &v);
      }
    }
    self.roll = roll;

    self.level_events(slot, &summary);
    for fired in self.alerts.evaluate(slot, &summary) {
      let _ = self.store.add_event(slot, None, "alert", fired.level, &fired.title, &fired.detail);
    }
  }

  fn scan_procs(&mut self) -> Scan {
    let now = now_secs();
    let scan = self.procs.scan(now);
    self.last_proc_scan = Instant::now();
    self.proc_window.add(&scan);
    for exit in &scan.exits {
      let title = format!("{} exited", exit.name);
      let detail = format!(
        "pid {} ({}), ran {}, peak memory {}, CPU time {}",
        exit.pid,
        exit.group,
        human_duration(now - exit.started),
        human_bytes(exit.peak_mem as f64),
        human_duration(exit.cpu_secs as i64),
      );
      let _ = self.store.add_event(now, None, "process", "info", &title, &detail);
    }

    let mut live = self.shared.live.lock().unwrap();
    // Keep only the latest snapshot, not per-process rows in the system sample ring.
    live.groups.clone_from(&scan.groups);
    live.groups_ts = Some(now);
    if self.shared.tx.receiver_count() > 0 {
      let _ = self.shared.tx.send(LiveMsg::Groups(now, Arc::new(live.groups.clone())));
    }
    scan
  }

  fn level_events(&mut self, slot: i64, summary: &HashMap<String, (f32, f32)>) {
    if let Some(&(_, max)) = summary.get("mem.pressure")
      && let Some((prev, level)) = self.pressure.update(max as i32)
    {
      let names = ["normal", "warning", "critical"];
      let name = names.get(level as usize).unwrap_or(&"unknown");
      let lvl = ["info", "warn", "crit"][level.clamp(0, 2) as usize];
      let detail = prev.map_or(String::new(), |p| format!("was {}", names.get(p as usize).unwrap_or(&"?")));
      let _ = self.store.add_event(slot, None, "pressure", lvl, &format!("Memory pressure {name}"), &detail);
    }
    if let Some(&(_, max)) = summary.get("thermal.pressure")
      && let Some((_, level)) = self.thermal.update(max as i32)
    {
      let names = ["nominal", "moderate", "heavy", "trapping", "sleeping"];
      let name = names.get(level as usize).unwrap_or(&"unknown");
      let lvl = if level >= 2 { "warn" } else { "info" };
      let _ = self.store.add_event(slot, None, "thermal", lvl, &format!("Thermal pressure {name}"), "");
    }
  }

  fn write(&self, key: &str, tier: crate::store::Tier, ts: i64, values: &[f32]) {
    if let Err(e) = self.store.write(key, tier, ts, values) {
      eprintln!("mac-monitor: write {key} failed: {e}");
    }
  }

  fn housekeeping(&mut self, now: i64) {
    if now - self.last_flush >= FLUSH_SECS {
      self.last_flush = now;
      if let Err(e) = self.store.flush(now) {
        eprintln!("mac-monitor: flush failed: {e}");
      }
    }
    if now - self.last_prune >= PRUNE_SECS {
      self.last_prune = now;
      if let Err(e) = self.store.prune(now) {
        eprintln!("mac-monitor: prune failed: {e}");
      }
    }
  }
}

fn macmon_values(m: &macmon::Metrics, out: &mut Vec<(String, f32)>) {
  let mut push = |k: &str, v: f32| out.push((k.to_string(), v));
  push("power.sys", m.sys_power);
  push("power.cpu", m.cpu_power);
  push("power.gpu", m.gpu_power);
  push("power.ane", m.ane_power);
  push("power.ram", m.ram_power);
  push("power.gpu_ram", m.gpu_ram_power);
  push("cpu.active", m.cpu_active_ratio);
  push("cpu.scaled", m.cpu_scaled_ratio);
  push("gpu.active", m.gpu_active_ratio);
  push("gpu.scaled", m.gpu_scaled_ratio);
  push("gpu.freq", m.gpu_freq_mhz as f32);
  if let Some(t) = m.temp.cpu_temp_avg {
    push("temp.cpu", t);
  }
  if let Some(t) = m.temp.gpu_temp_avg {
    push("temp.gpu", t);
  }
  push("mem.total", m.memory.ram_total as f32);
  push("swap.used", m.memory.swap_usage as f32);
  push("swap.total", m.memory.swap_total as f32);
  for tier in &m.cpu_tiers {
    out.push((key("cpu.tier.active", &tier.label), tier.active_ratio));
    out.push((key("cpu.tier.scaled", &tier.label), tier.scaled_ratio));
    out.push((key("cpu.tier.freq", &tier.label), tier.freq_mhz as f32));
  }
  for fan in &m.fans {
    out.push((key("fan.rpm", &fan.name), fan.rpm as f32));
  }
}

fn hostname() -> String {
  let mut buf = [0u8; 256];
  let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
  if ret != 0 {
    return String::new();
  }
  let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
  String::from_utf8_lossy(&buf[..end]).trim_end_matches(".local").to_string()
}

pub fn human_duration(secs: i64) -> String {
  let secs = secs.max(0);
  let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
  match (d, h, m) {
    (0, 0, 0) => format!("{s}s"),
    (0, 0, _) => format!("{m}m {s}s"),
    (0, _, _) => format!("{h}h {m}m"),
    _ => format!("{d}d {h}h"),
  }
}

pub fn human_bytes(b: f64) -> String {
  const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
  let mut v = b;
  let mut i = 0;
  while v >= 1024.0 && i < UNITS.len() - 1 {
    v /= 1024.0;
    i += 1;
  }
  if i == 0 { format!("{v:.0} {}", UNITS[i]) } else { format!("{v:.1} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn live_clients_track_process_acceleration_until_last_disconnect() {
    let shared = Shared::new(900);
    assert_eq!(shared.live_clients.load(Ordering::Relaxed), 0);
    let first = LiveClient::new(shared.clone());
    let second = LiveClient::new(shared.clone());
    assert_eq!(shared.live_clients.load(Ordering::Relaxed), 2);
    drop(first);
    assert_eq!(shared.live_clients.load(Ordering::Relaxed), 1);
    drop(second);
    assert_eq!(shared.live_clients.load(Ordering::Relaxed), 0);
  }

  #[test]
  fn process_history_weights_live_scans_and_retains_exited_usage() {
    let mut window = ProcessWindow::default();
    let scan = |seconds, cpu, name: &str| Scan {
      elapsed_secs: seconds,
      self_cpu: cpu,
      self_mem: 123.0,
      groups: vec![GroupStat {
        name: name.into(), cpu, mem: 100.0, power: cpu * 2.0,
        gpu: cpu / 10.0, disk_read: cpu * 100.0, disk_write: cpu * 50.0,
        count: 2, ..Default::default()
      }],
      ..Default::default()
    };
    window.add(&scan(3.0, 1.0, "exited"));
    window.add(&scan(7.0, 2.0, "running"));
    let latest = scan(5.0, 0.5, "running");
    window.add(&latest);
    let history = window.finish(latest);
    let exited = &history.groups[0];
    let running = &history.groups[1];
    assert_eq!(exited.count, 0);
    assert_eq!(running.count, 2);
    assert!((exited.cpu * 15.0 - 3.0).abs() < 0.0001);
    assert!((running.cpu * 15.0 - 16.5).abs() < 0.0001);
    assert!((running.power * 15.0 - 33.0).abs() < 0.0001);
    assert!((running.disk_read * 15.0 - 1650.0).abs() < 0.001);
    assert!((running.disk_write * 15.0 - 825.0).abs() < 0.001);
    assert!((running.gpu * 15.0 - 1.65).abs() < 0.0001);
    assert!((running.mem - 80.0).abs() < 0.001);
    assert!((history.self_cpu * 15.0 - 19.5).abs() < 0.0001);
    assert_eq!(history.self_mem, 123.0);
  }

  #[test]
  fn process_history_without_live_preserves_the_single_scan() {
    let scan = Scan {
      elapsed_secs: 15.2, self_cpu: 0.2,
      groups: vec![GroupStat { name: "app".into(), cpu: 0.75, count: 1, ..Default::default() }],
      ..Default::default()
    };
    let mut window = ProcessWindow::default();
    window.add(&scan);
    assert_eq!(window.finish(scan).groups[0].cpu, 0.75);
  }

  #[test]
  fn debounce_needs_two_slots() {
    let mut d = Debounced::default();
    assert_eq!(d.update(0), None); // initial level
    assert_eq!(d.update(1), None);
    assert_eq!(d.update(0), None); // flapped back
    assert_eq!(d.update(1), None);
    assert_eq!(d.update(1), Some((Some(0), 1)));
    assert_eq!(d.update(1), None);
  }

  #[test]
  fn live_keys_grow_and_rows_cap() {
    let mut live = Live { keep_ms: 1, ..Default::default() };
    let (keys, row) = live.push(1, &[("a".into(), 1.0)]);
    assert_eq!(keys.unwrap().as_slice(), ["a"]);
    assert_eq!(row.as_slice(), [1.0]);
    let (keys, row) = live.push(2, &[("b".into(), 2.0), ("a".into(), 3.0)]);
    assert_eq!(keys.unwrap().as_slice(), ["a", "b"]);
    assert_eq!(row.as_slice(), [3.0, 2.0]);
    let (keys, _) = live.push(3, &[("a".into(), 4.0)]);
    assert!(keys.is_none());
    assert_eq!(live.rows.len(), 2);
    let latest: Vec<_> = live.latest().collect();
    assert_eq!(latest, [("a", 4.0)]);
  }

  #[test]
  fn formats_durations_and_bytes() {
    assert_eq!(human_duration(42), "42s");
    assert_eq!(human_duration(3725), "1h 2m");
    assert_eq!(human_duration(90061), "1d 1h");
    assert_eq!(human_bytes(1536.0 * 1024.0 * 1024.0), "1.5 GB");
  }
}

#[cfg(test)]
mod bench {
  use super::*;

  fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(ru.ru_utime) + t(ru.ru_stime)
  }

  fn measure(name: &str, n: u32, mut f: impl FnMut()) {
    f();
    let t0 = cpu_secs();
    for _ in 0..n {
      f();
    }
    let per = (cpu_secs() - t0) / n as f64 * 1000.0;
    eprintln!("{name:<24} {per:8.3} ms CPU per call");
  }

  /// CPU cost of each collector step: `cargo test --release bench -- --ignored --nocapture`
  /// Busy-loops macmon for profiling with `sample`.
  #[test]
  #[ignore]
  fn macmon_loop() {
    let mut sampler = macmon::Sampler::new().unwrap();
    for _ in 0..120 {
      sampler.get_metrics(100).unwrap();
    }
  }

  #[test]
  #[ignore]
  fn collector_costs() {
    let mut sampler = macmon::Sampler::new().unwrap();
    measure("macmon get_metrics(250)", 8, || {
      sampler.get_metrics(250).unwrap();
    });
    let mut host = host::Host::new();
    let mut out = Vec::new();
    measure("host.sample", 200, || {
      out.clear();
      host.sample(&mut out);
    });
    measure("host.sample_slow", 200, || {
      out.clear();
      host.sample_slow(&mut out);
    });
    let mut p = Procs::new();
    let mut t = 0;
    measure("procs.scan", 10, || {
      t += 15;
      p.scan(t);
    });
  }
}
