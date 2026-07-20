# Task: image ops in `core-rs` (image tools, slice IMG-1)

You are adding **image operations** to the shared `localbench_core` Rust crate. This is
the compute core for a new client-side tool (`img.keeplocal.tools`) in the keeplocal
tools family. Same hard rules as the existing PDF ops. **Additive only** — do NOT touch
or refactor the existing PDF functions, the wasm worker, or the app. Core Rust only.

## Non-negotiable constraints (read `core-rs/src/lib.rs` first, match its style)

1. **Pure-Rust, permissive-license codecs ONLY.** MIT/Apache. Must compile for
   `wasm32-unknown-unknown` (the crate already builds there). This means:
   - USE: `image` crate (default MIT/Apache, pure-Rust png/jpeg/gif/bmp/webp-decode via
     `miniz_oxide`/`fdeflate`/`zune`/`image-webp`), and the already-present
     `jpeg-encoder` for JPEG encode.
   - FORBIDDEN (C-linked or copyleft — would break wasm and the license floor):
     `mozjpeg`, `libwebp`/`webp` (C), `oxipng`/`libdeflater` (C deflate), `ravif`/`rav1e`
     if they pull C, anything AGPL. **When you turn on `image` features, disable threads
     — no `rayon`.** Set `default-features = false` and enable only the codec features you
     need (`png`, `jpeg`, `gif`, `bmp`, and webp-decode). Verify the dependency tree pulls
     no C by building for wasm32.
   - **If an operation cannot be done pure-Rust cleanly (e.g. WebP *encode*, or PNG
     lossless *optimization* beyond what the `png` crate's compression gives), DROP that
     operation from v1 and note it in a `## Deferred` section at the top of the module.
     Do NOT fake it, do NOT ship a C dep. Honest scope over feature count.**

2. **Never panic on malformed input.** Every public op is `fn op(...) -> Result<Vec<u8>, String>`
   internally (pure, native-testable — `String` error, NOT `JsValue`), with a thin
   `#[wasm_bindgen]` wrapper that maps `Err(String)` to `JsValue`. This is the exact
   pattern `page_count`/the wasm wrapper already use. Native `cargo test` must never
   reference `JsValue`.

3. **Decompression-bomb / resource guards.** Reuse the existing `MAX_DECODED_PIXELS`
   (64M) ceiling — reject images whose `width*height` exceeds it BEFORE allocating the
   full decode. Cap output dimensions with the existing `MAX_REENCODED_DIMENSION` where
   sensible.

4. **Determinism.** Same input bytes + same params → byte-identical output. No time, no
   RNG (the `getrandom` wasm shim stays link-only, never called).

5. **Metadata strip = a feature, not a side effect.** Every re-encode path MUST drop
   EXIF / XMP / ICC-beyond-needed / all ancillary metadata — especially **GPS location**.
   Decoding to raw pixels and re-encoding via `jpeg-encoder` / the `png` encoder already
   drops EXIF; make sure no path preserves it, and cover it with a test that feeds a
   JPEG containing an EXIF/GPS marker and asserts the output has no `Exif`/APP1 marker.
   This is the tool's headline privacy claim — treat it as load-bearing.

## Operations (v1)

Implement in a new module `core-rs/src/image_ops.rs` (declare `mod image_ops;` in lib.rs,
re-export the wasm wrappers). Detect input format from the bytes (`image::guess_format`),
never trust a caller-provided extension.

1. `resize_image(bytes, max_w, max_h, keep_aspect) -> Result<Vec<u8>,String>`
   - Decode, resize with a good filter (Lanczos3) so the longest side fits within
     `max_w`/`max_h`. If `keep_aspect`, preserve aspect ratio (fit inside the box);
     else stretch to exactly `max_w`×`max_h`. Never upscale beyond original unless the
     box is larger AND caller asked (default: never upscale — clamp to original size).
   - Re-encode to the **same** format as the input (jpeg→jpeg via jpeg-encoder at a high
     quality e.g. 90, png→png, etc.). Strip metadata.
   - Reject `max_w==0 || max_h==0` with a clear error.

2. `convert_image(bytes, target) -> Result<Vec<u8>,String>` where `target` ∈ {`"png"`,`"jpeg"`}
   (add `"webp"` ONLY if you can encode it pure-Rust cleanly; otherwise defer webp-out).
   - Decode → encode to target. JPEG target: flatten alpha onto white (JPEG has no alpha),
     document that. PNG target: preserve alpha. Strip metadata.

3. `compress_image(bytes, quality) -> Result<Vec<u8>,String>`, `quality` 1..=100.
   - JPEG input: re-encode via `jpeg-encoder` at `quality`.
   - PNG input: re-encode via the `png` encoder at max compression + strip metadata
     (this is modest but real; if it can't beat the original, see the no-larger rule).
   - **Never return a file larger than the input.** Re-parse the output to confirm it's a
     valid image, compare byte length, and if the result is not smaller, return the
     original bytes unchanged (mirror how `compress_pdf` already falls back). A test must
     prove the no-larger guarantee on an already-optimal input.

## Tests (native `cargo test`, in the module)

Construct small images in-test with the `image` crate (e.g. build an `RgbImage`, encode to
a `Vec<u8>`), so no fixture files are needed. Cover:
- resize downscale fits the box + preserves aspect; refuses 0 dims; never upscales.
- convert png→jpeg and jpeg→png produce a decodable image of the target format.
- compress jpeg at low quality is smaller and still decodable.
- no-larger guarantee: compressing an already-tiny/optimal image returns ≤ input length.
- garbage bytes → `Err`, never a panic.
- pixel-bomb: a header claiming huge dimensions is rejected by the `MAX_DECODED_PIXELS`
  guard before a huge allocation (use a format where you can assert this without actually
  allocating 64M px — or assert the guard via a unit check on the dimension math).
- **metadata strip: a JPEG carrying an EXIF/GPS APP1 marker → output contains no EXIF.**

## Gates you must leave green (run them, paste results)

- `cargo test --manifest-path core-rs/Cargo.toml` (all pass, incl. the new tests)
- `cargo build --manifest-path core-rs/Cargo.toml --target wasm32-unknown-unknown`
  (builds clean for wasm — proves no C dep crept in)
- `cargo tree -p localbench_core --target wasm32-unknown-unknown` — paste it; there must be
  NO C-sys crate (`*-sys`, `libdeflater`, `mozjpeg-sys`, `libwebp-sys`) in the tree.

Do NOT run wasm-pack or touch the app — that's a later slice. Leave the PDF code untouched.
Work on a branch `feat/img-core` off `main`. Commit with a clear message. Report exactly
which ops you shipped, which (if any) you deferred and why, and paste the three gate outputs.
