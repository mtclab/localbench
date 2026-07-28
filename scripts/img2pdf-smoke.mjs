// Staging/live browser smoke for the images->PDF app (app-img2pdf). Runs on the
// staging box or against a live URL over a served dist/. Verifies the real flow:
// add multiple images -> build one PDF -> valid multi-page PDF, EXIF/GPS stripped
// from embedded photos, plus the provable-local claim (zero external + offline).
// One engine per run (ENGINE=chromium|firefox|webkit).
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5059";
const OUT = process.env.OUT ?? "./shots-img2pdf";
const CORPUS = process.env.CORPUS ?? "./img2pdf-corpus";
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

const isPdf = (b) => b.length > 5 && b.toString("latin1", 0, 5) === "%PDF-";
const has = (b, needle) => b.includes(Buffer.from(needle, "latin1"));
const pageCount = (b) => (b.toString("latin1").match(/\/Type\s*\/Page(?![s])/g) || []).length;

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
await page.waitForFunction(() => /^v?\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 20000 }).catch(() => {});
const versionText = await page.textContent("#core-version");
check(/\d+\.\d+\.\d+/.test(versionText ?? ""), `core version renders from WASM (got "${versionText}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-1-loaded.png` });

const a = await readFile(path.join(CORPUS, "a.jpg"));
const b = await readFile(path.join(CORPUS, "b.png"));
const c = await readFile(path.join(CORPUS, "c.jpg"));
check(has(a, "Exif\0\0"), "source a.jpg carries GPS EXIF (strip test is meaningful)");

// --- 2) add three images, build a PDF ---
await page.setInputFiles("#create-file-input", [
  { name: "a.jpg", mimeType: "image/jpeg", buffer: a },
  { name: "b.png", mimeType: "image/png", buffer: b },
  { name: "c.jpg", mimeType: "image/jpeg", buffer: c },
]);
await page.waitForSelector("#create-editor:not([hidden])", { timeout: 10000 }).catch(() => {});
const total = ((await page.textContent("#create-total").catch(() => "")) ?? "").trim();
check(/3 image/.test(total), `three images listed (got "${total}")`);
await page.selectOption("#page-size", "fit").catch(() => {});
await page.click("#create-button");
await page.waitForSelector("#create-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const [dl] = await Promise.all([
  page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
  page.click("#create-download-button"),
]);
if (dl) {
  const p = path.join(OUT, `${ENGINE}-combined.pdf`);
  await dl.saveAs(p);
  const pb = await readFile(p);
  check(isPdf(pb), "output is a valid PDF (%PDF-)");
  check(pageCount(pb) === 3, `PDF has 3 pages (found ${pageCount(pb)})`);
  check(!has(pb, "Exif\0\0"), "embedded photo EXIF/GPS was STRIPPED (headline claim)");
  check(!has(pb, "GPSLatitude") && !has(pb, "MtclabCam"), "no GPS/camera metadata survives in the PDF");
} else check(false, "Create PDF produced a download");
await page.screenshot({ path: `${OUT}/${ENGINE}-2-built.png` });

// --- 3) A4 mode still produces a valid PDF ---
await page.selectOption("#page-size", "a4").catch(() => {});
await page.click("#create-button");
await page.waitForSelector("#create-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const [dl2] = await Promise.all([
  page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
  page.click("#create-download-button"),
]);
if (dl2) {
  const pb2 = await save(dl2, `${ENGINE}-a4.pdf`);
  check(isPdf(pb2) && pageCount(pb2) === 3, "A4 mode also yields a valid 3-page PDF");
} else check(false, "A4 Create PDF produced a download");
async function save(d, name) { const p = path.join(OUT, name); await d.saveAs(p); return readFile(p); }

/*
 * The inspector fills the external-request counter ASYNCHRONOUSLY: opening it
 * paints a pending placeholder ("Checking page and worker…") and writes the
 * number only once the worker has answered over the message port with its own
 * tally. Reading the slot straight after the click is a race that reports the
 * placeholder as a failure - a harness bug, not a product one. Wait for the
 * counter to SETTLE (a number, or a fail-marked verdict like "Unproven — …")
 * and assert on that, so a genuinely non-zero count still fails this gate.
 */
