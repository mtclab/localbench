// Staging/live browser smoke for the archive app (app-zip). Runs on the staging
// box (or a live URL) over a served dist/. Verifies create (multi-file -> valid
// deterministic zip), extract (list + per-entry byte-identical download), the
// zip-slip flag, the encrypted-reject path, round-trip, and the provable-local
// claim (zero external + offline). One engine per run (ENGINE=chromium|firefox|webkit).
import { chromium, firefox, webkit } from "playwright";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";

const ENGINE = process.env.ENGINE ?? "chromium";
const BASE = process.env.BASE ?? "http://127.0.0.1:5058";
const OUT = process.env.OUT ?? "./shots-zip";
const CORPUS = process.env.CORPUS ?? "./zip-corpus";
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

const isZip = (b) => b.length > 4 && b[0] === 0x50 && b[1] === 0x4b && b[2] === 0x03 && b[3] === 0x04;
const bytesEqual = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

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

async function download(clickSelector) {
  const [dl] = await Promise.all([
    page.waitForEvent("download", { timeout: 30000 }).catch(() => null),
    page.click(clickSelector),
  ]);
  return dl;
}
async function save(dl, name) {
  const p = path.join(OUT, name);
  await dl.saveAs(p);
  return readFile(p);
}

// --- 1) load + WASM ready ---
await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForFunction(() => /^v\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 20000 }).catch(() => {});
const versionText = await page.textContent("#core-version");
check(/^v\d+\.\d+\.\d+/.test(versionText ?? ""), `core version renders from WASM (got "${versionText}")`);
await page.screenshot({ path: `${OUT}/${ENGINE}-1-loaded.png` });

// --- 2) CREATE: two inline files -> valid zip; deterministic; round-trips ---
const fileA = { name: "greeting.txt", mimeType: "text/plain", buffer: Buffer.from("hello from the create flow\n") };
const fileB = { name: "data.json", mimeType: "application/json", buffer: Buffer.from('{"local":true,"uploaded":false}') };
async function createZipDownload(shotName) {
  await page.click('[data-tool="create"]');
  await page.setInputFiles("#create-file-input", [fileA, fileB]);
  await page.waitForSelector("#create-editor:not([hidden])", { timeout: 10000 }).catch(() => {});
  await page.click("#create-button");
  await page.waitForSelector("#create-output:not([hidden])", { timeout: 30000 }).catch(() => {});
  const dl = await download("#create-download-button");
  return dl ? save(dl, shotName) : null;
}
const zip1 = await createZipDownload(`${ENGINE}-created-1.zip`);
if (zip1) {
  check(isZip(zip1), "create output is a valid ZIP (PK\\x03\\x04)");
} else check(false, "create produced a download");
// reset the create tool (remove the two files) before a second deterministic build
for (const _ of [0, 1]) await page.click(".remove-action").catch(() => {});
const zip2 = await createZipDownload(`${ENGINE}-created-2.zip`);
if (zip1 && zip2) {
  check(bytesEqual(zip1, zip2), `create is byte-deterministic (${zip1.length} == ${zip2?.length})`);
}
await page.screenshot({ path: `${OUT}/${ENGINE}-2-create.png` });

// loadExtract: set the file, then wait for THIS zip's own rendered content
// (waiting on #extract-editor visibility alone races when it's already shown).
async function loadExtract(name, buffer, waitSelector) {
  await page.setInputFiles("#extract-file-input", { name, mimeType: "application/zip", buffer });
  await page.waitForSelector(waitSelector, { timeout: 15000, state: "visible" }).catch(() => {});
}

// round-trip: feed the created zip into extract, expect 2 entries
if (zip1) {
  await page.click('[data-tool="extract"]');
  await loadExtract("created.zip", zip1, 'button[aria-label="Download greeting.txt"]');
  const rtCount = ((await page.textContent("#extract-entry-count").catch(() => "")) ?? "").trim();
  check(/2 entr/.test(rtCount), `round-trip: created zip lists 2 entries (got "${rtCount}")`);
  const grab = await download('button[aria-label="Download greeting.txt"]');
  if (grab) {
    const gb = await save(grab, `${ENGINE}-roundtrip-greeting.txt`);
    check(bytesEqual(gb, fileA.buffer), "round-trip: extracted greeting.txt is byte-identical to the input");
  } else check(false, "round-trip: greeting.txt extract produced a download");
}

