//! Settings: command-line flags, then `MAC_MONITOR_*` environment variables, then the YAML
//! config file (`~/.config/mac-monitor/config.yaml`), then built-in defaults. The file is
//! read once at startup; the LaunchAgent runs plain `serve`, so the file survives reinstalls.

use std::path::{Path, PathBuf};

use clap::Args;
use serde::Deserialize;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:9393";
const DEFAULT_SAMPLE_MS: u64 = 2000;
const DEFAULT_RAW_RETENTION_DAYS: u32 = 14;
const DEFAULT_RETENTION_DAYS: u32 = 365;
const DEFAULT_ALERT_CPU_TEMP: f32 = 100.0;
const DEFAULT_ALERT_DISK_FREE_GB: f32 = 10.0;
const SAMPLE_MS_RANGE: std::ops::RangeInclusive<u64> = 250..=15000;

/// Options that can come from flags, the environment or the config file. `None` means "not
/// set here"; [`Config::resolve`] fills in defaults.
#[derive(Args, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Options {
  /// Address to serve the dashboard on [default: 127.0.0.1:9393]
  #[arg(long, env = "MAC_MONITOR_LISTEN")]
  pub listen: Option<String>,
  /// Where the database lives [default: ~/Library/Application Support/mac-monitor]
  #[arg(long, env = "MAC_MONITOR_DATA_DIR")]
  pub data_dir: Option<PathBuf>,
  /// Sampling interval in milliseconds [default: 2000]
  #[arg(long, value_parser = clap::value_parser!(u64).range(250..=15000))]
  pub sample_ms: Option<u64>,
  /// Legacy option, accepted for compatibility but ignored; use --sample-ms.
  #[arg(long, hide = true, value_parser = clap::value_parser!(u64).range(250..=15000))]
  pub live_sample_ms: Option<u64>,
  /// Days to keep 15-second resolution data [default: 14]
  #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
  pub raw_retention_days: Option<u32>,
  /// Days to keep 5-minute resolution data and events [default: 365]
  #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
  pub retention_days: Option<u32>,
  /// Alert when CPU temperature stays above this many °C for 2 minutes [default: 100]
  #[arg(long)]
  pub alert_cpu_temp: Option<f32>,
  /// Alert when free disk space drops below this many GB [default: 10]
  #[arg(long)]
  pub alert_disk_free_gb: Option<f32>,
  /// Show macOS notifications when alerts fire (`--notify=false` to turn off)
  #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
  pub notify: Option<bool>,
  /// Also send alerts to this ntfy topic URL (e.g. https://ntfy.sh/my-topic)
  #[arg(long)]
  pub ntfy_url: Option<String>,
}

impl Options {
  /// Each option set here takes precedence over the same option in `lower`.
  pub fn or(self, lower: Options) -> Options {
    Options {
      listen: self.listen.or(lower.listen),
      data_dir: self.data_dir.or(lower.data_dir),
      sample_ms: self.sample_ms.or(lower.sample_ms),
      live_sample_ms: self.live_sample_ms.or(lower.live_sample_ms),
      raw_retention_days: self.raw_retention_days.or(lower.raw_retention_days),
      retention_days: self.retention_days.or(lower.retention_days),
      alert_cpu_temp: self.alert_cpu_temp.or(lower.alert_cpu_temp),
      alert_disk_free_gb: self.alert_disk_free_gb.or(lower.alert_disk_free_gb),
      notify: self.notify.or(lower.notify),
      ntfy_url: self.ntfy_url.or(lower.ntfy_url),
    }
  }

