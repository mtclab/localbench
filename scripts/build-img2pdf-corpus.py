#!/usr/bin/env python3
"""Generate the images->PDF smoke corpus (img2pdf-smoke.mjs reads ./img2pdf-corpus).

Files are gitignored. Regenerate with:
    /home/kasm-user/repot/localbench/.scrubvenv/bin/python3 scripts/build-img2pdf-corpus.py [out_dir]

Produces images to combine into a PDF:
  a.jpg   640x480 JPEG WITH GPS EXIF  -> proves EXIF/GPS stripped on embed
  b.png   320x240 RGBA (transparency) -> proves alpha flatten + FlateDecode path
  c.jpg   400x300 JPEG                -> a third page (multi-image order)
"""
import math
import os
import sys

from PIL import Image
import piexif

out = sys.argv[1] if len(sys.argv) > 1 else "img2pdf-corpus"
os.makedirs(out, exist_ok=True)

# a.jpg — a photo tagged with GPS.
w, h = 640, 480
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
exif = piexif.dump({"0th": {piexif.ImageIFD.Make: b"MtclabCam"}, "GPS": gps, "Exif": {}, "1st": {}, "thumbnail": None})
img.save(os.path.join(out, "a.jpg"), quality=92, exif=exif)

# b.png — RGBA with partial transparency.
img2 = Image.new("RGBA", (320, 240))
p2 = img2.load()
for y in range(240):
    for x in range(320):
        p2[x, y] = (x % 256, y % 256, (x * y) % 256, 255 if (x + y) % 3 else 60)
img2.save(os.path.join(out, "b.png"))

# c.jpg — a plain third image.
img3 = Image.new("RGB", (400, 300), (30, 140, 90))
img3.save(os.path.join(out, "c.jpg"), quality=90)

print(f"wrote {out}/a.jpg (GPS EXIF) + {out}/b.png (RGBA) + {out}/c.jpg")
