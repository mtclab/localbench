// Staging/live browser smoke for the metadata scrubber app (app-scrub). Runs on
// the staging box (or against a live URL) over a served dist/. Verifies the real
// inspect->strip->download flow for JPEG/PNG/PDF, that removable metadata is
// listed then GONE from the downloaded bytes end-to-end, the already-clean path,
// and the provable-local claim (zero external requests + offline boot).
// One engine per run (ENGINE=chromium|firefox|webkit).
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5057";
const OUT = process.env.OUT ?? "./shots-scrub";
const CORPUS = process.env.CORPUS ?? "./scrub-corpus";
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

// --- byte-level validators (no image/pdf lib on the box) ---
const isPng = (b) => b.length > 8 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47;
const isJpeg = (b) => b.length > 3 && b[0] === 0xff && b[1] === 0xd8 && b[b.length - 2] === 0xff && b[b.length - 1] === 0xd9;
const isPdf = (b) => b.length > 5 && b.toString("latin1", 0, 5) === "%PDF-";
const has = (b, needle) => b.includes(Buffer.from(needle, "latin1"));
function pngDims(b) {
  return { w: b.readUInt32BE(16), h: b.readUInt32BE(20) };
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

const photo = await readFile(path.join(CORPUS, "photo.jpg"));
const tagged = await readFile(path.join(CORPUS, "tagged.png"));
let ztxt = null;
try { ztxt = await readFile(path.join(CORPUS, "ztxt.png")); } catch { /* optional */ }
const clean = await readFile(path.join(CORPUS, "clean.png"));
let doc = null;
try { doc = await readFile(path.join(CORPUS, "doc.pdf")); } catch { /* optional */ }

// Sanity: sources actually carry the metadata we claim to strip.
check(has(photo, "Exif\0\0"), "source photo.jpg carries EXIF (strip test is meaningful)");
check(has(tagged, "Adobe Photoshop"), "source tagged.png carries a Software text chunk");
if (doc) check(has(doc, "Olli Kurki"), "source doc.pdf carries an /Info Author (strip test is meaningful)");

async function load(name, mimeType, buffer) {
  await page.setInputFiles("#scrub-file-input", { name, mimeType, buffer });
  await page.waitForSelector("#scrub-editor:not([hidden])", { timeout: 15000 }).catch(() => {});
  await page.waitForSelector("#metadata-report:not([hidden])", { timeout: 15000 }).catch(() => {});
}
async function scrubAndDownload() {
  await page.click("#scrub-button");
  await page.waitForSelector("#scrub-output:not([hidden])", { timeout: 30000 }).catch(() => {});
  const [dl] = await Promise.all([
    page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
    page.click("#scrub-download-button"),
  ]);
  return dl;
}
async function saveDownload(dl, filename) {
  const p = path.join(OUT, filename);
  await dl.saveAs(p);
  return readFile(p);
}

// --- 2) JPEG with GPS EXIF: inspect flags GPS, scrub strips it losslessly ---
await load("photo.jpg", "image/jpeg", photo);
const jCount = parseInt(((await page.textContent("#metadata-count").catch(() => "")) ?? "").trim(), 10);
check(Number.isFinite(jCount) && jCount > 0, `photo.jpg inspect lists metadata (${jCount} items)`);
check(await page.isVisible("text=GPS location"), "photo.jpg inspect flags a GPS-location sensitive item");
const jDl = await scrubAndDownload();
if (jDl) {
  const jb = await saveDownload(jDl, `${ENGINE}-photo-clean.jpg`);
  check(isJpeg(jb), "scrubbed JPEG is still a valid JPEG (SOI..EOI)");
  check(!has(jb, "Exif\0\0"), "scrubbed JPEG has EXIF/GPS REMOVED end-to-end (headline claim)");
  check(!has(jb, "secret location note"), "scrubbed JPEG comment segment removed");
  check(jb.length <= photo.length, `scrubbed JPEG never grows (${jb.length} <= ${photo.length})`);
  const removed = ((await page.textContent("#scrub-removed-summary").catch(() => "")) ?? "").trim();
  check(/EXIF/i.test(removed), `removed-summary names EXIF ("${removed}")`);
} else check(false, "photo.jpg produced a scrubbed download");
await page.screenshot({ path: `${OUT}/${ENGINE}-2-jpeg.png` });

// --- 3) PNG with tEXt + eXIf: inspect lists them, scrub strips, IDAT intact ---
await load("tagged.png", "image/png", tagged);
check(await page.isVisible("text=Adobe Photoshop"), "tagged.png inspect surfaces the Software text value");
const tDl = await scrubAndDownload();
if (tDl) {
  const tb = await saveDownload(tDl, `${ENGINE}-tagged-clean.png`);
  check(isPng(tb), "scrubbed PNG is still a valid PNG");
  check(!has(tb, "Adobe Photoshop"), "scrubbed PNG text chunk REMOVED");
  check(!has(tb, "tEXt"), "scrubbed PNG has no tEXt chunk type remaining");
  check(!has(tb, "eXIf"), "scrubbed PNG has no eXIf chunk remaining");
  const { w, h } = pngDims(tb);
  check(w === 320 && h === 240, `scrubbed PNG dimensions unchanged (${w}x${h}, lossless)`);
} else check(false, "tagged.png produced a scrubbed download");

// --- 3b) PNG with a COMPRESSED zTXt: inspect INFLATES it (shows hidden GPS) ---
if (ztxt) {
  await load("ztxt.png", "image/png", ztxt);
  check(await page.isVisible("text=GPSLatitude"),
    "compressed zTXt is inflated in the browser and its hidden GPS text is shown");
  const zDl = await scrubAndDownload();
  if (zDl) {
    const zb = await saveDownload(zDl, `${ENGINE}-ztxt-clean.png`);
    check(isPng(zb) && !has(zb, "zTXt"), "scrubbed PNG has the zTXt chunk removed");
  } else check(false, "ztxt.png produced a scrubbed download");
} else {
  console.log(`  SKIP  [${ENGINE}] ztxt.png not in corpus`);
}

// --- 4) PDF with /Info: inspect lists it, scrub strips, body survives ---
if (doc) {
  await load("doc.pdf", "application/pdf", doc);
  const pCount = parseInt(((await page.textContent("#metadata-count").catch(() => "")) ?? "").trim(), 10);
  check(Number.isFinite(pCount) && pCount > 0, `doc.pdf inspect lists /Info metadata (${pCount} items)`);
  const pDl = await scrubAndDownload();
  if (pDl) {
    const pb = await saveDownload(pDl, `${ENGINE}-doc-clean.pdf`);
    check(isPdf(pb), "scrubbed PDF is still a valid PDF (%PDF-)");
    check(!has(pb, "Olli Kurki"), "scrubbed PDF /Info Author REMOVED");
    check(!has(pb, "Quarterly Numbers"), "scrubbed PDF /Info Title REMOVED");
  } else check(false, "doc.pdf produced a scrubbed download");
} else {
  console.log(`  SKIP  [${ENGINE}] doc.pdf not in corpus (reportlab was unavailable at build)`);
}

// --- 5) already-clean file: no metadata, scrub button disabled ---
await load("clean.png", "image/png", clean);
check(await page.isVisible("#metadata-clean"), "clean.png shows the already-clean state");
check(await page.isDisabled("#scrub-button"), "scrub button is disabled for an already-clean file");

// --- 6) provable-local badge + inspector ---
await page.click("#local-badge");
check(await page.isVisible("#local-inspector"), "privacy inspector opens from the badge");
const netCount = ((await page.textContent("#external-request-count").catch(() => "")) ?? "").trim();
check(netCount === "0", `inspector external-request counter reads 0 (got "${netCount}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-3-inspector.png` });
await page.keyboard.press("Escape");
check(!(await page.isVisible("#local-inspector")), "inspector closes on Escape");

// --- 7) theme toggle ---
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);

// --- 8) zero external requests across the whole flow ---
check(external.length === 0, `zero external network requests (found ${external.length}${external.length ? ": " + external.slice(0, 5).join(", ") : ""})`);

const functionalConsoleErrors = [...consoleErrors];

// --- 9) offline PWA: SW control, go offline, reload, still boots ---
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
console.log(`${failures.length ? `SCRUB SMOKE FAILED (${ENGINE}): ${failures.length}` : `SCRUB SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