  /// The options that are set, as config-file keys and YAML values.
  pub fn entries(&self) -> Vec<(&'static str, String)> {
    let s = |v: &str| serde_json::to_string(v).unwrap(); // a JSON string is a YAML double-quoted scalar
    let mut e = Vec::new();
    if let Some(v) = &self.listen {
      e.push(("listen", s(v)));
    }
    if let Some(v) = &self.data_dir {
      e.push(("data_dir", s(&v.to_string_lossy())));
    }
    if let Some(v) = self.sample_ms {
      e.push(("sample_ms", v.to_string()));
    }
    if let Some(v) = self.live_sample_ms {
      e.push(("live_sample_ms", v.to_string()));
    }
    if let Some(v) = self.raw_retention_days {
      e.push(("raw_retention_days", v.to_string()));
    }
    if let Some(v) = self.retention_days {
      e.push(("retention_days", v.to_string()));
    }
    if let Some(v) = self.alert_cpu_temp {
      e.push(("alert_cpu_temp", v.to_string()));
    }
    if let Some(v) = self.alert_disk_free_gb {
      e.push(("alert_disk_free_gb", v.to_string()));
    }
    if let Some(v) = self.notify {
      e.push(("notify", v.to_string()));
    }
    if let Some(v) = &self.ntfy_url {
      e.push(("ntfy_url", s(v)));
    }
    e
  }
}

/// Fully resolved settings for `serve`.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
  pub listen: String,
  pub data_dir: PathBuf,
  pub sample_ms: u64,
  pub raw_retention_days: u32,
  pub retention_days: u32,
  pub alert_cpu_temp: f32,
  pub alert_disk_free_gb: f32,
  pub notify: bool,
  pub ntfy_url: Option<String>,
}

impl Config {
  /// Applies defaults and checks ranges (flags are checked by clap; file values are not).
  pub fn resolve(o: Options) -> Result<Config, String> {
    let c = Config {
      listen: o.listen.unwrap_or_else(|| DEFAULT_LISTEN.into()),
      data_dir: o.data_dir.map(|p| expand_tilde(&p)).unwrap_or_else(default_data_dir),
      sample_ms: o.sample_ms.unwrap_or(DEFAULT_SAMPLE_MS),
      raw_retention_days: o.raw_retention_days.unwrap_or(DEFAULT_RAW_RETENTION_DAYS),
      retention_days: o.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS),
      alert_cpu_temp: o.alert_cpu_temp.unwrap_or(DEFAULT_ALERT_CPU_TEMP),
      alert_disk_free_gb: o.alert_disk_free_gb.unwrap_or(DEFAULT_ALERT_DISK_FREE_GB),
      notify: o.notify.unwrap_or(false),
      ntfy_url: o.ntfy_url.filter(|u| !u.is_empty()),
    };
    if !SAMPLE_MS_RANGE.contains(&c.sample_ms) {
      return Err(format!("sample_ms must be between 250 and 15000 (got {})", c.sample_ms));
    }
    for (key, v) in [("raw_retention_days", c.raw_retention_days), ("retention_days", c.retention_days)] {
      if v < 1 {
        return Err(format!("{key} must be at least 1"));
      }
    }
    if c.listen.trim().is_empty() {
      return Err("listen must not be empty".into());
    }
    Ok(c)
  }
}

