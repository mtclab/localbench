/*
 * The gate on the in-page receipt.
 *
 * The receipt's headline number claims to count external requests. It used to
 * count `performance.getEntriesByType("resource")` IN THE WINDOW — while every
 * byte of the user's file is processed in a dedicated worker, which has its own
 * Resource Timing timeline. A request made from the worker never appeared. The
 * receipt printed a reassuring 0 for the one context that mattered.
 *
 * This script drives the REAL built dist in a real browser and proves three
 * things that a unit test cannot:
 *
 *  1. healthy        — a clean load reports a real number (not "Checking…", not
 *                      "Unproven"), and that number is 0.
 *  2. worker-external— when the WORKER makes a cross-origin request, the receipt
 *                      moves to 1. The same case also asserts, in the live page,
 *                      that the OLD algorithm (window-only getEntriesByType)
 *                      still sees 0 — so this case fails the moment the worker
 *                      reporting is reverted. That is the teeth.
 *  3. page-external  — the page half still works on its own.
 *  4. worker-silent  — when the worker cannot answer, the receipt says the
 *                      number is unproven. It must never fall back to 0.
 *
 * Venue: this runs on the staging box against a served dist/, never on the dev
 * workspace. See scripts/staging-smoke.mjs for the same convention.
 *
 * Usage: node scripts/receipt-proof.mjs [dist-directory]
 */
import { chromium } from "playwright";
import { readFile, readdir } from "node:fs/promises";
import { createServer } from "node:http";
import path from "node:path";

const DIST = path.resolve(process.argv[2] ?? "app/dist");
const HOST = "127.0.0.1";
let nextPort = Number(process.env.PORT ?? 5175);

const CONTENT_TYPES = new Map([
  [".css", "text/css"],
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript"],
  [".json", "application/json"],
  [".png", "image/png"],
  [".svg", "image/svg+xml"],
  [".txt", "text/plain"],
  [".wasm", "application/wasm"],
  [".webmanifest", "application/manifest+json"],
  [".xml", "application/xml"],
]);

async function filesBelow(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const nested = await Promise.all(
    entries.map(async (entry) => {
      const absolute = path.join(directory, entry.name);
      return entry.isDirectory() ? filesBelow(absolute) : [absolute];
    }),
  );
  return nested.flat();
}

/** The Content-Security-Policy the real deployment serves, from dist/_headers. */
async function servedPolicy() {
  const text = await readFile(path.join(DIST, "_headers"), "utf8");
  const line = text.split(/\r?\n/).find((row) => /^\s+Content-Security-Policy:/i.test(row));
  if (!line) throw new Error("dist/_headers carries no Content-Security-Policy");
  return line.replace(/^\s*Content-Security-Policy:\s*/i, "").trim();
}

/** Serves dist/ under the real policy, optionally rewriting one asset. */
async function serveDist({ policy, rewrite }) {
  const port = nextPort++;
  const files = new Map();
  for (const file of await filesBelow(DIST)) {
    files.set(`/${path.relative(DIST, file).split(path.sep).join("/")}`, file);
  }

  const server = createServer(async (request, response) => {
    const url = new URL(request.url, `http://${HOST}:${port}`);
    const pathname = url.pathname === "/" ? "/index.html" : url.pathname;
    const file = files.get(pathname);
    if (!file) {
      response.writeHead(404).end("not found");
      return;
    }

    const extension = path.extname(file);
    const headers = {
      "Content-Type": CONTENT_TYPES.get(extension) ?? "application/octet-stream",
      "Content-Security-Policy": policy,
      "Cache-Control": "no-store",
    };

    const isText = CONTENT_TYPES.get(extension)?.startsWith("text") || extension === ".json";
    const body = isText ? await readFile(file, "utf8") : await readFile(file);
    const patched = typeof body === "string" && rewrite ? rewrite(pathname, body) : body;
    response.writeHead(200, headers).end(patched);
  });

  await new Promise((resolve) => server.listen(port, HOST, resolve));
  return { port, origin: `http://${HOST}:${port}`, close: () => new Promise((r) => server.close(r)) };
}

