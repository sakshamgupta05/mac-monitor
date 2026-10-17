//! Threshold alerts, evaluated once per 15 s slot. A rule fires after its condition held for
//! `for_secs` and resolves when it clears; both are recorded as events, firing also notifies.

use std::collections::HashMap;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct AlertConfig {
  pub cpu_temp: f32,
  pub disk_free_gb: f32,
  /// Show a macOS notification when an alert fires.
  pub notify: bool,
  /// POST alerts to this ntfy.sh (or compatible) topic URL.
  pub ntfy_url: Option<String>,
}

pub struct Fired {
  pub level: &'static str,
  pub title: String,
  pub detail: String,
}

/// Slot values by key: (avg, max).
type Values = HashMap<String, (f32, f32)>;

struct Rule {
  id: &'static str,
  title: &'static str,
  level: &'static str,
  for_secs: i64,
  /// `Some(detail)` while the condition holds.
  check: Box<dyn Fn(&Values) -> Option<String> + Send>,
}

#[derive(Default)]
struct State {
  since: Option<i64>,
  firing: bool,
}

pub struct Alerts {
  cfg: AlertConfig,
  rules: Vec<Rule>,
  state: HashMap<&'static str, State>,
}

const GB: f32 = 1024.0 * 1024.0 * 1024.0;
const MB: f32 = 1024.0 * 1024.0;

impl Alerts {
  pub fn new(cfg: AlertConfig) -> Self {
    let cpu_temp = cfg.cpu_temp;
    let disk_free = cfg.disk_free_gb * GB;
    let rules = vec![
      Rule {
        id: "pressure",
        title: "Memory pressure critical",
        level: "crit",
        for_secs: 60,
        check: Box::new(|v| (v.get("mem.pressure")?.1 >= 2.0).then(|| "macOS reports critical memory pressure".into())),
      },
      Rule {
        id: "swapping",
        title: "Heavy swapping",
        level: "warn",
        for_secs: 120,
        check: Box::new(|v| {
          let out = v.get("swap.out")?.0;
          (out >= 20.0 * MB).then(|| format!("swapping out {:.0} MB/s", out / MB))
        }),
      },
      Rule {
        id: "cpu_temp",
        title: "CPU temperature high",
        level: "warn",
        for_secs: 120,
        check: Box::new(move |v| {
          let t = v.get("temp.cpu")?.0;
          (t >= cpu_temp).then(|| format!("{t:.0} °C (threshold {cpu_temp:.0} °C)"))
        }),
      },
      Rule {
        id: "thermal",
        title: "Thermal throttling",
        level: "warn",
        for_secs: 60,
        check: Box::new(|v| (v.get("thermal.pressure")?.1 >= 2.0).then(|| "thermal pressure heavy or worse".into())),
      },
      Rule {
        id: "disk",
        title: "Low disk space",
        level: "warn",
        for_secs: 0,
        check: Box::new(move |v| {
          let free = v.get("disk.free")?.0;
          (free < disk_free).then(|| format!("{:.1} GB free", free / GB))
        }),
      },
    ];
    Alerts { cfg, rules, state: HashMap::new() }
  }

  /// `values` maps keys to the slot's (avg, max).
  pub fn evaluate(&mut self, now: i64, values: &Values) -> Vec<Fired> {
    let mut fired = Vec::new();
    for rule in &self.rules {
      let state = self.state.entry(rule.id).or_default();
      match (rule.check)(values) {
        Some(detail) => {
          let since = *state.since.get_or_insert(now);
          if !state.firing && now - since >= rule.for_secs {
            state.firing = true;
            notify(&self.cfg, rule.title, &detail);
            fired.push(Fired { level: rule.level, title: rule.title.into(), detail });
          }
        }
        None => {
          if state.firing {
            fired.push(Fired { level: "info", title: format!("{} resolved", rule.title), detail: String::new() });
          }
          *state = State::default();
        }
      }
    }
    fired
  }
}

fn notify(cfg: &AlertConfig, title: &str, detail: &str) {
  if cfg.notify {
    let script = format!(
      "display notification {} with title \"mac-monitor\" subtitle {}",
      applescript_string(detail),
      applescript_string(title)
    );
    spawn(Command::new("/usr/bin/osascript").args(["-e", &script]));
  }
  if let Some(url) = &cfg.ntfy_url {
    spawn(Command::new("/usr/bin/curl").args(["-fsS", "-m", "10", "-H", &format!("Title: {title}"), "-d", detail, url]));
  }
}

fn applescript_string(s: &str) -> String {
  format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Runs a command in the background and reaps it.
fn spawn(cmd: &mut Command) {
  let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
  if let Ok(mut child) = child {
    std::thread::spawn(move || child.wait());
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn alerts() -> Alerts {
    Alerts::new(AlertConfig { cpu_temp: 90.0, disk_free_gb: 10.0, notify: false, ntfy_url: None })
  }

  #[test]
  fn fires_after_duration_and_resolves() {
    let mut a = alerts();
    let hot = HashMap::from([("temp.cpu".to_string(), (95.0, 97.0))]);
    let cool = HashMap::from([("temp.cpu".to_string(), (60.0, 61.0))]);
    assert!(a.evaluate(0, &hot).is_empty());
    assert!(a.evaluate(105, &hot).is_empty());
    let fired = a.evaluate(120, &hot);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].title, "CPU temperature high");
    assert!(a.evaluate(135, &hot).is_empty(), "fires once");
    let resolved = a.evaluate(150, &cool);
    assert_eq!(resolved[0].title, "CPU temperature high resolved");
    assert!(a.evaluate(165, &cool).is_empty());
  }

  #[test]
  fn instant_rules_and_missing_values() {
    let mut a = alerts();
    assert!(a.evaluate(0, &HashMap::new()).is_empty());
    let low = HashMap::from([("disk.free".to_string(), (5.0 * GB, 5.0 * GB))]);
    let fired = a.evaluate(0, &low);
    assert_eq!(fired[0].title, "Low disk space");
    assert_eq!(fired[0].detail, "5.0 GB free");
  }

  #[test]
  fn escapes_applescript() {
    assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
  }
}