// --- 3) EXTRACT sample.zip: list 3 entries, extract hello.txt byte-identical ---
const sample = await readFile(path.join(CORPUS, "sample.zip"));
await page.click('[data-tool="extract"]');
await loadExtract("sample.zip", sample, 'button[aria-label="Download hello.txt"]');
const sCount = ((await page.textContent("#extract-entry-count").catch(() => "")) ?? "").trim();
check(/3 entr/.test(sCount), `sample.zip lists 3 entries (got "${sCount}")`);
const hDl = await download('button[aria-label="Download hello.txt"]');
if (hDl) {
  const hb = await save(hDl, `${ENGINE}-hello.txt`);
  check(bytesEqual(hb, Buffer.from("hello world from a local zip\n")), "extracted hello.txt is byte-identical (lossless extract)");
} else check(false, "sample.zip hello.txt produced a download");
await page.screenshot({ path: `${OUT}/${ENGINE}-3-extract.png` });

// --- 4) EXTRACT slip.zip: the "../evil.txt" entry is flagged unsafe ---
const slip = await readFile(path.join(CORPUS, "slip.zip"));
await loadExtract("slip.zip", slip, 'button[aria-label="Download safe.txt"]');
check(await page.isVisible("text=Unsafe path"), "slip.zip flags a zip-slip entry as Unsafe path");

// --- 5) EXTRACT secret.zip: encrypted entry rejected cleanly on extract ---
let secret = null;
try { secret = await readFile(path.join(CORPUS, "secret.zip")); } catch { /* CLI absent */ }
if (secret) {
  /*
   * The reject is REPORTED at list time, not discovered by clicking: the core
   * marks an encrypted entry unextractable, so its action cell renders a
   * `.entry-blocked` status label ("Password-protected") INSTEAD of a Download
   * button, and the archive-wide "About this archive" note above the list says
   * why. Assert both, against the real markup - the old assertion clicked a
   * per-row button that no longer exists and grepped #extract-result-text for
   * the word "password", so it could only ever go green by accident.
   *
   * The wait is on the blocked label itself: no earlier archive in this run
   * produces one (a zip-slip entry is flagged unsafe but still extractable),
   * so it cannot match a previous zip's leftover render.
   */
  await loadExtract("secret.zip", secret, "#extract-entry-table .entry-blocked");
  const enc = await page.evaluate(() => {
    const rows = [...document.querySelectorAll("#extract-entry-table > [role='row']")]
      .filter((row) => row.querySelector(".file-order")); // drop the column header
    const row = rows.find((r) => r.querySelector(".file-actions .entry-blocked")) ?? rows[0];
    const action = row?.querySelector(".file-actions");
    const warnings = document.querySelector("#extract-warnings");
    return {
      rows: rows.length,
      name: row?.querySelector(".file-details .file-name")?.textContent?.trim() ?? "",
      label: action?.querySelector(".entry-blocked")?.textContent?.trim() ?? "",
      offersDownload: !!action?.querySelector("button"),
      noteShown: warnings instanceof HTMLElement && !warnings.hidden,
      noteTitle: warnings?.querySelector(".notices-title")?.textContent?.trim() ?? "",
      notes: [...document.querySelectorAll("#extract-warning-list > li.notice")]
        .map((li) => (li.textContent ?? "").trim()),
    };
  });
  const rowIsBlocked = enc.label === "Password-protected" && !enc.offersDownload;
  const noteExplains = enc.noteShown && enc.notes.some((t) => /password-protected/i.test(t));
  check(
    rowIsBlocked && noteExplains,
    `secret.zip: entry "${enc.name}" carries the "${enc.label}" status label instead of a download button` +
      `, and the "${enc.noteTitle}" note explains it ("${enc.notes.join(" | ")}")`,
  );
} else {
  console.log(`  SKIP  [${ENGINE}] secret.zip not in corpus (zip CLI absent at build)`);
}

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

// --- 6) provable-local badge + inspector ---
await page.click("#local-badge");
check(await page.isVisible("#local-inspector"), "privacy inspector opens from the badge");
const netCount = await readSettledExternalProof();
check(netCount === "0", `inspector external-request counter reads 0 (got "${netCount}")`);
await page.keyboard.press("Escape");
check(!(await page.isVisible("#local-inspector")), "inspector closes on Escape");

// --- 7) theme toggle ---
const before = await page.getAttribute("html", "data-theme");
await page.click("#theme-toggle");
const after = await page.getAttribute("html", "data-theme");
check(before !== after && (after === "light" || after === "dark"), `theme toggles ${before} -> ${after}`);

// --- 8) zero external requests ---
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

// --- 9) offline PWA boot ---
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
console.log(`${failures.length ? `ZIP SMOKE FAILED (${ENGINE}): ${failures.length}` : `ZIP SMOKE PASSED (${ENGINE})`}`);
process.exit(failures.length ? 1 : 0);
