#!/usr/bin/env python3
"""Generate the image-smoke corpus (img-smoke.mjs reads ./img-corpus).

Images are gitignored (not committed to the public repo) — regenerate with:
    python3 -m venv .imgvenv && .imgvenv/bin/pip install Pillow piexif
    .imgvenv/bin/python3 scripts/build-img-corpus.py [out_dir]

Produces:
  photo.jpg  800x600 gradient JPEG WITH GPS EXIF  -> compress + end-to-end strip test
  pic.png    640x480 RGBA (transparency)          -> resize + convert(alpha-flatten) test
"""
import math
import os
import sys

from PIL import Image
import piexif

out = sys.argv[1] if len(sys.argv) > 1 else "img-corpus"
os.makedirs(out, exist_ok=True)

# photo.jpg — smooth, compressible, tagged with a GPS location.
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
exif = piexif.dump({"0th": {piexif.ImageIFD.Make: b"MtclabCam"}, "GPS": gps, "Exif": {}, "1st": {}, "thumbnail": None})
img.save(os.path.join(out, "photo.jpg"), quality=95, exif=exif)

# pic.png — RGBA with partial transparency.
img2 = Image.new("RGBA", (640, 480))
p2 = img2.load()
for y in range(480):
    for x in range(640):
        p2[x, y] = (x % 256, y % 256, (x * y) % 256, 255 if (x + y) % 3 else 80)
img2.save(os.path.join(out, "pic.png"))

print(f"wrote {out}/photo.jpg (GPS EXIF) + {out}/pic.png (RGBA)")
