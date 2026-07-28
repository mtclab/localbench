//! Lossless metadata inspection and removal for PDF, JPEG, and PNG files.
//!
//! JPEG APP/COM segments and PNG ancillary chunks are removed by copying the
//! original byte ranges around them. Pixel data is never decoded or re-encoded:
//! JPEG scan data and PNG IDAT chunks remain byte-for-byte identical.
//!
//! ## The default is DROP, and the keep-list is explicit
//!
//! Both image walkers drop and report anything they do not positively recognise
//! as affecting how the file DECODES or RENDERS. Keeping the unknown case would
//! mean a vendor chunk or a crafted segment rides through while the tool tells
//! the user the file is already clean, which is the worst thing this module can
//! do. What is kept, and why:
//!
//! - JPEG: APP0 carrying `JFIF\0`, APP2 carrying `ICC_PROFILE\0`, APP14 carrying
//!   `Adobe` — compatibility and rendered colour. The keep is decided by PAYLOAD,
//!   not marker class, so a JFXX thumbnail or an MPF index does not inherit it.
//! - JPEG: EXIF **Orientation** is re-emitted as a minimal orientation-only EXIF
//!   block. It is the one EXIF field that changes rendered geometry, and dropping
//!   it turned portrait phone photos sideways.
//! - PNG: the critical chunks, plus the ancillary chunks listed in
//!   `PNG_KEEP_ANCILLARY` (transparency, colour, HDR, APNG animation).
//!
//! Bytes after the primary image's EOI (JPEG) or after IEND (PNG) are not part of
//! the image: a JPEG's trailing block is the MPF secondary image / Ultra-HDR gain
//! map / motion-photo video that phones append, each with its own full EXIF. They
//! are removed and reported. A PNG whose CRITICAL chunk we cannot identify is
//! refused rather than re-emitted with a meaning we guessed at.
//!
//! PDF XMP is excised from stream content directly (not merely by dropping
//! `/Metadata` references), so XMP is removed even when a stream is reachable
//! through another reference — in every legal shape, since the `<?xpacket`
//! wrapper is optional, and for every packet in a stream rather than the first.
//! Annotation authorship (`/T`), `/PieceInfo` and signer identity are removed;
//! attachments and XFA form data are document CONTENT, so they are reported and
//! kept rather than silently deleted.
//!
//! ## Verification is structural, never a second `inspect()`
//!
//! Every scrub is checked by `verify_*_structure`, which parses the OUTPUT with
//! its own walkers and asks what survived. Re-running the detector cannot fail
//! for a detector gap, so it certified as clean exactly the leaks it could not
//! see. Callers confirming a scrub must use [`verify_metadata_removed`].
//!
//! Compressed PNG text (zTXt / iTXt) is inflated so its contents can be inspected
//! and judged, not just reported.
//!
//! ## Deferred
//!
//! - GIF, BMP, and WebP metadata: safe, lossless container surgery for those
//!   formats is outside this slice.
//! - PNG `eXIf` orientation is dropped with the rest of the chunk rather than
//!   re-emitted: unlike JPEG, PNG viewers overwhelmingly ignore it, so preserving
//!   it would keep metadata without changing what anyone sees.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::{fmt::Write, io::Read};

use flate2::read::ZlibDecoder;
use image::{ImageFormat, ImageReader};
use lopdf::{Dictionary, Document, Object};
use wasm_bindgen::prelude::*;

use super::load_pdf;

const SUPPORTED_FORMATS_ERROR: &str = "Metadata scrubbing supports PDF, JPEG, and PNG files.";
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DETAIL_CHARS: usize = 160;
/// Cap for inflating a compressed PNG text chunk, so a decompression bomb in a
/// zTXt/iTXt value cannot exhaust memory while we inspect it.
const MAX_INFLATED_TEXT: usize = 4_000_000;
/// XMP packets are delimited by these processing-instruction markers.
const XMP_PACKET_BEGIN: &[u8] = b"<?xpacket begin";
const XMP_PACKET_END: &[u8] = b"<?xpacket end";
/// Guard against a pathological stream forcing an unbounded excision loop.
const MAX_XMP_REGIONS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataFormat {
    Pdf,
    Jpeg,
    Png,
}

impl MetadataFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct MetadataItem {
    label: String,
    detail: Option<String>,
    sensitive: bool,
}

impl MetadataItem {
    fn new(label: impl Into<String>, detail: Option<String>, sensitive: bool) -> Self {
        Self {
            label: label.into(),
            detail,
            sensitive,
        }
    }
}

fn detect_format(bytes: &[u8]) -> Result<MetadataFormat, String> {
    if bytes.starts_with(b"%PDF") {
        Ok(MetadataFormat::Pdf)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(MetadataFormat::Jpeg)
    } else if bytes.starts_with(PNG_SIGNATURE) {
        Ok(MetadataFormat::Png)
    } else {
        Err(SUPPORTED_FORMATS_ERROR.to_owned())
    }
}

fn truncate_detail(value: &str) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(MAX_DETAIL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn lossy_detail(value: &[u8]) -> String {
    truncate_detail(&String::from_utf8_lossy(value))
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing to a String cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn report_json(format: MetadataFormat, items: &[MetadataItem]) -> String {
    let mut output = format!("{{\"kind\":\"{}\",\"items\":[", format.name());
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"label\":");
        push_json_string(&mut output, &item.label);
        output.push_str(",\"detail\":");
        if let Some(detail) = &item.detail {
            push_json_string(&mut output, detail);
        } else {
            output.push_str("null");
        }
        output.push_str(",\"sensitive\":");
        output.push_str(if item.sensitive { "true" } else { "false" });
        output.push('}');
    }
    output.push_str("]}");
    output
}

fn read_u16(bytes: &[u8], little_endian: bool) -> Option<u16> {
    let bytes: [u8; 2] = bytes.get(..2)?.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    })
}

fn read_u32(bytes: &[u8], little_endian: bool) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

/// EXIF stores the GPS directory pointer as tag 0x8825 in TIFF IFD0.
fn exif_has_gps(payload: &[u8]) -> bool {
    let tiff = payload.strip_prefix(b"Exif\0\0").unwrap_or(payload);
    let little_endian = match tiff.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return false,
    };
    if read_u16(tiff.get(2..).unwrap_or_default(), little_endian) != Some(42) {
        return false;
    }
    let Some(ifd_offset) = read_u32(tiff.get(4..).unwrap_or_default(), little_endian)
        .and_then(|offset| usize::try_from(offset).ok())
    else {
        return false;
    };
    let Some(entry_count) = tiff
        .get(ifd_offset..)
        .and_then(|bytes| read_u16(bytes, little_endian))
        .map(usize::from)
    else {
        return false;
    };
    let Some(entries_start) = ifd_offset.checked_add(2) else {
        return false;
    };

    (0..entry_count).any(|index| {
        entries_start
            .checked_add(index.saturating_mul(12))
            .and_then(|offset| tiff.get(offset..))
            .and_then(|entry| read_u16(entry, little_endian))
            == Some(0x8825)
    })
}

fn image_dimensions(bytes: &[u8], expected: ImageFormat) -> Result<(u32, u32), String> {
    let actual = image::guess_format(bytes)
        .map_err(|error| format!("Could not validate this image: {error}"))?;
    if actual != expected {
        return Err("The image has an unexpected format.".to_owned());
    }
    let dimensions = ImageReader::with_format(Cursor::new(bytes), expected)
        .into_dimensions()
        .map_err(|error| format!("Could not read this image's dimensions: {error}"))?;
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err("This image has invalid zero-sized dimensions.".to_owned());
    }
    Ok(dimensions)
}

/// EXIF Orientation (TIFF tag 0x0112) as stored in IFD0, if present and valid.
///
/// Orientation is the one EXIF field that changes RENDERED GEOMETRY: dropping it
/// silently turns every portrait phone photo sideways. It is read here so the
/// scrubber can re-emit it (see `minimal_orientation_exif`) instead of losing it.
fn exif_orientation(payload: &[u8]) -> Option<u16> {
    let tiff = payload.strip_prefix(b"Exif\0\0").unwrap_or(payload);
    let little_endian = match tiff.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return None,
    };
    if read_u16(tiff.get(2..)?, little_endian) != Some(42) {
        return None;
    }
    let ifd_offset = usize::try_from(read_u32(tiff.get(4..)?, little_endian)?).ok()?;
    let entry_count = usize::from(read_u16(tiff.get(ifd_offset..)?, little_endian)?);
    let entries_start = ifd_offset.checked_add(2)?;

    (0..entry_count).find_map(|index| {
        let offset = entries_start.checked_add(index.checked_mul(12)?)?;
        let entry = tiff.get(offset..offset.checked_add(12)?)?;
        if read_u16(entry, little_endian) != Some(0x0112) {
            return None;
        }
        // SHORT (type 3), count 1: the value lives inline in the value field,
        // left-aligned regardless of byte order.
        if read_u16(&entry[2..], little_endian) != Some(3) {
            return None;
        }
        read_u16(&entry[8..], little_endian).filter(|value| (1..=8).contains(value))
    })
}

/// The smallest legal `Exif\0\0` APP1 payload that carries nothing but
/// Orientation: big-endian TIFF, one IFD0 entry, no IFD1, no thumbnail, no
/// maker notes, no GPS. Emitted in place of a stripped EXIF block so a portrait
/// photo keeps rendering portrait.
///
/// Re-emitting the tag is preferred over BAKING the rotation into the pixels
/// because this module's whole contract is losslessness: baking would require
/// re-encoding the scan data, which changes every pixel and cannot be undone.
fn minimal_orientation_exif(orientation: u16) -> Vec<u8> {
    let mut payload = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
    payload.extend_from_slice(&orientation.to_be_bytes());
    payload.extend_from_slice(&[0, 0]); // value-field padding
    payload.extend_from_slice(&[0, 0, 0, 0]); // "no next IFD"
    payload
}

fn jpeg_segment_bytes(marker: u8, payload: &[u8]) -> Vec<u8> {
    let length = u16::try_from(payload.len() + 2).expect("minimal EXIF payload fits in a segment");
    let mut segment = vec![0xff, marker];
    segment.extend_from_slice(&length.to_be_bytes());
    segment.extend_from_slice(payload);
    segment
}

/// Whether an APP1 payload is exactly a re-emitted orientation-only EXIF block.
/// Used by the structural verifier, so the one metadata-shaped segment the
/// scrubber is allowed to write is allowed by VALUE, not by marker class.
fn is_minimal_orientation_exif(payload: &[u8]) -> bool {
    exif_orientation(payload).is_some_and(|value| minimal_orientation_exif(value) == payload)
}

