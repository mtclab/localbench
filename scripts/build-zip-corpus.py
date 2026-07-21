#!/usr/bin/env python3
"""Generate the archive-tool smoke corpus (zip-smoke.mjs reads ./zip-corpus).

Files are gitignored (not committed to the public repo) — regenerate with:
    python3 scripts/build-zip-corpus.py [out_dir]
(stdlib zipfile for sample/slip; the `zip` CLI for the encrypted fixture.)

Produces .zip fixtures for the EXTRACT flow:
  sample.zip   hello.txt + nested dir/note.md + a stored (already-compressed) small.png
  slip.zip     a normal file + a zip-slip "../evil.txt" + an absolute "/abs.txt"
  secret.zip   password-protected (tests the encrypted-reject path)  [needs `zip` CLI]
The CREATE flow uses inline buffers in the smoke; no corpus file needed for it.
"""
import os
import shutil
import subprocess
import sys
import zipfile

out = sys.argv[1] if len(sys.argv) > 1 else "zip-corpus"
os.makedirs(out, exist_ok=True)

HELLO = b"hello world from a local zip\n"
NOTE = b"# Nested note\nThis lives in a folder inside the archive.\n"
# A minimal already-compressed blob (tiny PNG) so store-not-bloat is exercised.
PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d494844520000000100000001080600000"
    "01f15c4890000000d49444154789c6360000002000100ffff030000060005"
    "57bfabd40000000049454e44ae426082"
)

# sample.zip — normal entries, one nested, one stored.
with zipfile.ZipFile(os.path.join(out, "sample.zip"), "w") as z:
    z.writestr("hello.txt", HELLO, compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("dir/note.md", NOTE, compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("small.png", PNG, compress_type=zipfile.ZIP_STORED)

# slip.zip — a normal file plus zip-slip + absolute-path entries.
with zipfile.ZipFile(os.path.join(out, "slip.zip"), "w") as z:
    z.writestr("safe.txt", b"i am safe\n", compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("../evil.txt", b"i tried to escape\n", compress_type=zipfile.ZIP_DEFLATED)
    z.writestr("/abs.txt", b"absolute path\n", compress_type=zipfile.ZIP_DEFLATED)

# secret.zip — password-protected (stdlib zipfile can't WRITE encryption; use the CLI).
made_secret = False
if shutil.which("zip"):
    plain = os.path.join(out, "_plain.txt")
    with open(plain, "wb") as f:
        f.write(b"top secret contents\n")
    secret = os.path.join(out, "secret.zip")
    if os.path.exists(secret):
        os.remove(secret)
    try:
        subprocess.run(
            ["zip", "-j", "-e", "-P", "hunter2", secret, plain],
            check=True, capture_output=True,
        )
        made_secret = True
    except subprocess.CalledProcessError as exc:
        print(f"WARN: `zip` CLI failed, skipped secret.zip ({exc.stderr.decode(errors='replace')[:120]})")
    finally:
        os.remove(plain)
else:
    print("WARN: `zip` CLI missing, skipped secret.zip (encrypted-reject check will SKIP)")

made = ["sample.zip (3 entries)", "slip.zip (slip+abs)"]
if made_secret:
    made.append("secret.zip (encrypted)")
print("wrote " + ", ".join(f"{out}/{m}" for m in made))
