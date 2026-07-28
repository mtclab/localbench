/*
 * The notice gate, runtime half.
 *
 * What it forbids: the core returning a notice that the user never sees.
 *
 * How it knows what the core returned, without trusting the interface: this
 * script wraps the `Worker` constructor before any app code runs and records
 * every message the worker posts back. The worker is where the compiled Rust
 * core lives, so that recording IS the core's output — independent of whatever
 * the page then chooses to do with it. Each scenario then asserts that every
 * sentence in that recording is present in the page's VISIBLE text.
 *
 * That is what makes the gate revert-proof rather than descriptive. Delete the
 * notice rendering, or drop the `notices` field from a worker message, or hide
 * the block behind a `hidden` attribute, and the recording still contains the
 * sentence while the page does not. Red, with the exact sentence quoted.
 *
 * It is also self-checking in the other direction: a scenario declares which
 * notice codes it expects, so a fixture that quietly stopped provoking the core
 * fails here instead of passing vacuously. The same expectations are pinned in
 * scripts/notice-plumbing.test.mjs against the core running in Node.
 *
 * Venue: the staging box, against served dist/ directories — never the dev
 * workspace. Same convention as scripts/receipt-proof.mjs and
 * scripts/staging-smoke.mjs.
 *
 *   node scripts/notice-fixtures.mjs ./notice-corpus
 *   CHROME_PATH=... node scripts/notice-proof.mjs
 *
 * Options (environment):
 *   APPS=app,app-img      restrict to some apps
 *   CORPUS=./notice-corpus generated fixtures
 *   REPO=.                 for the committed corpora it also uses
 *   OUT=./notice-shots     screenshots
 *   SHOTS=0                skip screenshots
 */
import { chromium } from "playwright";
import { mkdir, readFile, readdir } from "node:fs/promises";
import { createServer } from "node:http";
import path from "node:path";

const REPO = path.resolve(process.env.REPO ?? ".");
const CORPUS = path.resolve(process.env.CORPUS ?? "./notice-corpus");
const OUT = path.resolve(process.env.OUT ?? "./notice-shots");
const TAKE_SHOTS = process.env.SHOTS !== "0";
const HOST = "127.0.0.1";
let nextPort = Number(process.env.PORT ?? 5310);

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

