# Task: archive tool — `create_zip` / `list_zip` / `extract_zip_entry` core + `app-zip/` app (slice ZIP-1)

Build the fifth tool in the keeplocal.tools family: an **archive tool** — zip files up, or open
a .zip and pull files out, 100% in-browser. Ships to `zip.keeplocal.tools`, Pages project
`keeplocal-zip`. ONE branch `feat/zip`: **Part A = core** (Rust ops), then **Part B = app**.
Do Part A fully green before starting Part B.

The wedge: people zip up folders of sensitive documents to send them; every online zipper
(ezyzip, files2zip, extract.me) uploads the files to a server first. We do it in the tab. The
same "your files never leave your device" promise, now for archives.

## Non-negotiable floors
- **Pure-Rust codecs only.** Add the `zip` crate configured for **Store + Deflate only**
  (Deflate via `flate2`'s pure-Rust `miniz_oxide` backend). **NO** `bzip2`, `zstd`,
  `deflate-zlib`/zlib-ng (C), `aes-crypto`, or any `*-sys`. Prove it: the no-C `cargo tree`
  gate below MUST stay empty. `zip` + `flate2`/`miniz_oxide` are MIT/permissive — within the
  established permissive-pure-Rust floor (same rule that admitted lopdf/image).
- **Deterministic + metadata-clean output.** On create, set every entry's modified-time to a
  FIXED constant (e.g. the DOS-epoch 1980-01-01), never the current clock — same output for
  the same input, and it does not leak the user's local time into the archive (coherent with
  the metadata-scrubber ethos). No `getrandom`/clock reads.
- Do NOT touch `app/`, `app-img/`, `app-scrub/`, the existing PDF/image/metadata core fns, the
  deploy, or any CF resource. Do NOT create CI.

## Part A — core (`core-rs/src/archive_ops.rs`, additive; `lib.rs` gets only `mod`+`pub use`)

Wasm-bindgen exports, each a thin wrapper over a pure inner fn returning `Result<_, String>`
(native-testable; JsValue only at the boundary — the established split):

```rust
/// Build a Store+Deflate zip from parallel name/byte arrays. `names[i]` pairs with
/// `buffers[i]` (a Uint8Array). Deterministic; fixed entry mtime. Deflate each entry,
/// but STORE (no compression) any entry that would grow under Deflate (already-compressed
/// data) so the archive never bloats an entry.
#[wasm_bindgen]
pub fn create_zip(names: Vec<String>, buffers: js_sys::Array) -> Result<Vec<u8>, JsValue>;
// inner: create_archive(Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, String>

/// List a zip's entries for the extract UI. JSON string (hand-serialize, NO serde):
///   {"entries":[{"name":..,"size":<u64 decompressed>,"compressed":<u64>,"is_dir":bool,
///                "unsafe_path":bool}, ...]}
/// `unsafe_path`=true if the stored name is absolute or contains a `..` path segment.
#[wasm_bindgen]
pub fn list_zip(bytes: &[u8]) -> Result<String, JsValue>;
// inner: list_archive(&[u8]) -> Result<String, String>

/// Extract ONE entry (by its index in list order) and return its raw bytes.
#[wasm_bindgen]
pub fn extract_zip_entry(bytes: &[u8], index: u32) -> Result<Vec<u8>, JsValue>;
// inner: extract_entry(&[u8], u32) -> Result<Vec<u8>, String>
```

### Security guards (all mandatory — these are the real surface of a zip tool)
1. **Zip-bomb / OOM.** Add `const MAX_ARCHIVE_ENTRY_BYTES: u64 = 512_000_000;` (per entry,
   decompressed) and `const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512_000_000;` (sum across
   `list_zip`). During extraction, do NOT trust the declared size — read the decompressing
   stream through `.take(MAX_ARCHIVE_ENTRY_BYTES + 1)` and error
   (`"This archive entry is too large to extract safely."`) if it exceeds the cap. In
   `list_zip`, if the summed declared decompressed size exceeds the total cap, still list but
   mark it (or error on extract) — never allocate an unbounded buffer.
2. **Zip-slip.** Never used to write a filesystem (browser download names are user-chosen), but
   still: report `unsafe_path` in the listing, and when the app derives a download filename it
   uses only the sanitized BASENAME (strip any directory, reject `..`). The core reports the
   flag; the app sanitizes the download name.
3. **Encrypted zips.** If an entry is encrypted, error cleanly:
   `"This archive is password-protected, which isn't supported."` (the `zip` crate surfaces
   this — do not panic).
4. **Malformed / truncated / not-a-zip** → `Err`, never panic. Empty input → `Err`.
5. `create_zip`: reject an empty file set (`"Choose at least one file to archive."`); handle
   duplicate names deterministically (keep both, e.g. suffix `-1`, or document the rule);
   guard total input size against the same total cap.

### Native tests (`#[cfg(test)]`, ~10+):
- round-trip: `create_archive` of two named byte blobs → `list_archive` reports both with
  correct decompressed sizes + `is_dir:false` + `unsafe_path:false` → `extract_entry` returns
  each blob **byte-identical**.
- deterministic: same input → byte-identical archive twice (proves fixed mtime, no clock).
- already-compressed entry (feed random-ish bytes) is STORED not bloated (compressed size not
  larger than input + zip overhead).
- zip-bomb guard: craft/con­struct an entry whose decompressed size exceeds the cap (or stub
  the cap low in a test-only path) → `extract_entry` errors, does not allocate unbounded.
