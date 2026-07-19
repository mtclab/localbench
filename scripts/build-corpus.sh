#!/usr/bin/env bash
# Build a varied PDF corpus for the staging smoke, with ground-truth page counts
# from pdfinfo. Output: test-corpus/*.pdf + test-corpus/manifest.json.
# The PDFs themselves are NOT committed (third-party / personal / generated);
# this script regenerates them. Requires: ghostscript (gs), pdfinfo.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=test-corpus
mkdir -p "$OUT"
rm -f "$OUT"/*.pdf

# Generated fixtures (self-authored, deterministic).
gs -q -o "$OUT/big50.pdf" -sDEVICE=pdfwrite -c "/n 50 def 1 1 n { pop showpage } for"
gs -q -o "$OUT/single.pdf" -sDEVICE=pdfwrite -c "showpage"
gs -q -o "$OUT/encrypted.pdf" -sDEVICE=pdfwrite -dEncryptionType=1 \
   -sUserPassword=secret -sOwnerPassword=secret "$OUT/single.pdf"

# Optional real-world fixtures if present on the host (not committed).
for src in /usr/share/cups/data/form_english.pdf /usr/share/cups/data/form_russian.pdf \
           /usr/share/cups/data/default-testpage.pdf; do
  [ -f "$src" ] && cp "$src" "$OUT/$(basename "$src")"
done

# Image-heavy PDF for the compress test: embed a real JPEG (DCTDecode) so
# re-encoding actually has something to shrink. Uses a host photo if available,
# else renders a synthetic JPEG. viewjpeg.ps ships with ghostscript.
VIEWJPEG=$(find /usr/share/ghostscript -name viewjpeg.ps 2>/dev/null | head -1)
if [ -n "$VIEWJPEG" ]; then
  SRC_JPG=""
  for cand in /home/kasm-user/repot/kattavuus-live.jpeg /home/kasm-user/repot/yritys-converted.jpeg; do
    [ -f "$cand" ] && SRC_JPG="$cand" && break
  done
  ( cd "$OUT"
    if [ -n "$SRC_JPG" ]; then cp "$SRC_JPG" _photo.jpg
    else gs -q -dNOPAUSE -dBATCH -sDEVICE=jpeg -r150 -g1200x1600 -o _photo.jpg \
           -c "0 1 300 { 300 mod 300 div 0.5 0.5 setrgbcolor 0 0 1200 1600 rectfill } for showpage" 2>/dev/null || true
    fi
    [ -f _photo.jpg ] && gs -q -dNOPAUSE -dBATCH -dNOSAFER -sDEVICE=pdfwrite \
      -dAutoFilterColorImages=false -dColorImageFilter=/DCTEncode -o photo.pdf \
      "$VIEWJPEG" -c "(_photo.jpg) viewJPEG showpage" 2>/dev/null || true
    rm -f _photo.jpg
  )
fi

# Build manifest of expected page counts. Encrypted -> "tolerant" (no password
# prompt exists; the tool must resolve to a count OR a clean error, never crash).
{
  echo "{"
  first=1
  for f in "$OUT"/*.pdf; do
    name=$(basename "$f")
    if [ "$name" = "encrypted.pdf" ]; then
      val='"error"'
    else
      val=$(pdfinfo "$f" 2>/dev/null | awk '/^Pages:/{print $2}')
      [ -z "$val" ] && val='"tolerant"'
    fi
    [ $first -eq 0 ] && echo ","
    printf '  "%s": %s' "$name" "$val"
    first=0
  done
  echo ""
  echo "}"
} > "$OUT/manifest.json"

echo "corpus built:"
cat "$OUT/manifest.json"
