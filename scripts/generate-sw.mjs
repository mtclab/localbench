import { createHash } from "node:crypto";
import { readdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";

const distDirectory = path.resolve(process.argv[2] ?? "app/dist");

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

function isRuntimeModel(file) {
  const relative = path.relative(distDirectory, file).split(path.sep).join("/");
  return relative.startsWith("models/") && relative.endsWith(".rten");
}

const allFiles = await filesBelow(distDirectory);
const runtimeModels = allFiles.filter(isRuntimeModel).sort();
const files = allFiles
  .filter((file) => path.basename(file) !== "sw.js" && !isRuntimeModel(file))
  .sort();
const urls = files.map((file) => `/${path.relative(distDirectory, file).split(path.sep).join("/")}`);
const hash = createHash("sha256");
for (const file of files) hash.update(await readFile(file));
const cacheName = `localbench-${hash.digest("hex").slice(0, 12)}`;

const serviceWorker = `const CACHE_NAME = ${JSON.stringify(cacheName)};
const MODEL_CACHE_NAME = "localbench-ocr-models-v1";
const PRECACHE_URLS = ${JSON.stringify(["/", ...urls], null, 2)};

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE_NAME)
      .then((cache) => cache.addAll(PRECACHE_URLS))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((names) => Promise.all(names.filter((name) => name !== CACHE_NAME && name !== MODEL_CACHE_NAME).map((name) => caches.delete(name))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const requestUrl = new URL(event.request.url);
  if (event.request.method !== "GET" || requestUrl.origin !== self.location.origin) return;

  if (requestUrl.pathname.startsWith("/models/") && requestUrl.pathname.endsWith(".rten")) {
    const modelResponse = caches.open(MODEL_CACHE_NAME).then(async (cache) => {
      const cached = await cache.match(event.request, { ignoreSearch: true });
      if (cached) return { response: cached, cacheWrite: Promise.resolve() };

      const response = await fetch(event.request);
      if (!response.ok) return { response, cacheWrite: Promise.resolve() };
      return { response, cacheWrite: cache.put(event.request, response.clone()) };
    });
    event.respondWith(modelResponse.then(({ response }) => response));
    event.waitUntil(modelResponse.then(({ cacheWrite }) => cacheWrite));
    return;
  }

  event.respondWith(
    caches.match(event.request, { ignoreSearch: true }).then((cached) => {
      if (cached) return cached;
      if (event.request.mode === "navigate") return caches.match("/index.html");
      return fetch(event.request).then((response) => {
        if (!response.ok) return response;
        const copy = response.clone();
        void caches.open(CACHE_NAME).then((cache) => cache.put(event.request, copy));
        return response;
      });
    }),
  );
});
`;

await writeFile(path.join(distDirectory, "sw.js"), serviceWorker);
console.log(
  `Generated offline service worker with ${urls.length} precached assets and ${runtimeModels.length} runtime-cached OCR models (${cacheName}).`,
);