/** A second origin the worker/page can be made to call. Counts its hits. */
async function serveProbe() {
  const port = nextPort++;
  let hits = 0;
  const server = createServer((_request, response) => {
    hits += 1;
    response
      .writeHead(200, { "Content-Type": "text/plain", "Access-Control-Allow-Origin": "*" })
      .end("probe");
  });
  await new Promise((resolve) => server.listen(port, HOST, resolve));
  return {
    origin: `http://${HOST}:${port}`,
    url: `http://${HOST}:${port}/probe.txt`,
    hits: () => hits,
    close: () => new Promise((r) => server.close(r)),
  };
}

/** Chunk names change every build; read them out of the generated service worker. */
async function chunkPaths() {
  const serviceWorker = await readFile(path.join(DIST, "sw.js"), "utf8");
  const worker = serviceWorker.match(/\/assets\/core\.worker-[\w-]+\.js/)?.[0];
  const entry = serviceWorker.match(/\/assets\/index-[\w-]+\.js/)?.[0];
  if (!worker || !entry) throw new Error("could not find the built chunks in sw.js");
  return { worker, entry };
}

const failures = [];
const check = (condition, message) => {
  console.log(`  ${condition ? "PASS" : "FAIL"}  ${message}`);
  if (!condition) failures.push(message);
};

// CHROME_PATH pins the browser binary when the box's installed build does not
// match what this playwright version downloads — same convention as
// scripts/staging-smoke.mjs.
const launchOptions = { args: ["--no-sandbox"] };
if (process.env.CHROME_PATH) launchOptions.executablePath = process.env.CHROME_PATH;
const browser = await chromium.launch(launchOptions);

/** Loads the app, waits for the receipt to settle, and reports what it shows. */
async function readReceipt(origin, { afterLoad } = {}) {
  const context = await browser.newContext();
  const page = await context.newPage();
  if (process.env.DEBUG) {
    page.on("pageerror", (error) => console.log(`    [pageerror] ${error.message}`));
    page.on("console", (message) => console.log(`    [console:${message.type()}] ${message.text()}`));
    page.on("requestfailed", (request) =>
      console.log(`    [requestfailed] ${request.url()} ${request.failure()?.errorText}`),
    );
  }
  await page.goto(`${origin}/`, { waitUntil: "load" });
  await page.waitForFunction(
    () =>
      !(document.querySelector('[data-proof="external"]')?.textContent ?? "Checking")
        .trim()
        .startsWith("Checking"),
    undefined,
    { timeout: 10_000 },
  );

  if (afterLoad) await afterLoad(page);

  await page.click("[data-proof-recheck]");
  await page.waitForFunction(
    () =>
      !(document.querySelector('[data-proof="external"]')?.textContent ?? "Checking")
        .trim()
        .startsWith("Checking"),
    undefined,
    { timeout: 10_000 },
  );

  const reading = await page.evaluate(() => {
    const slot = document.querySelector('[data-proof="external"]');
    const label = slot?.closest("div")?.querySelector("dt")?.textContent?.trim() ?? "";
    // The pre-fix algorithm, evaluated live in this very page: the window's own
    // timeline. Anything the worker requested is invisible to it.
    const windowOnly = performance.getEntriesByType("resource").filter((entry) => {
      try {
        return new URL(entry.name, location.href).origin !== location.origin;
      } catch {
        return true;
      }
    }).length;
    return { label, text: slot?.textContent?.trim() ?? "", state: slot?.dataset.state ?? "", windowOnly };
  });

  await context.close();
  return reading;
}

const policy = await servedPolicy();
const chunks = await chunkPaths();

/*
 * Widens connect-src for one probe origin. Both places have to be widened for
 * the DOCUMENT: a page is bound by the intersection of the served header and
 * the <meta> fallback. The dedicated WORKER, by contrast, is bound by the
 * policy served with its own script — the document's meta tag does not reach
 * it. (Which is exactly why the receipt shows the policy rows next to the count
 * rather than resting on either one alone.)
 */
function relax(probeOrigin) {
  return policy.replace("connect-src 'self'", `connect-src 'self' ${probeOrigin}`);
}

