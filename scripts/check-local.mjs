import { access, readdir, readFile } from "node:fs/promises";
import path from "node:path";

const distDirectory = path.resolve(process.argv[2] ?? "app/dist");

try {
  await access(distDirectory);
} catch {
  console.error(`Provable-local check failed: build directory not found at ${distDirectory}`);
  process.exit(1);
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

const scannable = new Set([".css", ".html", ".js", ".mjs"]);
const files = (await filesBelow(distDirectory)).filter(
  (file) => scannable.has(path.extname(file)) || path.basename(file) === "_headers",
);
const failures = [];
let cspCount = 0;

for (const file of files) {
  const relative = path.relative(distDirectory, file);
  const contents = await readFile(file, "utf8");
  const externalOrigins = contents.match(/https?:\/\/[^\s"'`<>)]+/gi) ?? [];
  for (const origin of externalOrigins) {
    failures.push(`${relative}: external URL is forbidden: ${origin}`);
  }

  const connectDirectives = contents.match(/connect-src\s+[^;"<]+/gi) ?? [];
  for (const directive of connectDirectives) {
    cspCount += 1;
    const sources = directive.trim().split(/\s+/).slice(1);
    if (sources.length !== 1 || sources[0] !== "'self'") {
      failures.push(`${relative}: CSP must be exactly connect-src 'self' (found: ${directive})`);
    }
  }
}

if (cspCount < 2) {
  failures.push("Expected connect-src CSP directives in both _headers and the HTML meta fallback.");
}

const wasmFiles = (await filesBelow(distDirectory)).filter((file) => path.extname(file) === ".wasm");
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

console.log(`Provable-local check passed: ${files.length} text assets, ${wasmFiles.length} WASM asset, CSP locked to self.`);

