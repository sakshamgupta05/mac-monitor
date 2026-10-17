// Full-page screenshots of a running dashboard: node scripts/screenshot.mjs <url> <out.png> [light|dark] [width]
import { chromium } from "@playwright/test";

const [url = "http://127.0.0.1:9393/", out = "screenshot.png", scheme = "dark", width = "1440"] = process.argv.slice(2);
const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: Number(width), height: 900 }, colorScheme: scheme, deviceScaleFactor: 1 });
const errors = [];
page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
page.on("pageerror", (e) => errors.push(String(e)));
await page.goto(url);
await page.waitForSelector(".uplot", { timeout: 15000 });
await page.waitForTimeout(2500);
// Whole page as viewport-sized segments: out-0.png, out-1.png, ...
const height = await page.evaluate(() => document.documentElement.scrollHeight);
const files = [];
for (let y = 0, i = 0; y < height; y += 900, i++) {
  const path = out.replace(/\.png$/, `-${i}.png`);
  await page.screenshot({ path, fullPage: true, clip: { x: 0, y, width: Number(width), height: Math.min(900, height - y) } });
  files.push(path);
}
console.log(JSON.stringify({ files, errors }));
await browser.close();
