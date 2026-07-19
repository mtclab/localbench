# localbench - V1 spec

Working codename. Real name = later branding decision (name-is-world lens).

## One-liner
Client-side file tools. **Your files never leave your device** - every operation runs in your browser over WASM. No upload, no account, no ads, free.

## The thesis (why this beats the incumbents)
Smallpdf / iLovePDF / Adobe online / cloudconvert all **upload your file to their servers**, then paywall or ad-gate the basic operation. The upload IS their business model (telemetry, upsell funnel). So they **structurally cannot** promise "your file never leaves your device" without killing their P&L. That promise is our permanent, un-copyable wedge.

The promise is only credible if **auditable** -> the repo is public, the app is fully static, and we can prove there is no upload path (see "Provable-local" below). This is why localbench is open-source: the openness is a product feature, not a giveaway.

## V1 scope: PDF ops (the anchor)
Ship ONE tool category, done best. A bench that launches as "everything" dies as a content-farm lookalike. PDF is the anchor: highest search volume + genuinely sensitive content (contracts, IDs, medical, legal) so the privacy wedge bites hardest.

V1 operations (all client-side):
1. **Merge** - combine PDFs, drag to reorder.
2. **Split** - extract page ranges / split into N files.
3. **Reorder / rotate / delete pages** - visual page grid.
4. **Compress** - reduce file size (re-encode embedded images, downsample, strip metadata). *The hard one - see codec spike.*

Out of V1 (later waves): OCR (tesseract.wasm), image convert/compress, format conversion, redaction, metadata strip as its own tool, `plainread` (ideas #11).

## Provable-local (the trust mechanism)
The privacy claim must be verifiable, not just asserted:
- **No network calls after asset load.** All WASM/JS/wasm-cores bundled and cache-first. A user can open devtools Network, do an operation, and see zero requests.
- **CSP that forbids exfiltration**: `connect-src 'none'` (or 'self' only for the static assets, none after load) so the browser itself blocks any accidental upload. Documented + shown as a badge users can click to inspect.
- **Installable PWA / full offline** - works with the network cable pulled. The strongest possible proof: unplug and it still works.
- **Public source** - "read the code" link in-app.

This CSP + offline + open-source triad is the moat. Guard it in CI (see below) - a regression that adds a `connect-src` is a P0.

## Architecture
Same shape as the forge family: **a Rust core compiled to WASM, driven by a thin TS shell.** No server - "backend" = the compute core, running client-side in the browser via WASM. New engine logic lives in Rust (`core-rs`), never in ad-hoc JS. This is our standing pattern and it makes the provable-local claim tighter (one auditable, permissive, deterministic core).

- **`core-rs`** - Rust crate holding all PDF logic (merge/split/page-ops/compress). Exposed to JS via `wasm-bindgen`, built with `wasm-pack` (or `wasm-bindgen` + `wasm-opt`). Pure computation, no I/O, no network - takes bytes in, returns bytes out. Unit-tested in Rust (`cargo test`), the primary correctness gate.
- **Thin TS shell** - Vite + TypeScript, minimal deps. Owns only UI, file drag-drop, worker orchestration, download. No PDF logic in JS.
- **Web Workers** - the WASM core runs in a worker so the UI never freezes on a big file; core loaded in-worker.
- **PWA** - service worker, offline, installable.
- **Static**, hosted on **CF Pages** (free, global, our stack).
- Coherent shell (shared tokens/nav/theme, light+dark, AA) - QoL/UX standing rule. One home, tools slot into it; no dead-ends.

### Rust PDF crates (verify licenses at adoption; prefer permissive)
- **Merge / split / reorder / rotate / delete**: `lopdf` (MIT, pure Rust, low-level PDF read/write) - handles all page-level ops. Low risk. Primary candidate.
- **Compress** is the hard part (real image re-encode/downsample of embedded XObjects). Pure-Rust path preferred so it wasm-compiles clean and stays permissive:
  - Parse/rewrite structure with `lopdf`; walk image XObjects; re-encode with pure-Rust codecs: `image` (MIT/Apache), `jpeg-encoder` (pure Rust, no C), `oxipng`, `zune-jpeg`. Downsample + quality knob.
  - AVOID C-dependent crates that fight wasm (`mozjpeg`-sys, pdfium, mupdf/ghostscript = AGPL). If a permissive pure-Rust path can't hit the quality/size floor, escalate to owner before pulling anything AGPL or C-linked.
  - **Spike deliverable (S3)**: a pure-Rust compression path that wasm-builds, a size-reduction benchmark on a corpus of real PDFs, and a visual quality floor. The one genuine technical risk in V1.

## Monetization (later, not V1)
SEO scale -> Ko-fi + optional pro (batch/queue, desktop build). Ad-light. No money-handling in the tool (liability filter). V1 = free, no monetization surface.

## Non-goals / guardrails
- No account, no login, no server-side anything in V1.
- No liability shapes (deterministic file transforms only - no advice, money, safety, subjective inputs). PASS.
- No CI cost concern (public repo -> free Actions), but keep workflows minimal and purposeful.

## CI (public repo -> allowed, keep minimal)
- `build + typecheck + unit` on PR.
- **Provable-local guard**: a test/script that fails if the built bundle references any non-self origin, or if CSP `connect-src` widens. This protects the core promise mechanically.
- Deploy: CF Pages (Git integration or wrangler). Decide in setup slice.

## Build slices (proposed - for owner review)
- **S0 - scaffold**: Cargo workspace + `core-rs` crate skeleton (one trivial wasm-bindgen export) + wasm-pack build wired; Vite+TS+PWA shell that loads the wasm in a worker and shows the version; coherent shell (nav/theme/tokens, light+dark); CSP `connect-src 'none'`; provable-local guard workflow; CF Pages deploy of the stub. Proves the Rust->WASM pipeline + provable-local frame before any tool code.
- **S1 - merge**: `core-rs` merge (lopdf) + drag-drop multi-PDF, reorder, download. First real tool end-to-end. Rust unit tests are the correctness gate.
- **S2 - split + page ops**: `core-rs` split/rotate/delete/reorder + page-grid UI, extract ranges, export.
- **S3 - compress (spike + build)**: resolve the pure-Rust codec path, ship compression with a benchmarked size/quality result.
- **S4 - polish + launch**: offline/PWA hardening, provable-local badge + inspector, SEO (per-op landing pages), a11y AA pass, real-device walk.

## Decisions (locked 2026-07-19)
- **Core**: Rust -> WASM (`core-rs`), same as forge_core. No server. (owner)
- **Framework**: vanilla TS + tiny helpers - auditability + bundle size.
- **Compression**: permissive pure-Rust codecs only; AGPL/C-linked cores need owner sign-off (escalate from S3 if the floor can't be met).
- **Deploy**: CF Pages Git-integration (auto on push).
- **Visibility**: public repo -> CI workflows allowed as needed (provable-local guard is the important one).
- **Anchor**: PDF ops.
