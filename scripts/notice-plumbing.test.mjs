/*
 * The notice gate, static half.
 *
 * What it forbids: a notice the Rust core returned reaching nobody.
 *
 * The core now discloses everything it does to a user's file that the file
 * itself cannot show — an animation flattened, a bit depth rounded, a document
 * handed back untouched under a page that promised otherwise. That disclosure
 * is worth exactly as much as the interface's willingness to print it, so the
 * wiring between the two is not left to reviewer attention.
 *
 * Three layers, deliberately overlapping:
 *
 *  1. The CONTRACT is read out of the Rust source, not restated here. Every
 *     `-> Result<FileResult, JsValue>` export is discovered, and any app worker
 *     that calls one must forward its notices in the same message as its bytes.
 *     Add a notice-bearing operation to the core and forget the front end, and
 *     this goes red without anyone editing this file.
 *
 *  2. The RENDERING path is single and mandatory: shared/answer.ts takes
 *     `notices` as a required field, so every result that reaches the screen
 *     carries them. Both that requirement and every call site are asserted.
 *
 *  3. The FIXTURES are proven to still bite, by running the real compiled core
 *     over them here in Node. A browser gate that fed the core something
 *     harmless would pass while proving nothing; this is what stops that.
 *
 * The runtime half — that the sentences actually reach a screen — is
 * scripts/notice-proof.mjs, which drives the built artefacts in a real browser
 * on the staging box.
 *
 * Run: node --test scripts/notice-plumbing.test.mjs
 */
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { writeFixtures } from "./notice-fixtures.mjs";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APPS = ["app", "app-img", "app-img2pdf", "app-scrub", "app-zip"];

const read = (relative) => readFile(path.join(REPO, relative), "utf8");

// ---------------------------------------------------------------------------
// Layer 1 — the contract, read from the Rust source.
// ---------------------------------------------------------------------------

/** Every `#[wasm_bindgen]` export whose success type is a FileResult. */
async function fileResultExports() {
  const names = new Set();
  for (const module of ["lib", "image_ops", "imagepdf_ops", "archive_ops", "metadata_ops"]) {
    const source = await read(`core-rs/src/${module}.rs`);
    // The parameter list is matched with balanced parens rather than a lazy
    // wildcard: a wildcard walks straight through a function body and picks up
    // the NEXT function's return type, which silently shrinks this set.
    for (const match of source.matchAll(
      /pub fn\s+(\w+)\s*\((?:[^()]|\([^()]*\))*\)\s*->\s*Result<\s*FileResult\s*,\s*JsValue\s*>/gs,
    )) {
      names.add(match[1]);
    }
  }
  return names;
}

test("the core still exposes FileResult-returning operations", async () => {
  const exports = await fileResultExports();
  assert.ok(
    exports.size >= 7,
    `expected the notice-bearing core exports to still exist, found: ${[...exports].join(", ")}`,
  );
  // Named explicitly so a rename in Rust is noticed here rather than silently
  // shrinking the set this gate covers.
  for (const name of [
    "merge_pdfs",
    "organize_pdf",
    "compress_pdf",
    "resize_image",
    "convert_image",
    "compress_image",
    "images_to_pdf",
  ]) {
    assert.ok(exports.has(name), `${name} no longer returns a FileResult`);
  }
});

