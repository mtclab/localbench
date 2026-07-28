# keeplocal

**Client-side file tools. Your files never leave your device.**

Every operation runs in your browser over WebAssembly. No upload. No account. No ads. Free and open-source.

Live at [keeplocal.tools](https://keeplocal.tools).

> **Naming.** The product is **keeplocal** — the domain people type, and the only name that
> appears in any user-facing surface. **localbench** is the repo and engine codename; it stays in
> the GitHub URL, the crate names and the build artifacts, where it is invisible to users. Same
> pattern as euwatch → Vigla elsewhere in the estate. Do not reintroduce "localbench" into UI copy.

## Why

The big file-tool sites (Smallpdf, iLovePDF, Adobe online, cloudconvert) all upload your file to their servers, then paywall or ad-gate the basic operation. The upload *is* their business model. keeplocal does the work in your browser instead - so your file physically never leaves your machine.

That promise is only worth anything if you can verify it, so:

- **Fully static, no backend.** Nothing to upload to.
- **Strict CSP** (`connect-src 'self'`) - WASM and app assets can load only from the same static origin, while external exfiltration is blocked.
- **Works offline.** Install it, pull your network cable, it still works. That is the proof.
- **Open source.** Read the code.
- **A live receipt.** Every surface shows readings taken in your own browser - external requests since load, the active connection policy, whether a service worker controls the page. A tool whose model requires an upload cannot print a zero there.

## Status

S0 proves the Rust → WASM → worker → UI pipeline with a local PDF page counter. The later V1 anchor remains **PDF ops** (merge / split / page-ops / compress). See [`docs/V1-SPEC.md`](docs/V1-SPEC.md).

Concept + wedge analysis: [mtclab/ideas #9](https://github.com/mtclab/ideas/issues/9).

## Stack

Rust core compiled to WebAssembly (all file logic), driven by a thin vanilla-TypeScript shell. WASM runs in Web Workers; PWA for offline; hosted on Cloudflare Pages. Minimal dependencies by design (smaller = more auditable).

Built by [MTC Lab](https://mtclab.net).

## Layout

| Path | What it is | Deploys to |
|---|---|---|
| `site/` | The landing page (static, no bundler) | `keeplocal.tools` |
| `app/` | PDF tools | `pdf.keeplocal.tools` |
| `app-img/` | Image tools | `img.keeplocal.tools` |
| `app-scrub/` | Metadata scrubber | `scrub.keeplocal.tools` |
| `app-zip/` | Archive tool | `zip.keeplocal.tools` |
| `app-img2pdf/` | Images to PDF | `img2pdf.keeplocal.tools` |
| `core-rs/` | The Rust core, compiled to WASM | — |
| `shared/identity.css` | **The design system. One file, every surface.** | — |

### Notices: the honesty layer

Some things a tool does to a file cannot be seen in the file. An animation flattened to one frame,
16-bit colour rounded to 8, a document handed back untouched because compressing it would have made
it worse — all of those look like success from the outside.

So the Rust core returns a `FileResult`: the bytes, plus `notices` (sentences written for the user)
and `notice_codes` (stable identifiers). **The core never changes a file in a way a user would not
expect without returning a notice saying so**, and the interface shows those sentences verbatim —
it owns the frame, never the wording.

Two codes, `pdf-returned-unchanged` and `image-returned-unchanged`, mean the advertised work did not
happen to the file that was just downloaded. Those escalate the whole result panel and repeat in the
status strip under the button, because the case being guarded against is someone who downloads and
leaves.

That contract is gated, not trusted:

| Gate | What it forbids | Where it runs |
|---|---|---|
| `make check-notices` | a `FileResult` operation whose notices are not forwarded, an answer rendered without them, a fixture that stopped provoking the core | anywhere |
| `scripts/notice-proof.mjs` | a sentence the core returned that is not on the screen | the staging box, real browser, built `dist` |

The runtime gate does not trust the interface to report on itself: it wraps `Worker` before any app
code runs, records what the core actually returned, and then requires every one of those sentences
to appear in the page's visible text.

### One stylesheet, not six

`shared/identity.css` holds every token, the type scale, the shell (masthead, receipt, footer)
and every shared component. Each app's `src/style.css` is a one-line `@import` of it, so a colour
or type change is made **once**. Vite inlines the import at build time, so each `dist` still ships
a single stylesheet and no runtime `@import` request. The landing links the same file, copied in
by `scripts/build-site.mjs`.

App-local rules go *below* the import in that app's `src/style.css`. Do not fork the shared file.

## Build and verify

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-pack`, and Node.js 22.

```sh
wasm-pack build core-rs --target web --out-dir ../app/src/wasm
cd app && npm ci && npm run build
cd .. && node scripts/check-local.mjs
```

The `../app/src/wasm` path is resolved by `wasm-pack` from the `core-rs` crate directory. `make build` runs this same pipeline. Cloudflare Pages should use `app/dist` as its output directory; deployment is intentionally handled through the owner's dashboard Git integration.

The CSP permits only same-origin connections because the origin serves static app assets and WASM—there is no server upload endpoint. `wasm-unsafe-eval` is the sole eval-class allowance and is required for WebAssembly instantiation.

## Deploying the landing page

`site/` has no bundler, but it must not carry its own copy of the design system. `scripts/build-site.mjs`
assembles `site/dist` by copying the page sources plus the canonical `shared/identity.css`:

```sh
node scripts/build-site.mjs                    # or: make site
npx wrangler pages deploy site/dist --project-name=keeplocal-site
```

`keeplocal-site` is an existing **direct-upload** Pages project, so this needs no dashboard change —
but note it is direct-upload, *not* Git-integrated like the tool apps: pushing to `main` does **not**
redeploy the landing. Someone has to run the two commands above. Always rebuild `site/dist` first;
deploying a stale `dist` will ship an old copy of `identity.css` and split the brand again.

Per the estate deploy rule, verification runs on the staging box, never on the workspace, and
production deploys happen only from a clean `main` that matches origin.
