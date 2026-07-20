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
function maskExpectedInertUrls(relative, contents) {
  if (relative === "index.html") {
    return contents
      .replace(
        /<link\b(?=[^>]*\brel=["']canonical["'])(?=[^>]*\bhref=["']https:\/\/pdf\.keeplocal\.tools\/["'])[^>]*>/gi,
        (tag) => tag.replace("https://pdf.keeplocal.tools/", ""),
      )
      .replace(
        /<meta\b(?=[^>]*\bproperty=["']og:url["'])(?=[^>]*\bcontent=["']https:\/\/pdf\.keeplocal\.tools\/["'])[^>]*>/gi,
        (tag) => tag.replace("https://pdf.keeplocal.tools/", ""),
      )
      .replace(
        /<a\b(?=[^>]*\bhref=["']https:\/\/github\.com\/mtclab\/localbench["'])[^>]*>/gi,
        (tag) => tag.replace("https://github.com/mtclab/localbench", ""),
      )
      .replace(
        /<script\b[^>]*\btype=["']application\/ld\+json["'][^>]*>[\s\S]*?<\/script>/gi,
        (block) =>
          block
            .replace("https://schema.org", "")
            .replace("https://pdf.keeplocal.tools/", ""),
      );
  }

  if (relative === "sitemap.xml") {
    return contents.replace(
      /<loc>https:\/\/pdf\.keeplocal\.tools\/<\/loc>/gi,
      "<loc></loc>",
    );
  }

  if (relative === "robots.txt") {
    return contents.replace(
      /^Sitemap:\s+https:\/\/pdf\.keeplocal\.tools\/sitemap\.xml\s*$/gim,
      "Sitemap:",
    );
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

const failures = [];
let cspCount = 0;

// Matches absolute (http://, https://) AND protocol-relative (//host.tld) URLs.
const urlPattern = /(?:https?:)?\/\/[a-z0-9.-]+\.[a-z]{2,}[^\s"'`<>)]*/gi;

for (const file of textFiles) {
  const relative = path.relative(distDirectory, file);
  const contents = await readFile(file, "utf8");
  const urlScanContents = maskExpectedInertUrls(relative, contents);

  for (const raw of urlScanContents.match(urlPattern) ?? []) {
    // Normalize protocol-relative to both http/https for allowlist comparison.
    const httpsForm = raw.startsWith("//") ? `https:${raw}` : raw;
    const httpForm = raw.startsWith("//") ? `http:${raw}` : raw;
    if (isAllowed(raw) || isAllowed(httpsForm) || isAllowed(httpForm)) continue;
    failures.push(`${relative}: external URL is forbidden: ${raw}`);
  }

  for (const directive of contents.match(/connect-src\s+[^;"<]+/gi) ?? []) {
    cspCount += 1;
    const sources = directive.trim().split(/\s+/).slice(1);
    if (sources.length !== 1 || sources[0] !== "'self'") {
      failures.push(`${relative}: CSP must be exactly connect-src 'self' (found: ${directive})`);
    }
  }

  // worker-src, when present, must also be locked to 'self' (workers can fetch).
  for (const directive of contents.match(/worker-src\s+[^;"<]+/gi) ?? []) {
    const sources = directive.trim().split(/\s+/).slice(1);
    if (!sources.every((s) => s === "'self'")) {
      failures.push(`${relative}: worker-src must be 'self' only (found: ${directive})`);
    }
  }
}

// Scan WASM binaries for hard-coded external URL strings (supply-chain guard).
for (const file of wasmFiles) {
  const relative = path.relative(distDirectory, file);
  const ascii = await readFile(file, "latin1");
  for (const raw of ascii.match(/https?:\/\/[a-z0-9.-]+\.[a-z]{2,}[^\s"'`<>)\x00]*/gi) ?? []) {
    if (isWasmAllowed(raw)) continue;
    failures.push(`${relative}: WASM embeds an unexpected external URL: ${raw}`);
  }
}

if (cspCount < 2) {
  failures.push("Expected connect-src CSP directives in both _headers and the HTML meta fallback.");
}

if (wasmFiles.length === 0) failures.push("No WASM asset was emitted into dist/.");

const swPath = path.join(distDirectory, "sw.js");
try {
  const serviceWorker = await readFile(swPath, "utf8");
  for (const wasmFile of wasmFiles) {
    const wasmUrl = `/${path.relative(distDirectory, wasmFile).split(path.sep).join("/")}`;
    if (!serviceWorker.includes(wasmUrl)) {
      failures.push(`sw.js does not precache WASM asset ${wasmUrl}.`);
    }
  }
} catch {
  failures.push("Offline service worker dist/sw.js is missing.");
}

if (failures.length > 0) {
  console.error("Provable-local check failed:\n" + failures.map((failure) => `- ${failure}`).join("\n"));
  process.exit(1);
}

console.log(
  `Provable-local check passed: ${textFiles.length} text assets + ${wasmFiles.length} WASM scanned, CSP locked to self.`,
);