test("every worker that calls a FileResult operation forwards its notices with the bytes", async () => {
  const exports = await fileResultExports();

  for (const app of APPS) {
    const worker = await read(`${app}/src/core.worker.ts`);
    const used = [...exports].filter((name) =>
      new RegExp(`\\b${name}\\s*\\(`).test(worker),
    );
    if (used.length === 0) continue;

    assert.match(
      worker,
      /toNotices\(\s*result\.notices\s*,\s*result\.notice_codes\s*\)/,
      `${app}/src/core.worker.ts calls ${used.join(", ")} but never reads the FileResult's notices`,
    );

    // The notices must ride the SAME postMessage as the bytes. A separate
    // message could be dropped, reordered, or simply never wired up.
    const posts = [...worker.matchAll(/scope\.postMessage\(\s*\{([^}]*)\}/g)].map(
      (match) => match[1],
    );
    const byteCarrying = posts.filter((body) => /\bbytes\b/.test(body));
    assert.ok(
      byteCarrying.length > 0,
      `${app}/src/core.worker.ts posts no message carrying bytes`,
    );
    for (const body of byteCarrying) {
      // The value must BE the list derived from the FileResult — shorthand
      // `notices` or `notices: notices`. Anything else (`notices: []` most
      // obviously) satisfies the type checker while quietly delivering nothing,
      // which is precisely the bug being forbidden.
      const forwarded =
        /(?:^|[\s,{])notices\s*(?=[,}]|$)/.test(body) || /\bnotices:\s*notices\b/.test(body);
      assert.ok(
        forwarded,
        `${app}/src/core.worker.ts posts bytes without forwarding the core's notices: {${body.trim()}}`,
      );
    }
  }
});

// ---------------------------------------------------------------------------
// Layer 2 — one rendering path, and it demands notices.
// ---------------------------------------------------------------------------

test("Answer.notices is required, not optional", async () => {
  const source = await read("shared/answer.ts");
  assert.match(
    source,
    /notices:\s*CoreNotice\[\]/,
    "shared/answer.ts must declare notices as a required CoreNotice[]",
  );
  assert.doesNotMatch(
    source,
    /notices\?\s*:/,
    "shared/answer.ts made notices optional, which lets a result reach the screen without them",
  );
  assert.match(
    source,
    /renderNotices\(noticeSlot,\s*answer\.notices\)/,
    "shared/answer.ts no longer renders the answer's notices",
  );
});