/// Classify a JPEG marker segment. `Some` = metadata-bearing (the scrubber drops
/// and reports it); `None` = the segment must be kept.
///
/// Application segments are dropped by MARKER CLASS by default: an `APPn` segment
/// carries only application metadata, so matching on a known payload prefix
/// (Exif/XMP/Photoshop) would let a crafted APP1 with an unrecognized prefix
/// survive with its PII intact. Three markers have decode-relevant payloads and
/// are kept — but ONLY when the payload is actually that decode-relevant thing:
///
/// * APP0 with a `JFIF\0` header (density/aspect). A `JFXX\0` APP0 is the JFIF
///   *extension* block, whose whole purpose is to embed a thumbnail of the
///   original image, so it is metadata and is dropped.
/// * APP2 with an `ICC_PROFILE\0` header (rendered colour). Any other APP2 —
///   most importantly `MPF\0`, the Multi-Picture Format index that points at the
///   appended secondary images this scrubber now removes — is dropped.
/// * APP14 with an `Adobe` header (colour transform). Any other APP14 is dropped.
fn jpeg_item(marker: u8, payload: &[u8]) -> Option<MetadataItem> {
    let bytes_detail = || Some(format!("{} bytes", payload.len()));
    match marker {
        // APP0: JFIF density is decode-relevant; JFXX carries a thumbnail.
        0xe0 if payload.starts_with(b"JFIF\0") => None,
        0xe0 if payload.starts_with(b"JFXX\0") => {
            Some(MetadataItem::new("JFXX thumbnail", bytes_detail(), false))
        }
        0xe0 => Some(MetadataItem::new(
            "Application metadata (APP0)",
            bytes_detail(),
            false,
        )),
        0xe1 if payload.starts_with(b"Exif\0\0") => {
            let sensitive = exif_has_gps(payload);
            let orientation = exif_orientation(payload);
            let detail = match (sensitive, orientation) {
                (true, _) => "includes GPS location".to_owned(),
                (false, Some(value)) if value != 1 => {
                    format!("{} bytes (orientation {value} preserved)", payload.len())
                }
                _ => format!("{} bytes", payload.len()),
            };
            Some(MetadataItem::new("EXIF", Some(detail), sensitive))
        }
        // Any other APP1 is XMP or an unrecognized application payload — still
        // metadata, still stripped (this closes the known-prefix bypass).
        0xe1 => Some(MetadataItem::new(
            "XMP / application metadata",
            bytes_detail(),
            false,
        )),
        // APP2: ICC colour profiles are kept; MPF and friends are not.
        0xe2 if payload.starts_with(b"ICC_PROFILE\0") => None,
        0xe2 if payload.starts_with(b"MPF\0") => Some(MetadataItem::new(
            "MPF multi-picture index",
            bytes_detail(),
            false,
        )),
        0xe2 => Some(MetadataItem::new(
            "Application metadata (APP2)",
            bytes_detail(),
            false,
        )),
        0xed if payload.starts_with(b"Photoshop 3.0") => {
            Some(MetadataItem::new("IPTC / Photoshop", bytes_detail(), false))
        }
        0xed => Some(MetadataItem::new(
            "Photoshop / application metadata",
            bytes_detail(),
            false,
        )),
        // APP14: the Adobe colour-transform marker is kept; anything else is not.
        0xee if payload.starts_with(b"Adobe") => None,
        0xee => Some(MetadataItem::new(
            "Application metadata (APP14)",
            bytes_detail(),
            false,
        )),
        0xfe => Some(MetadataItem::new(
            "JPEG comment",
            Some(lossy_detail(payload)),
            false,
        )),
        // Remaining application segments (APP3–APP12, APP15) are metadata too.
        0xe3..=0xec | 0xef => Some(MetadataItem::new(
            "Application metadata",
            bytes_detail(),
            false,
        )),
        _ => None,
    }
}

/// Find the end of entropy-coded scan data: the offset of the `FF` run that
/// begins the next real marker. Inside a scan, `FF 00` is a stuffed data byte and
/// `FF D0`–`FF D7` are restart markers, so neither terminates it.
///
/// Without this, "the scan runs to the end of the file" — which is how a whole
/// second JPEG appended after EOI (MPF / Ultra-HDR gain map / motion photo) used
/// to ride through untouched and unreported.
fn jpeg_scan_end(bytes: &[u8], from: usize) -> Result<usize, String> {
    let truncated = || "Could not read this truncated JPEG scan data.".to_owned();
    let mut position = from;
    while position < bytes.len() {
        if bytes[position] != 0xff {
            position += 1;
            continue;
        }
        let run_start = position;
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        match bytes.get(position).copied() {
            None => return Err(truncated()),
            // Stuffed byte or restart marker: still scan data.
            Some(0x00) | Some(0xd0..=0xd7) => position += 1,
            Some(_) => return Ok(run_start),
        }
    }
    Err(truncated())
}

/// Describe bytes found after the primary image's EOI. Such bytes are not part
/// of the image and are removed — see `walk_jpeg` for why they are not kept.
fn jpeg_trailing_item(trailing: &[u8]) -> MetadataItem {
    let embedded_image = find_subslice(trailing, &[0xff, 0xd8, 0xff]).is_some();
    // A trailing image usually carries its own full EXIF; check every Exif header
    // in the block rather than only the first.
    let mut sensitive = false;
    let mut search = trailing;
    while let Some(offset) = find_subslice(search, b"Exif\0\0") {
        if exif_has_gps(&search[offset..]) {
            sensitive = true;
            break;
        }
        search = &search[offset + 6..];
    }
    if !sensitive && text_mentions_location(&String::from_utf8_lossy(trailing)) {
        sensitive = true;
    }
    let mut detail = format!("{} bytes after the end of the image", trailing.len());
    if embedded_image {
        detail.push_str("; contains a secondary embedded image");
    }
    if sensitive {
        detail.push_str("; includes GPS location");
    }
    MetadataItem::new("Trailing data", Some(detail), sensitive)
}

/// Walk the whole marker stream, including every scan, to the EOI that closes the
/// primary image. Entropy-coded data is copied verbatim; the pixels are never
/// decoded or re-encoded.
///
/// Bytes after that EOI are REMOVED and reported rather than kept. They are the
/// MPF secondary image / Ultra-HDR gain map / motion-photo video that modern
/// phones append, each carrying its own complete EXIF. Keeping them would leak
/// exactly the metadata this tool promises to remove; keeping them "but scrubbed"
/// is not coherent either, because their only index (the MPF APP2 segment) is
/// itself metadata and is dropped, leaving offsets that point nowhere.
fn walk_jpeg(bytes: &[u8], strip: bool) -> Result<(Vec<MetadataItem>, Vec<u8>), String> {
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("Could not read this JPEG: invalid signature.".to_owned());
    }

    let mut output = Vec::with_capacity(bytes.len());
    output.extend_from_slice(&bytes[..2]);
    let mut items = Vec::new();
    let mut position = 2usize;
    let mut saw_scan = false;
    let mut kept_orientation = false;

    while position < bytes.len() {
        let marker_start = position;
        // Indexed access would be safe under the loop guard, but read through
        // `get` so a future change to the loop condition can never make this panic
        // (a panic aborts the whole wasm instance and would hang the worker).
        if bytes.get(position) != Some(&0xff) {
            return Err("Could not read this JPEG marker stream.".to_owned());
        }
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let Some(&marker) = bytes.get(position) else {
            return Err("Could not read this truncated JPEG marker.".to_owned());
        };
        position += 1;

        if marker == 0x00 {
            return Err("Could not read this JPEG marker stream.".to_owned());
        }
        if marker == 0xd9 {
            if !saw_scan {
                return Err("Could not read this JPEG: it ends before image scan data.".to_owned());
            }
            output.extend_from_slice(&bytes[marker_start..position]);
            if position < bytes.len() {
                items.push(jpeg_trailing_item(&bytes[position..]));
            }
            return Ok((items, output));
        }
        if matches!(marker, 0xd8 | 0x01 | 0xd0..=0xd7) {
            output.extend_from_slice(&bytes[marker_start..position]);
            continue;
        }

        let length_bytes = bytes
            .get(position..position + 2)
            .ok_or_else(|| "Could not read this truncated JPEG segment.".to_owned())?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err("Could not read this JPEG segment length.".to_owned());
        }
        let segment_end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "Could not read this truncated JPEG segment.".to_owned())?;

        if marker == 0xda {
            // SOS header, then unlength-prefixed entropy-coded data. Copy both and
            // resume marker walking at the next real marker (progressive JPEGs have
            // several scans, and the EOI is what actually ends the image).
            let scan_end = jpeg_scan_end(bytes, segment_end)?;
            output.extend_from_slice(&bytes[marker_start..scan_end]);
            position = scan_end;
            saw_scan = true;
            continue;
        }

        let payload = &bytes[position + 2..segment_end];
        let item = jpeg_item(marker, payload);
        let drop_segment = item.is_some();
        if let Some(item) = item {
            items.push(item);
        }
        if strip && drop_segment {
            // The one thing a stripped EXIF block leaves behind: a minimal EXIF
            // carrying only Orientation, written in the dropped block's place so
            // the photo keeps its rendered geometry. Orientation 1 is the default
            // and needs no segment at all.
            if marker == 0xe1 && !kept_orientation {
                if let Some(orientation) = exif_orientation(payload).filter(|value| *value != 1) {
                    output.extend_from_slice(&jpeg_segment_bytes(
                        0xe1,
                        &minimal_orientation_exif(orientation),
                    ));
                    kept_orientation = true;
                }
            }
        } else {
            output.extend_from_slice(&bytes[marker_start..segment_end]);
        }
        position = segment_end;
    }

    Err("Could not read this truncated JPEG.".to_owned())
}

/// Remove user metadata marker classes while preserving the JPEG's encoded image
/// data, its decode-relevant APP0/APP2/APP14 payloads, and its EXIF orientation.
///
/// Returns `None` when the marker stream cannot be walked safely or the result
/// fails structural verification. Callers must NOT fall back to the original
/// bytes: that would embed the unscrubbed file. `imagepdf_ops` re-encodes the
/// pixels instead.
pub(crate) fn strip_jpeg_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, stripped) = walk_jpeg(bytes, true).ok()?;
    verify_jpeg_structure(&stripped).ok()?;
    Some(stripped)
}

fn inspect_jpeg(bytes: &[u8]) -> Result<Vec<MetadataItem>, String> {
    image_dimensions(bytes, ImageFormat::Jpeg)?;
    walk_jpeg(bytes, false).map(|(items, _)| items)
}

fn scrub_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let source_dimensions = image_dimensions(bytes, ImageFormat::Jpeg)?;
    let (_, output) = walk_jpeg(bytes, true)?;
    if image_dimensions(&output, ImageFormat::Jpeg)? != source_dimensions {
        return Err("Could not validate the scrubbed JPEG.".to_owned());
    }
    verify_jpeg_structure(&output)?;
    Ok(output)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Inflate a zlib stream, refusing to allocate beyond the bomb cap so a
/// decompression bomb in a zTXt/iTXt value cannot exhaust memory.
fn inflate_zlib_bounded(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data).take(MAX_INFLATED_TEXT as u64 + 1);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output).ok()?;
    if output.len() > MAX_INFLATED_TEXT {
        return None;
    }
    Some(output)
}

