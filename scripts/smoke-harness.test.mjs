/*
 * The gate on the smoke harness.
 *
 * The live smokes are only worth running if they fail for PRODUCT reasons and
 * nothing else. Two harness defects broke that:
 *
 *   A. The privacy inspector's external-request counter is filled
 *      asynchronously (pending placeholder -> worker answers -> number). Three
 *      smokes read the slot the instant the badge was clicked and reported the
 *      placeholder "Checking page and worker…" as a failure. The other two
 *      passed by luck.
 *   B. Cloudflare injects an inline JavaScript-Detections loader zone-wide and
 *      our strict CSP blocks it - expected, the moat working. Three smokes
 *      partitioned that console error out of the functional gate; staging- and
 *      img-smoke did not, so the PDF and image tools went red for it.
 *
 * Both fixes live in five separate files on purpose (each smoke stays a single
 * self-contained script that can be scp'd to the box on its own), so the thing
 * that can rot is CONSISTENCY. This file reads the five smoke sources, proves
 * the fixes are present and identical in all of them, and - the part that
 * matters - evaluates the real CSP filter text out of the source and proves it
 * still has teeth: it excuses the CF injection on all three engines and
 * excuses nothing else.
 *
 * Run: node --test scripts/smoke-harness.test.mjs   (no browser, no network)
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const SCRIPTS = path.dirname(fileURLToPath(import.meta.url));
const SMOKES = [
  "staging-smoke.mjs",
  "img-smoke.mjs",
  "scrub-smoke.mjs",
  "zip-smoke.mjs",
  "img2pdf-smoke.mjs",
];

const sources = new Map(
  await Promise.all(
    SMOKES.map(async (name) => [name, await readFile(path.join(SCRIPTS, name), "utf8")]),
  ),
);

/** Lift the filter's real source text out of a smoke script. */
function cspFilterSource(source) {
  const match = source.match(/const isExpectedCspBlock = [\s\S]*?;\n/);
  return match ? match[0].trim() : null;
}

/** Compile that source text into the actual predicate the smoke will run. */
function compile(filterSource, name) {
  assert.ok(filterSource, `${name} has no isExpectedCspBlock filter to evaluate`);
  return new Function(`"use strict"; ${filterSource} return isExpectedCspBlock;`)();
}

// The wording each engine emits when our CSP blocks CF's injected
// JavaScript-Detections INLINE loader. `chromium` is captured verbatim from a
// live run against pdf.keeplocal.tools; `chromiumClassic` is the older phrasing
// the same browser used; the Firefox and WebKit lines are the wordings this
// filter was broadened for in 2298720. Engines reword these, so the set grows,
// it never shrinks.
const CF_JSD_BLOCKS = {
  chromium:
    "Executing inline script violates the following Content Security Policy directive 'script-src 'self' 'wasm-unsafe-eval''. Either the 'unsafe-inline' keyword, a hash ('sha256-xrvJOltKW6z3ZJm0bkIOxVvACGWMRvihdEpaCQ2NW70='), or a nonce ('nonce-...') is required to enable inline execution. The action has been blocked.",
  chromiumClassic:
    "Refused to execute inline script because it violates the following Content Security Policy directive: \"script-src 'self' 'wasm-unsafe-eval'\". Either the 'unsafe-inline' keyword, a hash ('sha256-...'), or a nonce ('nonce-...') is required to enable inline execution.",
  firefox:
    "Content-Security-Policy: The page’s settings blocked an inline script (script-src-elem) from being executed because it violates the following directive: “script-src 'self' 'wasm-unsafe-eval'”",
  firefoxNewer:
    "Content-Security-Policy: The page’s settings blocked the execution of an inline script (script-src-elem) because it violates the following directive: “script-src 'self' 'wasm-unsafe-eval'”",
  webkit:
    "Refused to execute a script because its hash, its nonce, or 'unsafe-inline' does not appear in the script-src directive of the Content Security Policy.",
};

