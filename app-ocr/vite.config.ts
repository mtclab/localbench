import { defineConfig } from "vite";

// The offline service worker is generated after the build by
// ../scripts/generate-sw.mjs (invoked from the "build" npm script), where the
// dist output is guaranteed to exist. Keeping it out of the Vite pipeline avoids
// a closeBundle race on the output directory.
export default defineConfig({
  // OCR and WASM initialization stay in a module worker so multi-second
  // inference never blocks the page's event loop.
  worker: {
    format: "es",
  },
  build: {
    target: "es2022",
  },
});
