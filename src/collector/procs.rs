//! Per-process usage, summed into process groups.
//!
//! A group is the app bundle a process belongs to (all `Brave Browser Helper` processes count as
//! `Brave Browser`), otherwise the process name. Only the current user's processes are readable
//! without root. Groups below the significance thresholds are summed into `other`.
//!
//! The libproc and GPU client code is adapted from macmon (MIT, https://github.com/vladkens/macmon).

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_void};
use std::mem;
use std::time::Instant;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use super::iokit::matching_services;

const RUSAGE_INFO_V4: c_int = 4;
const RUSAGE_INFO_V6: c_int = 6;
const MIB: u64 = 1 << 20;

/// A group is kept as its own series while it was significant within this many seconds.
const STICKY_SECS: i64 = 600;
pub const OTHER: &str = "other";

/// `struct rusage_info_v6` from `sys/resource.h`; `rusage_info_v4` is a prefix of it.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)]
struct RusageInfoV6 {
  ri_uuid: [u8; 16],
  ri_user_time: u64,
  ri_system_time: u64,
  ri_pkg_idle_wkups: u64,
  ri_interrupt_wkups: u64,
  ri_pageins: u64,
  ri_wired_size: u64,
  ri_resident_size: u64,
  ri_phys_footprint: u64,
  ri_proc_start_abstime: u64,
  ri_proc_exit_abstime: u64,
  ri_child_user_time: u64,
  ri_child_system_time: u64,
  ri_child_pkg_idle_wkups: u64,
  ri_child_interrupt_wkups: u64,
  ri_child_pageins: u64,
  ri_child_elapsed_abstime: u64,
  ri_diskio_bytesread: u64,
  ri_diskio_byteswritten: u64,
  ri_cpu_time_qos_default: u64,
  ri_cpu_time_qos_maintenance: u64,
  ri_cpu_time_qos_background: u64,
  ri_cpu_time_qos_utility: u64,
  ri_cpu_time_qos_legacy: u64,
  ri_cpu_time_qos_user_initiated: u64,
  ri_cpu_time_qos_user_interactive: u64,
  ri_billed_system_time: u64,
  ri_serviced_system_time: u64,
  ri_logical_writes: u64,
  ri_lifetime_max_phys_footprint: u64,
  ri_instructions: u64,
  ri_cycles: u64,
  ri_billed_energy: u64,
  ri_serviced_energy: u64,
  ri_interval_max_phys_footprint: u64,
  ri_runnable_time: u64,
  ri_flags: u64,
  ri_user_ptime: u64,
  ri_system_ptime: u64,
  ri_pinstructions: u64,
  ri_pcycles: u64,
  ri_energy_nj: u64,
  ri_penergy_nj: u64,
  ri_secure_time_in_system: u64,
  ri_secure_ptime_in_system: u64,
  ri_neural_footprint: u64,
  ri_lifetime_max_neural_footprint: u64,
  ri_interval_max_neural_footprint: u64,
  ri_conclave_footprint: u64,
  ri_page_wait_time_mach: u64,
  ri_page_cache_hits: u64,
  ri_reserved: [u64; 6],
}

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
  numer: u32,
  denom: u32,
}

