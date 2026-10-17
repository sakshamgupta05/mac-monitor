//! Time-series storage on SQLite.
//!
//! Every series is stored in fixed-interval tiers. A tier's slots are grouped into chunks (one
//! row per series, tier and chunk start); a chunk is a compressed blob of 1 (single series) or 3
//! (avg/min/max) arrays, NaN marking slots without data. The chunk being written lives in memory
//! and is written to SQLite on `flush`, so a crash loses at most one flush interval.

pub mod codec;
pub mod query;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::catalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
  pub id: u8,
  /// Seconds per slot.
  pub interval: i64,
  /// Seconds per chunk.
  pub span: i64,
}

impl Tier {
  pub const fn slots(&self) -> usize {
    (self.span / self.interval) as usize
  }

  pub const fn chunk_start(&self, ts: i64) -> i64 {
    ts.div_euclid(self.span) * self.span
  }
}

pub const RAW: Tier = Tier { id: 0, interval: 15, span: 3600 };
pub const ROLLUP: Tier = Tier { id: 1, interval: 300, span: 86400 };
pub const TIERS: [Tier; 2] = [RAW, ROLLUP];

#[derive(Debug, Clone, Copy)]
pub struct Retention {
  /// How long 15 s slots are kept.
  pub raw_secs: i64,
  /// How long 5 min slots and events are kept.
  pub secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
  pub id: i64,
  pub ts: i64,
  pub end: Option<i64>,
  pub kind: String,
  pub level: String,
  pub title: String,
  pub detail: String,
}

struct Chunk {
  start: i64,
  k: usize,
  data: Vec<f32>,
  dirty: bool,
}

struct Inner {
  conn: Connection,
  ids: BTreeMap<String, i64>,
  open: HashMap<(i64, u8), Chunk>,
  /// Chunks replaced in `open` and not yet written.
  closed: Vec<(i64, u8, i64, Vec<u8>)>,
}

pub struct Store {
  inner: Mutex<Inner>,
  /// Separate connection for range reads, so long queries don't block the writer (WAL allows
  /// concurrent readers).
  reader: Mutex<Connection>,
  pub retention: Retention,
}

