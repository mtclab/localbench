# Task: OCR (image → text) — `core-ocr` crate + `app-ocr/` app (slice OCR-1)

Seventh tool: extract text from an image, 100% in-browser, via `ocrs` + `rten` (pure-Rust ML).
Ships to `ocr.keeplocal.tools`, Pages project `keeplocal-ocr`. ONE branch `feat/ocr`: Part A =
core crate, then Part B = app. Do Part A green before Part B. Phase 0 already proved feasibility —
see `docs/spikes/OCR_SPIKE.md` for the measured numbers, the working API, and the landmines; the
throwaway harness at `repot/ocr-spike/` is a reference you may lift the core wrapper from.

## Non-negotiable architecture: ISOLATE the heavy crate
`rten` adds ~2 MB of wasm. It must NOT touch the shared `core-rs` (that would bloat every other
tool's wasm). Create a **separate crate `core-ocr/`** (sibling of `core-rs/`, not a member of any
shared build). `app-ocr` builds its wasm from `core-ocr` ONLY. Do NOT modify `core-rs` or any
other app.

## Non-negotiable floors
- **Pure-Rust.** `ocrs 0.12.2` + `rten 0.24.0` (MIT/Apache) + `image`. rten configured
  `default-features = false, features = ["rten_format"]`; `core-ocr/.cargo/config.toml` sets
  `rustflags = ["-C", "target-feature=+simd128", "--cfg", "getrandom_backend=\"wasm_js\""]` and
  `getrandom` with `wasm_js` for wasm (the Phase-0 config — reuse it verbatim). No C: a
  `cargo tree` check must show no `-sys`/`cc` (the `libc` type-stub is fine).
- **Moat.** Models are served **same-origin** static assets; the OCR runs client-side; the user's
  image never leaves the tab. Strict CSP unchanged (`connect-src 'self'` — the worker fetching
  `/models/*.rten` is same-origin, allowed). The provable-local inspector must still read
  external-requests = 0.
- Do NOT create CI. Do NOT touch other crates/apps/deploy/CF.

## Part A — `core-ocr/` (Rust → wasm)
Lift the working wrapper from `repot/ocr-spike/core/` and harden it. Exports:
```rust
#[wasm_bindgen] pub fn core_version() -> String;
/// Build the OcrEngine from the two model blobs; store in a thread_local OnceCell/RefCell.
#[wasm_bindgen] pub fn load_engine(detection: &[u8], recognition: &[u8]) -> Result<(), JsValue>;
/// Decode the image, run detection+recognition, return recognized text (lines joined with \n).
/// Returns Err (never panics) on undecodable/oversized input. Guard decoded pixels
/// (reuse a MAX_DECODED_PIXELS-style cap) before allocating.
#[wasm_bindgen] pub fn run_ocr(image: &[u8]) -> Result<String, JsValue>;
/// True once load_engine has succeeded (so the app can gate run_ocr).
#[wasm_bindgen] pub fn engine_ready() -> bool;
```
Deterministic (no clock/rand beyond getrandom link). All fallible ops → JsValue string errors,
no unwrap/panic across the boundary. A few native-ish tests where feasible (engine construction
from the real model bytes is heavy — at minimum test that `run_ocr` on garbage returns Err and
`engine_ready()` is false before load). Build: `wasm-pack build core-ocr --target web --out-dir
../app-ocr/src/wasm`.

## Model provisioning (NOT committed — public repo, 12 MB binaries)
Add `scripts/fetch-ocr-models.sh` that downloads into `app-ocr/public/models/`:
- `text-detection.rten` ← https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten (2.5 MB)
- `text-recognition.rten` ← https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten (9.7 MB)
Gitignore `app-ocr/public/models/*.rten` (mirror how the smoke corpora are ignored). Document in
the app README that the models are fetched at build/deploy time, licensed MIT/Apache from ocrs.

## Part B — `app-ocr/` (cloned from `app-img/`; single tool)
Copy the app-img scaffold+shell verbatim (header/theme, **`local-badge` + `local-inspector`**,
drop-zone, SW+PWA+SEO+a11y). URLs → `https://ocr.keeplocal.tools/`. Name `localbench-ocr-app`.
`public/_headers` verbatim (strict CSP; `img-src 'self' data: blob:` for the preview). Build wasm
from **core-ocr**.

### OCR runs in a WEB WORKER (mandatory)
The Phase-0 spike ran OCR on the main thread and froze the UI for ~2.5 s. The worker
(`app-ocr/src/core.worker.ts`, module worker, `await init()`) owns the engine:
```ts
type WorkerRequest =
  | { id: number; type: "loadModels"; detection: ArrayBuffer; recognition: ArrayBuffer }
  | { id: number; type: "ocr"; image: ArrayBuffer };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "modelsLoaded"; id: number }
  | { type: "text"; id: number; text: string }
  | { type: "error"; id?: number; message: string };
```
Imports `load_engine, run_ocr, core_version, engine_ready`. Transfer the image buffer in.

### Model loading — lazy + SW runtime-cache (don't pay 14 MB on first paint)
- Do NOT precache the models in the service worker install step (keeps first paint fast). Precache
  only the shell + wasm as usual.
- On the **first OCR** (or a "Load OCR engine" affordance), the main thread `fetch()`es
  `/models/text-detection.rten` + `/models/text-recognition.rten`, shows a clear "Downloading the
  offline OCR engine (~12 MB, one time)…" progress state, posts them to the worker (`loadModels`),
  and marks the engine ready. Subsequent OCRs skip the download.
- Add a **runtime cache** for `/models/*.rten` so after the first load they work **offline**
  (extend `scripts/generate-sw.mjs` or add an app-local SW rule — cache-first for `/models/`).
  Document: "the OCR engine downloads once, then works offline."

### The flow
- Drop/pick an image (jpeg/png/webp/gif/bmp). Show a preview thumbnail (object URL, revoke on
  replace; blob: allowed).
- "Extract text" → (load models if needed, with progress) → worker OCR (spinner; UI stays live) →
  show the recognized text in a `<textarea readonly>` (or a selectable block) with a **Copy**
  button and a **Download .txt** button.
- Honest framing in the copy: "Works best on clear printed English. This is an early-preview OCR —
  check the result." NOT "100% correct."
- Errors (undecodable image, engine load failure) → surfaced in `aria-live`, no dead-end.
- Keep the a11y bar (focus, aria-live, deep-link/title, dialog trap) and the badge/inspector.

## Gates (run + paste)
- `cargo tree --manifest-path core-ocr/Cargo.toml --target wasm32-unknown-unknown | grep -Ei '\-sys|cc ' | grep -vi 'libc\|js-sys\|wasm'` → empty (no C).
- `wasm-pack build core-ocr --target web --out-dir ../app-ocr/src/wasm` (clean) + `ls -l` the wasm size.
- `bash scripts/fetch-ocr-models.sh && ls -l app-ocr/public/models` (both .rten present).
- `npm --prefix app-ocr install && npm --prefix app-ocr run build` (clean).
- `node scripts/check-local.mjs app-ocr/dist` — **must PASS** (add `ocr.keeplocal.tools` inert-URL
  masks the SAME way `img.keeplocal.tools`'s are written — regex branch AND strip-target). Do NOT
  weaken the guard. Note: the `.rten` model binaries are data (not scanned); the wasm IS scanned.

## Report
Commit to `feat/ocr` (models gitignored). Final report: files created, ALL gate outputs, wasm
size, and note how to serve for the overseer's staging + Web-Worker verification. Do NOT touch
other crates/apps, the deploy, or CF.
