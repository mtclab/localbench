#!/usr/bin/env python3
"""Generate the metadata-scrubber smoke corpus (scrub-smoke.mjs reads ./scrub-corpus).

Files are gitignored (not committed to the public repo) — regenerate with:
    python3 -m venv .scrubvenv && .scrubvenv/bin/pip install Pillow piexif reportlab
    .scrubvenv/bin/python3 scripts/build-scrub-corpus.py [out_dir]

Produces, each carrying REAL removable metadata so the strip test is meaningful:
  photo.jpg  800x600 JPEG with GPS EXIF + a JPEG comment  -> inspect flags GPS; scrub strips
  tagged.png 320x240 PNG with a tEXt (Software) chunk + eXIf -> inspect lists both; scrub strips
  doc.pdf    1-page PDF with /Info (Author/Title/Producer) -> inspect lists Info; scrub strips
  clean.png  64x64 PNG with NO ancillary metadata          -> inspect empty (already-clean path)
"""
import math
import os
import struct
import sys
import zlib

from PIL import Image
import piexif

out = sys.argv[1] if len(sys.argv) > 1 else "scrub-corpus"
os.makedirs(out, exist_ok=True)

# --- photo.jpg: GPS EXIF + a comment segment -------------------------------
w, h = 800, 600
img = Image.new("RGB", (w, h))
px = img.load()
for y in range(h):
    for x in range(w):
        px[x, y] = (
            int(127 + 127 * math.sin(x / 40.0)),
            int(127 + 127 * math.sin(y / 55.0)),
            int(127 + 127 * math.sin((x + y) / 70.0)),
        )
gps = {
    piexif.GPSIFD.GPSLatitudeRef: b"N",
    piexif.GPSIFD.GPSLatitude: ((60, 1), (10, 1), (0, 1)),
    piexif.GPSIFD.GPSLongitudeRef: b"E",
    piexif.GPSIFD.GPSLongitude: ((24, 1), (56, 1), (0, 1)),
}
exif = piexif.dump(
    {"0th": {piexif.ImageIFD.Make: b"MtclabCam"}, "GPS": gps, "Exif": {}, "1st": {}, "thumbnail": None}
)
img.save(os.path.join(out, "photo.jpg"), quality=95, exif=exif, comment=b"secret location note")

# --- tagged.png: a tEXt "Software" chunk + an eXIf chunk -------------------
def png_chunk(ctype: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + ctype
        + data
        + struct.pack(">I", zlib.crc32(ctype + data) & 0xFFFFFFFF)
    )


base = Image.new("RGB", (320, 240))
bp = base.load()
for y in range(240):
    for x in range(320):
        bp[x, y] = (x % 256, y % 256, (x + y) % 256)
tmp_png = os.path.join(out, "_tmp.png")
base.save(tmp_png)
with open(tmp_png, "rb") as f:
    raw = f.read()
os.remove(tmp_png)
# Splice a tEXt + eXIf chunk in right after IHDR (offset 8 sig + 25 IHDR = 33).
sig, rest = raw[:8], raw[8:]
ihdr_end = 8 + 4 + 4 + 13 + 4  # len+type+data(13)+crc, absolute offset into raw
head, tail = raw[:ihdr_end], raw[ihdr_end:]
text = png_chunk(b"tEXt", b"Software\x00Adobe Photoshop 24.0")
# Minimal EXIF payload with a GPS IFD marker byte-pattern for the sensitive flag.
exif_payload = piexif.dump({"0th": {piexif.ImageIFD.Make: b"MtclabCam"}, "GPS": gps, "Exif": {}, "1st": {}, "thumbnail": None})
exif_chunk = png_chunk(b"eXIf", exif_payload)
with open(os.path.join(out, "tagged.png"), "wb") as f:
    f.write(head + text + exif_chunk + tail)

# --- clean.png: no ancillary metadata --------------------------------------
Image.new("RGB", (64, 64), (10, 120, 200)).save(os.path.join(out, "clean.png"))

# --- doc.pdf: /Info metadata via reportlab ---------------------------------
try:
    from reportlab.pdfgen import canvas
    from reportlab.lib.pagesizes import A4

    pdf_path = os.path.join(out, "doc.pdf")
    c = canvas.Canvas(pdf_path, pagesize=A4)
    c.setAuthor("Olli Kurki")
    c.setTitle("Quarterly Numbers")
    c.setSubject("internal")
    c.setCreator("Mtclab Writer")
    c.drawString(72, 720, "This document body must survive scrubbing.")
    c.showPage()
    c.save()
    made_pdf = True
except Exception as exc:  # noqa: BLE001
    made_pdf = False
    print(f"WARN: reportlab missing, skipped doc.pdf ({exc})")

made = ["photo.jpg (GPS EXIF+comment)", "tagged.png (tEXt+eXIf)", "clean.png (none)"]
if made_pdf:
    made.append("doc.pdf (/Info)")
print("wrote " + ", ".join(f"{out}/{m}" for m in made))