impl Store {
  pub fn open(path: &Path, retention: Retention) -> rusqlite::Result<Self> {
    let conn = Connection::open(path)?;
    // auto_vacuum only takes effect before the first table is created.
    conn.execute_batch(
      "PRAGMA auto_vacuum = INCREMENTAL;
       PRAGMA journal_mode = WAL;
       PRAGMA synchronous = NORMAL;
       PRAGMA cache_size = -2000;
       PRAGMA busy_timeout = 5000;
       CREATE TABLE IF NOT EXISTS series (id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE);
       CREATE TABLE IF NOT EXISTS chunks (
         series INTEGER NOT NULL, tier INTEGER NOT NULL, start INTEGER NOT NULL, data BLOB NOT NULL,
         PRIMARY KEY (series, tier, start)
       ) WITHOUT ROWID;
       CREATE TABLE IF NOT EXISTS events (
         id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, end_ts INTEGER, kind TEXT NOT NULL,
         level TEXT NOT NULL, title TEXT NOT NULL, detail TEXT NOT NULL DEFAULT ''
       );
       CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
       CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;

    let mut ids = BTreeMap::new();
    {
      let mut stmt = conn.prepare("SELECT id, key FROM series")?;
      let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
      for row in rows {
        let (id, key) = row?;
        ids.insert(key, id);
      }
    }

    let reader = Connection::open_with_flags(
      path,
      rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    reader.execute_batch("PRAGMA cache_size = -2000; PRAGMA busy_timeout = 5000;")?;
    let inner = Inner { conn, ids, open: HashMap::new(), closed: Vec::new() };
    Ok(Store { inner: Mutex::new(inner), reader: Mutex::new(reader), retention })
  }

  /// Writes one slot of `key` at `ts` (any time within the slot). `values` holds avg/min/max
  /// for band series or a single value.
  pub fn write(&self, key: &str, tier: Tier, ts: i64, values: &[f32]) -> rusqlite::Result<()> {
    let mut inner = self.inner.lock().unwrap();
    let id = inner.series_id(key)?;
    let start = tier.chunk_start(ts);
    let n = tier.slots();
    let k = values.len();
    let slot = ((ts - start) / tier.interval) as usize;

    let current = inner.open.get(&(id, tier.id)).map(|c| (c.start, c.k));
    if current != Some((start, k)) {
      if let Some(old) = inner.open.remove(&(id, tier.id))
        && old.dirty
      {
        inner.closed.push((id, tier.id, old.start, codec::encode(old.k, &old.data)));
      }
      let chunk = inner.load_chunk(id, tier, start, k)?;
      inner.open.insert((id, tier.id), chunk);
    }

    let chunk = inner.open.get_mut(&(id, tier.id)).unwrap();
    for (a, v) in values.iter().enumerate() {
      chunk.data[a * n + slot] = *v;
    }
    chunk.dirty = true;
    Ok(())
  }

  /// Writes pending chunks to SQLite and drops in-memory chunks of series that stopped.
  pub fn flush(&self, now: i64) -> rusqlite::Result<()> {
    let mut inner = self.inner.lock().unwrap();
    let inner = &mut *inner;
    let tx = inner.conn.transaction()?;
    {
      let mut stmt = tx.prepare_cached(
        "INSERT OR REPLACE INTO chunks (series, tier, start, data) VALUES (?1, ?2, ?3, ?4)",
      )?;
      for (id, tier, start, blob) in inner.closed.drain(..) {
        stmt.execute(params![id, tier, start, blob])?;
      }
      for ((id, tier), chunk) in inner.open.iter_mut() {
        if chunk.dirty {
          stmt.execute(params![id, tier, chunk.start, codec::encode(chunk.k, &chunk.data)])?;
          chunk.dirty = false;
        }
      }
    }
    tx.commit()?;

    inner.open.retain(|(_, tier), chunk| {
      let span = TIERS[*tier as usize].span;
      chunk.dirty || chunk.start + 2 * span > now
    });
    Ok(())
  }

  /// Deletes chunks and events past their retention.
  pub fn prune(&self, now: i64) -> rusqlite::Result<usize> {
    let inner = self.inner.lock().unwrap();
    let raw_cut = now - self.retention.raw_secs - RAW.span;
    let cut = now - self.retention.secs - ROLLUP.span;
    let mut deleted =
      inner.conn.execute("DELETE FROM chunks WHERE tier = 0 AND start < ?1", [raw_cut])?;
    deleted += inner.conn.execute("DELETE FROM chunks WHERE tier = 1 AND start < ?1", [cut])?;
    inner.conn.execute("DELETE FROM events WHERE ts < ?1", [now - self.retention.secs])?;
    if deleted > 0 {
      inner.conn.execute_batch("PRAGMA incremental_vacuum;")?;
    }
    Ok(deleted)
  }

  pub fn keys(&self) -> Vec<String> {
    self.inner.lock().unwrap().ids.keys().cloned().collect()
  }

  /// Exact keys plus keys matching `prefix*` patterns.
  pub fn resolve(&self, keys: &[String], patterns: &[String]) -> Vec<String> {
    let inner = self.inner.lock().unwrap();
    let mut out: Vec<String> = keys.iter().filter(|k| inner.ids.contains_key(*k)).cloned().collect();
    for pat in patterns {
      let prefix = pat.trim_end_matches('*');
      for key in inner.ids.keys() {
        if key.starts_with(prefix) && !out.contains(key) {
          out.push(key.clone());
        }
      }
    }
    out
  }

  /// Calls `f(start, k, data)` for every chunk of `key` in `tier` overlapping `[from, to]`,
  /// in-memory chunks taking precedence over stored ones.
  pub fn for_each_chunk(
    &self,
    key: &str,
    tier: Tier,
    from: i64,
    to: i64,
    mut f: impl FnMut(i64, usize, &[f32]),
  ) -> rusqlite::Result<()> {
    let overlaps = |start: i64| start + tier.span > from && start <= to;
    // Copy the unwritten chunks under the writer lock; whatever isn't in memory is already in
    // the database.
    let (id, memory) = {
      let inner = self.inner.lock().unwrap();
      let Some(&id) = inner.ids.get(key) else { return Ok(()) };
      let mut memory: Vec<(i64, usize, Vec<f32>)> = Vec::new();
      if let Some(chunk) = inner.open.get(&(id, tier.id))
        && overlaps(chunk.start)
      {
        memory.push((chunk.start, chunk.k, chunk.data.clone()));
      }
      for (cid, ctier, start, blob) in inner.closed.iter().rev() {
        if *cid == id
          && *ctier == tier.id
          && overlaps(*start)
          && !memory.iter().any(|m| m.0 == *start)
          && let Some((k, _, data)) = codec::decode(blob)
        {
          memory.push((*start, k, data));
        }
      }
      (id, memory)
    };

    let mut seen = HashSet::new();
    for (start, k, data) in &memory {
      f(*start, *k, data);
      seen.insert(*start);
    }

    let reader = self.reader.lock().unwrap();
    let mut stmt = reader.prepare_cached(
      "SELECT start, data FROM chunks WHERE series = ?1 AND tier = ?2 AND start > ?3 AND start <= ?4",
    )?;
    let mut rows = stmt.query(params![id, tier.id, from - tier.span, to])?;
    while let Some(row) = rows.next()? {
      let start: i64 = row.get(0)?;
      if seen.contains(&start) {
        continue;
      }
      let blob: Vec<u8> = row.get(1)?;
      if let Some((k, _, data)) = codec::decode(&blob) {
        f(start, k, &data);
      }
    }
    Ok(())
  }

  pub fn add_event(
    &self,
    ts: i64,
    end: Option<i64>,
    kind: &str,
    level: &str,
    title: &str,
    detail: &str,
  ) -> rusqlite::Result<()> {
    let inner = self.inner.lock().unwrap();
    inner.conn.execute(
      "INSERT INTO events (ts, end_ts, kind, level, title, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
      params![ts, end, kind, level, title, detail],
    )?;
    Ok(())
  }

  pub fn events(&self, from: i64, to: i64, limit: usize) -> rusqlite::Result<Vec<Event>> {
    let inner = self.inner.lock().unwrap();
    let mut stmt = inner.conn.prepare_cached(
      "SELECT id, ts, end_ts, kind, level, title, detail FROM events
       WHERE ts <= ?2 AND COALESCE(end_ts, ts) >= ?1 ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![from, to, limit as i64], |r| {
      Ok(Event {
        id: r.get(0)?,
        ts: r.get(1)?,
        end: r.get(2)?,
        kind: r.get(3)?,
        level: r.get(4)?,
        title: r.get(5)?,
        detail: r.get(6)?,
      })
    })?;
    rows.collect()
  }

  pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
    let inner = self.inner.lock().unwrap();
    inner
      .conn
      .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
      .optional()
  }

  pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
    let inner = self.inner.lock().unwrap();
    inner.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", [key, value])?;
    Ok(())
  }