/** Serves one app's dist under the exact policy the real deployment sends. */
async function serveDist(dist) {
  const port = nextPort++;
  const files = new Map();
  for (const file of await filesBelow(dist)) {
    files.set(`/${path.relative(dist, file).split(path.sep).join("/")}`, file);
  }

  const headerText = await readFile(path.join(dist, "_headers"), "utf8");
  const policyLine = headerText
    .split(/\r?\n/)
    .find((row) => /^\s+Content-Security-Policy:/i.test(row));
  if (!policyLine) throw new Error(`${dist}/_headers carries no Content-Security-Policy`);
  const policy = policyLine.replace(/^\s*Content-Security-Policy:\s*/i, "").trim();

  const server = createServer(async (request, response) => {
    const url = new URL(request.url, `http://${HOST}:${port}`);
    const pathname = url.pathname === "/" ? "/index.html" : url.pathname;
    const file = files.get(pathname);
    if (!file) {
      response.writeHead(404).end("not found");
      return;
    }
    response
      .writeHead(200, {
        "Content-Type": CONTENT_TYPES.get(path.extname(file)) ?? "application/octet-stream",
        "Content-Security-Policy": policy,
        "Cache-Control": "no-store",
      })
      .end(await readFile(file));
  });

  await new Promise((resolve) => server.listen(port, HOST, resolve));
  return {
    origin: `http://${HOST}:${port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

/*
 * Installed before any app script. Wrapping the constructor (rather than
 * listening from the page's own handler) means the recording does not depend on
 * a single line of application code being correct.
 */
const RECORD_WORKER_MESSAGES = () => {
  const Native = window.Worker;
  window.__coreMessages = [];
  window.Worker = class extends Native {
    constructor(...args) {
      super(...args);
      this.addEventListener("message", (event) => {
        const data = event.data;
        if (data && typeof data === "object") window.__coreMessages.push(data);
      });
    }
  };
};

const failures = [];
let checks = 0;
function check(condition, message) {
  checks += 1;
  console.log(`    ${condition ? "PASS" : "FAIL"}  ${message}`);
  if (!condition) failures.push(message);
}

/** Notices the worker has posted so far, flattened and de-duplicated in order. */
function readWorkerNotices(page) {
  return page.evaluate(() => {
    const seen = new Set();
    const out = [];
    for (const message of window.__coreMessages ?? []) {
      for (const notice of message.notices ?? []) {
        const key = `${notice.code}::${notice.message}`;
        if (seen.has(key)) continue;
        seen.add(key);
        out.push(notice);
      }
    }
    return out;
  });
}

/** Text a person could actually read on the page right now. */
function readVisibleText(page) {
  return page.evaluate(() => document.body.innerText.replace(/\s+/g, " "));
}

async function fixture(name) {
  const generated = path.join(CORPUS, name);
  try {
    return await readFile(generated);
  } catch {
    return readFile(path.join(REPO, name));
  }
}

async function setFile(page, selector, name, mimeType) {
  await page.setInputFiles(selector, {
    name: path.basename(name),
    mimeType,
    buffer: await fixture(name),
  });
}

async function shot(page, label) {
  if (!TAKE_SHOTS) return;
  await mkdir(OUT, { recursive: true });
  await page.screenshot({ path: path.join(OUT, `${label}.png`), fullPage: true });
}

// ---------------------------------------------------------------------------
// Scenarios.
//
// `expect` is the set of notice codes the core MUST produce. `run` drives the
// interface exactly as a person would: choose a file, press the button, wait
// for the result. Nothing here reads the app's internal state to decide
// whether a notice was shown — only the rendered page.
// ---------------------------------------------------------------------------

const SCENARIOS = [
  {
    app: "app",
    name: "pdf-compress-returns-original",
    hash: "#compress",
    expect: ["pdf-returned-unchanged"],
    async run(page) {
      await setFile(page, "#compress-file-input", "already-compact.pdf", "application/pdf");
      await page.waitForSelector("#compress-editor:not([hidden])", { timeout: 20_000 });
      await page.click("#compress-button");
      await page.waitForSelector("#compress-answer:not([hidden])", { timeout: 30_000 });
    },
    async assert(page) {
      check(
        (await page.getAttribute("#compress-answer", "data-notice")) === "contradiction",
        "the compress answer is escalated, not shown as a plain success",
      );
      check(
        (await page.getAttribute("#compress-result", "data-state")) === "notice",
        "the status strip under the button reports a notice, not success",
      );
      const metadata = (await page.textContent("#compress-metadata"))?.trim();
      check(
        metadata === "Not removed — original returned",
        `the metadata row states what happened to THIS file (read "${metadata}")`,
      );
      // The downloads folder outlives the notice. A file called
      // "…-compressed.pdf" that is byte-for-byte the input misleads there,
      // months later, where nothing can correct it.
      const downloaded = (
        await page.textContent("#compress-answer [data-answer-file]")
      )?.trim();
      check(
        downloaded === "already-compact.pdf",
        `an unchanged file keeps its own name (read "${downloaded}")`,
      );
      check(
        (await page.textContent("#compress-download-button"))?.trim() ===
          "Download your original PDF",
        "the download button does not offer a compression that did not happen",
      );
    },
  },
  {
    app: "app",
    name: "pdf-merge-forms",
    hash: "#merge",
    expect: ["pdf-forms-combined", "pdf-form-field-name-collision"],
    async run(page) {
      await page.setInputFiles("#merge-file-input", [
        {
          name: "form_english.pdf",
          mimeType: "application/pdf",
          buffer: await fixture("test-corpus/form_english.pdf"),
        },
        {
          name: "form_russian.pdf",
          mimeType: "application/pdf",
          buffer: await fixture("test-corpus/form_russian.pdf"),
        },
      ]);
      await page.click("#merge-button");
      await page.waitForSelector("#merge-answer:not([hidden])", { timeout: 60_000 });
    },
    async assert(page) {
      check(
        (await page.locator("#merge-answer .notice").count()) === 2,
        "both merge notices are listed, not just the first",
      );
    },
  },
  {
    app: "app-img",
    name: "img-16bit-to-jpeg",
    hash: "#convert",
    expect: ["image-bit-depth-reduced"],
    async run(page) {
      await setFile(page, "#convert-file-input", "deep16.png", "image/png");
      await page.waitForSelector("#convert-editor:not([hidden])", { timeout: 20_000 });
      await page.selectOption("#convert-target", "jpeg");
      await page.click("#convert-button");
      await page.waitForSelector("#convert-answer:not([hidden])", { timeout: 30_000 });
    },
  },
  {
    app: "app-img",
    name: "img-animation-to-png",
    hash: "#convert",
    expect: ["image-animation-dropped"],
    async run(page) {
      await setFile(page, "#convert-file-input", "animated.gif", "image/gif");
      await page.waitForSelector("#convert-editor:not([hidden])", { timeout: 20_000 });
      await page.selectOption("#convert-target", "png");
      await page.click("#convert-button");
      await page.waitForSelector("#convert-answer:not([hidden])", { timeout: 30_000 });
    },
  },
  {
    app: "app-img",
    name: "img-animation-resized",
    hash: "#resize",
    expect: ["image-animation-recoded"],
    async run(page) {
      await setFile(page, "#resize-file-input", "animated.gif", "image/gif");
      await page.waitForSelector("#resize-editor:not([hidden])", { timeout: 20_000 });
      await page.fill("#resize-max-width", "8");
      await page.fill("#resize-max-height", "8");
      await page.click("#resize-button");
      await page.waitForSelector("#resize-answer:not([hidden])", { timeout: 30_000 });
    },
  },
  {
    app: "app-img",
    name: "img-compress-returns-original",
    hash: "#compress",
    expect: ["image-returned-unchanged"],
    async run(page) {
      await setFile(page, "#compress-file-input", "already-minimal.png", "image/png");
      await page.waitForSelector("#compress-editor:not([hidden])", { timeout: 20_000 });
      await page.click("#compress-button");
      await page.waitForSelector("#compress-answer:not([hidden])", { timeout: 30_000 });
    },
    async assert(page) {
      check(
        (await page.getAttribute("#compress-answer", "data-notice")) === "contradiction",
        "the compress answer is escalated, not shown as a plain success",
      );
      check(
        (await page.getAttribute("#compress-result", "data-state")) === "notice",
        "the status strip under the button reports a notice, not success",
      );
      const visible = await readVisibleText(page);
      check(
        !/Location\/EXIF metadata was removed/.test(visible),
        "the page does not claim metadata was removed from a file it returned untouched",
      );
      const downloaded = (
        await page.textContent("#compress-answer [data-answer-file]")
      )?.trim();
      check(
        downloaded === "already-minimal.png",
        `an unchanged file keeps its own name (read "${downloaded}")`,
      );
      check(
        (await page.textContent("#compress-download-button"))?.trim() ===
          "Download your original image",
        "the download button does not offer a compression that did not happen",
      );
    },
  },
  {
    /*
     * The regression the audit found: images over 4096px on an edge were being
     * silently downscaled while the page called the operation lossless. The
     * core no longer does that, so the whole picture must come back.
     */
    app: "app-img",
    name: "img-over-4096-keeps-its-pixels",
    hash: "#convert",
    expect: [],
    async run(page) {
      await setFile(page, "#convert-file-input", "wide.png", "image/png");
      await page.waitForSelector("#convert-editor:not([hidden])", { timeout: 30_000 });
      await page.selectOption("#convert-target", "png");
      await page.click("#convert-button");
      await page.waitForSelector("#convert-answer:not([hidden])", { timeout: 120_000 });
    },
    async assert(page) {
      const dimensions = (await page.textContent("#convert-output-dimensions"))?.trim();
      check(
        /5000\s*×\s*3000/.test(dimensions ?? ""),
        `a 5000x3000 image keeps every pixel (read "${dimensions}")`,
      );
      check(
        (await page.getAttribute("#convert-answer", "data-notice")) === null,
        "a clean conversion is not dressed up as a problem",
      );
    },
  },
  {
    app: "app-zip",
    name: "zip-duplicates-and-blocked-entries",
    hash: "#extract",
    expect: [],
    async run(page) {
      await setFile(page, "#extract-file-input", "duplicate-names.zip", "application/zip");
      await page.waitForSelector("#extract-editor:not([hidden])", { timeout: 20_000 });
    },
    async assert(page) {
      const visible = await readVisibleText(page);

      check(
        !(await page.isHidden("#extract-warnings")),
        "the archive's own warnings are shown above its entry list",
      );
      check(
        /several files with the same name/.test(visible),
        "the duplicate-name warning reaches the page verbatim",
      );
      check(
        /password-protected and cannot be extracted here/.test(visible),
        "the encrypted-entry warning reaches the page verbatim",
      );
      check(
        (await page.locator('#extract-entry-table .entry-flag:text-is("Duplicate name")').count()) === 2,
        "both rows sharing a name are labelled, so the user knows why they repeat",
      );
      check(
        (await page.locator('#extract-entry-table .entry-blocked').count()) === 1,
        "the entry that cannot be extracted shows a reason instead of a button",
      );
      check(
        /Password-protected/.test(visible),
        "the reason names the actual obstacle",
      );

      // The defect this closes: a Download button that was always going to fail.
      const rows = await page.evaluate(() =>
        [...document.querySelectorAll("#extract-entry-table > div[role='row']")]
          .slice(1)
          .map((row) => ({
            name: row.querySelector(".file-name")?.textContent ?? "",
            hasButton: row.querySelector("button") !== null,
          })),
      );
      const sealed = rows.find((row) => row.name === "sealed.txt");
      check(sealed !== undefined && !sealed.hasButton, "no Download button on an entry that cannot be extracted");
      check(
        rows.filter((row) => row.hasButton).length === 3,
        "every entry that CAN be extracted still offers its button",
      );

      const count = (await page.textContent("#extract-entry-count"))?.trim();
      check(/uncompressed/.test(count ?? ""), `the total uncompressed size is shown (read "${count}")`);
      check(
        (await page.getAttribute("#extract-result", "data-state")) === "notice",
        "the status strip says some entries cannot be extracted",
      );
    },
  },
  {
    app: "app-scrub",
    name: "scrub-verified-structurally",
    hash: "#scrub",
    expect: [],
    async run(page) {
      await setFile(page, "#scrub-file-input", "scrub-corpus/photo.jpg", "image/jpeg");
      await page.waitForSelector("#scrub-editor:not([hidden])", { timeout: 20_000 });
      await page.click("#scrub-button");
      await page.waitForSelector("#scrub-output:not([hidden])", { timeout: 30_000 });
    },
    async assert(page) {
      const proof = (await page.textContent("#scrub-proof"))?.trim() ?? "";
      // The old copy claimed a "re-inspection", which re-ran the very detector
      // that had produced the original report and so could never fail.
      check(!/Re-inspection/i.test(proof), "the page no longer claims a re-inspection");
      check(
        /Structural check passed/.test(proof),
        `the page reports the independent structural check (read "${proof}")`,
      );
    },
  },
  {
    app: "app-img2pdf",
    name: "img2pdf-builds-with-notices-plumbed",
    hash: "#create",
    expect: [],
    async run(page) {
      await page.setInputFiles("#create-file-input", [
        { name: "deep16.png", mimeType: "image/png", buffer: await fixture("deep16.png") },
        {
          name: "a.jpg",
          mimeType: "image/jpeg",
          buffer: await fixture("img2pdf-corpus/a.jpg"),
        },
      ]);
      await page.click("#create-button");
      await page.waitForSelector("#create-answer:not([hidden])", { timeout: 60_000 });
    },
  },
];

// ---------------------------------------------------------------------------

const requested = process.env.APPS?.split(",").map((name) => name.trim()).filter(Boolean);
const scenarios = requested
  ? SCENARIOS.filter((scenario) => requested.includes(scenario.app))
  : SCENARIOS;
const apps = [...new Set(scenarios.map((scenario) => scenario.app))];

const launchOptions = { args: ["--no-sandbox"] };
if (process.env.CHROME_PATH) launchOptions.executablePath = process.env.CHROME_PATH;
const browser = await chromium.launch(launchOptions);

for (const app of apps) {
  const dist = path.join(REPO, app, "dist");
  const server = await serveDist(dist);
  console.log(`\n${app} (${server.origin})`);

  try {
    for (const scenario of scenarios.filter((entry) => entry.app === app)) {
      console.log(`  ${scenario.name}`);
      const context = await browser.newContext();
      await context.addInitScript(RECORD_WORKER_MESSAGES);
      const page = await context.newPage();
      const pageErrors = [];
      page.on("pageerror", (error) => pageErrors.push(error.message));

      try {
        await page.goto(`${server.origin}/${scenario.hash}`, { waitUntil: "load" });
        await page.waitForFunction(
          () => /^v\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""),
          undefined,
          { timeout: 30_000 },
        );
        await scenario.run(page);

        const notices = await readWorkerNotices(page);
        const codes = notices.map((notice) => notice.code);

        // 1. The fixture still provokes the core. Without this the next check
        //    would pass on an empty list and prove nothing at all.
        for (const expected of scenario.expect) {
          check(
            codes.includes(expected),
            `the core produced ${expected} (worker returned: ${codes.join(", ") || "nothing"})`,
          );
        }

        // 2. THE GATE. Everything the core said is on the screen, verbatim.
        const visible = await readVisibleText(page);
        for (const notice of notices) {
          const wanted = notice.message.replace(/\s+/g, " ").trim();
          check(
            visible.includes(wanted),
            `the user is shown [${notice.code}] "${wanted.slice(0, 72)}${wanted.length > 72 ? "…" : ""}"`,
          );
        }

        if (scenario.assert) await scenario.assert(page);

        check(pageErrors.length === 0, `no page errors (${pageErrors.slice(0, 2).join(" | ") || "none"})`);
        await shot(page, `${scenario.app}-${scenario.name}`);
      } catch (error) {
        check(false, `${scenario.name} ran to completion (${String(error).split("\n")[0]})`);
        await shot(page, `${scenario.app}-${scenario.name}-FAILED`);
      } finally {
        await context.close();
      }
    }
  } finally {
    await server.close();
  }
}

await browser.close();

console.log(
  `\n${failures.length ? `NOTICE PROOF FAILED: ${failures.length} of ${checks}` : `NOTICE PROOF PASSED: ${checks} checks`}`,
);
for (const failure of failures) console.log(`  - ${failure}`);
process.exit(failures.length ? 1 : 0);
