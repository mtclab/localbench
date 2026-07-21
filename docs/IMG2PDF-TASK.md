# Task: images → PDF — `images_to_pdf` core + `app-img2pdf/` app (slice IMG2PDF-1)

Sixth tool in the keeplocal.tools family: combine JPG/PNG/… images into a single PDF, 100%
in-browser. Ships to `img2pdf.keeplocal.tools`, Pages project `keeplocal-img2pdf`. ONE branch
`feat/img2pdf`: **Part A = core**, then **Part B = app**. Do Part A fully green before Part B.

The wedge: "jpg to pdf" is one of the highest-volume file-tool queries; every online converter
(ilovepdf, smallpdf, img2pdf.org) uploads your images first. We build the PDF in the tab. And
because we already strip metadata, embedded photos are **EXIF/GPS-cleaned on the way in**.

## Non-negotiable floors
- **Pure-Rust only.** Build the PDF with `lopdf` (already in the tree). Embed images with the
  `image` crate (already in the tree) + `flate2` (already in the tree). NO new C deps; the no-C
  `cargo tree` gate must stay empty. NO new crates unless truly needed — you should not need any.
- **Deterministic + metadata-clean output.** No `/Info`, no timestamps, no `getrandom`/clock —
  same images in the same order → byte-identical PDF. Every embedded image carries **zero**
  metadata (see EXIF handling below). Coherent with the scrubber's ethos.
- Do NOT touch `app/`, `app-img/`, `app-scrub/`, `app-zip/`, existing core fns, the deploy, or
  any CF resource. Do NOT create CI.

## Part A — core (`core-rs/src/imagepdf_ops.rs`, additive; `lib.rs` gets `mod`+`pub use`)

```rust
/// Build a one-image-per-page PDF from the images in order. `page` is the page
/// sizing mode: "fit" (page == image size), "a4", or "letter". Deterministic.
#[wasm_bindgen]
pub fn images_to_pdf(buffers: js_sys::Array, page: &str) -> Result<Vec<u8>, JsValue>;
// inner: build_images_pdf(images: Vec<Vec<u8>>, page: PageMode) -> Result<Vec<u8>, String>
```
`buffers` = a `js_sys::Array` of `Uint8Array` (each an image), same shape as `merge_pdfs`.

### Per-image embedding (pure-Rust, lossless where possible)
For each image, produce one PDF page whose only content is the image drawn to fill the image
area. Build an Image XObject:

1. **Baseline JPEG, 1 or 3 components** (detect via the existing `is_baseline_jpeg` in `lib.rs`
   — make it `pub(crate)`): embed the JPEG stream DIRECTLY as `/Filter /DCTDecode` (no decode,
   no re-encode — lossless passthrough, smallest output). **First strip its metadata**: reuse
   the scrubber's JPEG marker-class stripper — add `pub(crate) fn strip_jpeg_metadata(bytes:
   &[u8]) -> Vec<u8>` to `metadata_ops.rs` (wrap `walk_jpeg(bytes, true)`, return the stripped
   bytes; on any Err return the original bytes unchanged) and call it before embedding, so the
   embedded stream has no EXIF/XMP/Photoshop/comment. Parse the SOF0 segment for width, height,
   and component count → `/Width`, `/Height`, `/BitsPerComponent 8`, `/ColorSpace` = DeviceGray
   (1) or DeviceRGB (3).
2. **Everything else** (PNG/GIF/BMP/WebP, or a JPEG that isn't baseline-1/3): decode with the
   `image` crate to RGB8 (flatten any alpha onto white, exactly like `image_ops`
   `flatten_alpha_onto_white`), and embed the raw RGB bytes as `/Filter /FlateDecode` (compress
   with `flate2`) `/ColorSpace /DeviceRGB` `/BitsPerComponent 8` + `/Width`/`/Height`. Lossless;
   metadata is inherently dropped by decoding.

Guard dimensions before decoding (reuse `MAX_DECODED_PIXELS`); cap the number of images and the
total input bytes (add a `MAX_IMAGES: usize = 500` and reuse a total-size guard). An
unsupported/undecodable image → `Err` naming the image position (e.g. "Image 3 could not be
read: …"). Empty set → `Err("Choose at least one image to combine.")`.

### Page geometry (PDF points; 1px = 1pt at 72 dpi)
- `PageMode::Fit`: MediaBox = `[0 0 w h]` where w,h = image pixels as points. Draw the image
  filling the whole page: content stream `q\n{w} 0 0 {h} 0 0 cm\n/Im0 Do\nQ`.
- `PageMode::A4` (595×842) / `PageMode::Letter` (612×792): fixed MediaBox; scale the image to
  fit inside a 36pt margin preserving aspect ratio, centered. Compute draw width/height + x/y
  offset; content stream `q\n{dw} 0 0 {dh} {ox} {oy} cm\n/Im0 Do\nQ`.
- Unknown `page` string → default to "fit" (or Err — your call, document it).

Assemble a flat page tree (Catalog → Pages → N Page objects), each Page with its MediaBox,
`/Resources << /XObject << /Im0 <img> >> >>`, and `/Contents`. `save_to` → bytes. Do NOT set
`/Info`. Validate before returning: re-parse with `lopdf`, assert page count == image count.

### Native tests (`#[cfg(test)]`, ~10+):
- two images (a JPEG fixture + a PNG fixture, built like the `image_ops`/`metadata_ops` tests) →
  `build_images_pdf` → re-parse: 2 pages; each page has an Image XObject with the right
  Width/Height; the JPEG page's XObject `/Filter` is `DCTDecode`, the PNG page's is `FlateDecode`.
