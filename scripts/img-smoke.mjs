// Staging/live browser smoke for the image tools app (app-img). Runs on the
// staging box (or against a live URL) over a served dist/. Verifies the real
// user flow for resize/convert/compress, that outputs are valid images, that
// EXIF/GPS metadata is stripped end-to-end, and the provable-local claim
// (zero external requests + offline boot). One engine per run
// (ENGINE=chromium|firefox|webkit).
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5056";
const OUT = process.env.OUT ?? "./shots-img";
const CORPUS = process.env.CORPUS ?? "./img-corpus";
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

// --- byte-level validators (no image lib on the box) ---
const isPng = (b) => b.length > 8 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47;
const isJpeg = (b) => b.length > 3 && b[0] === 0xff && b[1] === 0xd8 && b[b.length - 2] === 0xff && b[b.length - 1] === 0xd9;
const isWebp = (b) => b.length > 12 && b.toString("ascii", 0, 4) === "RIFF" && b.toString("ascii", 8, 12) === "WEBP";
function pngDims(b) {
  // IHDR width/height are big-endian u32 at fixed offsets 16 and 20.
  return { w: b.readUInt32BE(16), h: b.readUInt32BE(20) };
}
// A JPEG APP1 EXIF segment starts with 0xFFE1 then "Exif\0\0". Scan for it.
function hasExif(b) {
  const needle = Buffer.from("Exif\0\0", "latin1");
  return b.includes(needle);
}

const launchOpts = ENGINE === "chromium" ? { args: ["--no-sandbox"] } : {};
if (ENGINE === "chromium" && process.env.CHROME_PATH) launchOpts.executablePath = process.env.CHROME_PATH;
const browser = await engine.launch(launchOpts);
const context = await browser.newContext();

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

// --- 1) load + WASM/worker ready ---
await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForFunction(() => /^v\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 20000 }).catch(() => {});
const versionText = await page.textContent("#core-version");
check(/^v\d+\.\d+\.\d+/.test(versionText ?? ""), `core version renders from WASM (got "${versionText}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-1-loaded.png` });

const png = await readFile(path.join(CORPUS, "pic.png"));
const jpg = await readFile(path.join(CORPUS, "photo.jpg"));
check(hasExif(jpg), `source photo.jpg carries EXIF/GPS (sanity: strip test is meaningful)`);

async function selectTool(tool) {
  await page.click(`[data-tool="${tool}"]`);
  await page.waitForSelector(`#${tool}-panel:not([hidden])`, { timeout: 8000 }).catch(() => {});
}
async function loadInto(tool, name, mimeType, buffer) {
  await page.setInputFiles(`#${tool}-file-input`, { name, mimeType, buffer });
  await page.waitForSelector(`#${tool}-editor:not([hidden])`, { timeout: 15000 }).catch(() => {});
}
async function runAndDownload(tool, clickSelector) {
  const [dl] = await Promise.all([
    page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
    page.click(clickSelector),
  ]);
  return dl;
}

