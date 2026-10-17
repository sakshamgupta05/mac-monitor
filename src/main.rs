mod catalog;
mod collector;
mod launchd;
mod server;
mod store;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap::{Args, Parser, Subcommand};
use tokio::sync::watch;

use collector::alerts::AlertConfig;
use collector::{Collector, CollectorConfig, Shared};
use store::{Retention, Store};

/// Low-overhead system monitor for Apple Silicon Macs: records power, CPU/GPU, memory,
/// thermals, I/O and per-process usage, and serves a dashboard.
#[derive(Parser)]
#[command(name = "mac-monitor", version, args_conflicts_with_subcommands = true)]
struct Cli {
  #[command(subcommand)]
  cmd: Option<Cmd>,
  #[command(flatten)]
  serve: ServeArgs,
}

#[derive(Subcommand)]
enum Cmd {
  /// Record metrics and serve the dashboard (the default)
  Serve(ServeArgs),
  /// Install a LaunchAgent that runs `serve` at login with these options
  Install(ServeArgs),
  /// Stop and remove the LaunchAgent (recorded data is kept)
  Uninstall,
  /// Show whether the LaunchAgent and server are running
  Status {
    #[arg(long, env = "MAC_MONITOR_LISTEN", default_value = DEFAULT_LISTEN)]
    listen: String,
  },
}

const DEFAULT_LISTEN: &str = "127.0.0.1:9393";

#[derive(Args, Clone, Debug)]
struct ServeArgs {
  /// Address to serve the dashboard on
  #[arg(long, env = "MAC_MONITOR_LISTEN", default_value = DEFAULT_LISTEN)]
  listen: String,
  /// Where the database lives [default: ~/Library/Application Support/mac-monitor]
  #[arg(long, env = "MAC_MONITOR_DATA_DIR")]
  data_dir: Option<PathBuf>,
  /// Sampling interval in milliseconds
  #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(250..=15000))]
  sample_ms: u64,
  /// Sampling interval while a live view is open
  #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(250..=15000))]
  live_sample_ms: u64,
  /// Days to keep 15-second resolution data
  #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(u32).range(1..))]
  raw_retention_days: u32,
  /// Days to keep 5-minute resolution data and events
  #[arg(long, default_value_t = 365, value_parser = clap::value_parser!(u32).range(1..))]
  retention_days: u32,
  /// Alert when CPU temperature stays above this many °C for 2 minutes
  #[arg(long, default_value_t = 100.0)]
  alert_cpu_temp: f32,
  /// Alert when free disk space drops below this many GB
  #[arg(long, default_value_t = 10.0)]
  alert_disk_free_gb: f32,
  /// Show macOS notifications when alerts fire
  #[arg(long)]
  notify: bool,
  /// Also send alerts to this ntfy topic URL (e.g. https://ntfy.sh/my-topic)
  #[arg(long)]
  ntfy_url: Option<String>,
}

impl ServeArgs {
  fn data_dir(&self) -> PathBuf {
    self.data_dir.clone().unwrap_or_else(|| {
      let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
      PathBuf::from(home).join("Library/Application Support/mac-monitor")
    })
  }

  /// The options as command-line arguments, for the LaunchAgent.
  fn to_args(&self) -> Vec<String> {
    let mut a = vec![
      "--listen".into(),
      self.listen.clone(),
      "--data-dir".into(),
      self.data_dir().to_string_lossy().into(),
      "--sample-ms".into(),
      self.sample_ms.to_string(),
      "--live-sample-ms".into(),
      self.live_sample_ms.to_string(),
      "--raw-retention-days".into(),
      self.raw_retention_days.to_string(),
      "--retention-days".into(),
      self.retention_days.to_string(),
      "--alert-cpu-temp".into(),
      self.alert_cpu_temp.to_string(),
      "--alert-disk-free-gb".into(),
      self.alert_disk_free_gb.to_string(),
    ];
    if self.notify {
      a.push("--notify".into());
    }
    if let Some(url) = &self.ntfy_url {
      a.extend(["--ntfy-url".into(), url.clone()]);
    }
    a
  }
}

fn main() {
  let cli = Cli::parse();
  let result = match cli.cmd {
    None => serve(cli.serve),
    Some(Cmd::Serve(args)) => serve(args),
    Some(Cmd::Install(args)) => launchd::install(args.to_args(), &args.listen),
    Some(Cmd::Uninstall) => launchd::uninstall(),
    Some(Cmd::Status { listen }) => launchd::status(&listen),
  };
  if let Err(e) = result {
    eprintln!("mac-monitor: {e}");
    std::process::exit(1);
  }
}

fn serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
  let data_dir = args.data_dir();
  std::fs::create_dir_all(&data_dir)?;
  let retention = Retention {
    raw_secs: args.raw_retention_days as i64 * 86400,
    secs: args.retention_days as i64 * 86400,
  };
  let store = Arc::new(Store::open(&data_dir.join("metrics.db"), retention)?);
  let shared = Shared::new(900);

  // Bind before starting the collector so a busy port fails fast.
  let listener = std::net::TcpListener::bind(&args.listen)
    .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
  listener.set_nonblocking(true)?;

  let cfg = CollectorConfig {
    sample_ms: args.sample_ms,
    live_sample_ms: args.live_sample_ms,
    alerts: AlertConfig {
      cpu_temp: args.alert_cpu_temp,
      disk_free_gb: args.alert_disk_free_gb,
      notify: args.notify,
      ntfy_url: args.ntfy_url.clone(),
    },
  };
  let collector = Collector::new(store.clone(), shared.clone(), cfg);
  let worker = std::thread::Builder::new().name("collector".into()).spawn(move || collector.run())?;

  let (shutdown_tx, shutdown_rx) = watch::channel(false);
  let app = server::App {
    store,
    shared: shared.clone(),
    settings: Arc::new(server::Settings {
      sample_ms: args.sample_ms,
      live_sample_ms: args.live_sample_ms,
      raw_retention_days: args.raw_retention_days,
      retention_days: args.retention_days,
      data_dir: data_dir.to_string_lossy().into(),
    }),
    shutdown: shutdown_rx,
  };

  eprintln!("mac-monitor {} listening on http://{}/", env!("CARGO_PKG_VERSION"), args.listen);
  let rt = tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build()?;
  rt.block_on(async move {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let signal = async move {
      let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
      tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
      }
      let _ = shutdown_tx.send(true);
    };
    axum::serve(listener, server::router(app)).with_graceful_shutdown(signal).await
  })?;

  shared.shutdown.store(true, Ordering::Relaxed);
  let _ = worker.join();
  eprintln!("mac-monitor stopped");
  Ok(())
}
