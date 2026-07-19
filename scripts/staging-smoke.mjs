// Staging browser smoke for localbench. Runs on the staging box (10.96.16.18)
// against a served dist/. Verifies the real user flow, a varied real-PDF corpus,
// the actual drag-drop path, and the provable-local claim. One engine per run
// (ENGINE=chromium|firefox|webkit); run-staging.sh loops all three.
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile, readdir } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5055";
const OUT = process.env.OUT ?? "./shots";
const CORPUS = process.env.CORPUS ?? "./test-corpus";
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

const launchOpts = { args: ["--no-sandbox"] };
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

// helper: feed via file input, wait for terminal state, return {state, text}
async function feed(name, buffer) {
  await page.setInputFiles("#file-input", { name, mimeType: "application/pdf", buffer });
  await page.waitForFunction(() => {
    const s = document.querySelector("#result")?.dataset.state;
    return s === "success" || s === "error";
  }, { timeout: 20000 }).catch(() => {});
  return {
    state: await page.getAttribute("#result", "data-state"),
    text: await page.textContent("#result-text"),
  };
}

// --- 2) real-PDF corpus vs pdfinfo ground truth ---
const manifest = JSON.parse(await readFile(path.join(CORPUS, "manifest.json"), "utf8"));
for (const [name, expected] of Object.entries(manifest)) {
  const buffer = await readFile(path.join(CORPUS, name));
  const { state, text } = await feed(name, buffer);
  if (expected === "error") {
    // e.g. encrypted: must surface a clean error, not a wrong count or a hang.
    check(state === "error", `${name} surfaces a clean error -> ${state}: "${text}"`);
  } else if (expected === "tolerant") {
    check(state === "success" || state === "error", `${name} resolves cleanly -> ${state}: "${text}"`);
  } else {
    check(state === "success" && (text ?? "").startsWith(`${expected} page`), `${name} -> "${text}" (expected ${expected})`);
  }
}
// worker still alive after the whole corpus (incl. the tolerant/encrypted one)?
const alive = await feed("single.pdf", await readFile(path.join(CORPUS, "single.pdf")));
check(alive.state === "success" && (alive.text ?? "").startsWith("1 page"), `worker still alive after corpus -> "${alive.text}"`);

// --- 3) the REAL drop event (DataTransfer), not just the file input ---
const dropBuf = await readFile(path.join(CORPUS, "big50.pdf"));
const dt = await page.evaluateHandle((b64) => {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  const t = new DataTransfer();
  t.items.add(new File([bytes], "dropped.pdf", { type: "application/pdf" }));
  return t;
}, dropBuf.toString("base64"));
await page.dispatchEvent("#drop-zone", "dragenter", { dataTransfer: dt });
await page.dispatchEvent("#drop-zone", "dragover", { dataTransfer: dt });
await page.dispatchEvent("#drop-zone", "drop", { dataTransfer: dt });
await page.waitForFunction(() => document.querySelector("#result")?.dataset.state === "success", { timeout: 20000 }).catch(() => {});
const dropText = await page.textContent("#result-text");
check((dropText ?? "").startsWith("50 page"), `real drop event -> "${dropText}" (expected 50)`);
await page.screenshot({ path: `${OUT}/${ENGINE}-2-drop.png` });

// --- 3b) MERGE tool: switch, add files, merge, verify merged page count ---
await page.click('[data-tool="merge"]');
const mergeShown = (await page.isVisible("#merge-panel")) && !(await page.isVisible("#page-count-panel"));
check(mergeShown, "tool switcher shows merge panel, hides page-count");
await page.setInputFiles("#merge-file-input", [
  { name: "a-big50.pdf", mimeType: "application/pdf", buffer: await readFile(path.join(CORPUS, "big50.pdf")) },
  { name: "b-single.pdf", mimeType: "application/pdf", buffer: await readFile(path.join(CORPUS, "single.pdf")) },
]);
await page.waitForFunction(() => document.querySelector("#merge-button") && !document.querySelector("#merge-button").disabled, { timeout: 10000 }).catch(() => {});
const [download] = await Promise.all([
  page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
  page.click("#merge-button"),
]);
if (download) {
  const mergedPath = path.join(OUT, `${ENGINE}-merged.pdf`);
  await download.saveAs(mergedPath);
  await page.click('[data-tool="page-count"]');
  const merged = await feed("merged.pdf", await readFile(mergedPath));
  check(merged.state === "success" && (merged.text ?? "").startsWith("51 page"), `merged big50+single -> "${merged.text}" (expected 51)`);
} else {
  check(false, "merge produced a downloadable PDF");
}
await page.screenshot({ path: `${OUT}/${ENGINE}-3-merge.png` });

