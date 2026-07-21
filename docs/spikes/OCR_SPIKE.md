# Spike: client-side OCR (ocrs + rten → wasm) — Phase 0 feasibility

**Date:** 2026-07-21 · **Verdict: GO** (all four unknowns retired positively, independently
verified in a real browser on the staging box).

## Why a spike first
OCR is unlike the other keeplocal tools (deterministic byte-surgery). It bolts an ML runtime +
trained weights into the wasm, so three unknowns (size, speed, accuracy) could each sink a
fully-built tool. Phase 0 was a throwaway harness (`repot/ocr-spike/`, not committed to
localbench) that made those measurable before committing to a tool build.

## Engine
- **`ocrs` 0.12.2 + `rten` 0.24.0** (Robert Knight). Pure-Rust, **MIT/Apache-2.0**. Latin/English,
  self-described "early preview" accuracy. No tesseract-C — keeps the provable-local moat.
- Models (the "data source"): `text-detection.rten` 2.5 MB + `text-recognition.rten` 9.7 MB from
  `https://ocrs-models.s3-accelerate.amazonaws.com/`. Bundled **same-origin** (no runtime network),
  SW-precacheable → offline + zero-external preserved.

## Measured results (verified by overseer, staging box, Chromium)
| Question | Result |
|---|---|
| **Compile to wasm32?** | **YES** — `wasm-pack build` exits 0. rten configured `default-features=false, features=["rten_format"]`; simd128 via `.cargo/config.toml` `-C target-feature=+simd128`. getrandom `wasm_js` backend (same landmine localbench hit). |
| **Wasm size** | **2.1 MB** (`ocr_spike_core_bg.wasm`) — barely above localbench's ~1.7 MB; rten is compact. Total download ≈ 2.1 + 12.2 = **~14.3 MB** (one-time, SW-cached). |
| **Speed** | **~1.6–2.7 s** per image (simd128 on). Model-load 1.3 s cold / 115 ms warm. |
| **Accuracy** (English print) | clean rendered: exact. Small dense invoice: 100% tokens, one glyph slip (`EUR`→`FUR` once). Rotated low-contrast receipt JPEG: exact. |
| **Pure-Rust floor** | Held — no `-sys`/`cc`; only the `libc` type-stub (no C compiled). |

## API used
`rten::Model::load(bytes)` ×2 → `OcrEngine::new(OcrEngineParams { detection_model, recognition_model, .. })`
→ `image::load_from_memory().into_rgb8()` → `ImageSource::from_bytes` → `prepare_input` →
`OcrEngine::get_text()` (higher-level; joins lines with `\n`).

## Recommendation for the real tool (v1 scope)
- **Image → plain text** only. **NOT** scanned-PDF → text (that needs PDF→image render = the
  pdfium/mupdf C-gate we've avoided). Honest v1 = "OCR an image."
- **Run OCR in a Web Worker** (the spike ran it on the main thread and froze the UI for ~2.5 s —
  the tool must not). Progress/spinner while running.
- Frame honestly: "works best on clear printed English; early-preview accuracy — check the result"
  (also keeps clear of the no-liability filter — suggestion, not authority).
- Models as static `.rten` assets, SW-precached; the provable-local inspector still reads zero
  external. Lazy-load the ~14 MB only when the user actually OCRs (don't pay it on first paint).
- Searchable-PDF output (text layer over image) = explicitly v2.

## Landmines banked
- Chrome blocks port 5060 (SIP) → `ERR_UNSAFE_PORT`; serve the spike on another port (5062 used).
- OCR on the main thread freezes the page for the whole inference — Web Worker is mandatory for the tool.
- rten needs `simd128` target-feature for usable speed; without it, expect much slower.
