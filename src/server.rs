//! HTTP API, live stream, Prometheus export and the embedded dashboard.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::catalog;
use crate::collector::{LiveClient, LiveMsg, Shared, now_f64, now_secs};
use crate::store::{Store, query};

pub struct Asset {
  pub path: &'static str,
  pub mime: &'static str,
  pub gz: &'static [u8],
  pub etag: &'static str,
}

mod assets {
  use super::Asset;
  include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

pub struct Settings {
  pub sample_ms: u64,
  pub raw_retention_days: u32,
  pub retention_days: u32,
  pub data_dir: String,
}

#[derive(Clone)]
pub struct App {
  pub store: Arc<Store>,
  pub shared: Arc<Shared>,
  pub settings: Arc<Settings>,
  pub shutdown: watch::Receiver<bool>,
}

pub fn router(app: App) -> Router {
  Router::new()
    .route("/api/meta", get(meta))
    .route("/api/query", get(range))
    .route("/api/summary", get(summary))
    .route("/api/events", get(events))
    .route("/api/live", get(live))
    .route("/api/health", get(health))
    .route("/metrics", get(metrics))
    .fallback(get(asset))
    .with_state(app)
}

fn split(s: &Option<String>) -> Vec<String> {
  s.as_deref()
    .unwrap_or("")
    .split(',')
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(String::from)
    .collect()
}

#[derive(Deserialize)]
struct RangeParams {
  keys: Option<String>,
  #[serde(rename = "match")]
  patterns: Option<String>,
  from: Option<f64>,
  to: Option<f64>,
  points: Option<usize>,
  step: Option<i64>,
  align: Option<i64>,
  cov: Option<u8>,
  /// Band series to return min/max for; all of them when absent.
  minmax: Option<String>,
}

impl RangeParams {
  fn window(&self) -> (i64, i64) {
    let to = self.to.map_or_else(now_secs, |t| t as i64);
    let from = self.from.map_or(to - 3600, |f| f as i64).min(to - 1);
    (from, to)
  }
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
  headers
    .get(header::ACCEPT_ENCODING)
    .and_then(|v| v.to_str().ok())
    .is_some_and(|v| v.contains("gzip"))
}

/// JSON response, gzip-compressed when large and the client accepts it.
fn json_gz<T: Serialize>(headers: &HeaderMap, value: &T) -> Response {
  let body = serde_json::to_vec(value).unwrap_or_default();
  let mut res = if body.len() > 2048 && accepts_gzip(headers) {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = gz.write_all(&body);
    let mut res = gz.finish().unwrap_or_default().into_response();
    res.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    res
  } else {
    body.into_response()
  };
  res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
  res.headers_mut().insert(header::VARY, HeaderValue::from_static("accept-encoding"));
  res
}

fn bad_request(msg: &str) -> Response {
  (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

async fn range(State(app): State<App>, headers: HeaderMap, Query(p): Query<RangeParams>) -> Response {
  let (from, to) = p.window();
  let keys = app.store.resolve(&split(&p.keys), &split(&p.patterns));
  if p.step.is_some_and(|s| s <= 0) {
    return bad_request("step must be positive");
  }
  let raw_secs = app.store.retention.raw_secs;
  let points = p.points.unwrap_or(600).clamp(1, 5000);
  let plan = query::plan(now_secs(), raw_secs, from, to, points, p.step, p.align.unwrap_or(0));
  let minmax = p.minmax.is_some().then(|| split(&p.minmax));
  let result = query::range(&app.store, &keys, plan, from, to, p.cov == Some(1), minmax.as_deref());
  let ts: Vec<i64> = (0..plan.buckets).map(|b| plan.first + b as i64 * plan.step).collect();
  let series: BTreeMap<String, query::Series> = result.into_iter().collect();
  json_gz(&headers, &RangeResponse { from, to, interval: plan.tier.interval, step: plan.step, ts, series })
}

/// Typed so f32 values serialize in their short form (a `Value` would widen them to f64).
#[derive(Serialize)]
struct RangeResponse {
  from: i64,
  to: i64,
  interval: i64,
  step: i64,
  ts: Vec<i64>,
  series: BTreeMap<String, query::Series>,
}

#[derive(Serialize)]
struct SummaryResponse {
  from: i64,
  to: i64,
  series: BTreeMap<String, query::Summary>,
}

async fn summary(State(app): State<App>, headers: HeaderMap, Query(p): Query<RangeParams>) -> Response {
  let (from, to) = p.window();
  let keys = app.store.resolve(&split(&p.keys), &split(&p.patterns));
  let out: BTreeMap<String, query::Summary> =
    query::summary(&app.store, &keys, now_secs(), from, to).into_iter().collect();
  json_gz(&headers, &SummaryResponse { from, to, series: out })
}

#[derive(Deserialize)]
struct EventParams {
  from: Option<f64>,
  to: Option<f64>,
  limit: Option<usize>,
}

async fn events(State(app): State<App>, headers: HeaderMap, Query(p): Query<EventParams>) -> Response {
  let to = p.to.map_or_else(now_secs, |t| t as i64);
  let from = p.from.map_or(to - 86400, |f| f as i64);
  match app.store.events(from, to, p.limit.unwrap_or(500).min(5000)) {
    Ok(events) => json_gz(&headers, &json!({ "events": events })),
    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
  }
}

async fn meta(State(app): State<App>) -> Json<Value> {
  let machine = app.shared.machine.lock().unwrap().clone();
  let families: Vec<Value> = catalog::FAMILIES
    .iter()
    .map(|f| json!({ "name": f.name, "unit": f.unit, "help": f.help }))
    .collect();
  let s = &app.settings;
  Json(json!({
    "version": env!("CARGO_PKG_VERSION"),
    "machine": machine,
    "now": now_secs(),
    "started": app.shared.started,
    "sample_ms": s.sample_ms,
    "live_sample_ms": s.sample_ms,
    "raw_retention_days": s.raw_retention_days,
    "retention_days": s.retention_days,
    "data_dir": s.data_dir,
    "db_bytes": app.store.disk_bytes(),
    "keys": app.store.keys(),
    "families": families,
  }))
}

async fn health(State(app): State<App>) -> Json<Value> {
  let last = app.shared.last_sample.load(Ordering::Relaxed);
  let now = now_secs();
  Json(json!({
    "ok": last > 0 && now - last < 30,
    "last_sample_age": if last > 0 { Some(now - last) } else { None },
    "uptime": now - app.shared.started,
  }))
}

fn finite(v: &[f32]) -> Vec<Option<f32>> {
  v.iter().map(|x| x.is_finite().then_some(*x)).collect()
}

#[derive(Serialize)]
struct Row {
  t: i64,
  v: Vec<Option<f32>>,
}

#[derive(Serialize)]
struct Init<'a> {
  keys: &'a [String],
  sample_ms: u64,
  rows: Vec<(i64, Vec<Option<f32>>)>,
  groups: &'a [crate::collector::procs::GroupStat],
  groups_ts: Option<i64>,
  process_sample_secs: u64,
}

async fn live(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
  let rx = app.shared.tx.subscribe();
  let init = {
    let live = app.shared.live.lock().unwrap();
    let rows = live.rows.iter().map(|(t, r)| (*t, finite(r))).collect();
    serde_json::to_string(&Init {
      keys: &live.keys,
      sample_ms: app.settings.sample_ms,
      rows,
      process_sample_secs: crate::collector::LIVE_PROC_SECS,
      groups: &live.groups,
      groups_ts: live.groups_ts,
    })
    .unwrap()
  };
  let first = Event::default().event("init").data(init);

  // The client guard lives in the stream state: dropped when the browser disconnects.
  let client = LiveClient::new(app.shared.clone());
  let state = (rx, app.shutdown.clone(), Some(first), client);
  let stream = futures_util::stream::unfold(state, |(mut rx, mut shutdown, first, client)| async move {
    if let Some(first) = first {
      return Some((Ok(first), (rx, shutdown, None, client)));
    }
    loop {
      tokio::select! {
        msg = rx.recv() => match msg {
          Ok(LiveMsg::Row(t, row)) => {
            let data = serde_json::to_string(&Row { t, v: finite(&row) }).unwrap();
            return Some((Ok(Event::default().event("row").data(data)), (rx, shutdown, None, client)));
          }
          Ok(LiveMsg::Groups(ts, groups)) => {
            let data = serde_json::to_string(&json!({ "ts": ts, "groups": *groups })).unwrap();
            return Some((Ok(Event::default().event("groups").data(data)), (rx, shutdown, None, client)));
          }
          Ok(LiveMsg::Keys(keys)) => {
            let data = serde_json::to_string(&json!({ "keys": *keys })).unwrap();
            return Some((Ok(Event::default().event("keys").data(data)), (rx, shutdown, None, client)));
          }
          Err(broadcast::error::RecvError::Lagged(_)) => continue,
          Err(broadcast::error::RecvError::Closed) => return None,
        },
        _ = shutdown.changed() => return None,
      }
    }
  });
  Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

fn escape_label(v: &str) -> String {
  v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

async fn metrics(State(app): State<App>) -> Response {
  let mut values: Vec<(String, f32)> = Vec::new();
  {
    let live = app.shared.live.lock().unwrap();
    values.extend(live.latest().map(|(k, v)| (k.to_string(), v)));
    for g in &live.groups {
      for (fam, v) in [
        ("proc.cpu", g.cpu),
        ("proc.mem", g.mem),
        ("proc.power", g.power),
        ("proc.gpu", g.gpu),
        ("proc.disk_read", g.disk_read),
        ("proc.disk_write", g.disk_write),
        ("proc.count", g.count as f32),
      ] {
        values.push((catalog::key(fam, &g.name), v));
      }
    }
  }

  let mut families: BTreeMap<String, (&str, Vec<String>)> = BTreeMap::new();
  for (key, v) in values {
    let Some((name, label)) = catalog::prometheus_name(&key) else { continue };
    let help = catalog::family(&key).map_or("", |f| f.help);
    let line = match label {
      Some((l, lv)) => format!("{name}{{{l}=\"{}\"}} {v}", escape_label(lv)),
      None => format!("{name} {v}"),
    };
    families.entry(name).or_insert_with(|| (help, Vec::new())).1.push(line);
  }

  let mut out = String::new();
  for (name, (help, lines)) in families {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
    for line in lines {
      out.push_str(&line);
      out.push('\n');
    }
  }
  let ts = now_f64();
  out.push_str(&format!("# HELP mac_monitor_scrape_timestamp_seconds Time of this scrape\n# TYPE mac_monitor_scrape_timestamp_seconds gauge\nmac_monitor_scrape_timestamp_seconds {ts:.3}\n"));
  ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

async fn asset(uri: Uri, headers: HeaderMap) -> Response {
  let path = match uri.path() {
    "/" => "/index.html",
    p => p,
  };
  let Some(asset) = assets::ASSETS.iter().find(|a| a.path == path) else {
    return (StatusCode::NOT_FOUND, "not found").into_response();
  };
  if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == asset.etag.as_bytes()) {
    return (StatusCode::NOT_MODIFIED, [(header::ETAG, asset.etag)]).into_response();
  }

  let mut res = if accepts_gzip(&headers) {
    let mut res = asset.gz.into_response();
    res.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    res
  } else {
    let mut body = Vec::new();
    let _ = flate2::read::GzDecoder::new(asset.gz).read_to_end(&mut body);
    body.into_response()
  };
  let h = res.headers_mut();
  h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
  h.insert(header::ETAG, HeaderValue::from_static(asset.etag));
  h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
  res
}