/*
 * A blocked EXTERNAL script is the case the filter must never swallow. CSP
 * stops the load BEFORE the request, so the smokes' zero-external-request check
 * cannot see it either - the console gate is the only thing standing between a
 * shipped third-party <script src> and a green run. One wording per engine.
 */
const EXTERNAL_SCRIPT_BLOCKS = {
  chromium:
    "Refused to load the script 'https://cdn.evil.example/x.js' because it violates the following Content Security Policy directive: \"script-src 'self' 'wasm-unsafe-eval'\". Note that 'script-src-elem' was not explicitly set, so 'script-src' is used as a fallback.",
  firefox:
    "Content-Security-Policy: The page’s settings blocked the loading of a resource at https://cdn.evil.example/x.js (“script-src-elem”).",
  webkit:
    "Refused to load https://cdn.evil.example/x.js because it does not appear in the script-src directive of the Content Security Policy.",
};

// Errors that must NEVER be excused. A green console gate has to mean the app
// was quiet - not that the filter swallowed the evidence.
const MUST_NOT_BE_EXCUSED = {
  "an ordinary app exception":
    "Uncaught TypeError: Cannot read properties of null (reading 'textContent')",
  "a failed worker boot": "Failed to load resource: the server responded with a status of 404 ()",
  "a WASM instantiation failure":
    "Uncaught (in promise) CompileError: WebAssembly.instantiate(): expected magic word",
  "a syntax error in an inline script":
    "SyntaxError: Unexpected token '<' (inline script, line 12)",
  // The exfiltration-shaped CSP violations: a different directive is a REAL
  // signal (the page tried to reach out) and must survive the filter.
  "a blocked connect-src (exfiltration attempt)":
    "Refused to connect to 'https://telemetry.example/collect' because it violates the following Content Security Policy directive: \"connect-src 'self'\".",
  "a blocked img-src beacon":
    "Refused to load the image 'https://beacon.example/px.gif' because it violates the following Content Security Policy directive: \"img-src 'self' data: blob:\".",
  "a blocked form-action":
    "Refused to send form data to 'https://forms.example/post' because it violates the following Content Security Policy directive: \"form-action 'self'\".",
  ...Object.fromEntries(
    Object.entries(EXTERNAL_SCRIPT_BLOCKS).map(([engine, message]) => [
      `a blocked EXTERNAL script, ${engine} wording`,
      message,
    ]),
  ),
};

/*
 * The filter this one replaced. It required only "content security policy" +
 * "script-src" + one of (inline script|refused to execute|blocked an
 * inline|violates), and that last alternative "violates" let Chromium's
 * external-script block through: a third-party <script src> shipped in a build
 * would have been logged as an expected CF injection and the run stayed green.
 * Kept here so the test below can prove the new cases are what closed it.
 */
const SUPERSEDED_BROAD_FILTER = `const isExpectedCspBlock = (m) =>
  /content[-\\s]security[-\\s]policy/i.test(m) &&
  /script-src/i.test(m) &&
  /(inline script|refused to execute|blocked an inline|violates)/i.test(m);`;

test("every live smoke defines the expected-CSP-block filter", () => {
  for (const [name, source] of sources) {
    assert.ok(
      cspFilterSource(source),
      `${name} has no isExpectedCspBlock filter - CF's blocked inline injection will fail it`,
    );
  }
});

test("all five filters are the same filter", () => {
  const [reference, ...rest] = SMOKES.map((name) => [name, cspFilterSource(sources.get(name))]);
  for (const [name, filterSource] of rest) {
    assert.equal(
      filterSource,
      reference[1],
      `${name}'s filter has drifted from ${reference[0]}'s - the five smokes must excuse the same thing`,
    );
  }
});

test("the filter excuses the CF JS-Detections block on every engine", () => {
  for (const [name, source] of sources) {
    const isExpectedCspBlock = compile(cspFilterSource(source), name);
    for (const [engine, message] of Object.entries(CF_JSD_BLOCKS)) {
      assert.ok(
        isExpectedCspBlock(message),
        `${name} would fail on ${engine}'s wording of the CF inline-script block`,
      );
    }
  }
});