- **EXIF strip**: a JPEG fixture with an injected `APP1 Exif\0\0…GPS…` segment → the output PDF
  bytes do NOT contain `Exif\0\0` (metadata stripped before embed).
- baseline-JPEG passthrough is lossless: the embedded DCTDecode stream equals the
  metadata-stripped input JPEG scan (byte-for-byte content, not re-encoded).
- determinism: same inputs → byte-identical PDF twice (no clock/Info).
- page modes: "fit" → MediaBox equals image px; "a4" → MediaBox 595×842 and the image is scaled
  within margins (draw dims ≤ page − margins, aspect preserved).
- empty set → Err; an undecodable/garbage image → Err naming its position; a non-image blob →
  Err. No panics on any of these.

### Part A gates (run + paste):
- `cargo test --manifest-path core-rs/Cargo.toml` (all green, old + new)
- `cargo tree --manifest-path core-rs/Cargo.toml --prefix none | grep -Ei '^(cc|bindgen|[^[:space:]]+-sys) v' | grep -v '^js-sys '` → **empty**
- `wasm-pack build core-rs --target web --out-dir ../app-img2pdf/src/wasm` (clean)

## Part B — app (`app-img2pdf/`, cloned from `app-zip/` — its multi-file "Create" flow is the closest template)

**`app-zip/` is your template. Do NOT modify the other apps.** Copy the scaffold+shell verbatim;
same header/brand/theme-toggle, the **`local-badge` + `local-inspector` dialog** (verbatim — the
wedge), drop-zone, SW + PWA + SEO + a11y bar. Copy exactly as `app-zip` was cloned from `app-img`:
- `package.json` → name `localbench-img2pdf-app`; scripts identical.
- `vite.config.ts`, `tsconfig.json`, `src/tokens.css`, `src/style.css` → as-is.
- `public/_headers` → **verbatim** (strict CSP; `img-src 'self' data: blob:` already present —
  needed for the image thumbnails).
- `public/icons/*`, `public/manifest.webmanifest` → copy; update name/short_name/description.
- `public/robots.txt`, `public/sitemap.xml` → copy; URL `https://img2pdf.keeplocal.tools/`.
- `index.html` → swap `<title>`/meta/OG/JSON-LD/canonical to `https://img2pdf.keeplocal.tools/`;
  single-tool panel (no switcher needed — one tool). Keep the badge/inspector.
- `src/wasm/.gitignore` → mirror. Build wasm: `wasm-pack build core-rs --target web --out-dir
  ../app-img2pdf/src/wasm`.

### Worker protocol (`app-img2pdf/src/core.worker.ts`) — EXACTLY this shape
Mirror `app-zip/src/core.worker.ts` (module worker, `await init()`, transferables,
`runCoreRequest` id+30s-timeout helper in main.ts). Build the `buffers` `js_sys::Array` of
`Uint8Array` in the worker from the transferred ArrayBuffers.
```ts
type WorkerRequest =
  | { id: number; type: "build"; buffers: ArrayBuffer[]; page: "fit" | "a4" | "letter" };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "built"; id: number; bytes: ArrayBuffer }   // the PDF
  | { type: "error"; id?: number; message: string };
```
Imports: `images_to_pdf, core_version`. Transfer the result buffer out.

### The single flow (mirror app-zip's "Create" list UX)
- Multi-image drop zone + picker (accept image/jpeg,image/png,image/gif,image/bmp,image/webp).
- Show the chosen images as an ordered list with a **thumbnail** (object URL, revoke on
  remove/replace; blob: is CSP-allowed), name, size, a remove button, and **reorder** controls
  (Up/Down buttons — the PDF page order follows this list). Running count + total size.
- A page-size selector: **Match image size** (fit, default) / A4 / Letter.
- Button "Create PDF". On success: show the PDF size, note "photos are EXIF/GPS-cleaned and no
  timestamps are stored", and a Download button (`combined.pdf`, `application/pdf`).
- Empty list → button disabled. Core errors (bad image, too many) → surfaced in `aria-live`.

### Part B gates (run + paste):
- `npm --prefix app-img2pdf install && npm --prefix app-img2pdf run build` (tsc+vite+SW, clean)
- `node scripts/check-local.mjs app-img2pdf/dist` — **must PASS**. Add `img2pdf.keeplocal.tools`
  inert-URL masks in `maskExpectedInertUrls` the SAME way the existing `img.keeplocal.tools`
  masks are written (BOTH the regex branch AND the strip-target string). Do NOT weaken the guard.

## Deliverable / report
Commit to `feat/img2pdf`. Final report: files created/changed, ALL gate outputs (Part A + B), a
real example (N images in → PDF pages/size, and confirm EXIF was stripped from a GPS JPEG), and
any deviation. Do NOT modify the other apps, the deploy, or any CF resource. Do NOT create CI.
