//! Host metrics macmon doesn't cover: memory breakdown and pressure, swap traffic, load, disk
//! and network throughput, free disk space, thermal pressure, boot time.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use super::iokit::{IoObject, matching_services};

unsafe extern "C" {
  fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int;
  fn if_indextoname(ifindex: u32, ifname: *mut c_char) -> *mut c_char;
  fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
  fn notify_get_state(token: c_int, state: *mut u64) -> u32;
}

/// Volume whose free space is reported; on APFS it shares the container with the system volume.
const DATA_VOLUME: &CStr = c"/System/Volumes/Data";

pub fn sysctl_raw<T: Copy>(name: &str) -> Option<T> {
  let name = CString::new(name).ok()?;
  let mut value: T = unsafe { mem::zeroed() };
  let mut len = mem::size_of::<T>();
  let ptr = &mut value as *mut T as *mut c_void;
  let ret = unsafe { libc::sysctlbyname(name.as_ptr(), ptr, &mut len, std::ptr::null_mut(), 0) };
  (ret == 0 && len == mem::size_of::<T>()).then_some(value)
}

/// Boot time in unix seconds.
pub fn boot_time() -> Option<i64> {
  sysctl_raw::<libc::timeval>("kern.boottime").map(|tv| tv.tv_sec)
}

/// Counter that turns into a per-second rate between calls.
#[derive(Default)]
struct Rate {
  last: Option<(Instant, u64)>,
}

impl Rate {
  fn update(&mut self, now: Instant, value: u64) -> Option<f32> {
    let rate = match self.last {
      Some((t, v)) if value >= v => {
        let dt = now.duration_since(t).as_secs_f64();
        (dt > 0.0).then(|| ((value - v) as f64 / dt) as f32)
      }
      _ => None,
    };
    self.last = Some((now, value));
    rate
  }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiskInfo {
  pub id: String,
  pub name: String,
  pub bsd: String,
  pub kind: String,
  pub connected: bool,
}

struct Disk {
  driver: IoObject,
  registry_id: u64,
  info: DiskInfo,
  read: Rate,
  write: Rate,
}

fn string_property(object: &IoObject, name: &str) -> Option<String> {
  object.property(&CFString::new(name))?.downcast_into::<CFString>().map(|v| v.to_string())
}

fn dict_string(object: &IoObject, dict: &str, name: &str) -> Option<String> {
  let dict = object.property(&CFString::new(dict))?.downcast_into::<CFDictionary>()?;
  let key = CFString::new(name);
  let value = *dict.find(key.as_CFTypeRef())?;
  let value = unsafe { core_foundation::base::CFType::wrap_under_get_rule(value) };
  value.downcast_into::<CFString>().map(|s| s.to_string())
}

// A stable, opaque series label. Never expose hardware serial numbers through the API.
fn disk_identity(serial: Option<&str>, product: &str, boot: i64, registry_id: u64) -> String {
  let identity = match serial.filter(|s| !s.trim().is_empty()) {
    Some(serial) => format!("{product}\0{serial}"),
    // Without a reliable hardware identifier, don't merge different devices that reuse diskN.
    None => format!("attachment:{boot}:{registry_id}"),
  };
  let hash = identity.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
  format!("{hash:016x}")
}

fn describe_disk(driver: &IoObject) -> DiskInfo {
  let registry_id = driver.registry_id();
  let media = driver.children().and_then(|mut it| it.find(|child| string_property(child, "BSD Name").is_some()));
  let bsd = media.as_ref().and_then(|m| string_property(m, "BSD Name")).unwrap_or_default();
  let mut name = media.as_ref().map(IoObject::name).unwrap_or_else(|| "Block device".into());
  if let Some(short) = name.strip_suffix(" Media") { name = short.to_string(); }
  let parent = driver.parent();
  let location = parent.as_ref().and_then(|p| dict_string(p, "Protocol Characteristics", "Physical Interconnect Location"));
  let kind = match location.as_deref() { Some("Internal") => "Internal", Some("External") => "External", Some("File") => "Disk image", _ => "Other" }.to_string();
  let mut serial = parent.as_ref().and_then(|p| dict_string(p, "Device Characteristics", "Serial Number"));
  let mut ancestor = parent;
  for _ in 0..12 {
    let Some(node) = ancestor else { break };
    if serial.as_ref().is_none_or(|s| s.trim().is_empty()) {
      serial = string_property(&node, "USB Serial Number").or_else(|| string_property(&node, "Serial Number"));
    }
    if serial.as_ref().is_some_and(|s| !s.trim().is_empty()) { break; }
    ancestor = node.parent();
  }
  let id = disk_identity(serial.as_deref(), &name, boot_time().unwrap_or(0), registry_id);
  DiskInfo { id, name, bsd, kind, connected: true }
}

pub struct Host {
  host_port: libc::mach_port_t,
  page_size: u64,
  thermal_token: Option<c_int>,
  disks: Vec<Disk>,
  disk_update: Option<Vec<DiskInfo>>,
  disks_refreshed: Option<Instant>,
  if_names: HashMap<u16, String>,
  swap_in: Rate,
  swap_out: Rate,
  net_rx: Rate,
  net_tx: Rate,
}

impl Host {
  pub fn new() -> Self {
    #[allow(deprecated)]
    let host_port = unsafe { libc::mach_host_self() };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut token = 0;
    let name = c"com.apple.system.thermalpressurelevel";
    let thermal_token = (unsafe { notify_register_check(name.as_ptr(), &mut token) } == 0).then_some(token);

    Host {
      host_port,
      page_size,
      thermal_token,
      disks: Vec::new(),
      disk_update: None,
      disks_refreshed: None,
      if_names: HashMap::new(),
      swap_in: Rate::default(),
      swap_out: Rate::default(),
      net_rx: Rate::default(),
      net_tx: Rate::default(),
    }
  }

