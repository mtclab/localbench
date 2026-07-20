import { defineConfig } from "vite";

// The offline service worker is generated after the build by
// ../scripts/generate-sw.mjs (invoked from the "build" npm script), where the
// dist output is guaranteed to exist. Keeping it out of the Vite pipeline avoids
// a closeBundle race on the output directory.
export default defineConfig({
  // The core worker is a module worker (new Worker(url, { type: "module" })) and
  // uses top-level await to init the WASM, so it must bundle as ES, not iife.
  worker: {
    format: "es",
  },
  build: {
    target: "es2022",
  },
});
