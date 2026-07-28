/*
 * Fixtures for the notice gate.
 *
 * Every file here exists to make the Rust core say something true and awkward:
 * an animation it had to flatten, a bit depth it had to round, a file it could
 * not improve and handed straight back. The gate then checks the interface put
 * that sentence in front of the user.
 *
 * Written from scratch with nothing but node:zlib on purpose. A fixture nobody
 * can regenerate is a fixture nobody can check, and adding an image library to
 * a repo whose whole pitch is "minimal dependencies, read the code" would be
 * the wrong trade. Everything below is a few dozen lines of file format.
 *
 * Run: node scripts/notice-fixtures.mjs [outputDirectory]
 * The output is deterministic: the same bytes every time, on every machine.
 */
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import zlib from "node:zlib";

// ---------------------------------------------------------------------------
// CRC-32 (PNG chunks and ZIP entries both need it).
// ---------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

// ---------------------------------------------------------------------------
// PNG.
// ---------------------------------------------------------------------------

function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const typed = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typed));
  return Buffer.concat([length, typed, crc]);
}

/**
 * A truecolour PNG. `bitDepth` is 8 or 16; `pixel(x, y)` returns [r, g, b] in
 * the range the depth allows. Filter byte 0 (None) on every scanline keeps the
 * encoder honest and the output reproducible.
 */
function png(width, height, bitDepth, pixel) {
  const sampleBytes = bitDepth === 16 ? 2 : 1;
  const stride = 1 + width * 3 * sampleBytes;
  const raw = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y += 1) {
    let at = y * stride;
    raw[at] = 0;
    at += 1;
    for (let x = 0; x < width; x += 1) {
      for (const sample of pixel(x, y)) {
        if (bitDepth === 16) {
          raw.writeUInt16BE(sample, at);
          at += 2;
        } else {
          raw[at] = sample;
          at += 1;
        }
      }
    }
  }

  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header[8] = bitDepth;
  header[9] = 2; // colour type 2: truecolour RGB
  header[10] = 0; // deflate
  header[11] = 0; // adaptive filtering
  header[12] = 0; // no interlace

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", header),
    pngChunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

// ---------------------------------------------------------------------------
// GIF (multi-frame).
// ---------------------------------------------------------------------------

/**
 * GIF image data as LZW with a CLEAR code before every literal.
 *
 * This is the least clever encoder that is still a correct one: resetting the
 * dictionary each time means no code ever grows past the initial width, so
 * there is no variable-width bit packing to get wrong. The output is larger
 * than a real encoder's and completely uninteresting to look at, which is
 * exactly what a fixture should be.
 */
function gifImageData(indices, minCodeSize) {
  const clear = 1 << minCodeSize;
  const end = clear + 1;
  const width = minCodeSize + 1;

  const bits = [];
  const emit = (code) => {
    for (let bit = 0; bit < width; bit += 1) bits.push((code >> bit) & 1);
  };
  for (const index of indices) {
    emit(clear);
    emit(index);
  }
  emit(clear);
  emit(end);

  const bytes = [];
  for (let at = 0; at < bits.length; at += 8) {
    let byte = 0;
    for (let bit = 0; bit < 8 && at + bit < bits.length; bit += 1) {
      byte |= bits[at + bit] << bit;
    }
    bytes.push(byte);
  }

  const blocks = [minCodeSize];
  for (let at = 0; at < bytes.length; at += 255) {
    const slice = bytes.slice(at, at + 255);
    blocks.push(slice.length, ...slice);
  }
  blocks.push(0); // block terminator
  return Buffer.from(blocks);
}

/** An animated GIF89a with a 2-colour global table and one frame per entry. */
function animatedGif(width, height, palette, frames, delayCentiseconds) {
  const parts = [Buffer.from("GIF89a", "latin1")];

  const screen = Buffer.alloc(7);
  screen.writeUInt16LE(width, 0);
  screen.writeUInt16LE(height, 2);
  // Global colour table present, 2 entries (size field 0).
  screen[4] = 0x80;
  screen[5] = 0;
  screen[6] = 0;
  parts.push(screen, Buffer.from(palette.flat()));

  // NETSCAPE2.0: loop forever. Without it a viewer may show one pass only.
  parts.push(
    Buffer.from([0x21, 0xff, 0x0b]),
    Buffer.from("NETSCAPE2.0", "latin1"),
    Buffer.from([0x03, 0x01, 0x00, 0x00, 0x00]),
  );

  for (const indices of frames) {
    const control = Buffer.alloc(8);
    control[0] = 0x21;
    control[1] = 0xf9;
    control[2] = 0x04;
    control[3] = 0x00; // no disposal, no transparency
    control.writeUInt16LE(delayCentiseconds, 4);
    control[6] = 0x00;
    control[7] = 0x00;
    parts.push(control);

    const descriptor = Buffer.alloc(10);
    descriptor[0] = 0x2c;
    descriptor.writeUInt16LE(0, 1);
    descriptor.writeUInt16LE(0, 3);
    descriptor.writeUInt16LE(width, 5);
    descriptor.writeUInt16LE(height, 7);
    descriptor[9] = 0x00; // no local colour table, not interlaced
    parts.push(descriptor, gifImageData(indices, 2));
  }

  parts.push(Buffer.from([0x3b]));
  return Buffer.concat(parts);
}