  /// System metrics sampled at the shared collection cadence.
  pub fn sample(&mut self, out: &mut Vec<(String, f32)>) {
    let now = Instant::now();
    let mut push = |k: &str, v: f32| out.push((k.to_string(), v));

    if let Some(vm) = self.vm_stats() {
      let page = self.page_size;
      let app = (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64) * page;
      let wired = vm.wire_count as u64 * page;
      let compressed = vm.compressor_page_count as u64 * page;
      let cached = (vm.external_page_count as u64 + vm.purgeable_count as u64) * page;
      let free = (vm.free_count as u64).saturating_sub(vm.speculative_count as u64) * page;
      push("mem.app", app as f32);
      push("mem.wired", wired as f32);
      push("mem.compressed", compressed as f32);
      push("mem.cached", cached as f32);
      push("mem.free", free as f32);
      push("mem.used", (app + wired + compressed) as f32);
      if let Some(r) = self.swap_in.update(now, vm.swapins) {
        push("swap.in", r * page as f32);
      }
      if let Some(r) = self.swap_out.update(now, vm.swapouts) {
        push("swap.out", r * page as f32);
      }
    }

    // 1 normal, 2 warning, 4 critical
    if let Some(level) = sysctl_raw::<i32>("kern.memorystatus_vm_pressure_level") {
      let level = match level {
        4 => 2.0,
        2 => 1.0,
        _ => 0.0,
      };
      push("mem.pressure", level);
    }

    let mut load = [0f64; 3];
    if unsafe { getloadavg(load.as_mut_ptr(), 3) } == 3 {
      push("load.1", load[0] as f32);
    }

    if let Some(token) = self.thermal_token {
      let mut state = 0u64;
      if unsafe { notify_get_state(token, &mut state) } == 0 {
        push("thermal.pressure", state.min(4) as f32);
      }
    }

    if let Some((rx, tx)) = self.net_bytes() {
      if let Some(r) = self.net_rx.update(now, rx) {
        push("net.rx", r);
      }
      if let Some(r) = self.net_tx.update(now, tx) {
        push("net.tx", r);
      }
    }
    self.disk_sample(now, out);
  }

