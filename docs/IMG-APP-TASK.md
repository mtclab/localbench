# Task: `app-img` — the image tools app (slice IMG-2)

Build a new Vite/TS app **`app-img/`** that is the UI for the image ops now in
`localbench_core` (`resize_image`, `convert_image`, `compress_image`). It ships to
`img.keeplocal.tools` in the keeplocal tools family. You are on branch `feat/img-app`.

**The existing `app/` (the PDF app) is your template and must NOT be modified.** Copy its
scaffold and shell, swap the PDF tooling for image tooling. The shell must stay visually
and structurally identical so the two apps read as one family (coherent shell): same
header/brand/theme-toggle, the same **`local-badge` + `local-inspector` dialog** (the
provable-local proof — keep it verbatim, it's the wedge), same drop-zone pattern, same
service-worker + PWA + SEO scaffolding.

## Copy these from `app/` into `app-img/`, adapting only what's noted
- `package.json` → name `localbench-img-app`; keep scripts identical
  (`"build": "tsc && vite build && node ../scripts/generate-sw.mjs dist"`), same devDeps.
- `vite.config.ts`, `tsconfig.json` → copy as-is (worker `format: "es"` is required — the
  core worker uses top-level `await init()`).
- `src/tokens.css`, `src/style.css` → copy as-is (shared design tokens; a later slice will
  extract these to a shared package — for now a copy is accepted, do not diverge them).
- `public/_headers` → copy **verbatim** (the strict CSP is the moat: do not weaken it).
- `public/icons/*`, `public/manifest.webmanifest` → copy; update manifest `name`/
  `short_name`/`description` to the image app.
- `public/robots.txt`, `public/sitemap.xml` → copy; set the URL to
  `https://img.keeplocal.tools/`.
- `index.html` → copy the shell; swap the `<title>`/meta/OG/JSON-LD/canonical to the image
  app at `https://img.keeplocal.tools/`; replace the tool-switcher + tool panels (below).

## WASM
The core now exports the image ops. Build fresh wasm INTO `app-img/src/wasm/`:
`wasm-pack build core-rs --target web --out-dir ../app-img/src/wasm` (the app's build reads
from `./wasm/localbench_core.js`, same as `app/`). Add the same `src/wasm/.gitignore` so the
built wasm is not committed (mirrors `app/src/wasm/.gitignore`).

## Worker protocol (`app-img/src/core.worker.ts`) — use EXACTLY this shape
Mirror `app/src/core.worker.ts` (module worker, `await init()`, `ready`/`result`/`error`
messages, transferables). Import `resize_image, convert_image, compress_image, core_version`.

```ts
type WorkerRequest =
  | { id: number; type: "resize"; bytes: ArrayBuffer; maxW: number; maxH: number; keepAspect: boolean }
  | { id: number; type: "convert"; bytes: ArrayBuffer; target: "png" | "jpeg" | "webp" }
  | { id: number; type: "compress"; bytes: ArrayBuffer; quality: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };
```
Op signatures (throw a JsValue string on error — catch and forward as `error`):
- `resize_image(new Uint8Array(bytes), maxW, maxH, keepAspect) -> Uint8Array`
- `convert_image(new Uint8Array(bytes), target) -> Uint8Array`  (target ∈ png|jpeg|webp)
- `compress_image(new Uint8Array(bytes), quality) -> Uint8Array` (quality 1..=100)
Return bytes as a transferred `ArrayBuffer` (`result.slice().buffer`), exactly like the PDF
worker. Reuse the `runCoreRequest(request, transfer)` helper pattern from `app/src/main.ts`
(id counter, 30s timeout, transferables).

## Three tools (tool-switcher + panels, mirroring the PDF app's switcher/panel a11y)
Accept input types image/png, image/jpeg, image/webp, image/gif, image/bmp (the core detects
format from bytes — do not trust the extension, but filter the picker to images). Show a
**preview thumbnail** of the loaded image (object URL; revoke it on replace). For every tool,
surface the privacy win: after a successful op, state that **location/EXIF metadata was
removed** (it always is — every re-encode strips it).

1. **Resize** — number inputs "Max width"/"Max height" (px) + a "Keep aspect ratio" checkbox
   (default checked). Button "Resize". On success: preview the result, show new dimensions +
   file size, and a Download button. Never upscales (core clamps) — if the box is larger than
   the source, say the image was kept at its original size.
2. **Convert** — a target-format selector (PNG / JPEG / WebP). Button "Convert". Note under
   JPEG that transparency is flattened onto white. Download the converted file with the right
   extension + MIME.
3. **Compress** — a quality slider (1–100, default ~75) with a live value label. Button
   "Compress". Only JPEG/PNG inputs are compressible (core errors otherwise — show that error
   cleanly). On success show **before → after size and % saved** (mirror how the PDF compress
   panel reports savings), note it never grows the file, Download.

Download helper: build a `Blob` with the correct MIME, object-URL, `<a download>`, revoke.
Derive output filename from the input name + new extension.

## QoL / a11y (match the PDF app's bar)
Keyboard-navigable tool switcher (`aria-current`, roving tabindex), `aria-live` status per
tool, focus-visible, the inspector dialog focus-trap, `#hash` deep-links per tool + per-tool
`<title>` (like the PDF app). No dead-ends, no external requests, works offline.

## Gates you must leave green (run + paste output)
- `cargo test --manifest-path core-rs/Cargo.toml` (unchanged, still green)
- `wasm-pack build core-rs --target web --out-dir ../app-img/src/wasm` (clean)
- `npm --prefix app-img install && npm --prefix app-img run build` (tsc + vite + SW gen, no errors)
- `node scripts/check-local.mjs app-img/dist` — **must PASS** (zero external origins, CSP
  locked to self). Note: `check-local.mjs` takes the dist dir as argv; if it currently
  hard-codes `app/dist`, pass `app-img/dist`. Do NOT weaken the guard to make it pass — if a
  real external URL appears, remove it. The guard already knows how to mask legit inert URLs
  (canonical/og/jsonld/sitemap/robots/source-anchor) for `pdf.keeplocal.tools`; add the
  equivalent masks for `img.keeplocal.tools` in `maskExpectedInertUrls` the same way (both the
  regex match AND the strip-target string), matching the existing structure exactly.

Do NOT modify `app/`, the PDF core functions, or the deploy. Commit to `feat/img-app`. In your
final report: list the files created, paste the four gate outputs, and note anything you had to
deviate on.