// ---------------------------------------------------------------------------
// ZIP (store method only).
// ---------------------------------------------------------------------------

/**
 * A ZIP built by hand so entries can share a name and carry the
 * encrypted flag — neither of which a normal archiver will produce on request.
 *
 * `encrypted: true` sets general-purpose bit 0, which is precisely what the
 * central directory means by "encrypted" and precisely what the core reads.
 * The entry is otherwise plain, so the fixture proves the LISTING contract
 * (this entry is not extractable, and the interface must not offer to try)
 * without shipping a password anywhere.
 */
function zip(entries) {
  const locals = [];
  const central = [];
  let offset = 0;

  for (const entry of entries) {
    const name = Buffer.from(entry.name, "utf8");
    const data = Buffer.from(entry.content, "utf8");
    const flags = entry.encrypted ? 0x0001 : 0x0000;
    const crc = crc32(data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(0, 8); // store
    local.writeUInt16LE(0, 10); // time (fixed: deterministic output)
    local.writeUInt16LE(0x0021, 12); // date 1980-01-01
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    local.writeUInt16LE(0, 28);

    const header = Buffer.alloc(46);
    header.writeUInt32LE(0x02014b50, 0);
    header.writeUInt16LE(20, 4); // version made by
    header.writeUInt16LE(20, 6); // version needed
    header.writeUInt16LE(flags, 8);
    header.writeUInt16LE(0, 10); // store
    header.writeUInt16LE(0, 12);
    header.writeUInt16LE(0x0021, 14);
    header.writeUInt32LE(crc, 16);
    header.writeUInt32LE(data.length, 20);
    header.writeUInt32LE(data.length, 24);
    header.writeUInt16LE(name.length, 28);
    header.writeUInt16LE(0, 30); // extra
    header.writeUInt16LE(0, 32); // comment
    header.writeUInt16LE(0, 34); // disk
    header.writeUInt16LE(0, 36); // internal attrs
    header.writeUInt32LE(0, 38); // external attrs
    header.writeUInt32LE(offset, 42);

    locals.push(local, name, data);
    central.push(header, name);
    offset += local.length + name.length + data.length;
  }

  const directory = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(0, 4);
  eocd.writeUInt16LE(0, 6);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(directory.length, 12);
  eocd.writeUInt32LE(offset, 16);
  eocd.writeUInt16LE(0, 20);

  return Buffer.concat([...locals, directory, eocd]);
}

// ---------------------------------------------------------------------------
// PDF.
// ---------------------------------------------------------------------------

/**
 * A PDF that is ALREADY as compact as this toolchain can write it, and that
 * carries document metadata.
 *
 * Both halves matter. It uses a PDF 1.5 cross-reference stream and packs its
 * objects into an object stream — the modern layout every current producer
 * emits — which is meaningfully tighter than the classic `xref` table plus
 * `N 0 obj` wrappers that the compressor writes back out. So the "compressed"
 * result is not smaller, the core refuses to hand over a worse file, and the
 * user gets their original bytes back with the title and author still in them.
 *
 * That is the `pdf-returned-unchanged` case with `had_metadata` true, whose
 * message ends "Its metadata was NOT removed." — in direct contradiction of
 * the compress panel's promise to remove it. It is the single most important
 * fixture here.
 *
 * The page count is what tunes it: each page adds ~23 bytes of structure that
 * the classic layout cannot match, and the metadata is only worth ~90 bytes,
 * so a couple of dozen pages puts the result firmly on the right side.
 */
function alreadyCompactPdf(pageCount = 24) {
  const kids = Array.from({ length: pageCount }, (_, i) => `${i + 3} 0 R`).join(" ");
  const packed = [
    "<</Type/Catalog/Pages 2 0 R>>",
    `<</Type/Pages/Kids[${kids}]/Count ${pageCount}>>`,
    ...Array.from(
      { length: pageCount },
      () => "<</Type/Page/Parent 2 0 R/MediaBox[0 0 595 842]>>",
    ),
    "<</Title(Board minutes 2026-03)/Author(A. Person)/Producer(keeplocal fixture)>>",
  ];
  const infoNumber = packed.length;
  const objectStreamNumber = packed.length + 1;
  const xrefNumber = packed.length + 2;

  // Object stream: an offset table, then the objects themselves.
  let blob = "";
  const offsets = [];
  for (const object of packed) {
    offsets.push(blob.length);
    blob += `${object}\n`;
  }
  const pairs = `${packed.map((_, i) => `${i + 1} ${offsets[i]}`).join(" ")}\n`;
  const objectStreamData = zlib.deflateSync(Buffer.from(pairs + blob, "latin1"), { level: 9 });

  const parts = [Buffer.from("%PDF-1.5\n", "latin1")];
  let at = parts[0].length;

  const objectStreamOffset = at;
  const objectStream = Buffer.concat([
    Buffer.from(
      `${objectStreamNumber} 0 obj\n<</Type/ObjStm/N ${packed.length}/First ${pairs.length}` +
        `/Filter/FlateDecode/Length ${objectStreamData.length}>>\nstream\n`,
      "latin1",
    ),
    objectStreamData,
    Buffer.from("\nendstream\nendobj\n", "latin1"),
  ]);
  parts.push(objectStream);
  at += objectStream.length;

  // Cross-reference stream, W = [1 3 1]: type, field 2 (3 bytes), field 3.
  const xrefOffset = at;
  const rows = [];
  const row = (type, second, third) => {
    const field = Buffer.alloc(3);
    field.writeUIntBE(second, 0, 3);
    rows.push(Buffer.concat([Buffer.from([type]), field, Buffer.from([third])]));
  };
  row(0, 0, 255); // object 0, free
  for (let i = 0; i < packed.length; i += 1) row(2, objectStreamNumber, i);
  row(1, objectStreamOffset, 0);
  row(1, xrefOffset, 0);
  const xrefData = zlib.deflateSync(Buffer.concat(rows), { level: 9 });

  parts.push(
    Buffer.concat([
      Buffer.from(
        `${xrefNumber} 0 obj\n<</Type/XRef/Size ${xrefNumber + 1}/W[1 3 1]/Root 1 0 R` +
          `/Info ${infoNumber} 0 R/Filter/FlateDecode/Length ${xrefData.length}>>\nstream\n`,
        "latin1",
      ),
      xrefData,
      Buffer.from(`\nendstream\nendobj\nstartxref\n${xrefOffset}\n%%EOF\n`, "latin1"),
    ]),
  );

  return Buffer.concat(parts);
}

/** Deterministic pseudo-random bytes (xorshift32) — the same image everywhere. */
function noiseSource(seed = 0x2545f491) {
  let state = seed;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state & 0xff;
  };
}