  /// Size of the database files in bytes.
  pub fn disk_bytes(&self) -> u64 {
    let inner = self.inner.lock().unwrap();
    let Some(path) = inner.conn.path() else { return 0 };
    ["", "-wal"]
      .iter()
      .filter_map(|suffix| std::fs::metadata(format!("{path}{suffix}")).ok())
      .map(|m| m.len())
      .sum()
  }
}

impl Inner {
  fn series_id(&mut self, key: &str) -> rusqlite::Result<i64> {
    if let Some(&id) = self.ids.get(key) {
      return Ok(id);
    }
    debug_assert!(catalog::family(key).is_some(), "unknown series {key}");
    self.conn.execute("INSERT OR IGNORE INTO series (key) VALUES (?1)", [key])?;
    let id = self.conn.query_row("SELECT id FROM series WHERE key = ?1", [key], |r| r.get(0))?;
    self.ids.insert(key.to_string(), id);
    Ok(id)
  }

  /// The stored chunk at `start`, or an empty one; a chunk written by a different layout is
  /// replaced.
  fn load_chunk(&self, id: i64, tier: Tier, start: i64, k: usize) -> rusqlite::Result<Chunk> {
    let n = tier.slots();
    let blob: Option<Vec<u8>> = self
      .conn
      .query_row(
        "SELECT data FROM chunks WHERE series = ?1 AND tier = ?2 AND start = ?3",
        params![id, tier.id, start],
        |r| r.get(0),
      )
      .optional()?;
    let data = blob
      .and_then(|b| codec::decode(&b))
      .filter(|(bk, bn, _)| *bk == k && *bn == n)
      .map(|(_, _, data)| data)
      .unwrap_or_else(|| vec![f32::NAN; k * n]);
    Ok(Chunk { start, k, data, dirty: false })
  }
}