async function readSettledExternalProof() {
  await page.waitForFunction(() => {
    const slot = document.querySelector('[data-proof="external"]');
    if (!slot) return false;
    return /^\d+$/.test((slot.textContent ?? "").trim()) || slot.dataset.state === "fail";
  }, { timeout: 5000 }).catch(() => {});
  return ((await page.textContent('[data-proof="external"]').catch(() => "")) ?? "").trim();
}

// --- 4) provable-local badge ---
await page.click("#local-badge");
check(await page.isVisible("#local-inspector"), "privacy inspector opens from the badge");
const netCount = await readSettledExternalProof();
check(netCount === "0", `inspector external-request counter reads 0 (got "${netCount}")`);
await page.keyboard.press("Escape");
check(!(await page.isVisible("#local-inspector")), "inspector closes on Escape");

// --- 5) theme toggle ---
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);

// --- 6) zero external requests ---
check(external.length === 0, `zero external network requests (found ${external.length}${external.length ? ": " + external.slice(0, 5).join(", ") : ""})`);

// Cloudflare injects an inline JavaScript-Detections loader
// (cdn-cgi/challenge-platform/scripts/jsd/main.js) zone-wide. Our strict CSP
// (no 'unsafe-inline') BLOCKS it by design - that is the moat working, not our
// bug: these apps ship zero inline executable scripts, so an INLINE-EXECUTION
// block can only be a third-party injection. Treat that exact violation as
// expected; still surface the count so it never hides a real one.
//
// Engine-agnostic on the inline wording: Chromium ("Executing inline script
// violates the following Content Security Policy directive 'script-src ...'",
// older builds "Refused to execute inline script"), Firefox ("blocked an
// inline script (script-src-elem)", newer "blocked the execution of an inline
// script"), WebKit ("Refused to execute a script because its hash, its nonce,
// or 'unsafe-inline' does not appear in the script-src directive").
//
// NOT excused: a blocked EXTERNAL script ("Refused to load the script
// 'https://...' ... script-src"). CSP stops it before the request, so the
// zero-external-request check above cannot see it either - a shipped external
// script tag is exactly what this suite exists to catch, so it must fail the
// console gate. scripts/smoke-harness.test.mjs holds that line.
const isExpectedCspBlock = (m) =>
  /content[-\s]security[-\s]policy/i.test(m) &&
  /script-src/i.test(m) &&
  (/(?:executing|refused to execute)(?: an?)? inline script/i.test(m) ||
    /blocked (?:an|the execution of an) inline script/i.test(m) ||
    /refused to execute a script/i.test(m)) &&
  !/(?:refused to load|blocked the loading of)/i.test(m);
const expectedCspBlocks = consoleErrors.filter(isExpectedCspBlock);
const functionalConsoleErrors = consoleErrors.filter((m) => !isExpectedCspBlock(m));
if (expectedCspBlocks.length) {
  console.log(`  NOTE  [${ENGINE}] CSP blocked ${expectedCspBlocks.length} injected inline script(s) — expected (CF JS-Detections), moat working`);
}

// --- 7) offline PWA boot ---
await page.evaluate(async () => {
  if (!("serviceWorker" in navigator)) return;
  await navigator.serviceWorker.ready;
  if (navigator.serviceWorker.controller) return;
  await new Promise((r) => { navigator.serviceWorker.addEventListener("controllerchange", r, { once: true }); setTimeout(r, 6000); });
});
await page.waitForTimeout(1200);
const swSupported = await page.evaluate(() => "serviceWorker" in navigator && !!navigator.serviceWorker.controller);
if (swSupported) {
  await context.setOffline(true);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForFunction(() => /\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 15000 }).catch(() => {});
  const offlineVersion = await page.textContent("#core-version").catch(() => "");
  check(/\d+\.\d+\.\d+/.test(offlineVersion ?? ""), `offline reload boots from SW cache (got "${offlineVersion}")`);
  await context.setOffline(false);
} else {
  console.log(`  SKIP  [${ENGINE}] offline: no SW controller`);
}

check(functionalConsoleErrors.length === 0, `no console/page errors during functional use (found ${functionalConsoleErrors.length}${functionalConsoleErrors.length ? ": " + functionalConsoleErrors.slice(0, 3).join(" | ") : ""})`);

await browser.close();
console.log(`${failures.length ? `IMG2PDF SMOKE FAILED (${ENGINE}): ${failures.length}` : `IMG2PDF SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
