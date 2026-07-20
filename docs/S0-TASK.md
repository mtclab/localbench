# S0 - scaffold + provable-local frame (codex task)

Goal: prove the **Rust -> WASM -> worker -> UI** pipeline and the **provable-local** trust frame end to end, before any real tool code. When S0 is done, a user can drop a PDF onto the page and see its page count, computed by Rust-in-WASM, with **zero network calls** after load.

Read `docs/V1-SPEC.md` first. Follow it. Do not add scope beyond this task.

## Deliverables

### 1. Cargo workspace + `core-rs`
- Cargo workspace at repo root. Member crate `core-rs/` (crate name `localbench_core`).
- Crate type `cdylib` + `rlib`. Deps: `wasm-bindgen`, `lopdf` (MIT - **verify it compiles to `wasm32-unknown-unknown`; this de-risks S1**). No C-linked crates.
- Public API (wasm-bindgen exports), pure computation, bytes-in/values-out, **no I/O, no network, no threads**:
  - `pub fn core_version() -> String` - returns the crate version.
  - `pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, JsValue>` - parse with lopdf, return page count; error -> readable message.
- `cargo test` in `core-rs`: a unit test that builds a tiny PDF in-memory (or embeds a minimal valid PDF as bytes) and asserts `pdf_page_count` returns the right number. This is the correctness gate.
- Build to wasm with `wasm-pack build core-rs --target web --out-dir ../app/src/wasm` (or equivalent); the shell imports the generated pkg. Document the exact build command in a root `Makefile` or `package.json` script.

### 2. Thin TS shell (`app/`)
- Vite + TypeScript, **vanilla** (no Preact/Solid/React). Minimal deps.
- Loads the wasm **in a Web Worker** (not the main thread). Worker imports the wasm-pack `web` output, initializes it, exposes `pageCount(bytes)` over `postMessage`.
- UI: coherent shell - header with app name + a tools nav placeholder, theme tokens for **light + dark** (`prefers-color-scheme` default **plus** a manual toggle), WCAG AA contrast. One clean home page.
- Home shows: core version (from wasm) + a drop-zone. Drop a PDF -> worker computes page count -> show "N pages". File read via `FileReader`/`arrayBuffer()` in-browser only. **Never fetch/XHR/upload the file.**
- No dead-ends, no placeholder-lorem clutter; it should already feel like a real (tiny) product.

### 3. Provable-local frame
- **CSP** via a CF Pages `app/public/_headers` file AND a `<meta http-equiv>` fallback:
  - `default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'none'; form-action 'none'`.
  - `connect-src 'self'` (NOT wider) - the wasm/asset fetches are same-origin; the origin serves only static files, so 'self' == no exfiltration. Document this reasoning in a comment.
  - `'wasm-unsafe-eval'` is required to instantiate wasm; that is the only eval-class allowance.
- **PWA**: service worker (cache-first, precache all assets incl. wasm) + manifest, installable, works fully offline. Offline = the ultimate proof.
- **Provable-local guard** (`scripts/check-local.mjs` + a GitHub Actions workflow `.github/workflows/ci.yml`, public repo so Actions is fine):
  - Build the app, then scan `dist/` (JS/CSS/HTML/_headers): FAIL if any absolute `http(s)://` origin other than same-origin placeholders appears in shipped code, or if the CSP `connect-src` lists anything beyond `'self'`. This mechanically protects the core promise.
  - Workflow jobs: `cargo test` (core-rs) + `cargo build --target wasm32-unknown-unknown` + `npm ci && npm run build` + `node scripts/check-local.mjs`. Triggers: `pull_request` + `push` to feature branches is fine (public repo).

### 4. CF Pages deploy prep (files only)
- `app/public/_headers` (CSP above) and correct Vite build output. Actual CF Pages project connection is an owner dashboard action - just make the build output + headers correct and note it in the PR body. Do NOT attempt `wrangler deploy` (wrangler absent; deploy = git-integration).

## Quality bars (family lessons - non-negotiable)
- All logic that touches file bytes is in **Rust** (`core-rs`), never JS. JS only orchestrates.
- Permissive licenses only (lopdf MIT). No AGPL, no C-linked, no crate that fails wasm32.
- Deterministic core, no network, no randomness in core.
- **Verify before claiming done**: `cargo test` green, `cargo build --target wasm32-unknown-unknown` green, `wasm-pack build` green, `npm run build` green, `node scripts/check-local.mjs` green. Paste the actual command output. Do not over-report - if something is red, say so.
- Do not widen CSP to make something work; if wasm needs a specific allowance, use the minimal one and document why.

## Done = 
Branch `feat/s0-scaffold` with all of the above committed, every verify command green with output shown, and a short PR-body summary noting the owner action (connect CF Pages project).