/// Case-insensitive check for GPS/location markers in decoded text metadata.
fn text_mentions_location(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper.contains("GPS") || upper.contains("GEOLOCATION")
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether a byte range carries XMP.
///
/// Defined as "a complete, excisable XMP region is present" — the SAME predicate
/// removal and verification use, so the three can never disagree. Testing for a
/// bare marker instead would let a page whose visible text merely mentions
/// `rdf:RDF` be reported as metadata that then cannot be excised, deadlocking the
/// scrub on a legitimate file.
fn contains_xmp(content: &[u8]) -> bool {
    xmp_region(content).is_some()
}

/// Locate an XML tag by LOCAL name, tolerating any namespace prefix
/// (`<x:xmpmeta`, `<foo:xmpmeta`, `<xmpmeta`). Returns the index of the `<`.
fn find_xml_tag(content: &[u8], name: &[u8], closing: bool) -> Option<usize> {
    let mut from = 0usize;
    while let Some(offset) = find_subslice(content.get(from..)?, name) {
        let at = from + offset;
        let after = content.get(at + name.len()).copied();
        // A real tag name ends at whitespace, `>` or `/`; this rejects matches
        // inside attribute values such as xmlns:rdf="…rdf-syntax-ns#".
        if matches!(after, Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')) {
            let mut start = at;
            if start > 0 && content[start - 1] == b':' {
                start -= 1;
                while start > 0
                    && (content[start - 1].is_ascii_alphanumeric()
                        || matches!(content[start - 1], b'_' | b'-' | b'.'))
                {
                    start -= 1;
                }
            }
            let matched = if closing {
                start >= 2 && &content[start - 2..start] == b"</"
            } else {
                start >= 1 && content[start - 1] == b'<'
            };
            if matched {
                return Some(if closing { start - 2 } else { start - 1 });
            }
        }
        from = at + 1;
    }
    None
}

fn xml_element_region(content: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    let open = find_xml_tag(content, name, false)?;
    let close = find_xml_tag(&content[open..], name, true)? + open;
    let end = find_subslice(&content[close..], b">")? + close + 1;
    Some((open, end))
}

/// The next XMP region in `content`, as a half-open byte range.
///
/// The `<?xpacket …?>` wrapper is tried first because it is the widest region
/// when present. It is OPTIONAL in the XMP specification, so a bare `x:xmpmeta`
/// or `rdf:RDF` element is matched too — anchoring on the wrapper alone is what
/// let a wrapper-less `/Metadata` stream pass as clean.
fn xmp_region(content: &[u8]) -> Option<(usize, usize)> {
    if let Some(begin) = find_subslice(content, XMP_PACKET_BEGIN) {
        if let Some(end_marker) = find_subslice(&content[begin..], XMP_PACKET_END) {
            let end_marker = end_marker + begin;
            if let Some(close) = find_subslice(&content[end_marker..], b"?>") {
                return Some((begin, close + end_marker + 2));
            }
        }
    }
    xml_element_region(content, b"xmpmeta").or_else(|| xml_element_region(content, b"RDF"))
}

/// Remove EVERY XMP region from stream content, leaving surrounding bytes intact.
/// `None` when the content held no complete region. Removing only the first one
/// left a second packet in the same stream fully readable.
fn strip_xmp_regions(content: &[u8]) -> Option<Vec<u8>> {
    let mut buffer = content.to_vec();
    let mut removed = false;
    for _ in 0..MAX_XMP_REGIONS {
        let Some((start, end)) = xmp_region(&buffer) else {
            break;
        };
        buffer.drain(start..end);
        removed = true;
    }
    removed.then_some(buffer)
}

fn png_keyword_sensitive(keyword: &str) -> bool {
    keyword.eq_ignore_ascii_case("XML:com.adobe.xmp")
        || keyword.to_ascii_uppercase().contains("GPS")
}

fn png_keyword(data: &[u8]) -> (&[u8], &[u8]) {
    data.iter()
        .position(|byte| *byte == 0)
        .map(|separator| (&data[..separator], &data[separator + 1..]))
        .unwrap_or((data, &[]))
}

fn png_text_item(chunk_type: &[u8; 4], data: &[u8]) -> MetadataItem {
    let (keyword_bytes, rest) = png_keyword(data);
    // Judge sensitivity on the FULL keyword (a valid PNG keyword is <=79 bytes,
    // but a crafted one must not hide "GPS" past the display truncation point).
    let keyword_full = String::from_utf8_lossy(keyword_bytes).into_owned();
    let keyword = truncate_detail(&keyword_full);
    let mut sensitive = png_keyword_sensitive(&keyword_full);

    // Recover the actual text value, inflating compressed zTXt/iTXt so the tool
    // shows what was hiding instead of an opaque "(compressed)".
    let value: Option<String> = match chunk_type {
        b"tEXt" => Some(String::from_utf8_lossy(rest).into_owned()),
        // zTXt: keyword\0 + compression_method(1) + zlib stream.
        b"zTXt" => rest
            .get(1..)
            .and_then(inflate_zlib_bounded)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        // iTXt: keyword\0 + comp_flag(1) + comp_method(1) + language\0 + translated\0 + text.
        b"iTXt" => {
            let compression_flag = rest.first().copied();
            let text = rest.get(2..).map(|language_and_text| {
                let (_, translated_and_text) = png_keyword(language_and_text);
                let (_, text) = png_keyword(translated_and_text);
                text.to_vec()
            });
            match (compression_flag, text) {
                (Some(0), Some(text)) => Some(String::from_utf8_lossy(&text).into_owned()),
                (Some(_), Some(text)) => inflate_zlib_bounded(&text)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            }
        }
        _ => None,
    };

    if value.as_deref().is_some_and(text_mentions_location) {
        sensitive = true;
    }
    let detail = match value {
        Some(text) => Some(truncate_detail(&text)),
        None => Some("(unreadable compressed text)".to_owned()),
    };
    MetadataItem::new(format!("Text: {keyword}"), detail, sensitive)
}

/// The critical (decode-mandatory) PNG chunks. A critical chunk this list does
/// not name cannot be understood, and the PNG specification requires a decoder to
/// abort rather than guess — so the file is refused rather than silently altered.
const PNG_CRITICAL_CHUNKS: [&[u8; 4]; 4] = [b"IHDR", b"PLTE", b"IDAT", b"IEND"];

/// The ONLY ancillary chunks kept. Every one of them changes how the image is
/// RENDERED, which is the same reason JFIF and ICC survive on the JPEG side:
///
/// * `tRNS` — transparency; dropping it makes transparent pixels opaque.
/// * `gAMA`, `cHRM`, `sRGB`, `iCCP`, `cICP` — colour interpretation.
/// * `sBIT` — significant bits per channel, needed to rescale samples correctly.
/// * `bKGD` — background for viewers that composite against one.
/// * `hIST`, `sPLT` — palette selection data for colour-limited displays.
/// * `pHYs` — physical pixel size, i.e. the image's printed/displayed dimensions.
/// * `mDCv`, `cLLi` — HDR mastering/luminance; dropping them changes tone mapping.
/// * `acTL`, `fcTL`, `fdAT` — APNG animation; dropping them silently freezes an
///   animated PNG into a single frame.
///
/// Everything else — known-but-informational (`tEXt`/`zTXt`/`iTXt`/`eXIf`/`tIME`)
/// AND anything unrecognized — is dropped and reported. Defaulting the unknown
/// case to "keep, say nothing" is what let vendor chunks such as Fireworks'
/// `prVW` or GIMP's `caNv` carry a payload through a "this file is already
/// clean" verdict.
const PNG_KEEP_ANCILLARY: [&[u8; 4]; 16] = [
    b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"bKGD", b"hIST", b"sPLT", b"pHYs",
    b"cICP", b"mDCv", b"cLLi", b"acTL", b"fcTL", b"fdAT",
];

/// Whether a chunk is allowed to remain in a scrubbed PNG. Shared by the scrubber
/// and the structural verifier so there is one definition of "allowed to remain".
fn png_chunk_is_retained(chunk_type: &[u8; 4]) -> bool {
    PNG_CRITICAL_CHUNKS.contains(&chunk_type) || PNG_KEEP_ANCILLARY.contains(&chunk_type)
}

/// PNG chunk types are four ASCII letters; render anything else safely so a
/// crafted type cannot smuggle control characters into the report.
fn png_chunk_name(chunk_type: &[u8; 4]) -> String {
    chunk_type
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect()
}

/// A short readable preview of an unrecognized chunk's payload, so the report can
/// show WHAT was hiding rather than only how many bytes it was.
fn printable_preview(data: &[u8]) -> Option<String> {
    let printable = data
        .iter()
        .filter(|byte| byte.is_ascii_graphic() || **byte == b' ')
        .count();
    (!data.is_empty() && printable * 2 >= data.len()).then(|| lossy_detail(data))
}

fn png_unknown_item(chunk_type: &[u8; 4], data: &[u8]) -> MetadataItem {
    let sensitive = exif_has_gps(data) || text_mentions_location(&String::from_utf8_lossy(data));
    let detail = match printable_preview(data) {
        Some(preview) => format!("{} bytes: {preview}", data.len()),
        None => format!("{} bytes", data.len()),
    };
    MetadataItem::new(
        format!("Unrecognized chunk: {}", png_chunk_name(chunk_type)),
        Some(truncate_detail(&detail)),
        sensitive,
    )
}

/// Decide what happens to a PNG chunk.
///
/// `Ok(None)` = keep, `Ok(Some(item))` = drop and report, `Err` = refuse the file.
fn png_item(chunk_type: &[u8; 4], data: &[u8]) -> Result<Option<MetadataItem>, String> {
    if PNG_CRITICAL_CHUNKS.contains(&chunk_type) {
        return Ok(None);
    }
    // Bit 5 of the first byte clear (uppercase) marks a chunk CRITICAL. An unknown
    // critical chunk means the file is not a PNG we can vouch for; refuse it
    // instead of emitting something whose meaning we changed.
    if chunk_type[0].is_ascii_uppercase() {
        return Err(format!(
            "Could not read this PNG: it uses an unsupported required chunk ({}).",
            png_chunk_name(chunk_type)
        ));
    }
    if PNG_KEEP_ANCILLARY.contains(&chunk_type) {
        return Ok(None);
    }
    Ok(Some(png_metadata_item(chunk_type, data)))
}

fn png_metadata_item(chunk_type: &[u8; 4], data: &[u8]) -> MetadataItem {
    match chunk_type {
        b"tEXt" | b"zTXt" | b"iTXt" => png_text_item(chunk_type, data),
        b"eXIf" => {
            let sensitive = exif_has_gps(data);
            MetadataItem::new(
                "EXIF",
                Some(if sensitive {
                    "includes GPS location".to_owned()
                } else {
                    format!("{} bytes", data.len())
                }),
                sensitive,
            )
        }
        b"tIME" if data.len() == 7 => MetadataItem::new(
            "Last-modified time",
            Some(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                u16::from_be_bytes([data[0], data[1]]),
                data[2],
                data[3],
                data[4],
                data[5],
                data[6]
            )),
            false,
        ),
        b"tIME" => MetadataItem::new(
            "Last-modified time",
            Some(format!("{} bytes", data.len())),
            false,
        ),
        _ => png_unknown_item(chunk_type, data),
    }
}

fn walk_png(bytes: &[u8], strip: bool) -> Result<(Vec<MetadataItem>, Vec<u8>), String> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("Could not read this PNG: invalid signature.".to_owned());
    }
    let mut output = Vec::with_capacity(bytes.len());
    output.extend_from_slice(PNG_SIGNATURE);
    let mut items = Vec::new();
    let mut position = PNG_SIGNATURE.len();
    let mut chunk_index = 0usize;
    let mut saw_idat = false;
    let mut saw_iend = false;

    while position < bytes.len() {
        let chunk_start = position;
        let length_bytes: [u8; 4] = bytes
            .get(position..position + 4)
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| "Could not read this truncated PNG chunk.".to_owned())?;
        let length = usize::try_from(u32::from_be_bytes(length_bytes))
            .map_err(|_| "This PNG chunk is too large to read.".to_owned())?;
        position += 4;
        let chunk_type: [u8; 4] = bytes
            .get(position..position + 4)
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| "Could not read this truncated PNG chunk type.".to_owned())?;
        position += 4;
        let data_end = position
            .checked_add(length)
            .filter(|end| {
                end.checked_add(4)
                    .is_some_and(|crc_end| crc_end <= bytes.len())
            })
            .ok_or_else(|| "Could not read this truncated PNG chunk.".to_owned())?;
        let chunk_end = data_end + 4;
        let data = &bytes[position..data_end];
        let expected_crc = u32::from_be_bytes(
            bytes[data_end..chunk_end]
                .try_into()
                .expect("four-byte CRC slice"),
        );
        if crc32(&bytes[chunk_start + 4..data_end]) != expected_crc {
            return Err("Could not read this PNG: a chunk has an invalid CRC.".to_owned());
        }
        if chunk_index == 0 && (chunk_type != *b"IHDR" || length != 13) {
            return Err("Could not read this PNG: IHDR must be the first chunk.".to_owned());
        }
        if chunk_type == *b"IDAT" {
            saw_idat = true;
        }

        let item = png_item(&chunk_type, data)?;
        let drop_chunk = item.is_some();
        if let Some(item) = item {
            items.push(item);
        }
        if !strip || !drop_chunk {
            output.extend_from_slice(&bytes[chunk_start..chunk_end]);
        }

        position = chunk_end;
        chunk_index += 1;
        if chunk_type == *b"IEND" {
            if length != 0 || position != bytes.len() {
                return Err("Could not read this PNG: invalid IEND chunk.".to_owned());
            }
            saw_iend = true;
            break;
        }
    }

    if !saw_idat || !saw_iend {
        return Err("Could not read this truncated PNG.".to_owned());
    }
    Ok((items, output))
}

fn inspect_png(bytes: &[u8]) -> Result<Vec<MetadataItem>, String> {
    image_dimensions(bytes, ImageFormat::Png)?;
    walk_png(bytes, false).map(|(items, _)| items)
}

