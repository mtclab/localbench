/*
 * The provable-local build gate.
 *
 * What this gate is for: keeplocal's whole promise is that the user's file never
 * leaves their device. This script is the *build-time half* of proving it. It
 * reads the artefact a browser would actually be served and fails the build when
 * that artefact could reach the network.
 *
 * Read the honesty notes at the bottom of the output before trusting it further
 * than it goes: static analysis can prove the absence of *shipped* network
 * endpoints and the presence of a *locked* policy. It cannot prove what a
 * computed expression evaluates to at runtime. The runtime half of the proof is
 * the browser smoke (scripts/staging-smoke.mjs, which asserts zero cross-origin
 * requests across a real user journey) and the in-page receipt.
 *
 * Three defects this file previously had, and the shape of the fix, so they are
 * not reintroduced:
 *  1. The CSP check counted regex hits for /connect-src .../ in the RAW text of
 *     every asset — so the marketing sentence "<code>connect-src 'self'</code>"
 *     in the trust card satisfied the gate. A build with no CSP at all passed.
 *     Now the policy is parsed only from the two places a browser reads it: the
 *     Cloudflare _headers rule and the <meta http-equiv> tag's content attribute.
 *  2. The exfiltration check was a grep for literal URLs, which any runtime
 *     string construction walks straight through. Now every network-capable API
 *     in the shipped JS must match a reviewed allowlist entry.
 *  3. (Not this file.) The in-page receipt counted requests in the window only,
 *     while the user's file is processed in a worker with its own timeline.
 */
import { access, readdir, readFile } from "node:fs/promises";
import path from "node:path";

const distDirectory = path.resolve(process.argv[2] ?? "app/dist");

try {
  await access(distDirectory);
} catch {
  console.error(`Provable-local check failed: build directory not found at ${distDirectory}`);
  process.exit(1);
}

// Benign URLs that are identifiers, not network fetches (XML/SVG namespaces).
// Anything not on this list is treated as a potential exfiltration endpoint.
const ALLOWED_URLS = [
  "http://www.w3.org/2000/svg",
  "http://www.w3.org/1999/xlink",
  "http://www.w3.org/XML/1998/namespace",
  "http://www.w3.org/1999/xhtml",
  "http://www.sitemaps.org/schemas/sitemap/0.9",
];

function isAllowed(url) {
  return ALLOWED_URLS.some((allowed) => url === allowed || url.startsWith(allowed));
}

// Known-benign URL strings baked into the WASM by dependencies (crate repo links
// in error/panic messages). These are inert data, not fetch endpoints — WASM
// cannot initiate a request without a JS bridge, and CSP connect-src 'self'
// would block one anyway. Listed explicitly so a genuinely UNexpected URL still
// fails. Add a crate's URL here only after confirming it is inert metadata.
const WASM_ALLOWED_URLS = ["https://github.com/J-F-Liu/lopdf/"];

function isWasmAllowed(url) {
  return isAllowed(url) || WASM_ALLOWED_URLS.some((allowed) => url.startsWith(allowed));
}

// These URLs appear only in non-fetching document contexts: canonical/social
// metadata, JSON-LD data, a user-activated source link, and crawler discovery
// files. Mask the exact approved occurrences before URL scanning. Keeping this
// structural (instead of globally allowlisting the origins) means the same URL
// would still fail if it appeared in JS, CSS, img/src, script/src, or style/link.
const SITE_ORIGINS = [
  "https://pdf.keeplocal.tools/",
  "https://img.keeplocal.tools/",
  "https://img2pdf.keeplocal.tools/",
  "https://zip.keeplocal.tools/",
  "https://scrub.keeplocal.tools/",
];

