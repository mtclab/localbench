// Staging/live browser walk for the OCR app (app-ocr). Verifies: image -> text
// via a Web Worker, accuracy on a known sample, that the UI STAYS RESPONSIVE
// during OCR (worker, not main thread), models load same-origin (zero external),
// copy/download, and offline OCR after the first (runtime-cached models).
// One engine per run (ENGINE=chromium|firefox|webkit).
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5062";
const OUT = process.env.OUT ?? "./shots-ocr";
const CORPUS = process.env.CORPUS ?? "./ocr-samples";
const origin = new URL(BASE).origin;
await mkdir(OUT, { recursive: true });

const engines = { chromium, firefox, webkit };
const engine = engines[ENGINE];
if (!engine) throw new Error(`unknown ENGINE ${ENGINE}`);

const failures = [];
const check = (cond, msg) => {
  console.log(`  ${cond ? "PASS" : "FAIL"}  [${ENGINE}] ${msg}`);
  if (!cond) failures.push(`[${ENGINE}] ${msg}`);
};
const norm = (s) => s.toLowerCase().replace(/\s+/g, " ").trim();
const tokenHit = (got, truth) => {
  const g = new Set(norm(got).split(" "));
  const t = norm(truth).split(" ");
  return t.filter((w) => g.has(w)).length / t.length;
};
const GROUND_TRUTH = "The quick brown fox jumps over the lazy dog. 1234567890 keeplocal.tools OCR spike test";

const launchOpts = ENGINE === "chromium" ? { args: ["--no-sandbox"] } : {};
if (ENGINE === "chromium" && process.env.CHROME_PATH) launchOpts.executablePath = process.env.CHROME_PATH;
const browser = await engine.launch(launchOpts);
const context = await browser.newContext({ acceptDownloads: true });

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

// --- 1) load + WASM ready ---
await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForFunction(() => /\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 20000 }).catch(() => {});
const versionText = await page.textContent("#core-version");
check(/\d+\.\d+\.\d+/.test(versionText ?? ""), `core version renders from WASM (got "${versionText}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-1-loaded.png` });

const sample = await readFile(path.join(CORPUS, "sample.png"));

// --- 2) load image ---
await page.setInputFiles("#ocr-file-input", { name: "sample.png", mimeType: "image/png", buffer: sample });
await page.waitForSelector("#ocr-editor:not([hidden])", { timeout: 10000 }).catch(() => {});
check(await page.isVisible("#ocr-editor"), "image loaded, editor shown");

// --- 3) extract text + UI-responsiveness-during-OCR probe ---
const beforeTheme = await page.getAttribute("html", "data-theme");
await page.click("#extract-button"); // kicks off (first-time) model download + worker OCR, async
await page.waitForTimeout(400);
await page.click("#theme-toggle");   // must respond while OCR runs (worker frees the main thread)
const midTheme = await page.getAttribute("html", "data-theme");
check(midTheme !== beforeTheme && midTheme !== null, `UI stays responsive during OCR — theme toggled mid-run (${beforeTheme}->${midTheme})`);

await page.waitForFunction(() => (document.querySelector("#ocr-text")?.value ?? "").length > 0, { timeout: 45000 }).catch(() => {});
const text = await page.inputValue("#ocr-text").catch(() => "");
check(text.length > 0, `recognized text is produced (${text.length} chars)`);
const ratio = tokenHit(text, GROUND_TRUTH);
check(ratio >= 0.85, `accuracy on clean sample: ${Math.round(ratio * 100)}% tokens (>=85%)`);
console.log(`  text: ${JSON.stringify(text.slice(0, 120))}`);
await page.screenshot({ path: `${OUT}/${ENGINE}-2-ocr.png` });

// --- 4) copy + download enabled ---
check(!(await page.isDisabled("#copy-button")), "Copy button enabled after OCR");
const dl = await Promise.all([
  page.waitForEvent("download", { timeout: 15000 }).catch(() => null),
  page.click("#download-button"),
]).then(([d]) => d);
if (dl) {
  const p = path.join(OUT, `${ENGINE}-ocr.txt`);
  await dl.saveAs(p);
  const tb = await readFile(p, "utf8");
  check(tb.length > 0 && tokenHit(tb, GROUND_TRUTH) >= 0.85, "downloaded .txt has the recognized text");
} else check(false, "download .txt produced a file");