fn scrub_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let source_dimensions = image_dimensions(bytes, ImageFormat::Png)?;
    let (_, output) = walk_png(bytes, true)?;
    if image_dimensions(&output, ImageFormat::Png)? != source_dimensions {
        return Err("Could not validate the scrubbed PNG.".to_owned());
    }
    verify_png_structure(&output)?;
    Ok(output)
}

fn pdf_value_detail(document: &Document, value: &Object) -> String {
    let value = document
        .dereference(value)
        .map(|(_, resolved)| resolved)
        .unwrap_or(value);
    match value {
        Object::String(..) => lopdf::decode_text_string(value)
            .unwrap_or_else(|_| lossy_detail(value.as_str().unwrap_or_default())),
        Object::Name(name) => lossy_detail(name),
        Object::Boolean(value) => value.to_string(),
        Object::Integer(value) => value.to_string(),
        Object::Real(value) => value.to_string(),
        _ => "(non-text value)".to_owned(),
    }
}

fn resolved_dictionary<'a>(document: &'a Document, value: &'a Object) -> Option<&'a Dictionary> {
    document
        .dereference(value)
        .ok()
        .and_then(|(_, object)| object.as_dict().ok())
}

fn stream_plain_content(stream: &lopdf::Stream) -> Vec<u8> {
    stream
        .get_plain_content()
        .unwrap_or_else(|_| stream.content.clone())
}

/// Whether any stream object embeds XMP, regardless of how it is referenced.
/// This is what makes XMP removal (and its verification) complete: a `/Metadata`
/// reference can be dropped while the stream lingers reachable through another
/// key.
fn document_has_xmp(document: &Document) -> bool {
    document.objects.values().any(|object| match object {
        Object::Stream(stream) => contains_xmp(&stream_plain_content(stream)),
        _ => false,
    })
}

fn object_dictionary(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(dictionary) => Some(dictionary),
        Object::Stream(stream) => Some(&stream.dict),
        _ => None,
    }
}

fn object_dictionary_mut(object: &mut Object) -> Option<&mut Dictionary> {
    match object {
        Object::Dictionary(dictionary) => Some(dictionary),
        Object::Stream(stream) => Some(&mut stream.dict),
        _ => None,
    }
}

fn dictionary_name(dictionary: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
    dictionary
        .get(key)
        .and_then(Object::as_name)
        .ok()
        .map(<[u8]>::to_vec)
}

/// Annotation dictionaries carry `/T`, the comment AUTHOR — one of the most
/// common real-world PDF PII leaks, and one this tool used to neither report nor
/// remove.
///
/// Annotations are found both by `/Type /Annot` (which is optional in the spec)
/// and by walking every `/Annots` array, so an annotation that omits `/Type` is
/// still caught. `/Widget` annotations are deliberately EXCLUDED: their `/T` is
/// the form field's name, which the form needs to function, not an author.
fn annotation_ids(document: &Document) -> BTreeSet<lopdf::ObjectId> {
    let mut ids = BTreeSet::new();
    for (id, object) in document.objects.iter() {
        let Some(dictionary) = object_dictionary(object) else {
            continue;
        };
        if dictionary_name(dictionary, b"Type").as_deref() == Some(b"Annot") {
            ids.insert(*id);
        }
        if let Ok(annots) = dictionary.get(b"Annots") {
            let annots = document
                .dereference(annots)
                .map(|(_, resolved)| resolved)
                .unwrap_or(annots);
            if let Ok(array) = annots.as_array() {
                for entry in array {
                    if let Object::Reference(id) = entry {
                        ids.insert(*id);
                    }
                }
            }
        }
    }
    ids
}

fn is_scrubbable_annotation(dictionary: &Dictionary) -> bool {
    dictionary_name(dictionary, b"Subtype").as_deref() != Some(b"Widget")
}

/// Author and modification-date keys removed from an annotation dictionary. The
/// annotation's `/Contents` (the visible comment text) is document CONTENT and is
/// deliberately left alone.
const ANNOTATION_METADATA_KEYS: [&[u8]; 2] = [b"T", b"M"];
/// Signer identity inside a signature dictionary. Rewriting the file already
/// invalidates the signature cryptographically, so keeping these protects nothing
/// while leaking a name, a place and a stated reason.
const SIGNATURE_METADATA_KEYS: [&[u8]; 5] = [b"Name", b"Location", b"Reason", b"ContactInfo", b"M"];

fn is_signature_dictionary(dictionary: &Dictionary) -> bool {
    dictionary_name(dictionary, b"Type").as_deref() == Some(b"Sig")
        || (dictionary.has(b"ByteRange") && dictionary.has(b"SubFilter"))
}

fn pdf_metadata_link_count(document: &Document) -> usize {
    usize::from(document.trailer.get(b"Metadata").is_ok())
        + document
            .objects
            .values()
            .filter(|object| match object {
                Object::Dictionary(dictionary) => dictionary.has(b"Metadata"),
                Object::Stream(stream) => stream.dict.has(b"Metadata"),
                _ => false,
            })
            .count()
}

/// Sensitivity of an `/Info` key. `/Author` names a person outright; the rest are
/// reported but not flagged.
fn info_key_is_sensitive(key: &[u8]) -> bool {
    key == b"Author"
}

fn pdf_items(document: &Document) -> Result<Vec<MetadataItem>, String> {
    let mut items = Vec::new();
    if let Ok(info_value) = document.trailer.get(b"Info") {
        let info = resolved_dictionary(document, info_value)
            .ok_or_else(|| "Could not read this PDF's document information.".to_owned())?;
        // EVERY key is reported, not a known-key list: a producer's custom key
        // (`/Company`, `/SourceModified`, …) is metadata too, and reporting only
        // the eight standard ones meant a PDF whose Info held nothing else was
        // announced as already clean while the whole dictionary was still there.
        for (key, value) in info.iter() {
            items.push(MetadataItem::new(
                String::from_utf8_lossy(key).into_owned(),
                Some(pdf_value_detail(document, value)),
                info_key_is_sensitive(key),
            ));
        }
    }

    let metadata_count = pdf_metadata_link_count(document);
    let embedded_xmp = document_has_xmp(document);
    if metadata_count > 0 || embedded_xmp {
        let detail = if metadata_count > 0 {
            format!(
                "{metadata_count} {}",
                if metadata_count == 1 {
                    "block"
                } else {
                    "blocks"
                }
            )
        } else {
            "embedded packet".to_owned()
        };
        items.push(MetadataItem::new("XMP metadata", Some(detail), false));
    }

    for id in annotation_ids(document) {
        let Some(dictionary) = document.objects.get(&id).and_then(object_dictionary) else {
            continue;
        };
        if !is_scrubbable_annotation(dictionary) {
            continue;
        }
        if let Ok(author) = dictionary.get(b"T") {
            items.push(MetadataItem::new(
                "Annotation author",
                Some(pdf_value_detail(document, author)),
                true,
            ));
        }
        if let Ok(modified) = dictionary.get(b"M") {
            items.push(MetadataItem::new(
                "Annotation modified",
                Some(pdf_value_detail(document, modified)),
                false,
            ));
        }
    }

    for object in document.objects.values() {
        let Some(dictionary) = object_dictionary(object) else {
            continue;
        };
        if dictionary.has(b"PieceInfo") {
            items.push(MetadataItem::new(
                "PieceInfo (private application data)",
                Some("removed".to_owned()),
                false,
            ));
        }
        // Attachments and XFA form data are DOCUMENT CONTENT, not metadata:
        // deleting them would silently destroy something the user put there. They
        // are reported instead, because an attachment is often the biggest leak in
        // the file and the user has to be told it is still inside.
        if dictionary_name(dictionary, b"Type").as_deref() == Some(b"Filespec")
            || dictionary.has(b"EF")
        {
            let name = dictionary
                .get(b"UF")
                .or_else(|_| dictionary.get(b"F"))
                .map(|value| pdf_value_detail(document, value))
                .unwrap_or_else(|_| "unnamed".to_owned());
            items.push(MetadataItem::new(
                "Embedded file",
                Some(format!("{name} (kept: an attachment is document content)")),
                true,
            ));
        }
        if let Ok(acroform) = dictionary.get(b"AcroForm") {
            if resolved_dictionary(document, acroform).is_some_and(|form| form.has(b"XFA")) {
                items.push(MetadataItem::new(
                    "XFA form data",
                    Some("kept: XFA is the form itself, and removing it breaks it".to_owned()),
                    true,
                ));
            }
        }
        if is_signature_dictionary(dictionary) {
            let signer = dictionary
                .get(b"Name")
                .map(|value| pdf_value_detail(document, value))
                .unwrap_or_else(|_| "unnamed signer".to_owned());
            items.push(MetadataItem::new(
                "Digital signature",
                Some(format!(
                    "{signer} (the signature is invalidated by scrubbing)"
                )),
                true,
            ));
        }
    }
    Ok(items)
}

fn inspect_pdf(bytes: &[u8]) -> Result<Vec<MetadataItem>, String> {
    let document = load_pdf(bytes)?;
    pdf_items(&document)
}

fn scrub_annotation_dictionary(dictionary: &mut Dictionary) {
    if !is_scrubbable_annotation(dictionary) {
        return;
    }
    for key in ANNOTATION_METADATA_KEYS {
        dictionary.remove(key);
    }
}

fn scrub_pdf(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut document = load_pdf(bytes)?;
    let source_page_count = document.get_pages().len();
    let annotations = annotation_ids(&document);

    document.trailer.remove(b"Info");
    document.trailer.remove(b"Metadata");
    for (id, object) in document.objects.iter_mut() {
        let is_annotation = annotations.contains(id);
        if let Object::Stream(stream) = object {
            // Excise every XMP region embedded directly in the stream content, so
            // XMP is gone even when the stream stays reachable through a reference
            // other than /Metadata (dropping the reference + prune alone would
            // leave it). Surrounding content bytes are preserved.
            let content = stream_plain_content(stream);
            if let Some(stripped) = strip_xmp_regions(&content) {
                stream.set_plain_content(stripped);
            }
        }
        let Some(dictionary) = object_dictionary_mut(object) else {
            continue;
        };
        dictionary.remove(b"Metadata");
        // Private application round-trip data (Illustrator/InDesign work files,
        // local paths, edit timestamps). Never needed to render the page.
        dictionary.remove(b"PieceInfo");
        if is_annotation {
            scrub_annotation_dictionary(dictionary);
        }
        if is_signature_dictionary(dictionary) {
            for key in SIGNATURE_METADATA_KEYS {
                dictionary.remove(key);
            }
        }
        // Annotations written inline into an /Annots array rather than as
        // indirect objects are not reachable through `annotations`.
        if let Ok(Object::Array(array)) = dictionary.get_mut(b"Annots") {
            for entry in array.iter_mut() {
                if let Object::Dictionary(annotation) = entry {
                    scrub_annotation_dictionary(annotation);
                }
            }
        }
    }
    document.prune_objects();

    let mut output = Vec::new();
    document
        .save_to(&mut output)
        .map_err(|error| format!("Could not create the scrubbed PDF: {error}"))?;
    let reparsed = load_pdf(&output)
        .map_err(|error| format!("Could not validate the scrubbed PDF: {error}"))?;
    if reparsed.get_pages().len() != source_page_count {
        return Err("Could not validate the scrubbed PDF page count.".to_owned());
    }
    verify_pdf_structure(&reparsed)?;
    Ok(output)
}

// ---------------------------------------------------------------------------
// Structural verification
//
// The scrubber used to verify itself by re-running `inspect()` on its own output.
// That oracle CANNOT fail for a detector gap: anything `inspect` is blind to is
// equally invisible in the re-inspection, so every bug above shipped with a
// green "re-inspection passed". These checks instead answer a different question
// — WHAT IS STILL IN THE FILE — with their own parsers, and they pass only when
// nothing outside an explicit keep-list survived. A future gap in the reporting
// layer therefore fails the scrub loudly instead of producing a clean verdict.
// ---------------------------------------------------------------------------

