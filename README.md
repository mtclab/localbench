# localbench

**Client-side file tools. Your files never leave your device.**

Every operation runs in your browser over WebAssembly. No upload. No account. No ads. Free and open-source.

> Working codename. Real product name is a later branding decision.

## Why

The big file-tool sites (Smallpdf, iLovePDF, Adobe online, cloudconvert) all upload your file to their servers, then paywall or ad-gate the basic operation. The upload *is* their business model. localbench does the work in your browser instead - so your file physically never leaves your machine.

That promise is only worth anything if you can verify it, so:

- **Fully static, no backend.** Nothing to upload to.
- **Strict CSP** (`connect-src 'self'`) - WASM and app assets can load only from the same static origin, while external exfiltration is blocked.
- **Works offline.** Install it, pull your network cable, it still works. That is the proof.
- **Open source.** Read the code.

## Status

S0 proves the Rust → WASM → worker → UI pipeline with a local PDF page counter. The later V1 anchor remains **PDF ops** (merge / split / page-ops / compress). See [`docs/V1-SPEC.md`](docs/V1-SPEC.md).

Concept + wedge analysis: [mtclab/ideas #9](https://github.com/mtclab/ideas/issues/9).

## Stack

Rust core compiled to WebAssembly (all file logic), driven by a thin vanilla-TypeScript shell. WASM runs in Web Workers; PWA for offline; hosted on Cloudflare Pages. Minimal dependencies by design (smaller = more auditable).

Built by [MTC Lab](https://mtclab.net).

## Build and verify

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-pack`, and Node.js 22.

```sh
wasm-pack build core-rs --target web --out-dir ../app/src/wasm
cd app && npm ci && npm run build
cd .. && node scripts/check-local.mjs
```

The `../app/src/wasm` path is resolved by `wasm-pack` from the `core-rs` crate directory. `make build` runs this same pipeline. Cloudflare Pages should use `app/dist` as its output directory; deployment is intentionally handled through the owner's dashboard Git integration.

The CSP permits only same-origin connections because the origin serves static app assets and WASM—there is no server upload endpoint. `wasm-unsafe-eval` is the sole eval-class allowance and is required for WebAssembly instantiation.