// --- 4b) SEARCHABLE PDF output mode ---
await page.check("#output-mode-pdf").catch(async () => { await page.click("#output-mode-pdf"); });
await page.click("#extract-button"); // "Create searchable PDF"
await page.waitForFunction(() => {
  const b = document.querySelector("#pdf-download-button");
  return b && !b.disabled;
}, { timeout: 45000 }).catch(() => {});
const pdfDl = await Promise.all([
  page.waitForEvent("download", { timeout: 15000 }).catch(() => null),
  page.click("#pdf-download-button"),
]).then(([d]) => d);
if (pdfDl) {
  const pp = path.join(OUT, `${ENGINE}-searchable.pdf`);
  await pdfDl.saveAs(pp);
  const pb = await readFile(pp);
  check(pb.length > 5 && pb.toString("latin1", 0, 5) === "%PDF-", "searchable PDF is a valid PDF (%PDF-)");
  check(pb.includes(Buffer.from("Helvetica")), "searchable PDF embeds a text-layer font (Helvetica)");
} else check(false, "searchable PDF produced a download");

// --- 5) models loaded same-origin => zero external ---
check(external.length === 0, `zero external network requests — models are same-origin (found ${external.length}${external.length ? ": " + external.slice(0, 3).join(", ") : ""})`);

// --- 6) provable-local badge ---
await page.click("#local-badge");
check(await page.isVisible("#local-inspector"), "privacy inspector opens");
const netCount = ((await page.textContent("#external-request-count").catch(() => "")) ?? "").trim();
check(netCount === "0", `inspector external-request counter reads 0 (got "${netCount}")`);
await page.keyboard.press("Escape");

// --- 7) CF injection filter (engine-agnostic) ---
const isExpectedCspBlock = (m) =>
  /content[-\s]security[-\s]policy/i.test(m) && /script-src/i.test(m) &&
  /(inline script|refused to execute|blocked an inline|violates)/i.test(m);
const expectedCspBlocks = consoleErrors.filter(isExpectedCspBlock);
const functionalConsoleErrors = consoleErrors.filter((m) => !isExpectedCspBlock(m));
if (expectedCspBlocks.length) console.log(`  NOTE  [${ENGINE}] CSP blocked ${expectedCspBlocks.length} injected inline script(s) — expected (CF JS-Detections)`);

// --- 8) OFFLINE OCR after first run (models runtime-cached) ---
await page.evaluate(async () => {
  if (!("serviceWorker" in navigator)) return;
  await navigator.serviceWorker.ready;
  if (navigator.serviceWorker.controller) return;
  await new Promise((r) => { navigator.serviceWorker.addEventListener("controllerchange", r, { once: true }); setTimeout(r, 6000); });
});
await page.waitForTimeout(1200);
const swReady = await page.evaluate(() => "serviceWorker" in navigator && !!navigator.serviceWorker.controller);
if (swReady) {
  await context.setOffline(true);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForFunction(() => /\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 15000 }).catch(() => {});
  await page.setInputFiles("#ocr-file-input", { name: "sample.png", mimeType: "image/png", buffer: sample }).catch(() => {});
  await page.waitForSelector("#ocr-editor:not([hidden])", { timeout: 10000 }).catch(() => {});
  await page.click("#extract-button").catch(() => {});
  await page.waitForFunction(() => (document.querySelector("#ocr-text")?.value ?? "").length > 0, { timeout: 45000 }).catch(() => {});
  const offlineText = await page.inputValue("#ocr-text").catch(() => "");
  check(tokenHit(offlineText, GROUND_TRUTH) >= 0.85, `OCR works OFFLINE after first load (models runtime-cached) — ${Math.round(tokenHit(offlineText, GROUND_TRUTH) * 100)}% tokens`);
  await context.setOffline(false);
} else {
  console.log(`  SKIP  [${ENGINE}] offline: no SW controller`);
}

check(functionalConsoleErrors.length === 0, `no console/page errors during functional use (found ${functionalConsoleErrors.length}${functionalConsoleErrors.length ? ": " + functionalConsoleErrors.slice(0, 3).join(" | ") : ""})`);

await browser.close();
console.log(`${failures.length ? `OCR SMOKE FAILED (${ENGINE}): ${failures.length}` : `OCR SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