/// Independent chunk walk: no chunk outside the retained set may remain, and
/// nothing may follow IEND.
fn verify_png_structure(bytes: &[u8]) -> Result<(), String> {
    let unclean = |what: &str| Err(format!("The scrubbed PNG still contains {what}."));
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("The scrubbed PNG lost its signature.".to_owned());
    }
    let mut position = PNG_SIGNATURE.len();
    while let Some(header) = bytes.get(position..position + 8) {
        let length = usize::try_from(u32::from_be_bytes(
            header[..4].try_into().expect("four-byte length"),
        ))
        .map_err(|_| "The scrubbed PNG has an unreadable chunk length.".to_owned())?;
        let chunk_type: [u8; 4] = header[4..].try_into().expect("four-byte chunk type");
        let chunk_end = position
            .checked_add(12)
            .and_then(|base| base.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "The scrubbed PNG is truncated.".to_owned())?;
        if chunk_type == *b"IEND" {
            return if chunk_end == bytes.len() {
                Ok(())
            } else {
                unclean("bytes after IEND")
            };
        }
        if !png_chunk_is_retained(&chunk_type) {
            return unclean(&format!("a `{}` chunk", png_chunk_name(&chunk_type)));
        }
        position = chunk_end;
    }
    Err("The scrubbed PNG is missing its IEND chunk.".to_owned())
}

/// Independent marker walk: no metadata-class segment may remain, the only APP1
/// allowed is a re-emitted orientation-only EXIF, and the file must end at EOI.
fn verify_jpeg_structure(bytes: &[u8]) -> Result<(), String> {
    let unclean = |what: &str| Err(format!("The scrubbed JPEG still contains {what}."));
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("The scrubbed JPEG lost its signature.".to_owned());
    }
    let truncated = || "The scrubbed JPEG is truncated.".to_owned();
    let mut position = 2usize;
    loop {
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *bytes.get(position).ok_or_else(truncated)?;
        position += 1;
        if marker == 0xd9 {
            return if position == bytes.len() {
                Ok(())
            } else {
                unclean("bytes after the end-of-image marker")
            };
        }
        if matches!(marker, 0xd8 | 0x01 | 0xd0..=0xd7) {
            continue;
        }
        let length = bytes
            .get(position..position + 2)
            .map(|value| usize::from(u16::from_be_bytes([value[0], value[1]])))
            .filter(|length| *length >= 2)
            .ok_or_else(truncated)?;
        let segment_end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(truncated)?;
        let payload = &bytes[position + 2..segment_end];
        match marker {
            0xfe => return unclean("a comment segment"),
            0xe0 if !payload.starts_with(b"JFIF\0") => return unclean("a non-JFIF APP0 segment"),
            0xe1 if !is_minimal_orientation_exif(payload) => {
                return unclean("an APP1 segment that is not an orientation-only EXIF block")
            }
            0xe2 if !payload.starts_with(b"ICC_PROFILE\0") => {
                return unclean("a non-ICC APP2 segment")
            }
            0xee if !payload.starts_with(b"Adobe") => return unclean("a non-Adobe APP14 segment"),
            0xe3..=0xed | 0xef => return unclean("an application metadata segment"),
            _ => {}
        }
        // Every branch advances `position` past at least the marker byte, so the
        // loop always terminates at EOI or at the truncation error above.
        position = if marker == 0xda {
            jpeg_scan_end(bytes, segment_end).map_err(|_| truncated())?
        } else {
            segment_end
        };
    }
}

/// Independent object-graph walk: none of the keys or byte patterns the scrubber
/// removes may survive anywhere in the saved document.
fn verify_pdf_structure(document: &Document) -> Result<(), String> {
    let unclean = |what: &str| Err(format!("The scrubbed PDF still contains {what}."));
    if document.trailer.get(b"Info").is_ok() || document.trailer.get(b"Metadata").is_ok() {
        return unclean("a trailer metadata entry");
    }
    let annotations = annotation_ids(document);
    for (id, object) in document.objects.iter() {
        if let Object::Stream(stream) = object {
            if contains_xmp(&stream_plain_content(stream)) {
                return unclean("an XMP packet");
            }
        }
        let Some(dictionary) = object_dictionary(object) else {
            continue;
        };
        if dictionary.has(b"Metadata") {
            return unclean("a /Metadata reference");
        }
        if dictionary.has(b"PieceInfo") {
            return unclean("a /PieceInfo entry");
        }
        if is_signature_dictionary(dictionary)
            && SIGNATURE_METADATA_KEYS
                .iter()
                .any(|key| dictionary.has(key))
        {
            return unclean("signer identity in a signature dictionary");
        }
        let inline_annotations = dictionary
            .get(b"Annots")
            .ok()
            .and_then(|annots| annots.as_array().ok())
            .map(|array| {
                array.iter().any(|entry| match entry {
                    Object::Dictionary(annotation) => {
                        is_scrubbable_annotation(annotation)
                            && ANNOTATION_METADATA_KEYS
                                .iter()
                                .any(|key| annotation.has(key))
                    }
                    _ => false,
                })
            })
            .unwrap_or(false);
        if inline_annotations {
            return unclean("authorship on an inline annotation");
        }
        if annotations.contains(id)
            && is_scrubbable_annotation(dictionary)
            && ANNOTATION_METADATA_KEYS
                .iter()
                .any(|key| dictionary.has(key))
        {
            return unclean("annotation authorship");
        }
    }
    Ok(())
}

/// Structural check that a file carries none of the metadata this tool removes.
///
/// Deliberately independent of `inspect`: it is the oracle a caller should use to
/// confirm a scrub worked, precisely because it cannot inherit a blind spot from
/// the reporting layer.
fn verify_removed(bytes: &[u8]) -> Result<(), String> {
    match detect_format(bytes)? {
        MetadataFormat::Pdf => verify_pdf_structure(&load_pdf(bytes)?),
        MetadataFormat::Jpeg => verify_jpeg_structure(bytes),
        MetadataFormat::Png => verify_png_structure(bytes),
    }
}

/// Pure core for native tests: describe metadata without mutating the input.
fn inspect(bytes: &[u8]) -> Result<String, String> {
    let format = detect_format(bytes)?;
    let items = match format {
        MetadataFormat::Pdf => inspect_pdf(bytes)?,
        MetadataFormat::Jpeg => inspect_jpeg(bytes)?,
        MetadataFormat::Png => inspect_png(bytes)?,
    };
    Ok(report_json(format, &items))
}

/// Pure core for native tests: remove only qualified metadata containers.
fn scrub(bytes: &[u8]) -> Result<Vec<u8>, String> {
    match detect_format(bytes)? {
        MetadataFormat::Pdf => scrub_pdf(bytes),
        MetadataFormat::Jpeg => scrub_jpeg(bytes),
        MetadataFormat::Png => scrub_png(bytes),
    }
}

