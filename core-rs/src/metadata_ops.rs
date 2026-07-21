//! Lossless metadata inspection and removal for PDF, JPEG, and PNG files.
//!
//! JPEG APP/COM segments and PNG ancillary chunks are removed by copying the
//! original byte ranges around them. Pixel data is never decoded or re-encoded:
//! JPEG scan data and PNG IDAT chunks remain byte-for-byte identical. JFIF and
//! ICC profiles are deliberately preserved because they affect compatibility
//! and rendered color rather than carrying the metadata covered by this tool.
//!
//! PDF XMP packets are excised from stream content directly (not merely by
//! dropping `/Metadata` references), so XMP is removed even when a stream is
//! reachable through another reference. Compressed PNG text (zTXt / iTXt) is
//! inflated so its contents can be inspected and judged, not just reported.
//!
//! ## Deferred
//!
//! - GIF, BMP, and WebP metadata: safe, lossless container surgery for those
//!   formats is outside this slice.

use std::{fmt::Write, io::Read};
use std::io::Cursor;

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

/// Classify a JPEG marker segment. Returns `Some` for every metadata-bearing
/// segment (which the scrubber drops) and `None` for segments that must be kept.
///
/// The drop decision is by MARKER CLASS, never by payload contents: an `APPn`
/// segment carries only application metadata in baseline JPEG, so matching on a
/// known payload prefix (Exif/XMP/Photoshop) would let a crafted APP1 with an
/// unrecognized prefix survive with its PII intact. We therefore strip ALL
/// application segments except the three that are decode-relevant and carry no
/// user metadata: APP0 (JFIF), APP2 (ICC colour profile), APP14 (Adobe colour
/// transform). The prefix checks below only choose a friendlier label.
fn jpeg_item(marker: u8, payload: &[u8]) -> Option<MetadataItem> {
    let bytes_detail = || Some(format!("{} bytes", payload.len()));
    match marker {
        0xe1 if payload.starts_with(b"Exif\0\0") => {
            let sensitive = exif_has_gps(payload);
            Some(MetadataItem::new(
                "EXIF",
                Some(if sensitive {
                    "includes GPS location".to_owned()
                } else {
                    format!("{} bytes", payload.len())
                }),
                sensitive,
            ))
        }
        // Any other APP1 is XMP or an unrecognized application payload — still
        // metadata, still stripped (this closes the known-prefix bypass).
        0xe1 => Some(MetadataItem::new("XMP / application metadata", bytes_detail(), false)),
        0xed if payload.starts_with(b"Photoshop 3.0") => {
            Some(MetadataItem::new("IPTC / Photoshop", bytes_detail(), false))
        }
        0xed => Some(MetadataItem::new("Photoshop / application metadata", bytes_detail(), false)),
        0xfe => Some(MetadataItem::new("JPEG comment", Some(lossy_detail(payload)), false)),
        // Remaining application segments (APP3–APP12, APP15) are metadata too.
        // APP0/APP2/APP14 fall through to None and are preserved for decoding.
        0xe3..=0xec | 0xef => {
            Some(MetadataItem::new("Application metadata", bytes_detail(), false))
        }
        _ => None,
    }
}

/// Walk markers only until the first SOS. Everything from SOS onward is scan
/// data and is copied as one untouched suffix.
fn walk_jpeg(bytes: &[u8], strip: bool) -> Result<(Vec<MetadataItem>, Vec<u8>), String> {
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("Could not read this JPEG: invalid signature.".to_owned());
    }

    let mut output = Vec::with_capacity(bytes.len());
    output.extend_from_slice(&bytes[..2]);
    let mut items = Vec::new();
    let mut position = 2usize;

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
            return Err("Could not read this JPEG: it ends before image scan data.".to_owned());
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
            // Entropy-coded scan data follows SOS and has no length prefix; the
            // JPEG spec requires it to terminate with an EOI (FF D9). Require that
            // marker before copying the tail verbatim, so truncated files (which
            // would otherwise emit a broken JPEG) are rejected instead.
            if !bytes[segment_end..]
                .windows(2)
                .any(|window| window == [0xff, 0xd9])
            {
                return Err("Could not read this truncated JPEG scan data.".to_owned());
            }
            output.extend_from_slice(&bytes[marker_start..]);
            return Ok((items, output));
        }

        let payload = &bytes[position + 2..segment_end];
        let item = jpeg_item(marker, payload);
        let drop_segment = item.is_some();
        if let Some(item) = item {
            items.push(item);
        }
        if !strip || !drop_segment {
            output.extend_from_slice(&bytes[marker_start..segment_end]);
        }
        position = segment_end;
    }

    Err("Could not read this truncated JPEG.".to_owned())
}

fn inspect_jpeg(bytes: &[u8]) -> Result<Vec<MetadataItem>, String> {
    image_dimensions(bytes, ImageFormat::Jpeg)?;
    walk_jpeg(bytes, false).map(|(items, _)| items)
}

