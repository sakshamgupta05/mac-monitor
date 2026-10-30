mod catalog;
mod collector;
mod config;
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
use config::{Config, Options, Source};
use store::{Retention, Store};

/// Low-overhead system monitor for Apple Silicon Macs: records power, CPU/GPU, memory,
/// thermals, I/O and per-process usage, and serves a dashboard.
///
/// Options are read from ~/.config/mac-monitor/config.yaml; flags and environment
/// variables override the file.
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
  /// Install a LaunchAgent that runs `serve` at login; options given here are saved to the config file
  Install(ServeArgs),
  /// Stop and remove the LaunchAgent (recorded data and the config file are kept)
  Uninstall,
  /// Restart the LaunchAgent, e.g. after editing the config file
  Restart {
    #[command(flatten)]
    file: ConfigArgs,
  },
  /// Show whether the LaunchAgent and server are running, and which config file applies
  Status {
    #[command(flatten)]
    file: ConfigArgs,
    #[arg(long, env = "MAC_MONITOR_LISTEN")]
    listen: Option<String>,
  },
}

#[derive(Args, Clone, Debug)]
struct ConfigArgs {
  /// Config file [default: ~/.config/mac-monitor/config.yaml]
  #[arg(long, env = "MAC_MONITOR_CONFIG", conflicts_with = "no_config")]
  config: Option<PathBuf>,
  /// Ignore the config file
  #[arg(long)]
  no_config: bool,
}

impl ConfigArgs {
  fn source(&self) -> Option<Source> {
    (!self.no_config).then(|| Source::locate(self.config.as_deref()))
  }

  /// The config file's options (empty with `--no-config` or no file).
  fn read(&self) -> Result<Options, String> {
    Ok(self.source().map(|s| s.read()).transpose()?.flatten().unwrap_or_default())
  }

  fn describe(&self) -> String {
    match self.source() {
      None => "none (--no-config)".into(),
      Some(s) if s.path.exists() => s.path.display().to_string(),
      Some(s) => format!("{} (not found, using defaults)", s.path.display()),
    }
  }
}

#[derive(Args, Clone, Debug)]
struct ServeArgs {
  #[command(flatten)]
  file: ConfigArgs,
  #[command(flatten)]
  opts: Options,
}

impl ServeArgs {
  /// Flags and environment over the config file over defaults.
  fn resolve(&self) -> Result<Config, String> {
    // clap checks flag values, so a range error comes from the file
    Config::resolve(self.opts.clone().or(self.file.read()?)).map_err(|e| match self.file.source() {
      Some(s) => format!("{}: {e}", s.path.display()),
      None => e,
    })
  }
}

fn main() {
  let cli = Cli::parse();
  let result = match cli.cmd {
    None => serve(cli.serve),
    Some(Cmd::Serve(args)) => serve(args),
    Some(Cmd::Install(args)) => install(args),
    Some(Cmd::Uninstall) => launchd::uninstall(),
    Some(Cmd::Restart { file }) => restart(file),
    Some(Cmd::Status { file, listen }) => status(file, listen),
  };
  if let Err(e) = result {
    eprintln!("mac-monitor: {e}");
    std::process::exit(1);
  }
}

/// Saves any options given into the config file (creating it if needed) and points the
/// LaunchAgent at it. The plist holds no options, so reinstalling never loses settings.
fn install(mut args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
  if args.file.no_config {
    return Err("install needs a config file; drop --no-config".into());
  }
  let source = args.file.source().unwrap();
  // launchd runs from /, so store an absolute data directory
  if let Some(dir) = &args.opts.data_dir {
    let dir = config::expand_tilde(dir);
    args.opts.data_dir = Some(if dir.is_relative() { std::env::current_dir()?.join(dir) } else { dir });
  }
  if args.opts == Options::default() {
    if config::ensure_exists(&source.path)? {
      println!("Created {} (all options at their defaults)", source.path.display());
    }
  } else {
    let existed = source.path.exists();
    let keys = config::save(&source.path, &args.opts)?;
    let verb = if existed { "Updated" } else { "Created" };
    println!("{verb} {} ({})", source.path.display(), keys.join(", "));
  }
  // Check the whole file before handing it to launchd.
  let cfg = ServeArgs { file: args.file.clone(), opts: Options::default() }.resolve()?;

  // Pass the path only if launchd would not find it by itself (it has no XDG_CONFIG_HOME).
  let home_default = config::home().join(".config/mac-monitor/config.yaml");
  let mut serve_args = Vec::new();
  if source.path != home_default {
    let path = std::path::absolute(&source.path)?;
    serve_args.extend(["--config".to_string(), path.to_string_lossy().into()]);
  }
  launchd::install(serve_args, &cfg.listen)
}

fn restart(file: ConfigArgs) -> Result<(), Box<dyn std::error::Error>> {
  // Catch config mistakes here rather than in a launchd restart loop.
  ServeArgs { file, opts: Options::default() }.resolve()?;
  launchd::restart()
}

fn status(file: ConfigArgs, listen: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
  let args = ServeArgs { file, opts: Options { listen, ..Default::default() } };
  println!("Config: {}", args.file.describe());
  let listen = match args.resolve() {
    Ok(cfg) => cfg.listen,
    Err(e) => {
      println!("Config error: {e}");
      args.opts.listen.unwrap_or_else(|| config::DEFAULT_LISTEN.into())
    }
  };
  launchd::status(&listen)
}

fn serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
  let config_file = args.file.describe();
  let args = args.resolve()?;
  let data_dir = args.data_dir.clone();
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
      raw_retention_days: args.raw_retention_days,
      retention_days: args.retention_days,
      data_dir: data_dir.to_string_lossy().into(),
    }),
    shutdown: shutdown_rx,
  };

  eprintln!("mac-monitor {} listening on http://{}/ (config: {config_file})", env!("CARGO_PKG_VERSION"), args.listen);
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
