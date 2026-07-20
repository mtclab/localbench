# Task: metadata scrubber — `scrub_metadata` core op + `app-scrub/` app (slice META-1)

Build the fourth tool in the keeplocal.tools family: a **metadata scrubber** that
**shows you the hidden metadata in a file, then strips it**. Ships to
`scrub.keeplocal.tools`, Pages project `keeplocal-scrub`. Two parts in ONE branch
`feat/meta-scrub`: **Part A = core** (Rust ops), then **Part B = app** (the UI).
Do Part A fully green before starting Part B.

The wedge: photos/PDFs carry GPS coordinates, camera serials, author names, editing
history, "Adobe Photoshop" software tags — people share files not knowing this rides
along. Our tool makes it visible ("here is what was hiding in your file") and removes it,
100% in-browser. Incumbents (exiftool-online, metadata2go) upload your file. We don't.

## Non-negotiable design law: SCRUB IS LOSSLESS

This is NOT the image `compress`/`convert` op. Those decode+re-encode (they degrade the
photo — that's fine for compression, WRONG for a scrubber). The scrubber must remove ONLY
metadata and leave the actual image/document content intact:

- **JPEG**: walk the marker stream and DROP the metadata APP segments; copy every other
  segment (SOI, APP0/JFIF, DQT, SOF, DHT, SOS + entropy-coded data, EOI) **byte-for-byte**.
  Do not decode pixels. Do not re-encode.
- **PNG**: walk the chunk stream and DROP the metadata chunks; copy IHDR/PLTE/IDAT/IEND and
  rendering chunks **byte-for-byte** (IDAT is never touched — no recompression).
- **PDF**: lopdf load → remove the metadata dictionaries → `save_to`. (PDF can't be
  byte-surgical the way JPEG/PNG can; lopdf re-serialization is content-preserving — the
  same approach the existing `compress` op already uses and we already trust. Page count
  must be preserved.)

If you cannot strip a thing losslessly, LEAVE it and don't claim you removed it. Honesty
over coverage.

## Part A — core (`core-rs/src/metadata_ops.rs`, additive; do NOT touch `lib.rs` PDF/image code except to `mod`+`pub use` the two new fns)

**Zero new dependencies.** JPEG/PNG = hand-rolled byte parsing (model the marker walk on
the existing `is_baseline_jpeg` in `lib.rs`). PDF = the `lopdf` already in the tree. Do NOT
add `kamadak-exif` or any crate — the pure-Rust floor stays dependency-free for this slice.

Two wasm-bindgen exports, each a thin wrapper over a pure inner fn returning
`Result<_, String>` (native-testable; JsValue only at the boundary — same split as
`page_count`/`pdf_page_count`):

```rust
/// Describe the metadata found in a file, for display. Never mutates. Returns a
/// JSON string (serialize by hand with a small helper — do NOT add serde):
///   {"kind":"pdf"|"jpeg"|"png","items":[{"label":..,"detail":..|null,"sensitive":bool}, ...]}
/// Empty items => the file is already clean.
#[wasm_bindgen]
pub fn inspect_metadata(bytes: &[u8]) -> Result<String, JsValue>;

/// Return the same file with all metadata removed. Lossless for jpeg/png (segment/
/// chunk surgery); content-preserving for pdf (page count preserved). Validates the
/// output before returning it; never returns a broken file. If the input is already
/// clean, returning it unchanged (or a clean re-serialization for pdf) is correct.
#[wasm_bindgen]
pub fn scrub_metadata(bytes: &[u8]) -> Result<Vec<u8>, JsValue>;
```

**Format detection** (both fns): sniff bytes. `%PDF` prefix → pdf (confirm via
`lopdf::Document::load_mem`, and reject encrypted like the existing `load_pdf` guard does —
reuse it). JPEG magic `FF D8 FF` → jpeg. PNG magic `89 50 4E 47 0D 0A 1A 0A` → png.
Anything else → `Err("Metadata scrubbing supports PDF, JPEG, and PNG files.")`.
(GIF/BMP/WebP deferred — document in a module `//! Deferred` block like `image_ops.rs`.)

### What counts as metadata to strip / report, per format

**JPEG** — drop these APP/COM segments; keep everything else byte-for-byte:
- `APP1` (`FF E1`) whose payload starts `Exif\0\0` → **EXIF**. If it contains a GPS IFD
  (tag 0x8825 in IFD0), report `sensitive:true`, detail `"includes GPS location"`.
- `APP1` whose payload starts `http://ns.adobe.com/xap/` → **XMP** (author/edit history).
- `APP13` (`FF ED`) `Photoshop 3.0` → **IPTC / Photoshop** (creator, location names).
- `COM` (`FF FE`) → **JPEG comment**; detail = the comment text (utf8-lossy, truncate long).
- KEEP `APP0` JFIF (needed to decode) and `APP2` ICC (color profile — not PII; stripping it
  shifts rendered color). Note this scope in the module doc.
- Detail for kept-but-reported EXIF/XMP/IPTC = `"<n> bytes"` is fine.
Marker walk rules (mirror `is_baseline_jpeg`): after SOI, each marker is `FF xx` + 2-byte
big-endian length (except standalone RSTn/SOI/EOI/TEM). Stop copying metadata decisions at
`SOS` (`FF DA`) — after SOS is entropy-coded scan data; copy the rest of the file verbatim to
EOI. Preserve original segment ORDER for everything kept.

**PNG** — drop these ancillary chunks; keep IHDR/PLTE/IDAT/IEND + rendering chunks
(tRNS, gAMA, cHRM, sRGB, iCCP, bKGD, pHYs, sBIT, hIST, sPLT) byte-for-byte:
- `tEXt` / `zTXt` / `iTXt` → **Text** metadata. These are literally `keyword\0value` — decode
  and report `label:"Text: <keyword>"`, `detail:"<value>"` (inflate zTXt/iTXt if trivial; if
  compressed-value decode is non-trivial, report the keyword + `"(compressed)"` and still
  DROP the chunk). `sensitive:true` if keyword is `XML:com.adobe.xmp` or contains "GPS".
- `eXIf` → **EXIF** (+ GPS detection same as jpeg → sensitive).
- `tIME` → **Last-modified time**; detail = the timestamp.
Chunk walk: 8-byte signature, then repeating `[4-byte big-endian length][4-byte type][data]
[4-byte CRC]`. Copy kept chunks verbatim (their CRC is still valid). Do NOT recompute IDAT.

**PDF** — remove and report:
- trailer `/Info` dict → report each present key (Title, Author, Subject, Keywords, Creator,
  Producer, CreationDate, ModDate) as an item with its string value as detail;
  Author/Creator/Producer/GPS-ish → `sensitive:true` for Author.
- catalog `/Metadata` (XMP stream) and any `/Metadata` on objects → **XMP metadata**; remove
  (exactly what `compress` already does: `trailer.remove(b"Info")`, `trailer.remove(b"Metadata")`,
  strip `/Metadata` off dicts+streams). Report presence.
- After removal: `prune_objects()` then `save_to`. Validate: re-parse + page count unchanged,
  else `Err`.

### Validity guards (scrub)
- jpeg/png: after surgery, re-sniff the magic + confirm `image::guess_format` still detects the
  same format (cheap sanity that you didn't corrupt the stream). Dimensions unchanged.
- pdf: re-parse, page count preserved.
- If a guard fails → return `Err(...)`, never a broken file.

### Native tests (add to `metadata_ops.rs #[cfg(test)]`, ~10+):
- JPEG: build a baseline jpeg fixture (reuse the jpeg-encoder pattern from `image_ops` tests),
  splice a synthetic `APP1 Exif\0\0...GPS...` segment in (like the existing
  `reencoding_removes_exif_gps_app1_metadata` test does) → `inspect_metadata` reports EXIF w/
  GPS sensitive; `scrub_metadata` output has NO `FF E1` / `Exif\0\0`, still decodes, same
  dimensions, and **the entropy/scan bytes are byte-identical to the clean fixture** (prove
  lossless — not a re-encode).
- PNG: build a png fixture, inject a `tEXt` chunk `Software\0Adobe` + an `eXIf` chunk →
  inspect reports both; scrub removes them, IDAT bytes byte-identical, still decodes.
- PDF: reuse the Info+Metadata fixture shape from the `compress` test → inspect lists Info
  keys + XMP; scrub removes Info+Metadata, page count preserved.
- Already-clean file of each type → inspect items empty; scrub returns a still-valid file.
- Garbage/empty/truncated for each type → `Err`, no panic.
- Unsupported (a gif fixture) → `Err` with the supported-formats message.

### Part A gates (run + paste):
- `cargo test --manifest-path core-rs/Cargo.toml` (all green, old + new)
- `cargo tree --manifest-path core-rs/Cargo.toml | grep -Ei '\-sys|cc |bindgen|libwebp|mozjpeg' | grep -v 'js-sys'` → empty (no C deps added)
- `wasm-pack build core-rs --target web --out-dir ../app-scrub/src/wasm` (clean)

## Part B — app (`app-scrub/`, cloned from `app-img/` — the freshest shell)

**`app-img/` is your template. Do NOT modify `app/` or `app-img/`.** Copy the scaffold and
shell verbatim, swap the image tooling for the scrubber. Same header/brand/theme-toggle, the
same **`local-badge` + `local-inspector` dialog** (keep verbatim — the provable-local wedge),
same drop-zone, same service-worker + PWA + SEO scaffolding, same a11y bar (keyboard nav,
`aria-live`, focus-visible, dialog focus-trap, `#hash` deep-link, per-view `<title>`).

Copy from `app-img/`, adapting only what's noted:
- `package.json` → name `localbench-scrub-app`; scripts identical
  (`"build": "tsc && vite build && node ../scripts/generate-sw.mjs dist"`).
- `vite.config.ts`, `tsconfig.json` → as-is (worker `format:"es"` required — top-level `await`).
- `src/tokens.css`, `src/style.css` → copy as-is (shared tokens; do not diverge).
- `public/_headers` → **verbatim** (strict CSP incl `img-src 'self' data: blob:` — the
  scrubber previews images too; do not weaken it). Same CSP `<meta>` in index.html.
- `public/icons/*`, `public/manifest.webmanifest` → copy; update name/short_name/description.
- `public/robots.txt`, `public/sitemap.xml` → copy; URL `https://scrub.keeplocal.tools/`.
- `index.html` → copy shell; swap `<title>`/meta/OG/JSON-LD/canonical to
  `https://scrub.keeplocal.tools/`; replace the tool panels with the single scrubber flow.
- `src/wasm/.gitignore` → mirror app-img's (built wasm NOT committed).

Build wasm into `app-scrub/src/wasm`:
`wasm-pack build core-rs --target web --out-dir ../app-scrub/src/wasm`.

### Worker protocol (`app-scrub/src/core.worker.ts`) — use EXACTLY this shape
Mirror `app-img/src/core.worker.ts` (module worker, `await init()`, ready/result/error,
transferables, the `runCoreRequest` id-counter+30s-timeout helper in main.ts). `inspect`
returns a JSON string, not bytes:

```ts
type WorkerRequest =
  | { id: number; type: "inspect"; bytes: ArrayBuffer }
  | { id: number; type: "scrub"; bytes: ArrayBuffer };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "inspected"; id: number; report: string }   // JSON string from inspect_metadata
  | { type: "result"; id: number; bytes: ArrayBuffer }   // scrubbed file
  | { type: "error"; id?: number; message: string };
```
Imports: `inspect_metadata, scrub_metadata, core_version`. For `scrub`, transfer
`result.slice().buffer` exactly like the image worker.

### The single scrubber flow (one tool, not a 3-way switcher — but keep the shell)
Accept `.pdf, .jpg/.jpeg, .png` (filter the picker; core still sniffs bytes — don't trust the
extension). On drop/select:
1. Show a preview thumbnail for images (object URL, revoke on replace; blob: is CSP-allowed).
   PDFs: show a file chip (name + size), no preview.
2. **Auto-inspect** → render the metadata report as a list. Each item: label, detail (if any).
   Flag `sensitive:true` items visibly (e.g. a warning pill "GPS location", tinted row) — this
   is the "look what was hiding" moment, make it land. If items empty → a clean state:
   "No metadata found — this file is already clean." (still allow Download of the original).
3. A primary **"Remove all metadata"** button → `scrub` → on success:
   - show before → after size (usually a small reduction; never claim big savings),
   - state exactly what was removed (summarize the item labels: "Removed: EXIF (GPS), XMP,
     Photoshop block"),
   - re-inspect the RESULT and show it now reports clean (proof the strip worked),
   - a **Download** button (Blob w/ correct MIME: application/pdf, image/jpeg, image/png;
     filename = input basename + `-clean` + original extension; revoke object URL).
4. Errors (encrypted PDF, unsupported type, garbage) → surfaced cleanly in the `aria-live`
   status, no dead-end.

Surface the wedge in copy: "Your file is inspected and cleaned in this tab. Nothing is
uploaded." Keep it honest — ICC color profile and JFIF are preserved (not PII); say the tool
removes EXIF/GPS, XMP, IPTC/Photoshop, comments, PNG text chunks, and PDF document info +
XMP.

### Part B gates (run + paste):
- `npm --prefix app-scrub install && npm --prefix app-scrub run build` (tsc + vite + SW gen, no errors)
- `node scripts/check-local.mjs app-scrub/dist` — **must PASS** (zero external origins, CSP
  locked to self). Add `scrub.keeplocal.tools` inert-URL masks in `maskExpectedInertUrls`
  (canonical/og:url/JSON-LD/sitemap/robots) the SAME way the `img.keeplocal.tools` masks are
  done — add BOTH the regex-match branch AND the strip-target string, mirroring the existing
  structure exactly. Do NOT weaken the guard to pass; if a real external URL appears, remove it.

## Deliverable / report
Commit everything to `feat/meta-scrub`. In your final report: list files created/changed, paste
ALL gate outputs (Part A + Part B), state the real before/after byte numbers on a JPEG-with-GPS
and a PDF-with-Info if you exercised them, and note any deviation. Do NOT modify `app/`,
`app-img/`, the existing PDF/image core fns, the deploy, or any CF resource. Do NOT create CI.
```
```
