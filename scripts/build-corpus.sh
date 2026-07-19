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
