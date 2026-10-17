// Bundles the dashboard into dist/ (embedded into the Rust binary by build.rs).
import * as esbuild from "esbuild";
import { copyFileSync, mkdirSync, rmSync } from "node:fs";

const watch = process.argv.includes("--watch");
rmSync("dist", { recursive: true, force: true });
mkdirSync("dist", { recursive: true });
copyFileSync("src/index.html", "dist/index.html");
copyFileSync("src/favicon.svg", "dist/favicon.svg");

const options = {
  entryPoints: { app: "src/main.ts" },
  bundle: true,
  minify: !watch,
  sourcemap: watch,
  target: ["safari16", "chrome110", "firefox110"],
  outdir: "dist",
  logLevel: "info",
};

if (watch) {
  const ctx = await esbuild.context(options);
  await ctx.watch();
} else {
  await esbuild.build(options);
}