  /// Slow metrics, sampled once per storage slot.
  pub fn sample_slow(&mut self, out: &mut Vec<(String, f32)>) {
    let mut st: libc::statfs = unsafe { mem::zeroed() };
    if unsafe { libc::statfs(DATA_VOLUME.as_ptr(), &mut st) } == 0 {
      let bsize = st.f_bsize as u64;
      out.push(("disk.free".into(), (st.f_bavail * bsize) as f32));
      out.push(("disk.total".into(), (st.f_blocks * bsize) as f32));
    }
  }

  fn vm_stats(&self) -> Option<libc::vm_statistics64> {
    let mut stats: libc::vm_statistics64 = unsafe { mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let ret = unsafe {
      libc::host_statistics64(
        self.host_port,
        libc::HOST_VM_INFO64,
        &mut stats as *mut _ as libc::host_info64_t,
        &mut count,
      )
    };
    (ret == 0).then_some(stats)
  }

  pub fn take_disk_update(&mut self) -> Option<Vec<DiskInfo>> {
    self.disk_update.take()
  }

  /// Read each driver once, derive per-device rates and sum those same rates for All disks.
  fn disk_sample(&mut self, now: Instant, out: &mut Vec<(String, f32)>) {
    let stale = self.disks_refreshed.is_none_or(|t| now.duration_since(t) > Duration::from_secs(60));
    if stale {
      if let Some(drivers) = matching_services(c"IOBlockStorageDriver") {
        let mut previous = std::mem::take(&mut self.disks);
        self.disks = drivers.map(|driver| {
          let registry_id = driver.registry_id();
          let info = describe_disk(&driver);
          let (read, write) = previous.iter().position(|d| d.registry_id == registry_id)
            .map(|i| { let d = previous.swap_remove(i); (d.read, d.write) }).unwrap_or_default();
          Disk { driver, registry_id, info, read, write }
        }).collect();
        self.disk_update = Some(self.disks.iter().map(|d| d.info.clone()).collect());
      }
      self.disks_refreshed = Some(now);
    }
    let stats_key = CFString::from_static_string("Statistics");
    let read_key = CFString::from_static_string("Bytes (Read)");
    let write_key = CFString::from_static_string("Bytes (Write)");
    let (mut read, mut write) = (0.0, 0.0);
    let (mut has_read, mut has_write) = (false, false);
    for disk in &mut self.disks {
      let Some(stats) = disk.driver.property(&stats_key).and_then(|v| v.downcast_into::<CFDictionary>()) else { continue };
      let get = |key: &CFString| -> Option<u64> {
        let value = *stats.find(key.as_CFTypeRef())?;
        let value = unsafe { core_foundation::base::CFType::wrap_under_get_rule(value) };
        value.downcast_into::<CFNumber>()?.to_i64().map(|n| n.max(0) as u64)
      };
      if let Some(value) = get(&read_key).and_then(|v| disk.read.update(now, v)) {
        out.push((format!("disk.device_read:{}", disk.info.id), value));
        read += value; has_read = true;
      }
      if let Some(value) = get(&write_key).and_then(|v| disk.write.update(now, v)) {
        out.push((format!("disk.device_write:{}", disk.info.id), value));
        write += value; has_write = true;
      }
    }
    if has_read { out.push(("disk.read".into(), read)); }
    if has_write { out.push(("disk.write".into(), write)); }
  }

  /// Total bytes received and sent on `en*` interfaces (Ethernet, Wi-Fi). Tunnels such as
  /// Tailscale's utun carry the same traffic again and are left out.
  fn net_bytes(&mut self) -> Option<(u64, u64)> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len = 0usize;
    let null = std::ptr::null_mut();
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 6, null, &mut len, null, 0) } != 0 {
      return None;
    }
    let mut buf = vec![0u8; len + 2048];
    len = buf.len();
    let ptr = buf.as_mut_ptr() as *mut c_void;
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 6, ptr, &mut len, null, 0) } != 0 {
      return None;
    }

    let (mut rx, mut tx) = (0u64, 0u64);
    let mut off = 0usize;
    let hdr_len = mem::size_of::<libc::if_msghdr>();
    while off + hdr_len <= len {
      let hdr: libc::if_msghdr = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
      let msg_len = hdr.ifm_msglen as usize;
      if msg_len == 0 {
        break;
      }
      if hdr.ifm_type as i32 == libc::RTM_IFINFO2
        && off + mem::size_of::<libc::if_msghdr2>() <= len
      {
        let h2: libc::if_msghdr2 =
          unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
        let name = self.if_name(h2.ifm_index);
        if name.starts_with("en") {
          rx += h2.ifm_data.ifi_ibytes;
          tx += h2.ifm_data.ifi_obytes;
        }
      }
      off += msg_len;
    }
    Some((rx, tx))
  }

  fn if_name(&mut self, index: u16) -> &str {
    self.if_names.entry(index).or_insert_with(|| {
      let mut buf = [0 as c_char; libc::IF_NAMESIZE];
      let ret = unsafe { if_indextoname(index as u32, buf.as_mut_ptr()) };
      if ret.is_null() {
        return String::new();
      }
      unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rate_needs_two_samples_and_ignores_resets() {
    let mut r = Rate::default();
    let t = Instant::now();
    assert_eq!(r.update(t, 100), None);
    assert_eq!(r.update(t + Duration::from_secs(2), 300), Some(100.0));
    assert_eq!(r.update(t + Duration::from_secs(3), 50), None);
  }

  #[test]
  fn disk_identity_survives_renumbering_but_never_reuses_unknown_attachments() {
    let a = disk_identity(Some("serial-a"), "SSD", 100, 1);
    assert_eq!(a, disk_identity(Some("serial-a"), "SSD", 200, 999));
    assert_ne!(a, disk_identity(Some("serial-b"), "SSD", 100, 1));
    assert!(!a.contains("serial-a"));
    assert_ne!(disk_identity(None, "SSD", 100, 1), disk_identity(None, "SSD", 200, 1));
    assert_ne!(disk_identity(None, "SSD", 100, 1), disk_identity(None, "SSD", 100, 2));
  }

  #[test]
  fn samples_on_this_machine() {
    let mut host = Host::new();
    let mut out = Vec::new();
    host.sample(&mut out);
    std::thread::sleep(Duration::from_millis(200));
    out.clear();
    host.sample(&mut out);
    host.sample_slow(&mut out);
    let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
    for k in ["mem.used", "mem.wired", "mem.pressure", "load.1", "disk.read", "net.rx", "disk.free"] {
      assert!(keys.contains(&k), "missing {k} in {keys:?}");
    }
    for (total, family) in [("disk.read", "disk.device_read:"), ("disk.write", "disk.device_write:")] {
      let sum: f32 = out.iter().filter(|(key, _)| key.starts_with(family)).map(|(_, v)| *v).sum();
      assert_eq!(sum, out.iter().find(|(key, _)| key == total).unwrap().1);
    }
    let disks = host.take_disk_update().unwrap();
    assert!(!disks.is_empty());
    assert_eq!(disks.iter().map(|d| &d.id).collect::<std::collections::HashSet<_>>().len(), disks.len());
    assert!(disks.iter().all(|d| !d.id.is_empty() && d.connected));
    // Re-discovery must retain baselines for unchanged drivers, not drop a sample every minute.
    host.disks_refreshed = Some(Instant::now() - Duration::from_secs(61));
    let mut refreshed = Vec::new();
    host.sample(&mut refreshed);
    assert!(refreshed.iter().any(|(key, _)| key == "disk.read"));
    let used = out.iter().find(|(k, _)| k == "mem.used").unwrap().1;
    assert!(used > 1e9, "mem.used {used}");
    assert!(boot_time().unwrap() > 1_600_000_000);
  }
}