unsafe extern "C" {
  fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupStat {
  pub name: String,
  /// Cores used, 1.0 = one fully busy core.
  pub cpu: f32,
  pub mem: f32,
  pub power: f32,
  /// Share of GPU time, 0..1.
  pub gpu: f32,
  pub disk_read: f32,
  pub disk_write: f32,
  pub count: u32,
}

impl GroupStat {
  fn add(&mut self, other: &GroupStat) {
    self.cpu += other.cpu;
    self.mem += other.mem;
    self.power += other.power;
    self.gpu += other.gpu;
    self.disk_read += other.disk_read;
    self.disk_write += other.disk_write;
    self.count += other.count;
  }

  /// Worth a series of its own; brief blips of background daemons stay in `other`.
  fn significant(&self) -> bool {
    self.cpu >= 0.02
      || self.mem >= (128 * MIB) as f32
      || self.power >= 0.1
      || self.gpu >= 0.01
      || self.disk_read + self.disk_write >= (4 * MIB) as f32
  }
}

#[derive(Debug, Clone)]
pub struct Exit {
  pub pid: i32,
  pub name: String,
  pub group: String,
  pub started: i64,
  pub peak_mem: u64,
  pub cpu_secs: f64,
}

#[derive(Debug, Default)]
pub struct Scan {
  pub groups: Vec<GroupStat>,
  pub exits: Vec<Exit>,
  /// Processes of all users.
  pub total: usize,
  pub self_cpu: f32,
  pub self_mem: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Counters {
  cpu_ns: u64,
  energy_nj: u64,
  disk_read: u64,
  disk_write: u64,
}

struct Known {
  start_abs: u64,
  counters: Counters,
  gpu_ns: Option<u64>,
  name: String,
  group: String,
  started: i64,
  peak_mem: u64,
}

pub struct Procs {
  timebase: (u32, u32),
  flavor: c_int,
  known: HashMap<i32, Known>,
  last: Option<(Instant, i64)>,
  sticky: HashMap<String, i64>,
  pids: Vec<i32>,
  self_pid: i32,
}

impl Procs {
  pub fn new() -> Self {
    let mut tb = MachTimebaseInfo::default();
    unsafe { mach_timebase_info(&mut tb) };
    Procs {
      timebase: (tb.numer.max(1), tb.denom.max(1)),
      flavor: RUSAGE_INFO_V6,
      known: HashMap::new(),
      last: None,
      sticky: HashMap::new(),
      pids: Vec::new(),
      self_pid: std::process::id() as i32,
    }
  }

  /// Usage since the previous scan; the first scan only takes a baseline (rates read 0).
  pub fn scan(&mut self, wall_now: i64) -> Scan {
    let now = Instant::now();
    let elapsed_ns = self.last.map_or(0, |(t, _)| now.duration_since(t).as_nanos() as u64);
    let last_wall = self.last.map(|(_, w)| w);
    self.last = Some((now, wall_now));

    list_pids(&mut self.pids);
    let gpu = gpu_times();
    let mut scan = Scan { total: self.pids.len(), ..Default::default() };
    let mut groups: HashMap<String, GroupStat> = HashMap::new();
    let mut seen = HashSet::with_capacity(self.pids.len());
    let secs = elapsed_ns as f64 / 1e9;

    let pids = mem::take(&mut self.pids);
    for &pid in &pids {
      let Some(info) = bsd_info(pid) else { continue };
      let Some(ru) = self.rusage(pid) else { continue };
      seen.insert(pid);
      let cur = Counters {
        cpu_ns: ticks_to_ns(ru.ri_user_time + ru.ri_system_time, self.timebase),
        energy_nj: ru.ri_energy_nj,
        disk_read: ru.ri_diskio_bytesread,
        disk_write: ru.ri_diskio_byteswritten,
      };
      let gpu_ns = gpu.get(&pid).copied();
      let started = info.pbi_start_tvsec as i64;

      let reused = self.known.get(&pid).is_some_and(|k| k.start_abs != ru.ri_proc_start_abstime);
      if reused && let Some(old) = self.known.remove(&pid) {
        scan.exits.extend(exit_if_significant(pid, old, wall_now));
      }
      let known = self.known.entry(pid).or_insert_with(|| {
        let name = proc_name(&info);
        let group = group_name(exe_path(pid).as_deref(), &name);
        // A process started since the last scan used all of its counters within the interval.
        let fresh = last_wall.is_some_and(|w| started >= w);
        Known {
          start_abs: ru.ri_proc_start_abstime,
          counters: if fresh { Counters::default() } else { cur },
          gpu_ns: if fresh { Some(0) } else { gpu_ns },
          name,
          group,
          started,
          peak_mem: 0,
        }
      });

      let prev = known.counters;
      let delta = |a: u64, b: u64| a.saturating_sub(b) as f64;
      let mut stat = GroupStat { count: 1, mem: ru.ri_phys_footprint as f32, ..Default::default() };
      if secs > 0.0 {
        stat.cpu = (delta(cur.cpu_ns, prev.cpu_ns) / elapsed_ns as f64) as f32;
        stat.power = (delta(cur.energy_nj, prev.energy_nj) / elapsed_ns as f64) as f32;
        stat.disk_read = (delta(cur.disk_read, prev.disk_read) / secs) as f32;
        stat.disk_write = (delta(cur.disk_write, prev.disk_write) / secs) as f32;
        if let (Some(p), Some(c)) = (known.gpu_ns, gpu_ns)
          && c > p
        {
          stat.gpu = ((c - p) as f64 / elapsed_ns as f64).min(1.0) as f32;
        }
      }
      known.counters = cur;
      known.gpu_ns = gpu_ns;
      known.peak_mem = known.peak_mem.max(ru.ri_phys_footprint);

      if pid == self.self_pid {
        scan.self_cpu = stat.cpu;
        scan.self_mem = stat.mem;
      }
      groups.entry(known.group.clone()).or_insert_with(|| GroupStat {
        name: known.group.clone(),
        ..Default::default()
      }).add(&stat);
    }

    self.pids = pids;
    let gone: Vec<i32> = self.known.keys().filter(|pid| !seen.contains(pid)).copied().collect();
    for pid in gone {
      if let Some(old) = self.known.remove(&pid) {
        scan.exits.extend(exit_if_significant(pid, old, wall_now));
      }
    }

    scan.groups = self.fold_small(groups, wall_now);
    scan
  }

  /// Keeps significant groups (and ones significant within `STICKY_SECS`), sums the rest into
  /// `other`.
  fn fold_small(&mut self, groups: HashMap<String, GroupStat>, now: i64) -> Vec<GroupStat> {
    let mut other = GroupStat { name: OTHER.into(), ..Default::default() };
    let mut out = Vec::new();
    for (name, stat) in groups {
      if stat.significant() {
        self.sticky.insert(name.clone(), now);
      }
      if self.sticky.get(&name).is_some_and(|t| now - t <= STICKY_SECS) {
        out.push(stat);
      } else {
        other.add(&stat);
      }
    }
    self.sticky.retain(|_, t| now - *t <= STICKY_SECS);
    out.push(other);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
  }

  fn rusage(&mut self, pid: i32) -> Option<RusageInfoV6> {
    let mut info = RusageInfoV6::default();
    let ptr = &mut info as *mut RusageInfoV6 as *mut libc::rusage_info_t;
    if unsafe { libc::proc_pid_rusage(pid, self.flavor, ptr) } == 0 {
      return Some(info);
    }
    // macOS before 13 has no v6; fall back once and keep v4 (energy reads 0).
    if self.flavor == RUSAGE_INFO_V6 && pid == self.self_pid {
      self.flavor = RUSAGE_INFO_V4;
      return self.rusage(pid);
    }
    None
  }
}

fn exit_if_significant(pid: i32, k: Known, now: i64) -> Option<Exit> {
  let cpu_secs = k.counters.cpu_ns as f64 / 1e9;
  let lived = now - k.started;
  (lived >= 600 && (k.peak_mem >= 256 * MIB || cpu_secs >= 60.0)).then_some(Exit {
    pid,
    name: k.name,
    group: k.group,
    started: k.started,
    peak_mem: k.peak_mem,
    cpu_secs,
  })
}

/// Group of a process: the outermost app bundle in its path, else its name.
pub fn group_name(path: Option<&str>, name: &str) -> String {
  if let Some(path) = path
    && let Some(idx) = path.find(".app/")
    && let Some(app) = path[..idx].rsplit('/').next()
    && !app.is_empty()
  {
    return app.to_string();
  }
  if is_version(name)
    && let Some(path) = path
    && let Some(dir) = path
      .rsplit('/')
      .skip(1)
      .find(|c| !c.is_empty() && !is_version(c) && !GENERIC_DIRS.contains(c))
  {
    // e.g. ~/.local/share/claude/versions/2.1.302 is "claude"
    return dir.to_string();
  }
  if name.is_empty() { "unknown".into() } else { name.to_string() }
}

/// Directories skipped when naming a process after its path.
const GENERIC_DIRS: &[&str] = &["versions", "bin", "sbin", "libexec", "lib", "current", "latest"];

/// Names like `2.1.302` or `v1.4`.
fn is_version(s: &str) -> bool {
  let s = s.strip_prefix('v').unwrap_or(s);
  !s.is_empty() && s.contains('.') && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn ticks_to_ns(ticks: u64, (numer, denom): (u32, u32)) -> u64 {
  (ticks as u128 * numer as u128 / denom as u128) as u64
}

fn c_chars(chars: &[c_char]) -> String {
  let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
  String::from_utf8_lossy(&bytes).into_owned()
}

fn proc_name(info: &libc::proc_bsdinfo) -> String {
  let name = c_chars(&info.pbi_name);
  if name.is_empty() { c_chars(&info.pbi_comm) } else { name }
}

fn list_pids(pids: &mut Vec<i32>) {
  pids.clear();
  let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
  if count <= 0 {
    return;
  }
  pids.resize(count as usize + 64, 0);
  let size = (pids.len() * mem::size_of::<i32>()) as c_int;
  let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, size) };
  pids.truncate(count.max(0) as usize);
}

fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
  let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
  let size = mem::size_of::<libc::proc_bsdinfo>() as c_int;
  let ptr = &mut info as *mut libc::proc_bsdinfo as *mut c_void;
  let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, ptr, size) };
  (read == size).then_some(info)
}