- unsafe path: an entry named `../evil.txt` and one named `/abs.txt` → `list_archive` flags
  `unsafe_path:true`; a normal `a/b.txt` → false.
- encrypted zip fixture → `Err` with the password message (build a tiny encrypted zip fixture,
  or assert the crate's encrypted branch maps to the right error).
- garbage/empty/truncated bytes → `Err`, no panic. `extract_entry` with an out-of-range index
  → `Err`.

### Part A gates (run + paste):
- `cargo test --manifest-path core-rs/Cargo.toml` (all green, old + new)
- `cargo tree --manifest-path core-rs/Cargo.toml --prefix none | grep -Ei '^(cc|bindgen|libbz2|bzip2|zstd|libz-ng|[^[:space:]]+-sys) v' | grep -v '^js-sys '` → **empty** (no C deps)
- `wasm-pack build core-rs --target web --out-dir ../app-zip/src/wasm` (clean)

## Part B — app (`app-zip/`, cloned from `app-img/` — the freshest shell)

**`app-img/` is your template. Do NOT modify `app/`, `app-img/`, or `app-scrub/`.** Copy the
scaffold+shell verbatim, swap in the archive tooling. Same header/brand/theme-toggle, the same
**`local-badge` + `local-inspector` dialog** (verbatim — the wedge), same drop-zone, same
service-worker + PWA + SEO + a11y bar (keyboard-nav tool switcher w/ `aria-current` + roving
tabindex, `aria-live` status, focus-visible, dialog focus-trap, `#hash` deep-link per tool,
per-tool `<title>`).

Copy from `app-img/`, adapting only what's noted (mirror exactly how `app-scrub` was cloned):
- `package.json` → name `localbench-zip-app`; scripts identical.
- `vite.config.ts`, `tsconfig.json`, `src/tokens.css`, `src/style.css` → copy as-is.
- `public/_headers` → **verbatim** (strict CSP; `img-src 'self' data: blob:` already present).
- `public/icons/*`, `public/manifest.webmanifest` → copy; update name/short_name/description.
- `public/robots.txt`, `public/sitemap.xml` → copy; URL `https://zip.keeplocal.tools/`.
- `index.html` → copy shell; swap `<title>`/meta/OG/JSON-LD/canonical to
  `https://zip.keeplocal.tools/`; two tool panels (Create / Extract).
- `src/wasm/.gitignore` → mirror.
Build wasm: `wasm-pack build core-rs --target web --out-dir ../app-zip/src/wasm`.

### Worker protocol (`app-zip/src/core.worker.ts`) — EXACTLY this shape
Mirror `app-img/src/core.worker.ts` (module worker, `await init()`, transferables, the
`runCoreRequest` id+30s-timeout helper in main.ts).

```ts
type WorkerRequest =
  | { id: number; type: "createZip"; names: string[]; buffers: ArrayBuffer[] }
  | { id: number; type: "listZip"; bytes: ArrayBuffer }
  | { id: number; type: "extractEntry"; bytes: ArrayBuffer; index: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "zipCreated"; id: number; bytes: ArrayBuffer }
  | { type: "zipListed"; id: number; report: string }        // JSON from list_zip
  | { type: "entryExtracted"; id: number; index: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };
```
Imports: `create_zip, list_zip, extract_zip_entry, core_version`. For `createZip`, pass the
`buffers` as `js_sys::Array` of `Uint8Array` (build it in the worker from the ArrayBuffers) and
`names` as a `string[]` (wasm-bindgen `Vec<String>`). Transfer result/extracted buffers out.

### Two tools (switcher + panels, mirroring the PDF/img switcher a11y)
1. **Create** — a multi-file drop zone + picker (accept any type). Show the chosen file list
   (name + size, a remove-per-file control, running total). Button "Create .zip". On success:
   show the archive size, note "entry timestamps are normalized (your clock isn't stored)",
   Download button (filename e.g. `archive.zip`, `application/zip`). Empty set → button
   disabled.
2. **Extract** — drop a single `.zip`. On load, call `listZip` and render the entry table:
   name, decompressed size, and a per-entry **Download** button (calls `extractEntry` for that
   index; download uses the SANITIZED basename + guessed MIME or `application/octet-stream`).
   Flag `unsafe_path` entries with a visible warning pill and download them under the sanitized
   basename only. Offer **"Download all"** (loop `extractEntry` over each safe entry, trigger a
   download each). Encrypted/oversize/garbage → surface the core error cleanly in `aria-live`.

Download helper + Blob/object-URL/revoke like the other apps.

### Part B gates (run + paste):
- `npm --prefix app-zip install && npm --prefix app-zip run build` (tsc + vite + SW gen, clean)
- `node scripts/check-local.mjs app-zip/dist` — **must PASS**. Add `zip.keeplocal.tools` inert-URL
  masks in `maskExpectedInertUrls` (canonical/og:url/JSON-LD/sitemap/robots) the SAME way the
  `img.keeplocal.tools` masks already present in `scripts/check-local.mjs` are written (BOTH the
  regex-match branch AND the strip-target string, mirroring that structure exactly). Do NOT
  weaken the guard. (This branch is off main; `app-scrub` is not present here — ignore it.)

## Deliverable / report
Commit to `feat/zip`. In the final report: list files created/changed, paste ALL gate outputs
(Part A + Part B), state a real round-trip example (N files in → zip size → extracted
byte-identical), and note any deviation. Do NOT modify the other apps, the deploy, or any CF
resource. Do NOT create CI.