function relaxDocument(probeOrigin) {
  return (pathname, body) =>
    pathname === "/index.html"
      ? body.replace("connect-src 'self'", `connect-src 'self' ${probeOrigin}`)
      : body;
}

// ---------------------------------------------------------------------------
console.log("\n[1] healthy load");
{
  const site = await serveDist({ policy });
  const reading = await readReceipt(site.origin);
  check(
    reading.label === "External requests (page + worker)",
    `the receipt row names both contexts (got "${reading.label}")`,
  );
  check(reading.text === "0", `a clean load reports a real number, not a placeholder (got "${reading.text}")`);
  check(reading.state === "", `a clean load is not flagged (got state "${reading.state}")`);
  await site.close();
}

// ---------------------------------------------------------------------------
console.log("\n[2] the WORKER makes a cross-origin request");
{
  const probe = await serveProbe();
  // The request has to be permitted, or CSP blocks it and Resource Timing
  // records nothing at all — which is itself one of the receipt's stated blind
  // spots. Widening connect-src here is what makes the request observable.
  const site = await serveDist({
    policy: relax(probe.origin),
    rewrite: (pathname, body) =>
      pathname === chunks.worker ? `fetch(${JSON.stringify(probe.url)});\n${body}` : body,
  });

  const reading = await readReceipt(site.origin, {
    afterLoad: async () => {
      const deadline = Date.now() + 10_000;
      while (probe.hits() === 0 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
    },
  });

  check(probe.hits() > 0, `the worker actually reached the other origin (${probe.hits()} hits)`);
  check(
    reading.windowOnly === 0,
    `the OLD window-only algorithm still sees nothing (got ${reading.windowOnly}) — this is what used to be printed`,
  );
  check(
    reading.text === "1",
    `the receipt counts the WORKER's request (got "${reading.text}"; a reverted fix reads "0")`,
  );
  check(reading.state === "fail", `a non-zero count is flagged (got state "${reading.state}")`);
  await site.close();
  await probe.close();
}

// ---------------------------------------------------------------------------
console.log("\n[3] the PAGE makes a cross-origin request");
{
  const probe = await serveProbe();
  const relaxMeta = relaxDocument(probe.origin);
  const site = await serveDist({
    policy: relax(probe.origin),
    rewrite: (pathname, body) =>
      pathname === chunks.entry
        ? `fetch(${JSON.stringify(probe.url)});\n${body}`
        : relaxMeta(pathname, body),
  });

  const reading = await readReceipt(site.origin, {
    afterLoad: async () => {
      const deadline = Date.now() + 10_000;
      while (probe.hits() === 0 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
    },
  });

  check(probe.hits() > 0, `the page actually reached the other origin (${probe.hits()} hits)`);
  check(reading.windowOnly === 1, `the page's own request is visible to the window (got ${reading.windowOnly})`);
  check(reading.text === "1", `the receipt counts the PAGE's request (got "${reading.text}")`);
  await site.close();
  await probe.close();
}

// ---------------------------------------------------------------------------
console.log("\n[4] the worker cannot report");
{
  // Break the worker's half of the protocol. The receipt has no way to know
  // what the worker did, and must say so rather than print a comforting 0.
  const site = await serveDist({
    policy,
    rewrite: (pathname, body) =>
      // Quote-agnostic: the minifier may emit the literal with any quote style.
      pathname === chunks.worker ? body.split("resource-proof").join("receipt-disabled") : body,
  });

  const reading = await readReceipt(site.origin);
  check(
    /unproven/i.test(reading.text),
    `an unanswerable worker reads as unproven (got "${reading.text}")`,
  );
  check(reading.text !== "0", "an unanswerable worker never reads as 0");
  check(reading.state === "fail", `the unproven reading is flagged (got state "${reading.state}")`);
  await site.close();
}

await browser.close();

console.log(
  failures.length === 0
    ? "\nReceipt proof passed: the counter covers the window AND the worker, and says so honestly when it cannot."
    : `\nReceipt proof FAILED:\n${failures.map((failure) => `- ${failure}`).join("\n")}`,
);
process.exit(failures.length === 0 ? 0 : 1);