test("the filter excuses NOTHING else", () => {
  for (const [name, source] of sources) {
    const isExpectedCspBlock = compile(cspFilterSource(source), name);
    for (const [label, message] of Object.entries(MUST_NOT_BE_EXCUSED)) {
      assert.equal(
        isExpectedCspBlock(message),
        false,
        `${name}'s filter excuses ${label} - a real console error would be hidden`,
      );
    }
  }
});

test("a blocked EXTERNAL script always fails the console gate", () => {
  for (const [name, source] of sources) {
    const isExpectedCspBlock = compile(cspFilterSource(source), name);
    for (const [engine, message] of Object.entries(EXTERNAL_SCRIPT_BLOCKS)) {
      assert.equal(
        isExpectedCspBlock(message),
        false,
        `${name} excuses ${engine}'s blocked-external-script wording - a third-party ` +
          "<script src> shipped in a build would pass unseen (CSP blocks it before the " +
          "request, so the zero-external-request check cannot catch it either)",
      );
    }
  }
});

test("the superseded broad filter would fail this gate", () => {
  // The proof that the cases above have teeth: run them against the filter that
  // was in place before this tightening. It must go red on Chromium's
  // external-script wording while still excusing the CF inline blocks it was
  // written for - i.e. the external cases, not luck, are what changed.
  const broad = compile(SUPERSEDED_BROAD_FILTER, "the superseded broad filter");
  assert.equal(
    broad(EXTERNAL_SCRIPT_BLOCKS.chromium),
    true,
    "the superseded filter no longer excuses the external-script block - this teeth check has gone vacuous",
  );
  assert.equal(broad(CF_JSD_BLOCKS.chromiumClassic), true);
  // ...and the filter now in the smokes does not.
  for (const [name, source] of sources) {
    assert.equal(
      compile(cspFilterSource(source), name)(EXTERNAL_SCRIPT_BLOCKS.chromium),
      false,
      `${name} still carries the superseded broad behaviour`,
    );
  }
});

test("expected blocks are partitioned out and SURFACED, never hidden", () => {
  for (const [name, source] of sources) {
    assert.match(
      source,
      /const expectedCspBlocks = \w+\.filter\(isExpectedCspBlock\);/,
      `${name} does not count the expected CSP blocks`,
    );
    assert.match(
      source,
      /const functionalConsoleErrors = \w+\.filter\(\(m\) => !isExpectedCspBlock\(m\)\);/,
      `${name} does not partition the expected blocks out of the functional console gate`,
    );
    assert.match(
      source,
      /if \(expectedCspBlocks\.length\) \{\s*\n\s*console\.log\(/,
      `${name} swallows the expected CSP blocks instead of reporting them as a NOTE`,
    );
    assert.match(
      source,
      /check\(functionalConsoleErrors\.length === 0,/,
      `${name} no longer gates on functional console errors`,
    );
  }
});

test("the external-request counter is read only after it settles", () => {
  for (const [name, source] of sources) {
    assert.match(
      source,
      /async function readSettledExternalProof\(\) \{[\s\S]*?waitForFunction\([\s\S]*?\/\^\\d\+\$\/[\s\S]*?timeout: 5000[\s\S]*?\}\n/,
      `${name} has no settle-then-read helper for the external-request counter`,
    );
    assert.match(
      source,
      /const netCount = await readSettledExternalProof\(\);/,
      `${name} does not read the counter through the settle helper`,
    );
    // The defect itself: reading the slot straight off the badge click races
    // the worker's answer and reports the pending placeholder as a failure.
    assert.doesNotMatch(
      source,
      /const netCount = \(\(await page\.textContent\('\[data-proof="external"\]'\)/,
      `${name} reads the external-request counter synchronously again (the race is back)`,
    );
    assert.match(
      source,
      /check\(netCount === "0",/,
      `${name} no longer asserts the counter reads 0`,
    );
  }
});