fn scrub_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let source_dimensions = image_dimensions(bytes, ImageFormat::Jpeg)?;
    let (_, output) = walk_jpeg(bytes, true)?;
    if !output.starts_with(&[0xff, 0xd8, 0xff])
        || image_dimensions(&output, ImageFormat::Jpeg)? != source_dimensions
    {
        return Err("Could not validate the scrubbed JPEG.".to_owned());
    }
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

/// Remove the first `<?xpacket begin ... ?> ... <?xpacket end ...?>` region from
/// stream content, leaving any surrounding bytes intact. None if no complete
/// packet is present.
fn strip_xmp_packet(content: &[u8]) -> Option<Vec<u8>> {
    let begin = find_subslice(content, XMP_PACKET_BEGIN)?;
    let end_marker = find_subslice(&content[begin..], XMP_PACKET_END)? + begin;
    let close = find_subslice(&content[end_marker..], b"?>")? + end_marker + 2;
    let mut output = Vec::with_capacity(content.len() - (close - begin));
    output.extend_from_slice(&content[..begin]);
    output.extend_from_slice(&content[close..]);
    Some(output)
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

fn png_item(chunk_type: &[u8; 4], data: &[u8]) -> Option<MetadataItem> {
    match chunk_type {
        b"tEXt" | b"zTXt" | b"iTXt" => Some(png_text_item(chunk_type, data)),
        b"eXIf" => {
            let sensitive = exif_has_gps(data);
            Some(MetadataItem::new(
                "EXIF",
                Some(if sensitive {
                    "includes GPS location".to_owned()
                } else {
                    format!("{} bytes", data.len())
                }),
                sensitive,
            ))
        }
        b"tIME" if data.len() == 7 => Some(MetadataItem::new(
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
        )),
        b"tIME" => Some(MetadataItem::new(
            "Last-modified time",
            Some(format!("{} bytes", data.len())),
            false,
        )),
        _ => None,
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

        let item = png_item(&chunk_type, data);
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
    if !output.starts_with(PNG_SIGNATURE)
        || image_dimensions(&output, ImageFormat::Png)? != source_dimensions
    {
        return Err("Could not validate the scrubbed PNG.".to_owned());
    }
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

/// Whether any stream object embeds an XMP packet, regardless of how it is
/// referenced. This is what makes XMP removal (and its verification) complete:
/// a `/Metadata` reference can be dropped while the stream lingers reachable
/// through another key.
fn document_has_xmp_packet(document: &Document) -> bool {
    document.objects.values().any(|object| match object {
        Object::Stream(stream) => {
            find_subslice(&stream_plain_content(stream), XMP_PACKET_BEGIN).is_some()
        }
        _ => false,
    })
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

fn pdf_items(document: &Document) -> Result<Vec<MetadataItem>, String> {
    let mut items = Vec::new();
    if let Ok(info_value) = document.trailer.get(b"Info") {
        let info = resolved_dictionary(document, info_value)
            .ok_or_else(|| "Could not read this PDF's document information.".to_owned())?;
        for key in [
            b"Title".as_slice(),
            b"Author",
            b"Subject",
            b"Keywords",
            b"Creator",
            b"Producer",
            b"CreationDate",
            b"ModDate",
        ] {
            if let Ok(value) = info.get(key) {
                let label = String::from_utf8_lossy(key).into_owned();
                items.push(MetadataItem::new(
                    label,
                    Some(pdf_value_detail(document, value)),
                    key == b"Author",
                ));
            }
        }
    }

    let metadata_count = pdf_metadata_link_count(document);
    let embedded_xmp = document_has_xmp_packet(document);
    if metadata_count > 0 || embedded_xmp {
        let detail = if metadata_count > 0 {
            format!(
                "{metadata_count} {}",
                if metadata_count == 1 { "block" } else { "blocks" }
            )
        } else {
            "embedded packet".to_owned()
        };
        items.push(MetadataItem::new("XMP metadata", Some(detail), false));
    }
    Ok(items)
}

fn inspect_pdf(bytes: &[u8]) -> Result<Vec<MetadataItem>, String> {
    let document = load_pdf(bytes)?;
    pdf_items(&document)
}

fn scrub_pdf(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut document = load_pdf(bytes)?;
    let source_page_count = document.get_pages().len();

    document.trailer.remove(b"Info");
    document.trailer.remove(b"Metadata");
    for object in document.objects.values_mut() {
        match object {
            Object::Dictionary(dictionary) => {
                dictionary.remove(b"Metadata");
            }
            Object::Stream(stream) => {
                stream.dict.remove(b"Metadata");
                // Excise an XMP packet embedded directly in the stream content,
                // so XMP is gone even when the stream stays reachable through a
                // reference other than /Metadata (dropping the reference + prune
                // alone would leave it). Surrounding content bytes are preserved.
                let content = stream_plain_content(stream);
                if find_subslice(&content, XMP_PACKET_BEGIN).is_some() {
                    if let Some(stripped) = strip_xmp_packet(&content) {
                        stream.set_plain_content(stripped);
                    }
                }
            }
            _ => {}
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
    if !pdf_items(&reparsed)?.is_empty() {
        return Err("Could not validate that the PDF metadata was removed.".to_owned());
    }
    Ok(output)
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

#[cfg(test)]
mod tests {
    use super::{crc32, inspect, scrub, SUPPORTED_FORMATS_ERROR};
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
        let source = inject_jpeg_segments(&clean, &[sneaky.clone()]);

        let report = inspect(&source).expect("crafted APP1 JPEG should inspect");
        assert!(report.contains("application metadata"));
        let scrubbed = scrub(&source).expect("crafted APP1 JPEG should scrub");
        assert!(!scrubbed.windows(2).any(|window| window == [0xff, 0xe1]));
        assert!(!scrubbed.windows(8).any(|window| window == b"Location"));
        // Nothing but the metadata segment was touched.
        assert_eq!(scrubbed, clean);
    }

    #[test]
    fn jpeg_keeps_decode_critical_app0_app2_app14() {
        let clean = jpeg_fixture(18, 9);
        let icc = jpeg_segment(0xe2, b"ICC_PROFILE\0color");
        // A well-formed APP14 Adobe marker (Adobe + version/flags/transform); a
        // malformed one would make the JPEG undecodable, which is a different test.
        let adobe = jpeg_segment(0xee, b"Adobe\0\x64\0\0\0\0\x01");
        let source = inject_jpeg_segments(&clean, &[icc.clone(), adobe.clone()]);

        // Neither APP2 (ICC) nor APP14 (Adobe) is user metadata: report is empty
        // and the scrub keeps both segments verbatim.
        assert_eq!(inspect(&source).unwrap(), "{\"kind\":\"jpeg\",\"items\":[]}");
        let scrubbed = scrub(&source).expect("decode-critical JPEG should scrub");
        assert!(scrubbed.windows(icc.len()).any(|window| window == icc));
        assert!(scrubbed.windows(adobe.len()).any(|window| window == adobe));
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
        assert_eq!(
            inspect(&scrubbed).unwrap(),
            "{\"kind\":\"pdf\",\"items\":[]}"
        );
    }

    #[test]
    fn clean_pdf_reports_empty_and_stays_valid() {
        let source = pdf_fixture(false);
        assert_eq!(inspect(&source).unwrap(), "{\"kind\":\"pdf\",\"items\":[]}");
        let scrubbed = scrub(&source).expect("clean PDF should scrub");
        assert_eq!(Document::load_mem(&scrubbed).unwrap().get_pages().len(), 1);
        assert_eq!(
            inspect(&scrubbed).unwrap(),
            "{\"kind\":\"pdf\",\"items\":[]}"
        );
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
        ztxt.extend_from_slice(&zlib(b"Shot on MtclabCam, GPSLatitude 60.1, GPSLongitude 24.9"));
        let source = inject_png_chunks(&clean, &[png_chunk(b"zTXt", &ztxt)]);
        let source_idat = chunk_data(&source, b"IDAT");

        let report = inspect(&source).expect("zTXt PNG should inspect");
        assert!(report.contains("GPSLatitude"), "inflated value is shown: {report}");
        assert!(
            report.contains("\"sensitive\":true"),
            "GPS in inflated text flags sensitive: {report}"
        );
        let scrubbed = scrub(&source).expect("zTXt PNG should scrub");
        assert!(!scrubbed.windows(4).any(|window| window == b"zTXt"));
        assert_eq!(chunk_data(&scrubbed, b"IDAT"), source_idat, "IDAT untouched");
    }

    #[test]
    fn png_compressed_itxt_is_inflated() {
        let clean = png_fixture(16, 8);
        // keyword \0 flag(1) method(0) language\0 translated\0 + zlib(text).
        let mut itxt = b"XML:com.adobe.xmp\0\x01\0en\0\0".to_vec();
        itxt.extend_from_slice(&zlib(b"<x:xmpmeta>studio edit trail</x:xmpmeta>"));
        let source = inject_png_chunks(&clean, &[png_chunk(b"iTXt", &itxt)]);

        let report = inspect(&source).expect("iTXt PNG should inspect");
        assert!(report.contains("studio edit trail"), "compressed iTXt inflated: {report}");
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
        assert!(report.contains("XMP metadata"), "reports embedded XMP: {report}");

        let scrubbed = scrub(&source).expect("embedded-XMP PDF should scrub");
        assert!(!scrubbed.windows(9).any(|window| window == b"<?xpacket"));
        assert!(!scrubbed.windows(18).any(|window| window == b"secret author line"));
        assert_eq!(Document::load_mem(&scrubbed).unwrap().get_pages().len(), 1);
        assert_eq!(inspect(&scrubbed).unwrap(), "{\"kind\":\"pdf\",\"items\":[]}");
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
