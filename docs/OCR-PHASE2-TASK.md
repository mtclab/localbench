# Task: OCR Phase 2 — searchable PDF output (on `feat/ocr`)

Add a second output mode to the OCR tool: besides "Extracted text" (Phase 1), let the user produce
a **Searchable PDF** — the image with an invisible, selectable/searchable text layer over each
recognized word. Continue on branch `feat/ocr`. Feasibility is proven — see
`docs/spikes/OCR_SPIKE.md` "Phase-2 probe" for the exact ocrs word-rect API and the approach; the
working native probe is `repot/ocr-spike/core/src/bin/searchable.rs` (reference implementation to
lift the PDF-building + ocrs-low-level logic from).

## Part A — `core-ocr` (add to the ISOLATED crate; do NOT touch core-rs or other apps)
Add `lopdf` (already used elsewhere in the repo; add it to `core-ocr/Cargo.toml`,
`default-features = false`) and a new export:
```rust
/// Build a searchable PDF from one image: a single page showing the image with an
/// invisible Helvetica text layer positioned over each recognized word. Requires the
/// engine to be loaded (same as run_ocr). Deterministic; no /Info; metadata-clean.
#[wasm_bindgen] pub fn searchable_pdf(image: &[u8]) -> Result<Vec<u8>, JsValue>;
```
Implementation (lift from the probe, harden for wasm):
- OCR with the low-level API for **word rects**: `detect_words` → `find_text_lines` →
  `recognize_text` → iterate `TextLine::words()` → `TextItem::bounding_rect()` (`use ocrs::TextItem`)
  → `rten_imageproc::Rect<i32>` (`.left()/.bottom()/.width()/.height()`).
- Build the PDF with `lopdf`:
  - Decode the image (`image` crate) to RGB8; embed as one Image XObject, `/Filter /FlateDecode`
    (compress raw RGB with the `flate2` already pulled in), `/ColorSpace /DeviceRGB`,
    `/BitsPerComponent 8`, `/Width`/`/Height`. (Always-FlateDecode-RGB is the simple correct path;
    decoding inherently drops image metadata. JPEG-DCTDecode passthrough is a later polish — NOT
    required now.)
  - Page MediaBox = image px as points (1px = 1pt). Content stream draws the image full-page
    (`q W 0 0 H 0 0 cm /Im0 Do Q`), then the **invisible text layer**: for each word,
    `BT 3 Tr /F0 <size> Tf <x> <y> Td (<escaped text>) Tj ET` where x = rect.left(),
    y = pageHeight − rect.bottom(), size ≈ rect.height() (clamp to a sane min). Escape `(` `)` `\`
    in the text; skip empty words.
  - Resources: `/Font << /F0 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>` (base-14,
    no embedding) + `/XObject << /Im0 … >>`.
  - No `/Info`, deterministic object ids. Guard decoded pixels before allocating (reuse the
    Phase-1 cap). Return `Err` (never panic) on undecodable image or engine-not-ready.
- Add a native test if feasible (engine load is heavy; at least test `searchable_pdf` returns Err
  before the engine is loaded, and that the produced bytes start with `%PDF` on a tiny fixture if
  you can construct one without the models).

Rebuild wasm: `wasm-pack build core-ocr --target web --out-dir ../app-ocr/src/wasm`.

## Part B — `app-ocr` (add an output-mode toggle; keep Phase-1 text mode intact)
- Add an output-mode control (segmented buttons / radios, keyboard-navigable, `aria`): **"Extracted
  text"** (default, the existing Phase-1 flow) vs **"Searchable PDF"**.
- Text mode: unchanged (textarea + Copy + Download .txt).
- Searchable-PDF mode: the primary button becomes **"Create searchable PDF"**; on click (engine
  lazy-loaded same as text mode) → worker builds the PDF → a **Download** button for
  `<basename>-searchable.pdf` (`application/pdf`). Show a one-line note: "The image with a hidden,
  selectable text layer — search or copy the text from any PDF viewer." No dead-ends; errors in the
  `aria-live` status.
- Worker (`core.worker.ts`): add a request `{ id; type: "searchablePdf"; image: ArrayBuffer }` →
  `searchable_pdf(new Uint8Array(image))` → respond `{ type: "pdf"; id; bytes: ArrayBuffer }`
  (transfer the buffer). Import `searchable_pdf`. Keep OCR OFF the main thread (worker owns it).
- Object-URL lifecycle: revoke the PDF blob URL on replace/new-run, like the .txt download.
- Keep the badge/inspector, honest framing ("early-preview OCR — check the result"), a11y bar,
  strict CSP (models same-origin; nothing new external).

## Gates (run + paste)
- `cargo test --manifest-path core-ocr/Cargo.toml` + `cargo tree ... | grep -Ei '\-sys|cc '`
  (no C beyond libc stub).
- `wasm-pack build core-ocr --target web --out-dir ../app-ocr/src/wasm` (clean) + wasm size.
- `npm --prefix app-ocr install && npm --prefix app-ocr run build` (clean).
- `node scripts/check-local.mjs app-ocr/dist` — **must PASS** (no new external; guard unchanged).

## Report
Commit to `feat/ocr`. Final report: files changed, gate outputs, wasm size delta, and how to
drive both output modes for the overseer's staging verification (the overseer will confirm the
searchable PDF's text layer with `pdftotext`). Do NOT touch other crates/apps, the deploy, or CF.
