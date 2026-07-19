// Staging browser smoke for localbench S0. Runs on the staging box (10.96.16.18)
// against a served dist/. Verifies the real user flow AND the provable-local claim.
//
// Usage: BASE=http://127.0.0.1:5055 OUT=./shots node staging-smoke.mjs
import { chromium } from "playwright";
import { mkdir } from "node:fs/promises";

const BASE = process.env.BASE ?? "http://127.0.0.1:5055";
const OUT = process.env.OUT ?? "./shots";
const origin = new URL(BASE).origin;
await mkdir(OUT, { recursive: true });

const failures = [];
const check = (cond, msg) => {
  if (cond) console.log(`  PASS  ${msg}`);
  else { console.log(`  FAIL  ${msg}`); failures.push(msg); }
};

const launchOpts = { args: ["--no-sandbox"] }; // headless
if (process.env.CHROME_PATH) launchOpts.executablePath = process.env.CHROME_PATH;
const browser = await chromium.launch(launchOpts);
const context = await browser.newContext();

// --- provable-local: record every network request origin ---
const external = [];
context.on("request", (req) => {
  const u = new URL(req.url());
  if (u.protocol === "data:" || u.protocol === "blob:") return;
  if (u.origin !== origin) external.push(`${req.method()} ${req.url()}`);
});

const page = await context.newPage();
const consoleErrors = [];
page.on("console", (m) => { if (m.type() === "error") consoleErrors.push(m.text()); });
page.on("pageerror", (e) => consoleErrors.push(String(e)));

// 1) Load + worker/wasm ready (core version rendered)
await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForFunction(() => {
  const v = document.querySelector("#core-version")?.textContent ?? "";
  return /^v\d+\.\d+\.\d+/.test(v);
}, { timeout: 15000 }).catch(() => {});
const versionText = await page.textContent("#core-version");
check(/^v\d+\.\d+\.\d+/.test(versionText ?? ""), `core version renders from WASM (got "${versionText}")`);
await page.screenshot({ path: `${OUT}/1-loaded.png` });

// 2) Real PDF -> page count (worker round-trip through Rust). Build a known 3-page
//    PDF with a throwaway chromium page, then feed it to the app's file input.
const pdfPage = await context.newPage();
await pdfPage.setContent(
  `<div style="page-break-after:always">Page 1</div>
   <div style="page-break-after:always">Page 2</div>
   <div>Page 3</div>`,
);
const pdfBuffer = await pdfPage.pdf({ format: "A4" });
await pdfPage.close();

await page.setInputFiles("#file-input", {
  name: "sample-3page.pdf",
  mimeType: "application/pdf",
  buffer: pdfBuffer,
});
await page.waitForFunction(() => {
  const t = document.querySelector("#result-text")?.textContent ?? "";
  return /\d+\s+pages?/.test(t);
}, { timeout: 15000 }).catch(() => {});
const resultText = await page.textContent("#result-text");
check(resultText?.includes("3 page") ?? false, `3-page PDF -> "${resultText}"`);
await page.screenshot({ path: `${OUT}/2-pagecount.png` });

// 3) Theme toggle actually flips the document theme
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);
await page.screenshot({ path: `${OUT}/3-theme.png` });

// 4) provable-local: no external requests happened during the whole flow
check(external.length === 0, `zero external network requests (found ${external.length}${external.length ? ": " + external.join(", ") : ""})`);

// 5) offline PWA: wait for the SW to actually CONTROL the page (not just be
// registered) so the precache is populated, then go offline, reload, still works.
await page.evaluate(async () => {
  if (!("serviceWorker" in navigator)) return;
  await navigator.serviceWorker.ready;
  // controller is null until the SW claims/controls this client
  if (navigator.serviceWorker.controller) return;
  await new Promise((resolve) => {
    navigator.serviceWorker.addEventListener("controllerchange", resolve, { once: true });
    setTimeout(resolve, 5000);
  });
});
await page.waitForTimeout(1000); // let cache.addAll settle
const diag = await page.evaluate(async () => {
  const controller = !!navigator.serviceWorker.controller;
  const keys = await caches.keys();
  const urls = [];
  for (const k of keys) {
    const c = await caches.open(k);
    urls.push(...(await c.keys()).map((r) => new URL(r.url).pathname));
  }
  return { controller, cacheKeys: keys, cached: urls.sort() };
});
console.log(`  [diag] controller=${diag.controller} caches=${JSON.stringify(diag.cacheKeys)}`);
console.log(`  [diag] cached=${JSON.stringify(diag.cached)}`);
const failedReqs = [];
page.on("requestfailed", (r) => failedReqs.push(`${r.url()} (${r.failure()?.errorText})`));
await context.setOffline(true);
await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
await page.waitForTimeout(1500);
if (failedReqs.length) console.log(`  [diag] offline failed reqs:\n    ${failedReqs.join("\n    ")}`);
const offlineVersion = await page.textContent("#core-version").catch(() => "");
check(/^v\d+\.\d+\.\d+/.test(offlineVersion ?? ""), `offline reload still boots WASM (got "${offlineVersion}")`);
await page.screenshot({ path: `${OUT}/4-offline.png` });
await context.setOffline(false);

check(consoleErrors.length === 0, `no console/page errors (found ${consoleErrors.length}${consoleErrors.length ? ": " + consoleErrors.slice(0,3).join(" | ") : ""})`);

await browser.close();

console.log(`\n${failures.length ? "SMOKE FAILED: " + failures.length + " failure(s)" : "SMOKE PASSED"}`);
process.exit(failures.length ? 1 : 0);