/// Describe the metadata found in a supported file as JSON.
#[wasm_bindgen]
pub fn inspect_metadata(bytes: &[u8]) -> Result<String, JsValue> {
    inspect(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Return a supported file with its qualified metadata removed.
#[wasm_bindgen]
pub fn scrub_metadata(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    scrub(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Confirm structurally that a file carries no removable metadata.
///
/// Callers verifying a scrub must use THIS, not a second `inspect_metadata`
/// call: re-inspecting asks the same detector the same question and so can never
/// catch anything that detector does not already know how to see.
#[wasm_bindgen]
pub fn verify_metadata_removed(bytes: &[u8]) -> Result<(), JsValue> {
    verify_removed(bytes).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{crc32, inspect, scrub, verify_removed, SUPPORTED_FORMATS_ERROR};
    use image::{
        codecs::png::PngEncoder, ExtendedColorType, GenericImageView, ImageEncoder, Rgb, RgbImage,
        Rgba, RgbaImage,
    };
    use jpeg_encoder::{ColorType, Encoder};
    use lopdf::{dictionary, Document, Object, Stream};

    fn jpeg_fixture(width: u16, height: u16) -> Vec<u8> {
        let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
            let noise = ((x * 73 + y * 151 + x * y * 17) % 251) as u8;
            Rgb([noise, noise.rotate_left(2), noise.rotate_left(4)])
        });
        let mut bytes = Vec::new();
        Encoder::new(&mut bytes, 90)
            .encode(image.as_raw(), width, height, ColorType::Rgb)
            .expect("fixture JPEG should encode");
        bytes
    }

    fn exif_gps(tiff_prefix: bool) -> Vec<u8> {
        let mut bytes = if tiff_prefix {
            b"Exif\0\0".to_vec()
        } else {
            Vec::new()
        };
        bytes.extend_from_slice(b"II*\0\x08\0\0\0\x01\0\x25\x88\x04\0\x01\0\0\0\x1a\0\0\0\0\0\0\0");
        bytes
    }

    fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let length = u16::try_from(payload.len() + 2).expect("fixture segment should fit");
        let mut segment = vec![0xff, marker];
        segment.extend_from_slice(&length.to_be_bytes());
        segment.extend_from_slice(payload);
        segment
    }

    fn inject_jpeg_segments(jpeg: &[u8], segments: &[Vec<u8>]) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&jpeg[..2]);
        for segment in segments {
            output.extend_from_slice(segment);
        }
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn scan_suffix(jpeg: &[u8]) -> &[u8] {
        let position = jpeg
            .windows(2)
            .position(|window| window == [0xff, 0xda])
            .expect("fixture should have an SOS marker");
        &jpeg[position..]
    }

    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                ((x * 29 + y * 7) % 256) as u8,
                ((x * 11 + y * 41) % 256) as u8,
                ((x * 53 + y * 17) % 256) as u8,
                255,
            ])
        });
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .expect("fixture PNG should encode");
        bytes
    }

    fn png_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(
            &u32::try_from(data.len())
                .expect("fixture chunk should fit")
                .to_be_bytes(),
        );
        chunk.extend_from_slice(chunk_type);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        chunk
    }

    fn inject_png_chunks(png: &[u8], chunks: &[Vec<u8>]) -> Vec<u8> {
        let ihdr_end = 8 + 4 + 4 + 13 + 4;
        let mut output = Vec::new();
        output.extend_from_slice(&png[..ihdr_end]);
        for chunk in chunks {
            output.extend_from_slice(chunk);
        }
        output.extend_from_slice(&png[ihdr_end..]);
        output
    }

    fn chunk_data(png: &[u8], wanted: &[u8; 4]) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        let mut position = 8usize;
        while position + 12 <= png.len() {
            let length =
                u32::from_be_bytes(png[position..position + 4].try_into().unwrap()) as usize;
            let chunk_type = &png[position + 4..position + 8];
            let data_start = position + 8;
            let data_end = data_start + length;
            if chunk_type == wanted {
                chunks.push(png[data_start..data_end].to_vec());
            }
            position = data_end + 4;
        }
        chunks
    }

    fn pdf_fixture(with_metadata: bool) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let metadata_id = with_metadata.then(|| {
            document.add_object(Stream::new(
                dictionary! {
                    "Type" => "Metadata",
                    "Subtype" => "XML",
                },
                b"<x:xmpmeta>private editing history</x:xmpmeta>".to_vec(),
            ))
        });
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! {},
            "Contents" => content_id,
        };
        if let Some(metadata_id) = metadata_id {
            page.set("Metadata", metadata_id);
        }
        let page_id = document.add_object(page);
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let mut catalog = dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        };
        if let Some(metadata_id) = metadata_id {
            catalog.set("Metadata", metadata_id);
        }
        let catalog_id = document.add_object(catalog);
        document.trailer.set("Root", catalog_id);
        if with_metadata {
            let info_id = document.add_object(dictionary! {
                "Title" => Object::string_literal("Location scouting"),
                "Author" => Object::string_literal("Ada Example"),
                "Creator" => Object::string_literal("Layout Tool"),
                "Producer" => Object::string_literal("PDF Engine"),
            });
            document.trailer.set("Info", info_id);
        }

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("fixture PDF should serialize");
        bytes
    }

    fn dimensions(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes)
            .expect("image should decode")
            .dimensions()
    }

    #[test]
    fn jpeg_exif_gps_is_reported_and_removed_losslessly() {
        let clean = jpeg_fixture(64, 32);
        let source = inject_jpeg_segments(&clean, &[jpeg_segment(0xe1, &exif_gps(true))]);

        let report = inspect(&source).expect("EXIF JPEG should inspect");
        assert!(report.contains("\"label\":\"EXIF\""));
        assert!(report.contains("includes GPS location"));
        assert!(report.contains("\"sensitive\":true"));

        let scrubbed = scrub(&source).expect("EXIF JPEG should scrub");
        eprintln!(
            "JPEG GPS fixture: {} bytes -> {} bytes",
            source.len(),
            scrubbed.len()
        );
        assert!(!scrubbed.windows(6).any(|window| window == b"Exif\0\0"));
        assert!(!scrubbed.windows(2).any(|window| window == [0xff, 0xe1]));
        assert_eq!(dimensions(&scrubbed), (64, 32));
        assert_eq!(scan_suffix(&scrubbed), scan_suffix(&source));
        assert_eq!(scrubbed, clean);
    }

    #[test]
    fn jpeg_xmp_iptc_and_comments_are_removed_but_icc_is_preserved() {
        let clean = jpeg_fixture(24, 12);
        let icc = jpeg_segment(0xe2, b"ICC_PROFILE\0kept color data");
        let source = inject_jpeg_segments(
            &clean,
            &[
                icc.clone(),
                jpeg_segment(0xe1, b"http://ns.adobe.com/xap/1.0/\0<xmp>history</xmp>"),
                jpeg_segment(0xed, b"Photoshop 3.0\0creator and city"),
                jpeg_segment(0xfe, b"shared from the studio"),
            ],
        );

        let report = inspect(&source).expect("metadata JPEG should inspect");
        assert!(report.contains("XMP"));
        assert!(report.contains("IPTC / Photoshop"));
        assert!(report.contains("shared from the studio"));
        let scrubbed = scrub(&source).expect("metadata JPEG should scrub");
        assert!(scrubbed.windows(icc.len()).any(|window| window == icc));
        assert!(!scrubbed.windows(9).any(|window| window == b"Photoshop"));
        assert!(!scrubbed.windows(4).any(|window| window == b"<xmp"));
        assert_eq!(scan_suffix(&scrubbed), scan_suffix(&source));
    }

    #[test]
    fn clean_jpeg_reports_empty_and_remains_byte_identical() {
        let clean = jpeg_fixture(16, 8);
        assert_eq!(inspect(&clean).unwrap(), "{\"kind\":\"jpeg\",\"items\":[]}");
        assert_eq!(scrub(&clean).unwrap(), clean);
    }

    #[test]
    fn jpeg_app1_without_a_known_prefix_is_still_stripped() {
        let clean = jpeg_fixture(20, 10);
        // An APP1 that is neither the Exif nor the canonical XMP prefix — the exact
        // crafted segment a payload-prefix scrubber would have left in place.
        let sneaky = jpeg_segment(0xe1, b"http://ns.adobe.com/xap\0<x>Location 60,24</x>");
        let source = inject_jpeg_segments(&clean, std::slice::from_ref(&sneaky));

        let report = inspect(&source).expect("crafted APP1 JPEG should inspect");
        assert!(report.contains("application metadata"));
        let scrubbed = scrub(&source).expect("crafted APP1 JPEG should scrub");
        assert!(!scrubbed.windows(2).any(|window| window == [0xff, 0xe1]));
        assert!(!scrubbed.windows(8).any(|window| window == b"Location"));
        // Nothing but the metadata segment was touched.
        assert_eq!(scrubbed, clean);
    }

    /// Decode-relevant PAYLOADS survive — not decode-relevant marker classes.
    ///
    /// The previous version of this test named itself after the marker numbers and
    /// asserted `scrubbed == source` for a fixture holding an arbitrary APP2. That
    /// made it the written specification of the MPF hole: it forbade ever stripping
    /// an APP2, so no APP2 leak could make it fail. It now fixes the fixture to
    /// real JFIF/ICC/Adobe payloads, and its negative twin
    /// (`jpeg_drops_non_decode_payloads_in_app0_and_app2`) covers the same markers
    /// carrying anything else.
    #[test]
    fn jpeg_keeps_decode_relevant_payloads_byte_for_byte() {
        let clean = jpeg_fixture(18, 9);
        let jfif = jpeg_segment(0xe0, b"JFIF\0\x01\x02\x01\0\x48\0\x48\0\0");
        let icc = jpeg_segment(0xe2, b"ICC_PROFILE\0color");
        // A well-formed APP14 Adobe marker (Adobe + version/flags/transform); a
        // malformed one would make the JPEG undecodable, which is a different test.
        let adobe = jpeg_segment(0xee, b"Adobe\0\x64\0\0\0\0\x01");
        let source = inject_jpeg_segments(&clean, &[jfif.clone(), icc.clone(), adobe.clone()]);

        // None of these three payloads is user metadata: the report is empty and
        // the scrub is a no-op.
        assert_eq!(
            inspect(&source).unwrap(),
            "{\"kind\":\"jpeg\",\"items\":[]}"
        );
        let scrubbed = scrub(&source).expect("decode-relevant JPEG should scrub");
        for segment in [&jfif, &icc, &adobe] {
            assert!(
                contains(&scrubbed, segment),
                "a decode-relevant segment must survive byte-for-byte"
            );
        }
        assert_eq!(scrubbed, source);
    }

    #[test]
    fn png_text_and_exif_are_reported_and_removed_without_touching_idat() {
        let clean = png_fixture(32, 16);
        let source = inject_png_chunks(
            &clean,
            &[
                png_chunk(b"tEXt", b"Software\0Adobe"),
                png_chunk(b"eXIf", &exif_gps(false)),
            ],
        );
        let source_idat = chunk_data(&source, b"IDAT");

        let report = inspect(&source).expect("metadata PNG should inspect");
        assert!(report.contains("Text: Software"));
        assert!(report.contains("Adobe"));
        assert!(report.contains("EXIF"));
        assert!(report.contains("includes GPS location"));
        let scrubbed = scrub(&source).expect("metadata PNG should scrub");
        assert!(chunk_data(&scrubbed, b"tEXt").is_empty());
        assert!(chunk_data(&scrubbed, b"eXIf").is_empty());
        assert_eq!(chunk_data(&scrubbed, b"IDAT"), source_idat);
        assert_eq!(dimensions(&scrubbed), (32, 16));
        assert_eq!(scrubbed, clean);
    }

    #[test]
    fn png_compressed_text_itxt_and_time_are_described() {
        let clean = png_fixture(8, 4);
        let source = inject_png_chunks(
            &clean,
            &[
                png_chunk(b"zTXt", b"GPS Position\0\0compressed"),
                png_chunk(b"iTXt", b"Caption\0\0\0en\0Caption\0A local file"),
                png_chunk(b"tIME", &[0x07, 0xea, 7, 21, 12, 34, 56]),
            ],
        );
        let report = inspect(&source).expect("PNG metadata should inspect");
        assert!(report.contains("Text: GPS Position"));
        // The zTXt payload here is not valid zlib, so it cannot be inflated; the
        // tool says so honestly (valid compressed text is inflated — see the
        // png_compressed_ztxt/itxt tests) while still stripping the chunk.
        assert!(report.contains("(unreadable compressed text)"));
        assert!(report.contains("\"sensitive\":true"));
        assert!(report.contains("A local file"));
        assert!(report.contains("2026-07-21 12:34:56 UTC"));
        assert_eq!(scrub(&source).unwrap(), clean);
    }

    #[test]
    fn clean_png_reports_empty_and_remains_byte_identical() {
        let clean = png_fixture(10, 5);
        assert_eq!(inspect(&clean).unwrap(), "{\"kind\":\"png\",\"items\":[]}");
        assert_eq!(scrub(&clean).unwrap(), clean);
    }

    #[test]
    fn pdf_info_and_xmp_are_reported_and_removed() {
        let source = pdf_fixture(true);
        let report = inspect(&source).expect("metadata PDF should inspect");
        assert!(report.contains("Location scouting"));
        assert!(report.contains("Ada Example"));
        assert!(report.contains("\"label\":\"Author\""));
        assert!(report.contains("XMP metadata"));

        let scrubbed = scrub(&source).expect("metadata PDF should scrub");
        eprintln!(
            "PDF Info fixture: {} bytes -> {} bytes",
            source.len(),
            scrubbed.len()
        );
        let document = Document::load_mem(&scrubbed).expect("scrubbed PDF should parse");
        assert_eq!(document.get_pages().len(), 1);
        assert!(document.trailer.get(b"Info").is_err());
        assert!(document.objects.values().all(|object| match object {
            Object::Dictionary(dictionary) => !dictionary.has(b"Metadata"),
            Object::Stream(stream) => !stream.dict.has(b"Metadata"),
            _ => true,
        }));
        // The payload strings themselves are gone from the bytes — the check that
        // does not depend on any detector agreeing.
        assert!(!contains(&scrubbed, b"Location scouting"));
        assert!(!contains(&scrubbed, b"Ada Example"));
        verify_removed(&scrubbed).expect("scrubbed PDF is structurally clean");
    }

    #[test]
    fn clean_pdf_reports_empty_and_stays_valid() {
        let source = pdf_fixture(false);
        assert_eq!(inspect(&source).unwrap(), "{\"kind\":\"pdf\",\"items\":[]}");
        let scrubbed = scrub(&source).expect("clean PDF should scrub");
        assert_eq!(Document::load_mem(&scrubbed).unwrap().get_pages().len(), 1);
        verify_removed(&scrubbed).expect("scrubbed PDF is structurally clean");
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).expect("zlib fixture should encode");
        encoder.finish().expect("zlib fixture should finish")
    }

    #[test]
    fn png_compressed_ztxt_is_inflated_reported_and_removed() {
        let clean = png_fixture(16, 8);
        // keyword "Comment" \0 + compression_method(0) + zlib(text with GPS).
        let mut ztxt = b"Comment\0\0".to_vec();
        ztxt.extend_from_slice(&zlib(
            b"Shot on MtclabCam, GPSLatitude 60.1, GPSLongitude 24.9",
        ));
        let source = inject_png_chunks(&clean, &[png_chunk(b"zTXt", &ztxt)]);
        let source_idat = chunk_data(&source, b"IDAT");

        let report = inspect(&source).expect("zTXt PNG should inspect");
        assert!(
            report.contains("GPSLatitude"),
            "inflated value is shown: {report}"
        );
        assert!(
            report.contains("\"sensitive\":true"),
            "GPS in inflated text flags sensitive: {report}"
        );
        let scrubbed = scrub(&source).expect("zTXt PNG should scrub");
        assert!(!scrubbed.windows(4).any(|window| window == b"zTXt"));
        assert_eq!(
            chunk_data(&scrubbed, b"IDAT"),
            source_idat,
            "IDAT untouched"
        );
    }

    #[test]
    fn png_compressed_itxt_is_inflated() {
        let clean = png_fixture(16, 8);
        // keyword \0 flag(1) method(0) language\0 translated\0 + zlib(text).
        let mut itxt = b"XML:com.adobe.xmp\0\x01\0en\0\0".to_vec();
        itxt.extend_from_slice(&zlib(b"<x:xmpmeta>studio edit trail</x:xmpmeta>"));
        let source = inject_png_chunks(&clean, &[png_chunk(b"iTXt", &itxt)]);

        let report = inspect(&source).expect("iTXt PNG should inspect");
        assert!(
            report.contains("studio edit trail"),
            "compressed iTXt inflated: {report}"
        );
        let scrubbed = scrub(&source).expect("iTXt PNG should scrub");
        assert!(!scrubbed.windows(4).any(|window| window == b"iTXt"));
    }

    /// A PDF whose XMP stream is reachable from the catalog through `/AF`, not
    /// `/Metadata` — dropping the `/Metadata` reference and pruning would leave it.
    fn pdf_with_reachable_xmp_stream() -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let xmp_id = document.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<?xpacket begin=\"\xef\xbb\xbf\"?><x:xmpmeta>secret author line</x:xmpmeta><?xpacket end=\"w\"?>".to_vec(),
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! {},
            "Contents" => content_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "AF" => vec![Object::Reference(xmp_id)],
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("embedded-XMP fixture should serialize");
        bytes
    }

    #[test]
    fn pdf_xmp_reachable_without_metadata_ref_is_reported_and_removed() {
        let source = pdf_with_reachable_xmp_stream();
        let report = inspect(&source).expect("embedded-XMP PDF should inspect");
        assert!(
            report.contains("XMP metadata"),
            "reports embedded XMP: {report}"
        );

        let scrubbed = scrub(&source).expect("embedded-XMP PDF should scrub");
        assert!(!scrubbed.windows(9).any(|window| window == b"<?xpacket"));
        assert!(!scrubbed
            .windows(18)
            .any(|window| window == b"secret author line"));
        assert_eq!(Document::load_mem(&scrubbed).unwrap().get_pages().len(), 1);
        verify_removed(&scrubbed).expect("scrubbed PDF is structurally clean");
    }

    #[test]
    fn encrypted_pdf_is_rejected_for_inspect_and_scrub() {
        let mut document = Document::load_mem(&pdf_fixture(false)).unwrap();
        document.trailer.set("Encrypt", dictionary! {});
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        assert!(inspect(&bytes).unwrap_err().contains("password-protected"));
        assert!(scrub(&bytes).unwrap_err().contains("password-protected"));
    }

    #[test]
    fn detected_but_truncated_files_return_errors_without_panicking() {
        for bytes in [
            b"%PDF-1.7\ngarbage".as_slice(),
            b"\xff\xd8\xff\xe1\0\x10Exif".as_slice(),
            b"\x89PNG\r\n\x1a\ntruncated".as_slice(),
        ] {
            assert!(inspect(bytes).is_err());
            assert!(scrub(bytes).is_err());
        }
    }

    #[test]
    fn empty_garbage_and_gif_return_the_supported_formats_error() {
        for bytes in [
            b"".as_slice(),
            b"not a supported file".as_slice(),
            b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff".as_slice(),
        ] {
            assert_eq!(inspect(bytes).unwrap_err(), SUPPORTED_FORMATS_ERROR);
            assert_eq!(scrub(bytes).unwrap_err(), SUPPORTED_FORMATS_ERROR);
        }
    }

    // ------------------------------------------------------------------
    // Regression gates for the "we said it was clean and it was not" class.
    // Each of these was written against the pre-fix code and observed to FAIL.
    // ------------------------------------------------------------------

    /// Build an `Exif\0\0` APP1 payload: big-endian TIFF, IFD0 carrying the given
    /// raw entries (tag, type, count, 4-byte value field).
    fn exif_payload(entries: &[(u16, u16, u32, [u8; 4])], with_prefix: bool) -> Vec<u8> {
        let mut bytes = if with_prefix {
            b"Exif\0\0".to_vec()
        } else {
            Vec::new()
        };
        bytes.extend_from_slice(b"MM\0\x2a\0\0\0\x08");
        bytes.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_be_bytes());
        for (tag, kind, count, value) in entries {
            bytes.extend_from_slice(&tag.to_be_bytes());
            bytes.extend_from_slice(&kind.to_be_bytes());
            bytes.extend_from_slice(&count.to_be_bytes());
            bytes.extend_from_slice(value);
        }
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    fn orientation_entry(value: u16) -> (u16, u16, u32, [u8; 4]) {
        (0x0112, 3, 1, [(value >> 8) as u8, value as u8, 0, 0])
    }

    const GPS_POINTER_ENTRY: (u16, u16, u32, [u8; 4]) = (0x8825, 4, 1, [0, 0, 0, 0x1a]);

    /// Read the EXIF Orientation tag out of a JPEG the way a viewer does, so the
    /// test asserts the rendered geometry survives rather than that some bytes do.
    fn jpeg_orientation(jpeg: &[u8]) -> Option<u16> {
        let start = jpeg.windows(6).position(|window| window == b"Exif\0\0")?;
        let tiff = &jpeg[start + 6..];
        let big_endian = tiff.starts_with(b"MM");
        let ifd = u32::from_be_bytes(tiff.get(4..8)?.try_into().ok()?) as usize;
        let count = u16::from_be_bytes(tiff.get(ifd..ifd + 2)?.try_into().ok()?) as usize;
        assert!(big_endian, "fixture and emitter both use big-endian TIFF");
        (0..count).find_map(|index| {
            let entry = tiff.get(ifd + 2 + index * 12..ifd + 2 + index * 12 + 12)?;
            (u16::from_be_bytes(entry[..2].try_into().ok()?) == 0x0112)
                .then(|| u16::from_be_bytes(entry[8..10].try_into().ok().unwrap()))
        })
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// FINDING 1: an unknown ancillary PNG chunk was preserved AND unreported —
    /// `png_item`'s `_ => None` arm meant "keep silently".
    #[test]
    fn png_unknown_ancillary_chunks_are_reported_and_removed() {
        let clean = png_fixture(32, 16);
        let payload = b"GPSLatitude 60.1699 GPSLongitude 24.9384 shot at home";
        let source = inject_png_chunks(
            &clean,
            &[
                png_chunk(b"prVW", payload),
                png_chunk(b"caNv", b"GIMP canvas 1024x768 /home/ada/private"),
            ],
        );

        let report = inspect(&source).expect("vendor-chunk PNG should inspect");
        assert!(
            report.contains("prVW") && report.contains("caNv"),
            "unknown ancillary chunks must be reported: {report}"
        );
        assert!(
            report.contains("\"sensitive\":true"),
            "a GPS-shaped payload in an unknown chunk is sensitive: {report}"
        );

        let scrubbed = scrub(&source).expect("vendor-chunk PNG should scrub");
        assert!(
            !contains(&scrubbed, payload),
            "the unknown chunk's payload must not survive the scrub"
        );
        assert!(!contains(&scrubbed, b"prVW"));
        assert!(!contains(&scrubbed, b"caNv"));
        assert!(!contains(&scrubbed, b"/home/ada/private"));
        assert_eq!(scrubbed, clean, "only the vendor chunks were removed");
    }

    /// The other half of finding 1: chunks that affect how the image RENDERS are
    /// deliberately kept, and keeping them is not silent — the report stays empty
    /// because they carry no user metadata.
    #[test]
    fn png_keeps_rendering_chunks_and_reports_nothing() {
        let clean = png_fixture(16, 8);
        let rendering = [
            png_chunk(b"sRGB", &[0]),
            png_chunk(b"gAMA", &45455u32.to_be_bytes()),
            png_chunk(b"pHYs", b"\0\0\x0b\x13\0\0\x0b\x13\x01"),
        ];
        let source = inject_png_chunks(&clean, &rendering);

        assert_eq!(inspect(&source).unwrap(), "{\"kind\":\"png\",\"items\":[]}");
        let scrubbed = scrub(&source).expect("rendering-chunk PNG should scrub");
        for chunk in &rendering {
            assert!(
                contains(&scrubbed, chunk),
                "rendering chunk must survive byte-for-byte"
            );
        }
    }

    /// FINDING 2: a second full JPEG appended after EOI (MPF / Ultra-HDR gain map /
    /// Samsung motion photo) kept its entire EXIF, including GPS, unreported.
    #[test]
    fn jpeg_trailing_second_image_is_reported_and_removed() {
        let primary = jpeg_fixture(40, 20);
        let secondary = inject_jpeg_segments(
            &jpeg_fixture(20, 10),
            &[
                jpeg_segment(0xe1, &exif_payload(&[GPS_POINTER_ENTRY], true)),
                jpeg_segment(0xfe, b"SECONDARY-IMAGE-GPS-60.1699-24.9384"),
            ],
        );
        let mut source = primary.clone();
        source.extend_from_slice(&secondary);

        let report = inspect(&source).expect("appended-image JPEG should inspect");
        assert!(
            report.contains("Trailing data"),
            "trailing image data must be reported: {report}"
        );
        assert!(
            report.contains("\"sensitive\":true"),
            "a trailing image carrying GPS is sensitive: {report}"
        );

        let scrubbed = scrub(&source).expect("appended-image JPEG should scrub");
        assert!(
            !contains(&scrubbed, b"SECONDARY-IMAGE-GPS-60.1699-24.9384"),
            "the appended image's payload must not survive the scrub"
        );
        assert!(!contains(&scrubbed, b"Exif\0\0"));
        assert_eq!(scrubbed, primary, "the primary image is kept untouched");
        assert_eq!(dimensions(&scrubbed), (40, 20));
    }

    /// The img2pdf path calls `strip_jpeg_metadata` directly, so it leaked the same
    /// appended image into every generated PDF.
    #[test]
    fn strip_jpeg_metadata_drops_trailing_image_data() {
        let primary = jpeg_fixture(24, 12);
        let mut source = primary.clone();
        source.extend_from_slice(&inject_jpeg_segments(
            &jpeg_fixture(12, 6),
            &[jpeg_segment(0xfe, b"APPENDED-PII")],
        ));
        let stripped = super::strip_jpeg_metadata(&source).expect("baseline JPEG should strip");
        assert!(!contains(&stripped, b"APPENDED-PII"));
        assert_eq!(stripped, primary);
    }

    /// FINDING 6: stripping APP1 wholesale threw away EXIF Orientation, silently
    /// rotating every portrait phone photo that passed through the tool.
    #[test]
    fn jpeg_orientation_survives_while_gps_is_removed() {
        let clean = jpeg_fixture(32, 64);
        let source = inject_jpeg_segments(
            &clean,
            &[jpeg_segment(
                0xe1,
                &exif_payload(&[orientation_entry(6), GPS_POINTER_ENTRY], true),
            )],
        );
        assert_eq!(jpeg_orientation(&source), Some(6), "fixture is portrait");

        let report = inspect(&source).expect("portrait JPEG should inspect");
        assert!(report.contains("includes GPS location"), "{report}");

        let scrubbed = scrub(&source).expect("portrait JPEG should scrub");
        assert_eq!(
            jpeg_orientation(&scrubbed),
            Some(6),
            "the photo must still render portrait after scrubbing"
        );
        // The GPS pointer tag (0x8825) is gone even though an EXIF block remains.
        assert!(!contains(&scrubbed, &[0x88, 0x25, 0x00, 0x04]));
        assert_eq!(scan_suffix(&scrubbed), scan_suffix(&source));
    }

    /// An upright photo gets no re-emitted EXIF at all: orientation 1 is the
    /// default, so preserving it would mean leaving metadata for no reason.
    #[test]
    fn jpeg_upright_orientation_leaves_no_exif_behind() {
        let clean = jpeg_fixture(16, 8);
        let source = inject_jpeg_segments(
            &clean,
            &[jpeg_segment(
                0xe1,
                &exif_payload(&[orientation_entry(1), GPS_POINTER_ENTRY], true),
            )],
        );
        let scrubbed = scrub(&source).expect("upright JPEG should scrub");
        assert!(!contains(&scrubbed, b"Exif\0\0"));
        assert_eq!(scrubbed, clean);
    }

    /// FINDING 6 (second half): APP0/APP2/APP14 were kept by MARKER CLASS, so a
    /// JFXX thumbnail and a non-ICC APP2 (MPF, FPXR) rode through unreported.
    #[test]
    fn jpeg_drops_non_decode_payloads_in_app0_and_app2() {
        let clean = jpeg_fixture(20, 10);
        let jfxx = jpeg_segment(0xe0, b"JFXX\0\x11THUMBNAIL-PIXELS-OF-THE-ORIGINAL");
        let mpf = jpeg_segment(0xe2, b"MPF\0MM\0\x2aOFFSETS-AND-CAMERA-SERIAL");
        let source = inject_jpeg_segments(&clean, &[jfxx, mpf]);

        let report = inspect(&source).expect("JFXX/MPF JPEG should inspect");
        assert!(report.contains("JFXX"), "JFXX thumbnail reported: {report}");
        assert!(report.contains("MPF"), "MPF index reported: {report}");

        let scrubbed = scrub(&source).expect("JFXX/MPF JPEG should scrub");
        assert!(!contains(&scrubbed, b"THUMBNAIL-PIXELS-OF-THE-ORIGINAL"));
        assert!(!contains(&scrubbed, b"OFFSETS-AND-CAMERA-SERIAL"));
        assert_eq!(scrubbed, clean);
    }

    /// A PDF whose XMP uses no `<?xpacket` wrapper — which the XMP spec does not
    /// require. FINDING 3: it was neither reported nor removed.
    fn pdf_with_xmp(content: &[u8]) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let xmp_id = document.add_object(Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            content.to_vec(),
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! {},
            "Contents" => content_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "AF" => vec![Object::Reference(xmp_id)],
        });
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("XMP fixture serializes");
        bytes
    }

    #[test]
    fn pdf_xmp_without_an_xpacket_wrapper_is_reported_and_removed() {
        let source =
            pdf_with_xmp(b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><dc:creator>ADA WRAPPERLESS</dc:creator></x:xmpmeta>");
        let report = inspect(&source).expect("wrapper-less XMP PDF should inspect");
        assert!(
            report.contains("XMP metadata"),
            "XMP without the optional xpacket wrapper must still be reported: {report}"
        );
        let scrubbed = scrub(&source).expect("wrapper-less XMP PDF should scrub");
        assert!(!contains(&scrubbed, b"ADA WRAPPERLESS"));
        assert!(!contains(&scrubbed, b"xmpmeta"));
    }

    #[test]
    fn pdf_second_xmp_packet_in_one_stream_is_also_removed() {
        let source = pdf_with_xmp(
            b"<?xpacket begin=\"\"?><x:xmpmeta>FIRST PACKET</x:xmpmeta><?xpacket end=\"w\"?>\
              <?xpacket begin=\"\"?><x:xmpmeta>SECOND PACKET</x:xmpmeta><?xpacket end=\"w\"?>",
        );
        let scrubbed = scrub(&source).expect("two-packet XMP PDF should scrub");
        assert!(!contains(&scrubbed, b"FIRST PACKET"));
        assert!(
            !contains(&scrubbed, b"SECOND PACKET"),
            "every packet in the stream must go, not just the first"
        );
    }

    /// FINDING 4: annotation authorship (`/Annots` -> `/T`) was invisible.
    fn pdf_with_extras(annotation: bool, piece_info: bool, embedded_file: bool) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! {},
            "Contents" => content_id,
        };
        if annotation {
            let annot_id = document.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Text",
                "Rect" => vec![0.into(), 0.into(), 20.into(), 20.into()],
                "T" => Object::string_literal("Ada Reviewer"),
                "M" => Object::string_literal("D:20260101120000Z"),
                "Contents" => Object::string_literal("looks fine to me"),
            });
            page.set("Annots", vec![Object::Reference(annot_id)]);
        }
        if piece_info {
            page.set(
                "PieceInfo",
                dictionary! {
                    "MtclabTool" => dictionary! {
                        "LastModified" => Object::string_literal("D:20260101120000Z"),
                        "Private" => Object::string_literal("/home/ada/drafts/secret.indd"),
                    },
                },
            );
        }
        let page_id = document.add_object(page);
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let mut catalog = dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        };
        if embedded_file {
            let file_id = document.add_object(Stream::new(
                dictionary! { "Type" => "EmbeddedFile" },
                b"ATTACHED-PAYROLL-CSV".to_vec(),
            ));
            let spec_id = document.add_object(dictionary! {
                "Type" => "Filespec",
                "F" => Object::string_literal("payroll.csv"),
                "EF" => dictionary! { "F" => Object::Reference(file_id) },
            });
            catalog.set(
                "Names",
                dictionary! {
                    "EmbeddedFiles" => dictionary! {
                        "Names" => vec![
                            Object::string_literal("payroll.csv"),
                            Object::Reference(spec_id),
                        ],
                    },
                },
            );
        }
        let catalog_id = document.add_object(catalog);
        document.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("extras fixture serializes");
        bytes
    }

    #[test]
    fn pdf_annotation_author_is_reported_and_removed() {
        let source = pdf_with_extras(true, false, false);
        let report = inspect(&source).expect("annotated PDF should inspect");
        assert!(
            report.contains("Ada Reviewer"),
            "the comment author must be reported: {report}"
        );
        assert!(report.contains("\"sensitive\":true"), "{report}");

        let scrubbed = scrub(&source).expect("annotated PDF should scrub");
        assert!(
            !contains(&scrubbed, b"Ada Reviewer"),
            "the comment author must not survive the scrub"
        );
        // The comment itself is document content and is deliberately preserved.
        assert!(contains(&scrubbed, b"looks fine to me"));
        assert_eq!(Document::load_mem(&scrubbed).unwrap().get_pages().len(), 1);
    }

    #[test]
    fn pdf_piece_info_is_reported_and_removed() {
        let source = pdf_with_extras(false, true, false);
        let report = inspect(&source).expect("PieceInfo PDF should inspect");
        assert!(report.contains("PieceInfo"), "{report}");
        let scrubbed = scrub(&source).expect("PieceInfo PDF should scrub");
        assert!(!contains(&scrubbed, b"/home/ada/drafts/secret.indd"));
        assert!(!contains(&scrubbed, b"PieceInfo"));
    }

    #[test]
    fn pdf_embedded_file_is_reported_even_though_it_is_kept() {
        let source = pdf_with_extras(false, false, true);
        let report = inspect(&source).expect("attachment PDF should inspect");
        assert!(
            report.contains("Embedded file"),
            "an attachment is a leak the user must be told about: {report}"
        );
        assert!(report.contains("payroll.csv"), "{report}");
    }

    /// The `/Info` reader only knew eight keys, so a producer's custom key was
    /// reported as nothing at all — the same "unknown means invisible" defect.
    #[test]
    fn pdf_custom_info_keys_are_reported() {
        let mut document = Document::load_mem(&pdf_fixture(true)).unwrap();
        let info_id = match document.trailer.get(b"Info").unwrap() {
            Object::Reference(id) => *id,
            _ => panic!("fixture Info is a reference"),
        };
        if let Ok(Object::Dictionary(info)) = document.get_object_mut(info_id) {
            info.set("Company", Object::string_literal("Mtclab Oy"));
        }
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();

        let report = inspect(&bytes).expect("custom-Info PDF should inspect");
        assert!(
            report.contains("Company") && report.contains("Mtclab Oy"),
            "unknown Info keys must be reported, not skipped: {report}"
        );
        let scrubbed = scrub(&bytes).expect("custom-Info PDF should scrub");
        assert!(!contains(&scrubbed, b"Mtclab Oy"));
    }

    /// FINDING 5: the scrub used to verify itself by re-running `inspect()`, so
    /// the oracle could not fail for any bug the detector had — which is how all
    /// of the above shipped behind a green "re-inspection passed".
    ///
    /// These are the teeth of the replacement. Every case below is a file that a
    /// detector gap would wave through, and the STRUCTURAL verifier rejects it
    /// without consulting the reporting layer at all.
    #[test]
    fn structural_verification_rejects_what_a_detector_gap_would_miss() {
        // A PNG chunk type no rule anywhere in this module knows about.
        let png = inject_png_chunks(&png_fixture(12, 6), &[png_chunk(b"zzZz", b"anything")]);
        assert!(
            verify_removed(&png).is_err(),
            "an unrecognized ancillary chunk must fail verification"
        );

        // Bytes after EOI, in any amount.
        let mut jpeg = jpeg_fixture(12, 6);
        jpeg.extend_from_slice(b"\x00trailing");
        assert!(
            verify_removed(&jpeg).is_err(),
            "trailing bytes must fail verification"
        );

        // An APP1 that is not the one orientation-only EXIF block we may emit.
        let crafted = inject_jpeg_segments(
            &jpeg_fixture(12, 6),
            &[jpeg_segment(
                0xe1,
                b"Exif\0\0MM\0\x2a\0\0\0\x08\0\0\0\0\0\0extra",
            )],
        );
        assert!(
            verify_removed(&crafted).is_err(),
            "an APP1 that is not a minimal orientation EXIF must fail verification"
        );

        // A PDF that still has its Info dictionary.
        assert!(
            verify_removed(&pdf_fixture(true)).is_err(),
            "a surviving /Info must fail verification"
        );
        assert!(
            verify_removed(&pdf_with_extras(true, false, false)).is_err(),
            "surviving annotation authorship must fail verification"
        );
        assert!(
            verify_removed(&pdf_with_xmp(b"<x:xmpmeta>anything</x:xmpmeta>")).is_err(),
            "a surviving XMP element must fail verification"
        );
    }

    /// The other direction: everything this module produces passes its own
    /// structural check, including the one metadata-shaped thing it may emit.
    #[test]
    fn structural_verification_accepts_every_scrubbed_output() {
        let portrait = inject_jpeg_segments(
            &jpeg_fixture(16, 32),
            &[jpeg_segment(
                0xe1,
                &exif_payload(&[orientation_entry(8), GPS_POINTER_ENTRY], true),
            )],
        );
        for source in [
            portrait,
            inject_jpeg_segments(&jpeg_fixture(16, 8), &[jpeg_segment(0xfe, b"comment")]),
            inject_png_chunks(&png_fixture(16, 8), &[png_chunk(b"tEXt", b"Software\0X")]),
            pdf_fixture(true),
            pdf_with_extras(true, true, true),
        ] {
            let scrubbed = scrub(&source).expect("fixture should scrub");
            verify_removed(&scrubbed).expect("scrubbed output passes structural verification");
        }
    }

    /// The scan walker now has to cross MULTIPLE scans and restart markers to find
    /// the EOI that ends the image, instead of assuming "SOS means the rest of the
    /// file". A progressive JPEG with restart intervals is the shape that breaks a
    /// walker which gets that wrong: `FF D0`–`FF D7` inside entropy data must not
    /// be mistaken for the end of the scan.
    #[test]
    fn progressive_jpeg_with_restart_markers_scrubs_losslessly() {
        let width = 48u16;
        let height = 32u16;
        let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
            let noise = ((x * 31 + y * 97 + x * y * 13) % 251) as u8;
            Rgb([noise, noise.rotate_left(3), noise.rotate_left(6)])
        });
        let mut clean = Vec::new();
        let mut encoder = Encoder::new(&mut clean, 85);
        encoder.set_progressive(true);
        encoder.set_restart_interval(2);
        encoder
            .encode(image.as_raw(), width, height, ColorType::Rgb)
            .expect("progressive fixture should encode");

        let source =
            inject_jpeg_segments(&clean, &[jpeg_segment(0xfe, b"PROGRESSIVE-COMMENT-PII")]);
        let report = inspect(&source).expect("progressive JPEG should inspect");
        assert!(report.contains("PROGRESSIVE-COMMENT-PII"), "{report}");

        let scrubbed = scrub(&source).expect("progressive JPEG should scrub");
        assert!(!contains(&scrubbed, b"PROGRESSIVE-COMMENT-PII"));
        // Every scan survived: the image still decodes at full size and the bytes
        // match the untouched original exactly.
        assert_eq!(dimensions(&scrubbed), (48, 32));
        assert_eq!(scrubbed, clean);
        verify_removed(&scrubbed).expect("scrubbed progressive JPEG verifies");
    }

    /// A PNG whose CRITICAL chunk we do not understand is refused, not silently
    /// re-emitted with a meaning we guessed at.
    #[test]
    fn png_with_an_unknown_critical_chunk_is_refused() {
        // An uppercase first letter marks a chunk CRITICAL.
        let source = inject_png_chunks(&png_fixture(8, 4), &[png_chunk(b"UnKn", b"required")]);
        let error = scrub(&source).expect_err("unknown critical chunks are refused");
        eprintln!("unknown critical chunk refused with: {error}");
        assert!(
            inspect(&source).is_err(),
            "and it is refused on inspect too"
        );
    }

    #[test]
    fn report_json_escapes_comment_text() {
        let clean = jpeg_fixture(8, 8);
        let source = inject_jpeg_segments(
            &clean,
            &[jpeg_segment(0xfe, b"name=\"Ada\"\npath=C:\\photos")],
        );
        let report = inspect(&source).expect("comment JPEG should inspect");
        assert!(report.contains("name=\\\"Ada\\\"\\npath=C:\\\\photos"));
    }
}