// ---------------------------------------------------------------------------
// The fixture set.
// ---------------------------------------------------------------------------

export const FIXTURES = {
  /*
   * Bigger than 4096px on both edges. The audit found images this size being
   * silently downscaled under a "losslessly re-encoded" claim; this fixture is
   * how we watch that stay fixed — converted, it must come back 5000x3000.
   */
  "wide.png": () =>
    png(5000, 3000, 8, (x, y) => [(x * 7) & 0xff, (y * 11) & 0xff, (x ^ y) & 0xff]),

  /* 16 bits per channel: PNG keeps them, JPEG and WebP cannot. */
  "deep16.png": () =>
    png(240, 160, 16, (x, y) => [x * 271, y * 409, (x + y) * 137]),

  /* Three frames. Converting to a still format loses two of them. */
  "animated.gif": () =>
    animatedGif(
      12,
      12,
      [
        [0x10, 0x20, 0x30],
        [0xe0, 0xd0, 0xc0],
      ],
      [
        Array.from({ length: 144 }, (_, i) => (i % 2 === 0 ? 0 : 1)),
        Array.from({ length: 144 }, (_, i) => (i % 2 === 0 ? 1 : 0)),
        Array.from({ length: 144 }, (_, i) => (i % 3 === 0 ? 1 : 0)),
      ],
      20,
    ),

  /*
   * Pure noise at a size where deflate's own framing already dominates: no
   * re-encoding can make it smaller, so the core hands the original back with
   * its metadata intact and says so. This is `image-returned-unchanged`, the
   * image half of the contradiction pair.
   */
  "already-minimal.png": (() => {
    const next = noiseSource();
    return () => png(64, 64, 8, () => [next(), next(), next()]);
  })(),

  /* Already compact AND carrying metadata: `pdf-returned-unchanged`. */
  "already-compact.pdf": () => alreadyCompactPdf(24),

  /* Two entries with the same name, one entry flagged encrypted. */
  "duplicate-names.zip": () =>
    zip([
      { name: "notes.txt", content: "the first notes file\n" },
      { name: "receipt.txt", content: "a plain entry that extracts fine\n" },
      { name: "notes.txt", content: "a DIFFERENT file with the same name\n" },
      { name: "sealed.txt", content: "flagged encrypted in the directory\n", encrypted: true },
    ]),
};

export async function writeFixtures(directory) {
  await mkdir(directory, { recursive: true });
  const written = [];
  for (const [name, build] of Object.entries(FIXTURES)) {
    const bytes = build();
    await writeFile(path.join(directory, name), bytes);
    written.push({ name, bytes: bytes.length });
  }
  return written;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const directory = path.resolve(process.argv[2] ?? "notice-corpus");
  for (const { name, bytes } of await writeFixtures(directory)) {
    console.log(`${name.padEnd(24)} ${bytes} bytes`);
  }
  console.log(`\nWritten to ${directory}`);
}
