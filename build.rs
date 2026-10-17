//! Embeds the built dashboard (`web/dist`) into the binary, gzip-compressed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{env, fs};

const PLACEHOLDER: &str = "<!doctype html><title>mac-monitor</title><p>The dashboard was not \
  built. Run <code>make web</code> (or <code>npm --prefix web run build</code>) and rebuild.</p>";

fn mime(path: &str) -> &'static str {
  match path.rsplit('.').next().unwrap_or("") {
    "html" => "text/html; charset=utf-8",
    "js" => "text/javascript; charset=utf-8",
    "css" => "text/css; charset=utf-8",
    "svg" => "image/svg+xml",
    "png" => "image/png",
    "ico" => "image/x-icon",
    "json" | "map" => "application/json",
    "woff2" => "font/woff2",
    _ => "application/octet-stream",
  }
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
  let Ok(entries) = fs::read_dir(dir) else { return };
  let mut entries: Vec<_> = entries.flatten().collect();
  entries.sort_by_key(|e| e.path());
  for entry in entries {
    let path = entry.path();
    if path.is_dir() {
      walk(&path, base, out);
    } else if !path.extension().is_some_and(|e| e == "map") {
      let rel = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
      out.push((format!("/{rel}"), fs::read(&path).unwrap()));
    }
  }
}

fn main() {
  println!("cargo:rerun-if-changed=web/dist");
  let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
  let dist = Path::new("web/dist");

  let mut files = Vec::new();
  walk(dist, dist, &mut files);
  if !files.iter().any(|(p, _)| p == "/index.html") {
    println!("cargo:warning=web/dist/index.html missing; embedding a placeholder page");
    files.push(("/index.html".into(), PLACEHOLDER.as_bytes().to_vec()));
  }

  let mut code = String::from("pub static ASSETS: &[Asset] = &[\n");
  for (i, (path, bytes)) in files.iter().enumerate() {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(bytes).unwrap();
    let gz = gz.finish().unwrap();
    let file = out_dir.join(format!("asset_{i}.gz"));
    fs::write(&file, gz).unwrap();

    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    code.push_str(&format!(
      "  Asset {{ path: {path:?}, mime: {mime:?}, gz: include_bytes!({file:?}), etag: \"\\\"{hash:016x}\\\"\" }},\n",
      mime = mime(path),
      file = file.to_string_lossy(),
      hash = h.finish(),
    ));
  }
  code.push_str("];\n");
  fs::write(out_dir.join("assets.rs"), code).unwrap();
}
