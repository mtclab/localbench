# localbench

**Client-side file tools. Your files never leave your device.**

Every operation runs in your browser over WebAssembly. No upload. No account. No ads. Free and open-source.

> Working codename. Real product name is a later branding decision.

## Why

The big file-tool sites (Smallpdf, iLovePDF, Adobe online, cloudconvert) all upload your file to their servers, then paywall or ad-gate the basic operation. The upload *is* their business model. localbench does the work in your browser instead - so your file physically never leaves your machine.

That promise is only worth anything if you can verify it, so:

- **Fully static, no backend.** Nothing to upload to.
- **Strict CSP** (`connect-src 'none'` after load) - the browser itself blocks any network call.
- **Works offline.** Install it, pull your network cable, it still works. That is the proof.
- **Open source.** Read the code.

## Status

V1 in planning. Anchor tool = **PDF ops** (merge / split / page-ops / compress). See [`docs/V1-SPEC.md`](docs/V1-SPEC.md).

Concept + wedge analysis: [mtclab/ideas #9](https://github.com/mtclab/ideas/issues/9).

## Stack

Rust core compiled to WebAssembly (all file logic), driven by a thin vanilla-TypeScript shell. WASM runs in Web Workers; PWA for offline; hosted on Cloudflare Pages. Minimal dependencies by design (smaller = more auditable).

Built by [MTC Lab](https://mtclab.net).
