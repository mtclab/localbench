# localbench OCR

Private image-to-text OCR for `https://ocr.keeplocal.tools/`. The image is decoded and recognized
inside a Web Worker by the separate pure-Rust `core-ocr` WASM crate; image bytes never leave the
tab.

The OCR models are not committed to this public repository. Fetch them before building:

```sh
bash scripts/fetch-ocr-models.sh
wasm-pack build core-ocr --target web --out-dir ../app-ocr/src/wasm
npm --prefix app-ocr install
npm --prefix app-ocr run build
```

The detection and recognition models come from the `ocrs` project and are licensed MIT or
Apache-2.0. They are served as same-origin static assets. The OCR engine downloads once on first
use, then works offline through a cache-first service-worker runtime cache; models are deliberately
not part of the install-time app-shell precache.

For local or overseer staging after building:

```sh
npm --prefix app-ocr run preview -- --host 0.0.0.0 --port 5062
```

Open `/\#ocr`, choose an image, and keep interacting with the theme toggle or privacy inspector
while OCR runs to verify that recognition is off the main thread.