function maskExpectedInertUrls(relative, contents) {
  if (relative === "index.html") {
    let masked = contents;
    for (const origin of SITE_ORIGINS) {
      const escaped = origin.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&");
      masked = masked
        .replace(
          new RegExp(
            `<link\\b(?=[^>]*\\brel=["']canonical["'])(?=[^>]*\\bhref=["']${escaped}["'])[^>]*>`,
            "gi",
          ),
          (tag) => tag.replace(origin, ""),
        )
        .replace(
          new RegExp(
            `<meta\\b(?=[^>]*\\bproperty=["']og:url["'])(?=[^>]*\\bcontent=["']${escaped}["'])[^>]*>`,
            "gi",
          ),
          (tag) => tag.replace(origin, ""),
        );
    }
    return masked
      .replace(
        /<a\b(?=[^>]*\bhref=["']https:\/\/github\.com\/mtclab\/localbench["'])[^>]*>/gi,
        (tag) => tag.replace("https://github.com/mtclab/localbench", ""),
      )
      .replace(
        /<script\b[^>]*\btype=["']application\/ld\+json["'][^>]*>[\s\S]*?<\/script>/gi,
        (block) => {
          let cleaned = block.replace("https://schema.org", "");
          for (const origin of SITE_ORIGINS) cleaned = cleaned.replace(origin, "");
          return cleaned;
        },
      );
  }

  if (relative === "sitemap.xml") {
    let masked = contents;
    for (const origin of SITE_ORIGINS) {
      const escaped = origin.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&");
      masked = masked.replace(new RegExp(`<loc>${escaped}</loc>`, "gi"), "<loc></loc>");
    }
    return masked;
  }

  if (relative === "robots.txt") {
    let masked = contents;
    for (const origin of SITE_ORIGINS) {
      const escaped = `${origin}sitemap.xml`.replace(/[.*+?^${}()|[\]\\/]/g, "\\$&");
      masked = masked.replace(new RegExp(`^Sitemap:\\s+${escaped}\\s*$`, "gim"), "Sitemap:");
    }
    return masked;
  }

  return contents;
}

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

// Any text asset a browser parses can carry a URL: scripts, styles, markup,
// the manifest, SVGs, JSON, and the CF _headers file. Scan them all.
const scannable = new Set([
  ".css",
  ".html",
  ".js",
  ".mjs",
  ".json",
  ".svg",
  ".webmanifest",
  ".xml",
  ".txt",
]);
const allFiles = await filesBelow(distDirectory);
const textFiles = allFiles.filter(
  (file) => scannable.has(path.extname(file)) || path.basename(file) === "_headers",
);
const wasmFiles = allFiles.filter((file) => path.extname(file) === ".wasm");
const relativeOf = (file) => path.relative(distDirectory, file).split(path.sep).join("/");

const failures = [];
const notes = [];

// ---------------------------------------------------------------------------
// 1. Literal external URLs in shipped assets.
// ---------------------------------------------------------------------------

// Matches absolute (http://, https://) AND protocol-relative (//host.tld) URLs.
const urlPattern = /(?:https?:)?\/\/[a-z0-9.-]+\.[a-z]{2,}[^\s"'`<>)]*/gi;

const fileContents = new Map();
for (const file of textFiles) {
  const relative = relativeOf(file);
  const contents = await readFile(file, "utf8");
  fileContents.set(relative, contents);
  const urlScanContents = maskExpectedInertUrls(relative, contents);

  for (const raw of urlScanContents.match(urlPattern) ?? []) {
    // Normalize protocol-relative to both http/https for allowlist comparison.
    const httpsForm = raw.startsWith("//") ? `https:${raw}` : raw;
    const httpForm = raw.startsWith("//") ? `http:${raw}` : raw;
    if (isAllowed(raw) || isAllowed(httpsForm) || isAllowed(httpForm)) continue;
    failures.push(`${relative}: external URL is forbidden: ${raw}`);
  }
}

// ---------------------------------------------------------------------------
// 2. The Content Security Policy, read the way a browser reads it.
//
// A browser gets the policy from exactly two places in this build: the
// Content-Security-Policy response header (Cloudflare Pages emits it from
// dist/_headers) and the <meta http-equiv="Content-Security-Policy"> fallback
// that applies when the page is opened from disk or from a cache that dropped
// the header. Prose in the page body is NOT a policy and can never satisfy this
// section — that was the bug this replaced.
// ---------------------------------------------------------------------------

function decodeEntities(value) {
  return value
    .replace(/&#(\d+);/g, (_, code) => String.fromCodePoint(Number(code)))
    .replace(/&#x([0-9a-f]+);/gi, (_, code) => String.fromCodePoint(Number.parseInt(code, 16)))
    .replace(/&quot;/gi, '"')
    .replace(/&apos;/gi, "'")
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&amp;/gi, "&");
}

function attributeValue(tag, name) {
  const match = tag.match(
    new RegExp(`\\b${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)'|([^\\s"'>]+))`, "i"),
  );
  if (!match) return null;
  return decodeEntities(match[1] ?? match[2] ?? match[3] ?? "");
}

/** Cloudflare _headers: unindented path patterns, indented "Name: value" lines. */
function parseHeadersFile(text) {
  const rules = [];
  let current = null;
  for (const rawLine of text.split(/\r?\n/)) {
    const trimmed = rawLine.trim();
    if (trimmed === "" || trimmed.startsWith("#")) continue;
    if (/^\s/.test(rawLine)) {
      const match = trimmed.match(/^([^:\s]+)\s*:\s*(.*)$/);
      if (match && current) current.headers.push({ name: match[1].toLowerCase(), value: match[2] });
      continue;
    }
    current = { pattern: trimmed, headers: [] };
    rules.push(current);
  }
  return rules;
}

/** "a 'self'; b 'none'" -> Map { a => ["'self'"], b => ["'none'"] } */
function parsePolicy(text) {
  const directives = new Map();
  for (const chunk of text.split(";")) {
    const parts = chunk.trim().split(/\s+/).filter(Boolean);
    if (parts.length === 0) continue;
    const name = parts[0].toLowerCase();
    if (!directives.has(name)) directives.set(name, parts.slice(1));
  }
  return directives;
}

/**
 * A source expression that can name somewhere other than this origin. Keywords
 * ('self', 'none', hashes, nonces) and inline-data schemes are not remote; a
 * bare "*", a scheme like https:/ws:, and any host expression are.
 */
function isRemoteSource(source) {
  const value = source.toLowerCase();
  if (value === "*") return true;
  if (value.startsWith("'")) return false;
  if (["data:", "blob:", "filesystem:", "mediastream:"].includes(value)) return false;
  return true;
}

// Directives that must be present and exactly equal to these sources. Together
// they are the whole of "this build cannot talk to anywhere else":
// connect-src binds fetch/XHR/WebSocket/beacon, worker-src binds the thread that
// holds the user's file, default-src catches anything not listed, and the last
// three remove the classic non-connect exfiltration routes.
const REQUIRED_EXACT = new Map([
  ["default-src", ["'self'"]],
  ["connect-src", ["'self'"]],
  ["worker-src", ["'self'"]],
  ["object-src", ["'none'"]],
  ["base-uri", ["'none'"]],
  ["form-action", ["'none'"]],
]);

// wasm-unsafe-eval is required to instantiate the Rust core; nothing else.
const SCRIPT_SRC_ALLOWED = new Set(["'self'", "'wasm-unsafe-eval'"]);

// Directives compared between the header policy and the meta fallback. The two
// legitimately differ on frame-ancestors (a meta tag cannot carry it), so only
// the binding set is required to agree.
const CONSISTENCY_DIRECTIVES = [...REQUIRED_EXACT.keys(), "script-src"];

function validatePolicy(label, policyText) {
  const directives = parsePolicy(policyText);
  const problems = [];

  for (const [name, expected] of REQUIRED_EXACT) {
    const actual = directives.get(name);
    if (!actual) {
      problems.push(`${label}: CSP is missing the ${name} directive`);
      continue;
    }
    const same =
      actual.length === expected.length &&
      actual.every((source, index) => source.toLowerCase() === expected[index]);
    if (!same) {
      problems.push(
        `${label}: CSP ${name} must be exactly "${expected.join(" ")}" (found "${actual.join(" ")}")`,
      );
    }
  }

  const scriptSrc = directives.get("script-src");
  if (!scriptSrc) {
    problems.push(`${label}: CSP is missing the script-src directive`);
  } else {
    for (const source of scriptSrc) {
      if (!SCRIPT_SRC_ALLOWED.has(source.toLowerCase())) {
        problems.push(`${label}: CSP script-src may not contain ${source}`);
      }
    }
  }

  for (const [name, sources] of directives) {
    for (const source of sources) {
      if (isRemoteSource(source)) {
        problems.push(`${label}: CSP ${name} names a remote source: ${source}`);
      }
    }
  }

  return { directives, problems };
}

const headersRelative = "_headers";
const headersText = fileContents.get(headersRelative);
let headerPolicy = null;

if (headersText === undefined) {
  failures.push("dist/_headers is missing, so no Content-Security-Policy is served.");
} else {
  const rules = parseHeadersFile(headersText);
  const cspRules = rules.flatMap((rule) =>
    rule.headers
      .filter((header) => header.name === "content-security-policy")
      .map((header) => ({ pattern: rule.pattern, value: header.value })),
  );

  if (cspRules.length === 0) {
    failures.push("dist/_headers declares no Content-Security-Policy header.");
  }

  // Every declared policy is validated; the one on the catch-all rule is the one
  // that must exist, so a narrower rule cannot be mistaken for site-wide cover.
  for (const rule of cspRules) {
    const { problems } = validatePolicy(`_headers ${rule.pattern}`, rule.value);
    failures.push(...problems);
  }

  const catchAll = cspRules.find((rule) => rule.pattern === "/*");
  if (!catchAll) {
    failures.push(
      `dist/_headers has no Content-Security-Policy on the catch-all "/*" rule (found rules: ${
        cspRules.map((rule) => rule.pattern).join(", ") || "none"
      }).`,
    );
  } else {
    headerPolicy = parsePolicy(catchAll.value);
  }
}

// Every shipped HTML document must carry the meta fallback, and every meta CSP
// found anywhere in the build must itself be locked down.
let metaPolicy = null;
for (const [relative, contents] of fileContents) {
  if (!relative.endsWith(".html")) continue;
  const policies = (contents.match(/<meta\b[^>]*>/gi) ?? [])
    .filter((tag) => (attributeValue(tag, "http-equiv") ?? "").toLowerCase() === "content-security-policy")
    .map((tag) => attributeValue(tag, "content") ?? "");

  if (policies.length === 0) {
    failures.push(`${relative}: no <meta http-equiv="Content-Security-Policy"> fallback.`);
    continue;
  }

  for (const policy of policies) {
    const { problems } = validatePolicy(`${relative} meta`, policy);
    failures.push(...problems);
  }
  if (relative === "index.html") metaPolicy = parsePolicy(policies[0]);
}

if (headerPolicy && metaPolicy) {
  for (const name of CONSISTENCY_DIRECTIVES) {
    const fromHeader = (headerPolicy.get(name) ?? []).join(" ");
    const fromMeta = (metaPolicy.get(name) ?? []).join(" ");
    if (fromHeader !== fromMeta) {
      failures.push(
        `CSP ${name} differs between the served header ("${fromHeader}") and the meta fallback ("${fromMeta}"); the receipt reads the meta tag, so a drifting fallback misreports the policy.`,
      );
    }
  }
}

// ---------------------------------------------------------------------------
// 3. Network-capable APIs in the shipped JavaScript.
//
// A grep for literal URLs proves nothing: fetch(["ht","tps://x"].join("")) has
// no literal URL in it. So instead of hunting for destinations, this section
// enumerates every API in the bundle that could REACH a destination, and
// requires each occurrence to be one of the reviewed same-origin call sites
// below. A new one fails the build until a human reviews it and adds it here.
// ---------------------------------------------------------------------------

const NETWORK_APIS = [
  {
    id: "sendBeacon",
    pattern: /\bsendBeacon\b/g,
    why: "navigator.sendBeacon() posts bytes to any URL and never appears in the resource-timing receipt",
  },
  {
    id: "WebSocket",
    pattern: /\bWebSocket\b/g,
    why: "a WebSocket is a bidirectional channel that is never reported as a resource entry",
  },
  { id: "EventSource", pattern: /\bEventSource\b/g, why: "server-sent events open a long-lived remote stream" },
  { id: "XMLHttpRequest", pattern: /\bXMLHttpRequest\b/g, why: "XHR can upload a file body to any URL" },
  {
    id: "WebRTC",
    pattern: /\bRTC(?:PeerConnection|DataChannel)\b/g,
    why: "WebRTC data channels are not covered by connect-src at all",
  },
  { id: "importScripts", pattern: /\bimportScripts\s*\(/g, why: "importScripts() can pull remote code into a worker" },
  {
    id: "dynamic-import",
    pattern: /\bimport\s*\(/g,
    why: "a dynamic import with a computed specifier can load remote code",
  },
  {
    id: "fetch",
    pattern: /\bfetch\s*\(/g,
    why: "fetch() with a computed argument can be pointed anywhere at runtime",
  },
];

/*
 * The reviewed call sites. An occurrence is approved only when it falls INSIDE
 * the span the entry's own pattern matches — not merely near it — so a call
 * injected next to an approved one cannot inherit its approval. There is no
 * blanket exemption for literal arguments either: every network-capable call in
 * the bundle gets a human's eyes, which on a three-call-site codebase costs
 * nothing and is the entire point of the gate.
 *
 * All three entries are generated code (wasm-bindgen, Vite, our own
 * service-worker template), which is why the patterns look like minified noise.
 * A toolchain upgrade that reshapes them will fail this gate on purpose.
 */
const REVIEWED_CALL_SITES = [
  {
    id: "wasm-bindgen-init",
    api: "fetch",
    files: /^assets\/core\.worker-[\w-]+\.js$/,
    context: /instanceof URL\)&&\(\w+=fetch\(\w+\)\)/,
    why: "wasm-bindgen's loader fetching the .wasm URL built from import.meta.url (same origin, hashed asset name)",
  },
  {
    id: "vite-modulepreload-polyfill",
    api: "fetch",
    files: /^assets\/index-[\w-]+\.js$/,
    context: /\.ep=!0[;,][\s\S]{0,80}?fetch\(\w+\.href,\w+\)/,
    why: "Vite's modulepreload polyfill re-fetching a <link rel=modulepreload> href already present in this document",
  },
  {
    id: "service-worker-passthrough",
    api: "fetch",
    files: /^sw\.js$/,
    context: /return fetch\(event\.request\)\.then\(/,
    requires:
      /if \(event\.request\.method !== "GET" \|\| requestUrl\.origin !== self\.location\.origin\) return;/,
    why: "the offline service worker forwarding a cache miss, behind an explicit same-origin guard",
  },
];

/** Byte spans in one source that a reviewed call site vouches for. */
function reviewedSpans(source) {
  const spans = [];
  for (const site of REVIEWED_CALL_SITES) {
    if (!site.files.test(source.relative)) continue;
    if (site.requires && !site.requires.test(source.whole)) continue;
    const pattern = new RegExp(site.context.source, `${site.context.flags.replace("g", "")}g`);
    let match;
    while ((match = pattern.exec(source.code)) !== null) {
      spans.push({ id: site.id, api: site.api, start: match.index, end: match.index + match[0].length });
      if (match[0].length === 0) pattern.lastIndex += 1;
    }
  }
  return spans;
}

/** JS a browser will execute: bundled chunks plus any inline (non-JSON-LD) script. */
function executableSources() {
  const sources = [];
  for (const [relative, contents] of fileContents) {
    if (relative.endsWith(".js") || relative.endsWith(".mjs")) {
      sources.push({ relative, code: contents, whole: contents });
      continue;
    }
    if (!relative.endsWith(".html")) continue;
    for (const block of contents.match(/<script\b[^>]*>[\s\S]*?<\/script>/gi) ?? []) {
      const open = block.match(/<script\b[^>]*>/i)?.[0] ?? "";
      const type = (attributeValue(open, "type") ?? "").toLowerCase();
      if (type === "application/ld+json" || type === "application/json") continue;
      const body = block.replace(/^<script\b[^>]*>/i, "").replace(/<\/script>$/i, "");
      if (body.trim() === "") continue;
      sources.push({ relative: `${relative} (inline script)`, code: body, whole: contents });
    }
  }
  return sources;
}

const usedCallSites = new Set();
let reviewedHits = 0;

for (const source of executableSources()) {
  const spans = reviewedSpans(source);

  for (const api of NETWORK_APIS) {
    const pattern = new RegExp(api.pattern.source, api.pattern.flags);
    let match;
    while ((match = pattern.exec(source.code)) !== null) {
      const index = match.index;
      const covering = spans.find(
        (span) => span.api === api.id && index >= span.start && index < span.end,
      );

      if (covering) {
        usedCallSites.add(covering.id);
        reviewedHits += 1;
        continue;
      }

      failures.push(
        `${source.relative}: unreviewed network-capable call site "${match[0].trim()}" at offset ${index} — ${api.why}. If this is legitimate, add a reviewed call site to REVIEWED_CALL_SITES in scripts/check-local.mjs.`,
      );
    }
  }
}

for (const site of REVIEWED_CALL_SITES) {
  if (!usedCallSites.has(site.id)) {
    notes.push(
      `reviewed call site "${site.id}" matched nothing in this build — if the toolchain stopped emitting it, drop the entry.`,
    );
  }
}

// Script inside an SVG never runs when the SVG is used as an image, but it does
// when the SVG is navigated to directly — and these are same-origin assets.
for (const [relative, contents] of fileContents) {
  if (!relative.endsWith(".svg")) continue;
  if (/<script\b/i.test(contents)) failures.push(`${relative}: SVG assets must not contain <script>.`);
}

// ---------------------------------------------------------------------------
// 4. WASM binaries and the offline service worker.
// ---------------------------------------------------------------------------

// Scan WASM binaries for hard-coded external URL strings (supply-chain guard).
for (const file of wasmFiles) {
  const relative = relativeOf(file);
  const ascii = await readFile(file, "latin1");
  for (const raw of ascii.match(/https?:\/\/[a-z0-9.-]+\.[a-z]{2,}[^\s"'`<>)\x00]*/gi) ?? []) {
    if (isWasmAllowed(raw)) continue;
    failures.push(`${relative}: WASM embeds an unexpected external URL: ${raw}`);
  }
}

if (wasmFiles.length === 0) failures.push("No WASM asset was emitted into dist/.");

const serviceWorker = fileContents.get("sw.js");
if (serviceWorker === undefined) {
  failures.push("Offline service worker dist/sw.js is missing.");
} else {
  for (const wasmFile of wasmFiles) {
    const wasmUrl = `/${relativeOf(wasmFile)}`;
    if (!serviceWorker.includes(wasmUrl)) {
      failures.push(`sw.js does not precache WASM asset ${wasmUrl}.`);
    }
  }
}

// ---------------------------------------------------------------------------
// Verdict.
// ---------------------------------------------------------------------------

if (failures.length > 0) {
  console.error("Provable-local check failed:\n" + failures.map((failure) => `- ${failure}`).join("\n"));
  process.exit(1);
}

const servedPolicy = headerPolicy
  ? [...headerPolicy].map(([name, sources]) => [name, ...sources].join(" ")).join("; ")
  : "none";

console.log(
  [
    `Provable-local check passed for ${path.relative(process.cwd(), distDirectory) || distDirectory}:`,
    `  assets scanned              ${textFiles.length} text + ${wasmFiles.length} WASM`,
    `  served policy (_headers /*) ${servedPolicy}`,
    `  meta fallback               agrees with the served policy on ${CONSISTENCY_DIRECTIVES.join(", ")}`,
    `  network-capable call sites  ${reviewedHits} reviewed, 0 unreviewed`,
    "",
    "  What this proves: the shipped assets contain no external URL, the policy a",
    "  browser will enforce is locked to this origin (parsed from the _headers rule",
    "  and the meta fallback, never from page copy), and every API in the bundle that",
    "  could reach the network is a reviewed same-origin call site.",
    "",
    "  What this CANNOT prove: static analysis cannot decide what a computed",
    "  expression evaluates to at runtime, cannot see into the WASM's control flow,",
    "  and says nothing about what the CDN actually serves. Those need the runtime",
    "  half: scripts/staging-smoke.mjs drives a real browser over a real user journey",
    "  and asserts zero cross-origin requests (Playwright sees every request the page",
    "  and its workers make, including ones CSP blocks); the served headers must be",
    "  re-verified against the live hosts after each deploy.",
    ...notes.map((note) => `  NOTE: ${note}`),
  ].join("\n"),
);
