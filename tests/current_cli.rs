use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_mac-monitor");

struct Server { child: Child, dir: std::path::PathBuf }
impl Drop for Server {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
    let _ = std::fs::remove_dir_all(&self.dir);
  }
}

#[test]
fn current_queries_real_collector_with_filtering_units_and_no_history() {
  let listener = TcpListener::bind("0.0.0.0:0").unwrap();
  let address = format!("0.0.0.0:{}", listener.local_addr().unwrap().port());
  drop(listener);
  let dir = std::env::temp_dir().join(format!("mac-monitor-current-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
  let child = Command::new(BIN).args(["serve", "--no-config", "--listen", &address, "--data-dir"])
    .arg(&dir).args(["--sample-ms", "250"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
  let _server = Server { child, dir };
  let run = |args: &[&str]| Command::new(BIN).args(["current", "--no-config", "--listen", &address]).args(args).output().unwrap();
  let deadline = Instant::now() + Duration::from_secs(15);
  let output = loop {
    let output = run(&["--json"]);
    if output.status.success() { break output; }
    assert!(Instant::now() < deadline, "{}", String::from_utf8_lossy(&output.stderr));
    std::thread::sleep(Duration::from_millis(100));
  };
  let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert!(data["timestamp_ms"].as_i64().unwrap() > 1_700_000_000_000);
  assert!(data["age_ms"].as_i64().unwrap() < 5000);
  assert_eq!(data["sample_ms"], 250);
  let m = &data["metrics"];
  let used = m["mem.used"].as_f64().unwrap();
  let parts: f64 = ["mem.app", "mem.wired", "mem.compressed"].iter().map(|k| m[k].as_f64().unwrap()).sum();
  assert!((used - parts).abs() < 16384.0);
  assert!(m["swap.used"].as_f64().unwrap() >= 0.0);
  assert_eq!(data["units"]["mem.used"], "B");
  assert!(data.get("rows").is_none());
  let filtered = run(&["--json", "--keys", "mem.used,swap.used"]);
  assert!(filtered.status.success());
  let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
  assert_eq!(filtered["metrics"].as_object().unwrap().len(), 2);
  assert_eq!(filtered["units"].as_object().unwrap().len(), 2);
  let table = run(&["--keys", "mem.used,mem.pressure"]);
  assert!(table.status.success());
  assert!(String::from_utf8_lossy(&table.stdout).contains("METRIC"));
  let config = _server.dir.join("client.yaml");
  std::fs::write(&config, format!("listen: '{address}'\n")).unwrap();
  let configured = Command::new(BIN).env_remove("MAC_MONITOR_LISTEN").args(["current", "--config"])
    .arg(&config).args(["--json", "--keys", "mem.used"]).output().unwrap();
  assert!(configured.status.success(), "{}", String::from_utf8_lossy(&configured.stderr));
  let unknown = run(&["--keys", "mem.typo"]);
  assert!(!unknown.status.success());
  assert!(unknown.stdout.is_empty());
  assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown or unavailable"));
}

fn fixture(status: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
  let listener = TcpListener::bind("0.0.0.0:0").unwrap();
  let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
  let response = format!("HTTP/1.0 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
  let task = std::thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
      let mut byte = [0];
      stream.read_exact(&mut byte).unwrap();
      request.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&request).starts_with("GET /api/current HTTP/1.0"));
    stream.write_all(response.as_bytes()).unwrap();
  });
  (address, task)
}

#[test]
fn current_rejects_stale_empty_and_unsupported_servers_without_json_on_stdout() {
  for (status, body, message) in [
    ("200 OK", r#"{"timestamp_ms":1,"age_ms":31000,"sample_ms":2000,"metrics":{"mem.used":10},"units":{"mem.used":"B"}}"#, "old"),
    ("200 OK", r#"{"timestamp_ms":null,"age_ms":null,"sample_ms":2000,"metrics":{},"units":{}}"#, "no system sample"),
    ("404 Not Found", "not found", "upgrade and restart"),
    ("200 OK", "<html>old dashboard</html>", "invalid snapshot response"),
  ] {
    let (address, task) = fixture(status, body);
    let output = Command::new(BIN).args(["current", "--no-config", "--listen", &address, "--json"]).output().unwrap();
    task.join().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains(message), "{}", String::from_utf8_lossy(&output.stderr));
  }
}

#[test]
fn current_allows_explicit_age_limit_and_preserves_unavailable_sensor() {
  let body = r#"{"timestamp_ms":1,"age_ms":31000,"sample_ms":2000,"metrics":{"swap.used":null},"units":{"swap.used":"B"}}"#;
  let (address, task) = fixture("200 OK", body);
  let output = Command::new(BIN).args(["current", "--no-config", "--listen", &address, "--json", "--max-age", "60"]).output().unwrap();
  task.join().unwrap();
  assert!(output.status.success());
  let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert!(data["metrics"]["swap.used"].is_null());
  assert_eq!(data["age_ms"], 31000);
}