#[cfg(test)]
pub(crate) fn temp_store() -> (Store, std::path::PathBuf) {
  use std::sync::atomic::{AtomicU32, Ordering};
  static N: AtomicU32 = AtomicU32::new(0);
  let dir = std::env::temp_dir()
    .join(format!("mac-monitor-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let retention = Retention { raw_secs: 14 * 86400, secs: 365 * 86400 };
  (Store::open(&dir.join("test.db"), retention).unwrap(), dir)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn collect(store: &Store, key: &str, tier: Tier, from: i64, to: i64) -> Vec<(i64, f32)> {
    let mut out = Vec::new();
    store
      .for_each_chunk(key, tier, from, to, |start, k, data| {
        let n = data.len() / k;
        for i in 0..n {
          if !data[i].is_nan() {
            out.push((start + i as i64 * tier.interval, data[i]));
          }
        }
      })
      .unwrap();
    out.sort_by_key(|x| x.0);
    out
  }

  #[test]
  fn write_flush_reopen() {
    let (store, dir) = temp_store();
    let t0 = 1_800_000_000; // a multiple of 3600
    for i in 0..300 {
      let v = i as f32;
      store.write("power.sys", RAW, t0 + i * 15, &[v, v - 1.0, v + 1.0]).unwrap();
    }
    // spans two chunks; the first one is closed and pending
    assert_eq!(collect(&store, "power.sys", RAW, t0, t0 + 300 * 15).len(), 300);
    store.flush(t0 + 300 * 15).unwrap();
    drop(store);

    let store = Store::open(&dir.join("test.db"), Retention { raw_secs: 1, secs: 1 }).unwrap();
    let points = collect(&store, "power.sys", RAW, t0, t0 + 300 * 15);
    assert_eq!(points.len(), 300);
    assert_eq!(points[299], (t0 + 299 * 15, 299.0));

    // writing into an existing stored chunk keeps its earlier slots
    store.write("power.sys", RAW, t0 + 300 * 15, &[1.0, 1.0, 1.0]).unwrap();
    assert_eq!(collect(&store, "power.sys", RAW, t0, t0 + 301 * 15).len(), 301);
  }

  #[test]
  fn resolve_patterns() {
    let (store, _dir) = temp_store();
    store.write("proc.cpu:claude", RAW, 0, &[1.0]).unwrap();
    store.write("proc.cpu:node", RAW, 0, &[1.0]).unwrap();
    store.write("proc.mem:node", RAW, 0, &[1.0]).unwrap();
    let keys = store.resolve(&["power.sys".into()], &["proc.cpu:*".into()]);
    assert_eq!(keys, vec!["proc.cpu:claude".to_string(), "proc.cpu:node".to_string()]);
  }

  #[test]
  fn prune_old_chunks_and_events() {
    let (store, _dir) = temp_store();
    let now = 1_800_000_000;
    store.write("power.sys", RAW, now - 30 * 86400, &[1.0, 1.0, 1.0]).unwrap();
    store.write("power.sys", RAW, now, &[1.0, 1.0, 1.0]).unwrap();
    store.add_event(now - 400 * 86400, None, "boot", "info", "old", "").unwrap();
    store.add_event(now, None, "boot", "info", "new", "").unwrap();
    store.flush(now).unwrap();
    assert_eq!(store.prune(now).unwrap(), 1);
    let events = store.events(0, now + 1, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "new");
  }
}