fn exe_path(pid: i32) -> Option<String> {
  let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
  let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
  (len > 0).then(|| String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

/// Pid from a GPU user client's `IOUserClientCreator`: `"pid 631, WindowServer"`.
fn parse_creator(creator: &str) -> Option<i32> {
  let rest = creator.strip_prefix("pid ")?;
  rest.split_once(',').map_or(rest, |(pid, _)| pid).parse().ok()
}

/// Total `accumulatedGPUTime` (ns) of a client's `AppUsage` array.
fn app_usage_ns(usage: &CFType) -> u64 {
  let Some(entries) = usage.downcast::<CFArray>() else { return 0 };
  let key = CFString::from_static_string("accumulatedGPUTime");
  let entry_ns = |entry: *const c_void| {
    let entry = (!entry.is_null()).then(|| unsafe { CFType::wrap_under_get_rule(entry) })?;
    let entry = entry.downcast_into::<CFDictionary>()?;
    let value = *entry.find(key.as_CFTypeRef())?;
    let value = (!value.is_null()).then(|| unsafe { CFType::wrap_under_get_rule(value) })?;
    u64::try_from(value.downcast_into::<CFNumber>()?.to_i64()?).ok()
  };
  entries.iter().filter_map(|entry| entry_ns(*entry)).fold(0, u64::saturating_add)
}

/// GPU time per pid from the GPU user clients in the IORegistry; empty when unreadable.
fn gpu_times() -> HashMap<i32, u64> {
  let creator_key = CFString::from_static_string("IOUserClientCreator");
  let usage_key = CFString::from_static_string("AppUsage");
  let mut times: HashMap<i32, u64> = HashMap::new();
  let Some(gpus) = matching_services(c"IOAccelerator") else { return times };
  for gpu in gpus {
    // Clients come and go; a registry change mid-walk invalidates the iterator, retry.
    for _ in 0..3 {
      let mut found = Vec::new();
      let Some(mut iter) = gpu.children() else { break };
      for client in iter.by_ref() {
        let creator = client.property(&creator_key).and_then(|v| v.downcast_into::<CFString>());
        let Some(pid) = creator.and_then(|c| parse_creator(&c.to_string())) else { continue };
        let ns = client.property(&usage_key).map_or(0, |usage| app_usage_ns(&usage));
        found.push((pid, ns));
      }
      if iter.is_valid() {
        for (pid, ns) in found {
          *times.entry(pid).or_default() += ns;
        }
        break;
      }
    }
  }
  times
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn groups_by_outermost_app_bundle() {
    let brave = "/Applications/Brave Browser.app/Contents/Frameworks/Brave Browser Framework.framework/Versions/1/Helpers/Brave Browser Helper (Renderer).app/Contents/MacOS/Brave Browser Helper (Renderer)";
    assert_eq!(group_name(Some(brave), "Brave Browser Helper (Renderer)"), "Brave Browser");
    assert_eq!(group_name(Some("/opt/homebrew/bin/ollama"), "ollama"), "ollama");
    assert_eq!(group_name(None, "claude"), "claude");
    assert_eq!(group_name(None, ""), "unknown");
    let claude = "/Users/me/.local/share/claude/versions/2.1.302";
    assert_eq!(group_name(Some(claude), "2.1.302"), "claude");
    assert_eq!(group_name(None, "2.1.302"), "2.1.302");
    assert_eq!(group_name(Some("/opt/x/bin/python3.12"), "python3.12"), "python3.12");
  }

  #[test]
  fn parses_gpu_client_creator() {
    assert_eq!(parse_creator("pid 631, WindowServer"), Some(631));
    assert_eq!(parse_creator("pid 7"), Some(7));
    assert_eq!(parse_creator("WindowServer"), None);
  }

  #[test]
  fn folds_small_groups_into_other_with_stickiness() {
    let mut p = Procs::new();
    let big = GroupStat { name: "big".into(), cpu: 0.5, count: 1, ..Default::default() };
    let small = GroupStat { name: "small".into(), cpu: 0.001, count: 2, ..Default::default() };
    let groups = HashMap::from([("big".into(), big.clone()), ("small".into(), small)]);
    let out = p.fold_small(groups, 1000);
    let names: Vec<&str> = out.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["big", "other"]);
    assert_eq!(out[1].count, 2);

    // "big" goes quiet but stays its own series for STICKY_SECS
    let quiet = GroupStat { cpu: 0.0, ..big };
    let out = p.fold_small(HashMap::from([("big".into(), quiet.clone())]), 1000 + STICKY_SECS);
    assert_eq!(out.len(), 2);
    let out = p.fold_small(HashMap::from([("big".into(), quiet)]), 1001 + STICKY_SECS);
    assert_eq!(out.len(), 1);
  }

  #[test]
  fn scans_this_machine() {
    let mut p = Procs::new();
    let first = p.scan(1_000);
    assert!(first.total > 10);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let scan = p.scan(1_001);
    assert!(scan.self_mem > 0.0);
    assert!(scan.groups.iter().any(|g| g.name == OTHER));
    let total: u32 = scan.groups.iter().map(|g| g.count).sum();
    assert!(total > 0);
  }
}