pub fn home() -> PathBuf {
  PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn default_data_dir() -> PathBuf {
  home().join("Library/Application Support/mac-monitor")
}

/// `~/.config/mac-monitor/config.yaml`, or under `$XDG_CONFIG_HOME` if that is set.
pub fn default_path() -> PathBuf {
  let base = std::env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .filter(|p| p.is_absolute())
    .unwrap_or_else(|| home().join(".config"));
  base.join("mac-monitor/config.yaml")
}

pub fn expand_tilde(p: &Path) -> PathBuf {
  match p.to_str() {
    Some("~") => home(),
    Some(s) if s.starts_with("~/") => home().join(&s[2..]),
    _ => p.to_path_buf(),
  }
}

/// Which config file applies: an explicit one (`--config`, `MAC_MONITOR_CONFIG`) must exist;
/// the default one is optional.
#[derive(Clone, Debug)]
pub struct Source {
  pub path: PathBuf,
  pub explicit: bool,
}

impl Source {
  pub fn locate(explicit: Option<&Path>) -> Source {
    match explicit {
      Some(p) => Source { path: expand_tilde(p), explicit: true },
      None => Source { path: default_path(), explicit: false },
    }
  }

  /// The file's options, or `None` if the default file does not exist.
  pub fn read(&self) -> Result<Option<Options>, String> {
    match std::fs::read_to_string(&self.path) {
      Ok(text) => parse(&text).map(Some).map_err(|e| format!("{}: {e}", self.path.display())),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.explicit => Ok(None),
      Err(e) => Err(format!("cannot read config {}: {e}", self.path.display())),
    }
  }
}

pub fn parse(text: &str) -> Result<Options, String> {
  // An empty file, or one with only comments, is an empty document.
  yaml_serde::from_str::<Option<Options>>(text).map(Option::unwrap_or_default).map_err(|e| e.to_string())
}

/// Sets top-level keys in a config file's text, keeping everything else (comments included):
/// replaces `key: …`, else uncomments `# key: …`, else appends.
pub fn set_keys(text: &str, entries: &[(&str, String)]) -> String {
  let mut lines: Vec<String> = text.lines().map(String::from).collect();
  for (key, value) in entries {
    let new = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let is_key = |l: &str| l.starts_with(&prefix);
    let is_commented = |l: &str| l.strip_prefix('#').is_some_and(|r| r.trim_start().starts_with(&prefix));
    if let Some(i) = lines.iter().position(|l| is_key(l)) {
      lines[i] = new;
    } else if let Some(i) = lines.iter().position(|l| is_commented(l)) {
      lines[i] = new;
    } else {
      lines.push(new);
    }
  }
  let mut out = lines.join("\n");
  out.push('\n');
  out
}

/// A starting config file: every option, commented out at its default.
pub fn template() -> String {
  format!(
    r#"# mac-monitor configuration (YAML). Uncomment a line to change a setting.
# Command-line flags and MAC_MONITOR_* environment variables override these.
# Apply changes with: mac-monitor restart

# Address of the dashboard and API. "0.0.0.0:9393" listens on all interfaces;
# there is no authentication, so only do that on a network you trust.
# listen: "{DEFAULT_LISTEN}"

# Where the database lives.
# data_dir: "~/Library/Application Support/mac-monitor"

# System sampling interval in milliseconds (250-15000), shared by all views.
# sample_ms: {DEFAULT_SAMPLE_MS}

# Days to keep 15-second data, and 5-minute data and events.
# raw_retention_days: {DEFAULT_RAW_RETENTION_DAYS}
# retention_days: {DEFAULT_RETENTION_DAYS}

# Alert when CPU temperature stays above this (°C) for 2 minutes, or free disk
# space drops below this (GB).
# alert_cpu_temp: {DEFAULT_ALERT_CPU_TEMP}
# alert_disk_free_gb: {DEFAULT_ALERT_DISK_FREE_GB}

# Show alerts as macOS notifications, and/or POST them to an ntfy topic.
# notify: false
# ntfy_url: "https://ntfy.sh/my-topic"
"#
  )
}

/// Saves `opts` into the config file (creating it from the template if needed) and returns
/// the keys written. The result is re-read to make sure the edit took.
pub fn save(path: &Path, opts: &Options) -> Result<Vec<&'static str>, String> {
  let entries = opts.entries();
  let text = match std::fs::read_to_string(path) {
    Ok(t) => t,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => template(),
    Err(e) => return Err(format!("cannot read config {}: {e}", path.display())),
  };
  let edited = set_keys(&text, &entries);
  let parsed = parse(&edited).map_err(|e| format!("could not update {}: {e}", path.display()))?;
  if opts.clone().or(parsed.clone()) != parsed {
    return Err(format!("could not update {} automatically; edit it by hand", path.display()));
  }
  Config::resolve(parsed).map_err(|e| format!("{}: {e}", path.display()))?;
  write_atomic(path, &edited)?;
  Ok(entries.into_iter().map(|(k, _)| k).collect())
}