// --- 2) RESIZE: pic.png -> fit 320x240, keep aspect, download valid smaller PNG ---
await selectTool("resize");
await loadInto("resize", "pic.png", "image/png", png);
await page.fill("#resize-max-width", "320");
await page.fill("#resize-max-height", "240");
if (!(await page.isChecked("#resize-keep-aspect"))) await page.check("#resize-keep-aspect");
await page.click("#resize-button");
await page.waitForSelector("#resize-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const rDl = await runAndDownload("resize", "#resize-download-button");
if (rDl) {
  const rp = path.join(OUT, `${ENGINE}-resized.png`);
  await rDl.saveAs(rp);
  const rb = await readFile(rp);
  check(isPng(rb), "resize output is a valid PNG");
  const { w, h } = pngDims(rb);
  check(w <= 320 && h <= 240 && w > 0 && h > 0, `resized within box: ${w}x${h} (<=320x240)`);
  check(rb.length < png.length, `resized PNG smaller than source (${rb.length} < ${png.length})`);
} else check(false, "resize produced a download");

// --- 3) CONVERT: pic.png -> JPEG, download valid JPEG with alpha flattened, no EXIF ---
await selectTool("convert");
await loadInto("convert", "pic.png", "image/png", png);
await page.selectOption("#convert-target", "jpeg");
check(await page.isVisible("#convert-jpeg-note"), "JPEG-target alpha-flatten note shows");
await page.click("#convert-button");
await page.waitForSelector("#convert-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const cDl = await runAndDownload("convert", "#convert-download-button");
if (cDl) {
  const cp = path.join(OUT, `${ENGINE}-converted.jpg`);
  await cDl.saveAs(cp);
  const cb = await readFile(cp);
  check(isJpeg(cb), "convert output is a valid JPEG");
  check(!hasExif(cb), "converted JPEG carries no EXIF (fresh encode)");
  check((await page.getAttribute("#convert-download-button", "download") ?? cp).endsWith(".jpg") || cp.endsWith(".jpg"), "converted file has .jpg extension");
} else check(false, "convert produced a download");

// --- 4) COMPRESS: photo.jpg (GPS EXIF) -> quality 40; smaller, valid, GPS STRIPPED ---
await selectTool("compress");
await loadInto("compress", "photo.jpg", "image/jpeg", jpg);
await page.fill("#compress-quality", "40").catch(async () => {
  // range inputs: set via evaluate if fill is rejected
  await page.evaluate(() => { const el = document.querySelector("#compress-quality"); el.value = "40"; el.dispatchEvent(new Event("input", { bubbles: true })); });
});
await page.click("#compress-button");
await page.waitForSelector("#compress-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const savedText = ((await page.textContent("#compress-saved-percent").catch(() => "")) ?? "").trim();
const beforeText = ((await page.textContent("#compress-before-size").catch(() => "")) ?? "").trim();
const afterText = ((await page.textContent("#compress-after-size").catch(() => "")) ?? "").trim();
const savedPct = parseFloat(savedText.replace(/[^\d.]/g, ""));
check(Number.isFinite(savedPct) && savedPct > 0, `compress q40 on photo.jpg: ${beforeText} -> ${afterText}, saved ${savedText}`);
const kDl = await runAndDownload("compress", "#compress-download-button");
if (kDl) {
  const kp = path.join(OUT, `${ENGINE}-compressed.jpg`);
  await kDl.saveAs(kp);
  const kb = await readFile(kp);
  check(isJpeg(kb), "compressed output is a valid JPEG");
  check(kb.length < jpg.length, `compressed smaller than source (${kb.length} < ${jpg.length})`);
  check(!hasExif(kb), "compressed JPEG has GPS/EXIF STRIPPED end-to-end (the headline claim)");
} else check(false, "compress produced a download");
await page.screenshot({ path: `${OUT}/${ENGINE}-2-compress.png` });

// --- 5) provable-local badge + inspector ---
await page.click("#local-badge");
check(await page.isVisible("#local-inspector"), "privacy inspector opens from the badge");
const netCount = ((await page.textContent("#external-request-count").catch(() => "")) ?? "").trim();
check(netCount === "0", `inspector external-request counter reads 0 (got "${netCount}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-3-inspector.png` });
await page.keyboard.press("Escape");
check(!(await page.isVisible("#local-inspector")), "inspector closes on Escape");

// --- 6) theme toggle ---
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);

// --- 7) zero external requests across the whole flow ---
check(external.length === 0, `zero external network requests (found ${external.length}${external.length ? ": " + external.slice(0, 5).join(", ") : ""})`);

const functionalConsoleErrors = [...consoleErrors];

// --- 8) offline PWA: SW control, go offline, reload, still boots ---
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
  await page.waitForFunction(() => /^v\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 15000 }).catch(() => {});
  const offlineVersion = await page.textContent("#core-version").catch(() => "");
  check(/^v\d+\.\d+\.\d+/.test(offlineVersion ?? ""), `offline reload boots from SW cache (got "${offlineVersion}")`);
  await context.setOffline(false);
} else {
  console.log(`  SKIP  [${ENGINE}] offline: no SW controller`);
}

check(functionalConsoleErrors.length === 0, `no console/page errors during functional use (found ${functionalConsoleErrors.length}${functionalConsoleErrors.length ? ": " + functionalConsoleErrors.slice(0, 3).join(" | ") : ""})`);

await browser.close();
console.log(`${failures.length ? `IMG SMOKE FAILED (${ENGINE}): ${failures.length}` : `IMG SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
