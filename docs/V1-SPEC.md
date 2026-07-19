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
- **Static SPA**, no backend. Hosted on **CF Pages** (free, global, our stack).
- **Vite + TypeScript**, minimal deps (fewer deps = more auditable = smaller attack surface for the privacy claim).
- **Heavy work in Web Workers** so the UI never freezes on a big file; WASM cores loaded in-worker.
- **PWA** (service worker, offline, installable).
- Coherent shell (shared tokens/nav/theme, light+dark, AA) - QoL/UX standing rule. One home, tools slot into it; no dead-ends.

### PDF engine choices (resolve in the codec spike - do not pre-commit)
- **Merge / split / reorder / rotate / delete**: `pdf-lib` (pure JS, no wasm, mature) handles all of these client-side today. Low risk.
- **Compress** is the hard part (real image re-encode/downsample). Candidates to spike:
  - `mupdf.wasm` (Artifex, AGPL - license check REQUIRED before adopting; AGPL on a hosted app has obligations).
  - `pdfium` wasm builds (BSD) - render/rasterize path.
  - Manual path: parse with pdf-lib, extract images, re-encode via canvas/`@jsquash` (webp/mozjpeg wasm), rewrite. Most control, most work.
  - Ghostscript.wasm (AGPL - same license flag).
  - **Spike deliverable**: a compression path with an acceptable license (prefer permissive; AGPL only with owner sign-off), a size-reduction benchmark on a corpus of real PDFs, and a quality floor. This is the one genuine technical risk in V1.

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

## Build slices (proposed - for owner review, not started)
- **S0 - scaffold**: Vite+TS+PWA skeleton, coherent shell (nav/theme/tokens), CSP `connect-src 'none'`, empty tool routes, CF Pages deploy of a "hello" page. Proves the provable-local frame first.
- **S1 - merge**: drag-drop multi-PDF, reorder, merge, download. pdf-lib in a worker. First real tool end-to-end.
- **S2 - split + page ops**: page-grid view, extract ranges, rotate/delete/reorder, export.
- **S3 - compress (spike + build)**: resolve the codec/license question, ship compression with a benchmarked size/quality result.
- **S4 - polish + launch**: offline/PWA hardening, provable-local badge + inspector, SEO (per-op landing pages), a11y AA pass, real-device walk.

## Open questions for owner
1. Compression license posture: hard-permissive-only, or AGPL acceptable for a compress core (we would owe source, which is fine since we are public - but AGPL is viral on any linked server code; V1 has none)?
2. Framework: vanilla TS + tiny helpers, or a light framework (Preact/Solid)? Lean vanilla for auditability + bundle size.
3. Deploy: CF Pages Git-integration (auto on push) vs wrangler manual. Public repo so Actions is free either way.
