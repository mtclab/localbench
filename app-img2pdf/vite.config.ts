import { defineConfig } from "vite";

// The offline service worker is generated after the build by
// ../scripts/generate-sw.mjs (invoked from the "build" npm script), where the
// dist output is guaranteed to exist. Keeping it out of the Vite pipeline avoids
// a closeBundle race on the output directory.
export default defineConfig({
  // src/style.css imports ../../shared/identity.css, which lives outside this
  // app's Vite root. Dev-serving a file outside the root needs an explicit
  // allow; the production build inlines the @import, so dist is unaffected.
  server: {
    fs: {
      allow: [".", "../shared"],
    },
  },
  // The core worker is a module worker (new Worker(url, { type: "module" })) and
  // uses top-level await to init the WASM, so it must bundle as ES, not iife.
  worker: {
    format: "es",
  },
  build: {
    target: "es2022",
  },
});
