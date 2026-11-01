//! Names, units and descriptions of every series mac-monitor records.
//!
//! A series key is `family` or `family:label`, e.g. `power.sys`, `cpu.tier.active:E`,
//! `proc.cpu:claude`. Families under `proc.` and `self.` are sampled once per storage slot and
//! keep a single value; the others are sampled every second and keep avg/min/max per slot.

pub struct Family {
  pub name: &'static str,
  pub unit: &'static str,
  /// Label name for the part after `:`; used by the Prometheus export.
  pub label: Option<&'static str>,
  pub help: &'static str,
}

const fn f(
  name: &'static str,
  unit: &'static str,
  label: Option<&'static str>,
  help: &'static str,
) -> Family {
  Family { name, unit, label, help }
}

pub const FAMILIES: &[Family] = &[
  f("power.sys", "W", None, "Total system power (SMC)"),
  f("power.cpu", "W", None, "CPU power"),
  f("power.gpu", "W", None, "GPU power"),
  f("power.ane", "W", None, "Neural Engine power"),
  f("power.ram", "W", None, "DRAM power"),
  f("power.gpu_ram", "W", None, "GPU SRAM power"),
  f("cpu.active", "ratio", None, "CPU active residency, all cores"),
  f("cpu.scaled", "ratio", None, "CPU frequency-scaled usage, all cores"),
  f("cpu.tier.active", "ratio", Some("tier"), "CPU active residency per tier"),
  f("cpu.tier.scaled", "ratio", Some("tier"), "CPU frequency-scaled usage per tier"),
  f("cpu.tier.freq", "MHz", Some("tier"), "CPU frequency per tier"),
  f("gpu.active", "ratio", None, "GPU active residency"),
  f("gpu.scaled", "ratio", None, "GPU frequency-scaled usage"),
  f("gpu.freq", "MHz", None, "GPU frequency"),
  f("temp.cpu", "C", None, "Average CPU temperature"),
  f("temp.gpu", "C", None, "Average GPU temperature"),
  f("fan.rpm", "rpm", Some("fan"), "Fan speed"),
  f("mem.used", "B", None, "Memory used (app + wired + compressed)"),
  f("mem.total", "B", None, "Physical memory"),
  f("mem.app", "B", None, "App memory"),
  f("mem.wired", "B", None, "Wired memory"),
  f("mem.compressed", "B", None, "Compressed memory"),
  f("mem.cached", "B", None, "Cached files"),
  f("mem.free", "B", None, "Free memory"),
  f("mem.pressure", "level", None, "Memory pressure: 0 normal, 1 warning, 2 critical"),
  f("swap.used", "B", None, "Swap used"),
  f("swap.total", "B", None, "Swap allocated"),
  f("swap.in", "B/s", None, "Swap-in rate"),
  f("swap.out", "B/s", None, "Swap-out rate"),
  f("load.1", "load", None, "1-minute load average"),
  f("disk.read", "B/s", None, "Disk read rate, all block devices"),
  f("disk.write", "B/s", None, "Disk write rate, all block devices"),
  f("disk.device_read", "B/s", Some("disk"), "Disk read rate per device"),
  f("disk.device_write", "B/s", Some("disk"), "Disk write rate per device"),
  f("disk.free", "B", None, "Free space on the data volume"),
  f("disk.total", "B", None, "Size of the data volume"),
  f("net.rx", "B/s", None, "Network receive rate (en* interfaces)"),
  f("net.tx", "B/s", None, "Network transmit rate (en* interfaces)"),
  f("thermal.pressure", "level", None, "Thermal pressure: 0 nominal .. 4 sleeping"),
  f("procs.count", "count", None, "Processes visible to mac-monitor"),
  f("proc.cpu", "cores", Some("group"), "CPU used by a process group (1 = one core)"),
  f("proc.mem", "B", Some("group"), "Memory footprint of a process group"),
  f("proc.power", "W", Some("group"), "Energy rate attributed to a process group"),
  f("proc.gpu", "ratio", Some("group"), "GPU time share of a process group"),
  f("proc.disk_read", "B/s", Some("group"), "Disk reads of a process group"),
  f("proc.disk_write", "B/s", Some("group"), "Disk writes of a process group"),
  f("proc.count", "count", Some("group"), "Processes in a process group"),
  f("self.cpu", "cores", None, "CPU used by mac-monitor"),
  f("self.mem", "B", None, "Memory footprint of mac-monitor"),
];

pub fn family(key: &str) -> Option<&'static Family> {
  let name = key.split_once(':').map_or(key, |(name, _)| name);
  FAMILIES.iter().find(|f| f.name == name)
}

/// Single-value series (one sample per slot) vs band series (avg/min/max per slot).
pub fn is_single(key: &str) -> bool {
  key.starts_with("proc.") || key.starts_with("self.")
}

pub fn key(family: &str, label: &str) -> String {
  format!("{family}:{label}")
}

/// Prometheus metric name and labels for a key.
pub fn prometheus_name(key: &str) -> Option<(String, Option<(&'static str, &str)>)> {
  let fam = family(key)?;
  let suffix = match fam.unit {
    "W" => "_watts",
    "ratio" => "_ratio",
    "MHz" => "_mhz",
    "C" => "_celsius",
    "rpm" => "_rpm",
    "B" => "_bytes",
    "B/s" => "_bytes_per_second",
    "cores" => "_cores",
    _ => "",
  };
  let name = format!("mac_monitor_{}{suffix}", fam.name.replace('.', "_"));
  let label = match (fam.label, key.split_once(':')) {
    (Some(l), Some((_, v))) => Some((l, v)),
    _ => None,
  };
  Some((name, label))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn families_resolve() {
    assert_eq!(family("power.sys").unwrap().unit, "W");
    assert_eq!(family("cpu.tier.freq:E").unwrap().label, Some("tier"));
    assert!(family("nope").is_none());
    assert!(is_single("proc.cpu:claude"));
    assert!(!is_single("power.sys"));
  }

  #[test]
  fn prometheus_names() {
    let (name, label) = prometheus_name("proc.mem:claude").unwrap();
    assert_eq!(name, "mac_monitor_proc_mem_bytes");
    assert_eq!(label, Some(("group", "claude")));
    let (name, label) = prometheus_name("power.sys").unwrap();
    assert_eq!(name, "mac_monitor_power_sys_watts");
    assert_eq!(label, None);
  }
}
