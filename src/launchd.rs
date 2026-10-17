//! Installs mac-monitor as a per-user LaunchAgent: started at login, restarted if it exits,
//! scheduled as a background process (efficiency cores, throttled I/O).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub const LABEL: &str = "io.github.mac-monitor";

fn home() -> PathBuf {
  PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

pub fn plist_path() -> PathBuf {
  home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn log_path() -> PathBuf {
  home().join("Library/Logs/mac-monitor.log")
}

fn domain() -> String {
  format!("gui/{}", unsafe { libc::getuid() })
}

fn xml_escape(s: &str) -> String {
  s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn plist(program: &str, args: &[String]) -> String {
  let mut argv = format!("    <string>{}</string>\n", xml_escape(program));
  for a in args {
    argv.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
  }
  let log = xml_escape(&log_path().to_string_lossy());
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{argv}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
  )
}

fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
  Command::new("/bin/launchctl").args(args).output()
}

pub fn install(serve_args: Vec<String>, listen: &str) -> Result<(), Box<dyn std::error::Error>> {
  let exe = std::env::current_exe()?.canonicalize()?;
  let exe = exe.to_string_lossy().to_string();
  if exe.contains("/target/") || exe.starts_with("/private/var/folders") {
    eprintln!("warning: the LaunchAgent will run {exe}");
    eprintln!("         move the binary somewhere stable (e.g. /usr/local/bin) and install from there");
  }

  let mut args = vec!["serve".to_string()];
  args.extend(serve_args);
  let path = plist_path();
  std::fs::create_dir_all(path.parent().unwrap())?;
  std::fs::create_dir_all(log_path().parent().unwrap())?;
  std::fs::write(&path, plist(&exe, &args))?;

  let target = format!("{}/{LABEL}", domain());
  let _ = launchctl(&["bootout", &target]);
  // bootout returns before the old instance is fully gone
  for _ in 0..20 {
    if !launchctl(&["print", &target]).is_ok_and(|o| o.status.success()) {
      break;
    }
    std::thread::sleep(Duration::from_millis(250));
  }
  let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
  if !out.status.success() {
    return Err(format!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
  }

  println!("Installed {}", path.display());
  println!("mac-monitor now starts at login. Dashboard: http://{listen}/");
  println!("Logs: {}", log_path().display());
  Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
  let target = format!("{}/{LABEL}", domain());
  let _ = launchctl(&["bootout", &target]);
  let path = plist_path();
  if path.exists() {
    std::fs::remove_file(&path)?;
    println!("Removed {}", path.display());
  } else {
    println!("Not installed ({} not found)", path.display());
  }
  println!("Recorded data is kept; delete the data directory to remove it.");
  Ok(())
}

fn http_get(addr: &str, path: &str) -> Option<String> {
  let sock = addr.to_socket_addrs().ok()?.next()?;
  let mut stream = TcpStream::connect_timeout(&sock, Duration::from_secs(2)).ok()?;
  stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
  write!(stream, "GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").ok()?;
  let mut body = String::new();
  stream.read_to_string(&mut body).ok()?;
  body.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

pub fn status(listen: &str) -> Result<(), Box<dyn std::error::Error>> {
  let target = format!("{}/{LABEL}", domain());
  let out = launchctl(&["print", &target])?;
  if out.status.success() {
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
      text.lines().map(str::trim).find_map(|l| l.strip_prefix(&format!("{name} = "))).unwrap_or("?").to_string()
    };
    println!("LaunchAgent: loaded (state {}, pid {})", field("state"), field("pid"));
  } else if plist_path().exists() {
    println!("LaunchAgent: installed but not loaded ({})", plist_path().display());
  } else {
    println!("LaunchAgent: not installed");
  }
  match http_get(listen, "/api/health") {
    Some(body) => println!("Server http://{listen}/: {}", body.trim()),
    None => println!("Server http://{listen}/: not reachable"),
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plist_escapes_arguments() {
    let p = plist("/usr/local/bin/mac-monitor", &["serve".into(), "--ntfy-url".into(), "https://x/?a=1&b=2".into()]);
    assert!(p.contains("<string>https://x/?a=1&amp;b=2</string>"));
    assert!(p.contains("<string>Background</string>"));
    assert!(p.contains(&format!("<string>{LABEL}</string>")));
  }
}