test("no app keeps a private copy of setAnswer", async () => {
  for (const app of APPS) {
    const main = await read(`${app}/src/main.ts`);
    assert.doesNotMatch(
      main,
      /function\s+setAnswer\s*\(/,
      `${app}/src/main.ts redefines setAnswer; the shared one is the only path that guarantees notices`,
    );
    assert.match(
      main,
      /import\s*\{\s*setAnswer\s*\}\s*from\s*"\.\.\/\.\.\/shared\/answer"/,
      `${app}/src/main.ts does not use the shared answer renderer`,
    );
  }
});

/** The balanced `{ ... }` object literal starting at `from`, or null. */
function objectLiteralAt(source, from) {
  const start = source.indexOf("{", from);
  if (start < 0) return null;
  let depth = 0;
  for (let at = start; at < source.length; at += 1) {
    if (source[at] === "{") depth += 1;
    else if (source[at] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(start, at + 1);
    }
  }
  return null;
}

test("every setAnswer call supplies notices", async () => {
  for (const app of APPS) {
    const main = await read(`${app}/src/main.ts`);
    const calls = [...main.matchAll(/setAnswer\(/g)];
    assert.ok(calls.length > 0, `${app}/src/main.ts never renders an answer`);

    for (const call of calls) {
      const tail = main.slice(call.index, call.index + 40);
      if (/setAnswer\([^,]+,\s*null\s*\)/.test(tail)) continue; // clearing is fine
      const literal = objectLiteralAt(main, call.index);
      assert.ok(literal, `${app}/src/main.ts: could not read the answer at offset ${call.index}`);
      assert.match(
        literal,
        /\bnotices:/,
        `${app}/src/main.ts: an answer is rendered without notices at offset ${call.index}`,
      );
    }
  }
});

test("the contradiction codes still exist in the core", async () => {
  const shared = await read("shared/notices.ts");
  const declared = [...shared.matchAll(/^\s*"([a-z-]+)",$/gm)].map((match) => match[1]);
  assert.deepEqual(
    declared,
    ["pdf-returned-unchanged", "image-returned-unchanged"],
    "the contradiction set changed; confirm the page copy it contradicts changed with it",
  );

  const rust = (
    await Promise.all(["lib", "image_ops"].map((module) => read(`core-rs/src/${module}.rs`)))
  ).join("\n");
  for (const code of declared) {
    assert.ok(
      rust.includes(`"${code}"`),
      `${code} is treated as a contradiction by the UI but the core no longer emits it`,
    );
  }
});

test("notices are shown verbatim, never rewritten", async () => {
  const source = await read("shared/notices.ts");
  // textContent is the whole point: the core owns the wording, and it also
  // means a message can never inject markup into the page.
  assert.match(source, /item\.textContent = notice\.message;/);
  assert.doesNotMatch(
    source.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g, ""),
    /innerHTML/,
    "a notice must never be written as markup",
  );
  assert.doesNotMatch(
    source,
    /notice\.message\.(slice|substring|replace|toLowerCase|toUpperCase)/,
    "a notice's message must reach the user as the core wrote it",
  );
});

// ---------------------------------------------------------------------------
// Layer 2b — the built artefact, not just the source.
// ---------------------------------------------------------------------------

test("each built app ships the notice renderer", async () => {
  for (const app of APPS) {
    const dist = path.join(REPO, app, "dist");
    if (!existsSync(dist)) continue;
    const html = await readFile(path.join(dist, "index.html"), "utf8");
    assert.match(
      html,
      /data-answer-notices/,
      `${app}/dist/index.html has no slot for notices, so they would have nowhere to render`,
    );

    const bundle = await readFile(
      path.join(dist, "sw.js"),
      "utf8",
    ).then(async (serviceWorker) => {
      const entry = serviceWorker.match(/\/assets\/index-[\w-]+\.js/)?.[0];
      assert.ok(entry, `could not locate ${app}'s entry chunk`);
      return readFile(path.join(dist, entry.slice(1)), "utf8");
    });
    assert.match(
      bundle,
      /What this did to your file/,
      `${app}'s bundle does not contain the notice heading, so notices are not being rendered`,
    );
    assert.match(
      bundle,
      /your download is not what this page promised/,
      `${app}'s bundle does not contain the contradiction heading`,
    );
  }
});

// ---------------------------------------------------------------------------
// Layer 2c — the scrub app's verification must be able to fail.
// ---------------------------------------------------------------------------

test("REGRESSION: a scrub is verified structurally, not by re-running the same detector", async () => {
  const worker = await read("app-scrub/src/core.worker.ts");
  assert.match(
    worker,
    /verify_metadata_removed\(/,
    "app-scrub's worker no longer exposes the structural verifier",
  );

  const main = await read("app-scrub/src/main.ts");
  assert.match(main, /await verifyRemoved\(/, "app-scrub no longer verifies a scrub at all");
  // The exact defect: proving a scrub by asking inspect_metadata a second time.
  // That detector already reported the file's metadata, so re-running it can
  // never catch a container it does not know how to see — the reassurance
  // could not fail, which made it worthless.
  assert.doesNotMatch(
    main,
    /const\s+proof\s*=\s*parseReport\(await inspectBytes\(scrubbed/,
    "app-scrub is proving its scrub by re-inspecting, which is the defect this replaced",
  );
  assert.doesNotMatch(
    main,
    /Re-inspection passed/,
    "app-scrub still claims a re-inspection it no longer performs",
  );
});

// ---------------------------------------------------------------------------
// Layer 3 — the fixtures still make the core talk.
// ---------------------------------------------------------------------------

/** Loads the compiled core the apps ship, in Node. */
async function loadCore() {
  const wasmDirectory = path.join(REPO, "app", "src", "wasm");
  const core = await import(path.join(wasmDirectory, "localbench_core.js"));
  await core.default({
    module_or_path: await readFile(path.join(wasmDirectory, "localbench_core_bg.wasm")),
  });
  return core;
}

/*
 * What each fixture must make the core say. The browser gate feeds these same
 * files; if one of them stopped producing a notice, that gate would go green
 * while proving nothing, so the expectation is pinned here.
 */
const FIXTURE_EXPECTATIONS = [
  {
    what: "an already-compact PDF with metadata comes back untouched, and says so",
    fixture: "already-compact.pdf",
    run: (core, bytes) => core.compress_pdf(bytes, 55),
    codes: ["pdf-returned-unchanged"],
    saying: /metadata was NOT removed/,
  },
  {
    what: "an image that cannot shrink comes back untouched, and says so",
    fixture: "already-minimal.png",
    run: (core, bytes) => core.compress_image(bytes, 90),
    codes: ["image-returned-unchanged"],
    saying: /returned exactly as it was, metadata included/,
  },
  {
    what: "a 16-bit PNG converted to JPEG reports the rounding",
    fixture: "deep16.png",
    run: (core, bytes) => core.convert_image(bytes, "jpeg"),
    codes: ["image-bit-depth-reduced"],
  },
  {
    what: "an animation converted to a still format reports the lost frames",
    fixture: "animated.gif",
    run: (core, bytes) => core.convert_image(bytes, "png"),
    codes: ["image-animation-dropped"],
  },
  {
    what: "a resized animation reports that its colours were re-selected",
    fixture: "animated.gif",
    run: (core, bytes) => core.resize_image(bytes, 8, 8, true),
    codes: ["image-animation-recoded"],
  },
  {
    what: "a resize box larger than the image reports the clamp",
    fixture: "deep16.png",
    run: (core, bytes) => core.resize_image(bytes, 9999, 9999, false),
    codes: ["image-resize-clamped"],
  },
];

test("every fixture still makes the core disclose something", async (t) => {
  const scratch = await mkdtemp(path.join(os.tmpdir(), "notice-fixtures-"));
  try {
    await writeFixtures(scratch);
    const core = await loadCore();

    for (const expectation of FIXTURE_EXPECTATIONS) {
      await t.test(expectation.what, async () => {
        const bytes = new Uint8Array(await readFile(path.join(scratch, expectation.fixture)));
        const result = expectation.run(core, bytes);
        assert.deepEqual(
          [...result.notice_codes],
          expectation.codes,
          `${expectation.fixture} no longer produces ${expectation.codes.join(", ")}`,
        );
        if (expectation.saying) {
          assert.match(result.notices.join(" "), expectation.saying);
        }
      });
    }

    await t.test("the ZIP fixture exercises every listing field the UI reads", async () => {
      const core = await loadCore();
      const bytes = new Uint8Array(await readFile(path.join(scratch, "duplicate-names.zip")));
      const listing = JSON.parse(core.list_zip(bytes));

      assert.ok(
        listing.entries.some((entry) => entry.duplicate_name),
        "the ZIP fixture no longer contains a duplicate name",
      );
      assert.ok(
        listing.entries.some((entry) => entry.encrypted && !entry.extractable),
        "the ZIP fixture no longer contains an entry that cannot be extracted",
      );
      assert.ok(
        listing.entries.some((entry) => entry.extractable),
        "the ZIP fixture must also contain an entry that DOES extract",
      );
      assert.ok(listing.total_size > 0, "the listing no longer reports a total size");
      assert.ok(listing.warnings.length >= 2, "the listing no longer reports its warnings");
    });
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Layer 3b — the zip interface reads the fields the core added.
// ---------------------------------------------------------------------------

test("the zip interface reads extractable, duplicate_name, and the archive warnings", async () => {
  const main = await read("app-zip/src/main.ts");

  assert.match(main, /typeof entry\.extractable === "boolean"/, "extractable is not validated");
  assert.match(main, /typeof entry\.duplicate_name === "boolean"/, "duplicate_name is not validated");
  assert.match(main, /typeof entry\.encrypted === "boolean"/, "encrypted is not validated");

  // The defect this closes: a Download button on an entry the core already
  // said it could not read.
  assert.match(
    main,
    /}\s*else if \(!entry\.extractable\) \{/,
    "app-zip still offers the same action for entries it cannot extract",
  );
  assert.match(
    main,
    /entry\.extractable\)?\s*;?\s*$/m,
    "app-zip does not consult extractable when choosing what to bulk-download",
  );
  assert.match(
    main,
    /&& entry\.extractable\)/,
    "'Download all' still includes entries the core cannot extract",
  );
  assert.match(main, /duplicate_name\) addEntryFlag/, "duplicate names are not labelled");
  assert.match(main, /archiveWarnings = listing\.warnings/, "archive warnings are never read");
  assert.match(main, /renderArchiveWarnings\(\)/, "archive warnings are never rendered");
});