// --- 3c) ORGANIZE tool: load, add a range, export, verify page count ---
await page.click('[data-tool="organize"]');
check(await page.isVisible("#organize-panel"), "tool switcher shows organize panel");
await page.setInputFiles("#organize-file-input", { name: "src50.pdf", mimeType: "application/pdf", buffer: await readFile(path.join(CORPUS, "big50.pdf")) });
await page.waitForSelector("#organize-editor:not([hidden])", { timeout: 15000 }).catch(() => {});
const d0 = await page.evaluate(() => document.querySelectorAll("#organize-list > li").length);
await page.fill("#page-range-input", "1-3");
await page.click("#add-pages-button");
await page.waitForFunction((n) => document.querySelectorAll("#organize-list > li").length === n + 3, d0, { timeout: 8000 }).catch(() => {});
const d1 = await page.evaluate(() => document.querySelectorAll("#organize-list > li").length);
const [orgDl] = await Promise.all([
  page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
  page.click("#organize-button"),
]);
if (orgDl) {
  const op = path.join(OUT, `${ENGINE}-organized.pdf`);
  await orgDl.saveAs(op);
  await page.click('[data-tool="page-count"]');
  const oc = await feed("organized.pdf", await readFile(op));
  check(oc.state === "success" && (oc.text ?? "").startsWith(`${d1} page`), `organized export (${d0}+3) -> "${oc.text}" (expected ${d1})`);
} else {
  check(false, "organize produced a downloadable PDF");
}

// --- 3d) COMPRESS tool: load image-heavy PDF, compress Strong, verify real reduction ---
await page.click('[data-tool="compress"]');
check(await page.isVisible("#compress-panel"), "tool switcher shows compress panel");
const photoBuf = await readFile(path.join(CORPUS, "photo.pdf"));
await page.setInputFiles("#compress-file-input", { name: "photo.pdf", mimeType: "application/pdf", buffer: photoBuf });
await page.waitForSelector("#compress-editor:not([hidden])", { timeout: 15000 }).catch(() => {});
await page.check('input[name="compress-quality"][value="30"]'); // Strong
await page.click("#compress-button");
await page.waitForSelector("#compress-output:not([hidden])", { timeout: 30000 }).catch(() => {});
const savedText = (await page.textContent("#compress-saved-percent").catch(() => "")) ?? "";
const beforeText = await page.textContent("#compress-before-size").catch(() => "");
const afterText = await page.textContent("#compress-after-size").catch(() => "");
const savedPct = parseFloat(savedText.replace(/[^\d.]/g, ""));
check(Number.isFinite(savedPct) && savedPct > 0, `compress Strong on 447KB photo PDF: ${beforeText} -> ${afterText}, saved ${savedText}`);
// verify the compressed output is a valid 1-page PDF
const [cmpDl] = await Promise.all([
  page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
  page.click("#compress-download-button"),
]);
if (cmpDl) {
  const cp = path.join(OUT, `${ENGINE}-compressed.pdf`);
  await cmpDl.saveAs(cp);
  const cbytes = await readFile(cp);
  check(cbytes.length < photoBuf.length, `compressed file smaller (${cbytes.length} < ${photoBuf.length})`);
  await page.click('[data-tool="page-count"]');
  const cc = await feed("compressed.pdf", cbytes);
  check(cc.state === "success" && (cc.text ?? "").startsWith("1 page"), `compressed PDF still valid -> "${cc.text}"`);
} else {
  check(false, "compress produced a downloadable PDF");
}

// --- 4) theme toggle ---
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);

// --- 5) provable-local: zero external requests during the whole flow ---
check(external.length === 0, `zero external network requests (found ${external.length}${external.length ? ": " + external.slice(0,5).join(", ") : ""})`);

// --- 6) offline PWA: wait for SW control, go offline, reload, still boots ---
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
  console.log(`  SKIP  [${ENGINE}] offline: no SW controller (engine may not support SW in headless)`);
}

check(consoleErrors.length === 0, `no console/page errors (found ${consoleErrors.length}${consoleErrors.length ? ": " + consoleErrors.slice(0,3).join(" | ") : ""})`);

await browser.close();
console.log(`${failures.length ? `SMOKE FAILED (${ENGINE}): ${failures.length}` : `SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