/// Writes the template if the file does not exist yet. Returns whether it was created.
pub fn ensure_exists(path: &Path) -> Result<bool, String> {
  if path.exists() {
    return Ok(false);
  }
  write_atomic(path, &template())?;
  Ok(true)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
  let err = |e: std::io::Error| format!("cannot write config {}: {e}", path.display());
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir).map_err(err)?;
  }
  let tmp = path.with_extension("yaml.tmp");
  std::fs::write(&tmp, text).map_err(err)?;
  std::fs::rename(&tmp, path).map_err(err)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_and_comment_only_files_set_nothing() {
    assert_eq!(parse("").unwrap(), Options::default());
    assert_eq!(parse("# nothing here\n\n").unwrap(), Options::default());
    assert_eq!(parse(&template()).unwrap(), Options::default());
  }

  #[test]
  fn template_defaults_match_built_in_defaults() {
    let is_setting = |r: &&str| r.split_once(": ").is_some_and(|(k, _)| k.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    let template = template();
    let uncommented: Vec<&str> = template.lines().filter_map(|l| l.strip_prefix("# ").filter(is_setting)).collect();
    assert_eq!(uncommented.len(), 9, "{uncommented:?}");
    let mut o = parse(&uncommented.join("\n")).unwrap();
    o.ntfy_url = None; // the template shows an example, not the default
    let c = Config::resolve(o).unwrap();
    assert_eq!(c, Config::resolve(Options::default()).unwrap());
  }

  #[test]
  fn parses_every_key() {
    let o = parse(
      "listen: 0.0.0.0:9393\ndata_dir: ~/mm\nsample_ms: 500\nlive_sample_ms: 250\nraw_retention_days: 30\n\
       retention_days: 400\nalert_cpu_temp: 95.5\nalert_disk_free_gb: 20\nnotify: true\nntfy_url: https://ntfy.sh/x\n",
    )
    .unwrap();
    let c = Config::resolve(o).unwrap();
    assert_eq!(c.listen, "0.0.0.0:9393");
    assert_eq!(c.data_dir, home().join("mm"));
    assert_eq!(c.sample_ms, 500);
    assert_eq!((c.raw_retention_days, c.retention_days), (30, 400));
    assert_eq!((c.alert_cpu_temp, c.alert_disk_free_gb), (95.5, 20.0));
    assert!(c.notify);
    assert_eq!(c.ntfy_url.as_deref(), Some("https://ntfy.sh/x"));
  }

  #[test]
  fn legacy_live_interval_cannot_change_system_cadence() {
    let c = Config::resolve(parse("live_sample_ms: 1000\n").unwrap()).unwrap();
    assert_eq!(c.sample_ms, 2000);
    let c = Config::resolve(parse("sample_ms: 3000\nlive_sample_ms: 250\n").unwrap()).unwrap();
    assert_eq!(c.sample_ms, 3000);
  }

  #[test]
  fn rejects_unknown_keys_and_bad_values() {
    assert!(parse("listne: 0.0.0.0:9393\n").unwrap_err().contains("unknown field"));
    assert!(parse("sample_ms: fast\n").is_err());
    let e = Config::resolve(parse("sample_ms: 100\n").unwrap()).unwrap_err();
    assert!(e.contains("sample_ms"), "{e}");
    assert!(Config::resolve(parse("retention_days: 0\n").unwrap()).is_err());
  }

  #[test]
  fn flags_override_file_which_overrides_defaults() {
    let file = parse("listen: 0.0.0.0:9393\nsample_ms: 500\nnotify: true\n").unwrap();
    let flags = Options { sample_ms: Some(1000), notify: Some(false), ..Default::default() };
    let c = Config::resolve(flags.or(file)).unwrap();
    assert_eq!(c.listen, "0.0.0.0:9393");
    assert_eq!(c.sample_ms, 1000);
    assert!(!c.notify);
    assert_eq!(c.retention_days, DEFAULT_RETENTION_DAYS);
  }

  #[test]
  fn empty_ntfy_url_means_none() {
    assert_eq!(Config::resolve(parse("ntfy_url: \"\"\n").unwrap()).unwrap().ntfy_url, None);
    assert_eq!(Config::resolve(parse("ntfy_url:\n").unwrap()).unwrap().ntfy_url, None);
  }

  #[test]
  fn set_keys_replaces_uncomments_or_appends_and_keeps_comments() {
    let text = "# top comment\nlisten: \"127.0.0.1:9393\" # inline\n# sample_ms: 2000\n";
    let entries = [
      ("listen", "\"0.0.0.0:9393\"".to_string()),
      ("sample_ms", "500".to_string()),
      ("notify", "true".to_string()),
    ];
    let out = set_keys(text, &entries);
    assert_eq!(out, "# top comment\nlisten: \"0.0.0.0:9393\"\nsample_ms: 500\nnotify: true\n");
  }

  #[test]
  fn set_keys_does_not_touch_prose_or_similar_keys() {
    let out = set_keys("# listen on all interfaces\n# retention_days: 365\n", &[("retention_days", "30".into())]);
    assert_eq!(out, "# listen on all interfaces\nretention_days: 30\n");
    let out = set_keys("raw_retention_days: 14\n", &[("retention_days", "30".into())]);
    assert_eq!(out, "raw_retention_days: 14\nretention_days: 30\n");
  }

  #[test]
  fn entries_round_trip_through_the_template() {
    let opts = Options {
      listen: Some("0.0.0.0:9393".into()),
      data_dir: Some("/tmp/a \"quoted\" dir".into()),
      alert_cpu_temp: Some(92.5),
      notify: Some(true),
      ntfy_url: Some("https://ntfy.sh/x?a=1#b".into()),
      ..Default::default()
    };
    let out = set_keys(&template(), &opts.entries());
    assert_eq!(parse(&out).unwrap(), opts);
  }

  #[test]
  fn save_creates_then_updates_the_file() {
    let dir = std::env::temp_dir().join(format!("mac-monitor-config-test-{}", std::process::id()));
    let path = dir.join("config.yaml");
    let _ = std::fs::remove_dir_all(&dir);

    let keys = save(&path, &Options { listen: Some("0.0.0.0:9393".into()), ..Default::default() }).unwrap();
    assert_eq!(keys, ["listen"]);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# mac-monitor configuration"));
    assert!(text.contains("\nlisten: \"0.0.0.0:9393\"\n"));

    save(&path, &Options { retention_days: Some(30), ..Default::default() }).unwrap();
    let o = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(o.listen.as_deref(), Some("0.0.0.0:9393"));
    assert_eq!(o.retention_days, Some(30));

    assert!(save(&path, &Options { sample_ms: Some(1), ..Default::default() }).is_err());
    assert_eq!(parse(&std::fs::read_to_string(&path).unwrap()).unwrap(), o, "invalid values are not written");
    assert!(!ensure_exists(&path).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn missing_files() {
    let missing = PathBuf::from("/nonexistent/mac-monitor/config.yaml");
    assert_eq!(Source { path: missing.clone(), explicit: false }.read().unwrap(), None);
    assert!(Source { path: missing, explicit: true }.read().is_err());
  }

  #[test]
  fn tilde_expansion() {
    assert_eq!(expand_tilde(Path::new("~")), home());
    assert_eq!(expand_tilde(Path::new("~/x/y")), home().join("x/y"));
    assert_eq!(expand_tilde(Path::new("/a/~/b")), PathBuf::from("/a/~/b"));
  }
}
