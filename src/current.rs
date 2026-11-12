//! One-shot queries of the running collector, without reading stored aggregates or opening SSE.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog;
use crate::collector::{Live, now_f64};

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
  /// Unix milliseconds, from the latest system sample (not the time of the request).
  pub timestamp_ms: Option<i64>,
  pub age_ms: Option<i64>,
  pub sample_ms: u64,
  pub metrics: BTreeMap<String, Option<f32>>,
  pub units: BTreeMap<String, String>,
}

impl Snapshot {
  pub fn from_live(live: &Live, sample_ms: u64) -> Self {
    let row = live.rows.back();
    let timestamp_ms = row.map(|(t, _)| *t);
    let metrics = live.keys.iter().enumerate().map(|(i, key)| {
      let value = row.and_then(|(_, values)| values.get(i)).copied().filter(|v| v.is_finite());
      (key.clone(), value)
    }).collect();
    let units = live.keys.iter().filter_map(|key| {
      catalog::family(key).map(|f| (key.clone(), f.unit.to_string()))
    }).collect();
    Self {
      timestamp_ms,
      age_ms: timestamp_ms.map(|t| ((now_f64() * 1000.0) as i64 - t).max(0)),
      sample_ms,
      metrics,
      units,
    }
  }

  pub fn select(&mut self, keys: &[String]) -> Result<(), String> {
    for key in keys {
      if !self.metrics.contains_key(key) {
        return Err(format!("unknown or unavailable metric {key:?}; run `mac-monitor current` to list available keys"));
      }
    }
    if !keys.is_empty() {
      self.metrics.retain(|key, _| keys.contains(key));
      self.units.retain(|key, _| keys.contains(key));
    }
    Ok(())
  }

  pub fn check_age(&self, max_age: u64) -> Result<(), String> {
    match self.age_ms {
      None => Err("no system sample yet; wait for the collector's first sample and retry".into()),
      Some(age) if age as u64 > max_age.saturating_mul(1000) => {
        Err(format!("latest sample is {:.1}s old (limit {max_age}s); check `mac-monitor status`", age as f64 / 1000.0))
      }
      _ => Ok(()),
    }
  }

  pub fn table(&self) -> String {
    let mut out = format!("Sample: {} (Unix ms) · age {:.3}s · cadence {:.3}s\n", self.timestamp_ms.unwrap_or(0), self.age_ms.unwrap_or(0) as f64 / 1000.0, self.sample_ms as f64 / 1000.0);
    out.push_str(&format!("{:<36} {:>16}  UNIT\n", "METRIC", "VALUE"));
    for (key, value) in &self.metrics {
      let unit = self.units.get(key).map_or("", String::as_str);
      let display = match value {
        None => "unavailable".into(),
        Some(v) if key == "mem.pressure" => match *v as i32 {
          0 => "0 (normal)".into(), 1 => "1 (warning)".into(), 2 => "2 (critical)".into(), _ => v.to_string(),
        },
        Some(v) => v.to_string(),
      };
      out.push_str(&format!("{key:<36} {display:>16}  {unit}\n"));
    }
    out
  }
}

/// HTTP/1.0 makes the server close-delimit its JSON response, avoiding chunked transfer.
/// No external executable or HTTP client dependency is needed for this local/LAN command.
pub fn fetch(listen: &str) -> Result<Snapshot, Box<dyn std::error::Error>> {
  let address = crate::launchd::local_addr(listen);
  if address.contains(['\r', '\n', '/', ' ', '\t']) {
    return Err("--listen must be a host:port or [IPv6]:port, without a URL scheme or path".into());
  }
  let addresses: Vec<_> = address.to_socket_addrs()?.collect();
  let mut stream = addresses.iter().find_map(|addr| TcpStream::connect_timeout(addr, Duration::from_secs(2)).ok())
    .ok_or_else(|| format!("cannot reach mac-monitor at {listen}; start the service or check --listen"))?;
  stream.set_read_timeout(Some(Duration::from_secs(5)))?;
  stream.set_write_timeout(Some(Duration::from_secs(5)))?;
  write!(stream, "GET /api/current HTTP/1.0\r\nHost: {address}\r\nAccept: application/json\r\nConnection: close\r\n\r\n")?;
  let mut response = Vec::new();
  stream.take(8 * 1024 * 1024 + 1).read_to_end(&mut response)?;
  if response.len() > 8 * 1024 * 1024 { return Err("snapshot response exceeds 8 MiB".into()); }
  let boundary = response.windows(4).position(|bytes| bytes == b"\r\n\r\n").ok_or("invalid HTTP response")?;
  let headers = std::str::from_utf8(&response[..boundary])?;
  let status = headers.lines().next().unwrap_or("");
  if status.split_whitespace().nth(1) != Some("200") {
    return Err(format!("snapshot request failed: {status}; the server must support /api/current (upgrade and restart it)").into());
  }
  serde_json::from_slice(&response[boundary + 4..]).map_err(|e| format!("invalid snapshot response ({e}); upgrade and restart the server if it predates /api/current").into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn current_row_preserves_missing_values_and_never_backfills() {
    let mut live = Live::default();
    live.keys = vec!["mem.used".into(), "swap.used".into(), "mem.pressure".into()];
    live.rows.push_back((1, std::sync::Arc::new(vec![10.0, 6.0, 1.0])));
    live.rows.push_back((2, std::sync::Arc::new(vec![20.0, f32::NAN])));
    let mut snapshot = Snapshot::from_live(&live, 2000);
    assert_eq!(snapshot.timestamp_ms, Some(2));
    assert_eq!(snapshot.metrics["mem.used"], Some(20.0));
    assert_eq!(snapshot.metrics["swap.used"], None);
    assert_eq!(snapshot.metrics["mem.pressure"], None);
    assert!(snapshot.select(&["typo".into()]).is_err());
    snapshot.select(&["swap.used".into()]).unwrap();
    assert_eq!(snapshot.metrics.len(), 1);
    assert_eq!(snapshot.units.len(), 1);
    assert!(serde_json::to_string(&snapshot).unwrap().contains("\"swap.used\":null"));
  }

  #[test]
  fn empty_and_stale_samples_fail_but_exact_age_limit_is_allowed() {
    let mut snapshot = Snapshot::from_live(&Live::default(), 2000);
    assert!(snapshot.check_age(30).is_err());
    snapshot.age_ms = Some(30_000);
    assert!(snapshot.check_age(30).is_ok());
    snapshot.age_ms = Some(30_001);
    assert!(snapshot.check_age(30).is_err());
  }
}
