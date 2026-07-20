//! Lossless metadata inspection and removal for PDF, JPEG, and PNG files.
//!
//! JPEG APP/COM segments and PNG ancillary chunks are removed by copying the
//! original byte ranges around them. Pixel data is never decoded or re-encoded:
//! JPEG scan data and PNG IDAT chunks remain byte-for-byte identical. JFIF and
//! ICC profiles are deliberately preserved because they affect compatibility
//! and rendered color rather than carrying the metadata covered by this tool.
//!
//! ## Deferred
//!
//! - GIF, BMP, and WebP metadata: safe, lossless container surgery for those
//!   formats is outside this slice.
//! - Compressed PNG text inflation: zTXt and compressed iTXt are identified and
//!   removed, but their values are reported as compressed rather than inflated.

use std::{fmt::Write, io::Cursor};

use image::{ImageFormat, ImageReader};
use lopdf::{Dictionary, Document, Object};
use wasm_bindgen::prelude::*;

use super::load_pdf;

const SUPPORTED_FORMATS_ERROR: &str = "Metadata scrubbing supports PDF, JPEG, and PNG files.";
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DETAIL_CHARS: usize = 160;

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

fn jpeg_item(marker: u8, payload: &[u8]) -> Option<MetadataItem> {
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
        0xe1 if payload.starts_with(b"http://ns.adobe.com/xap/") => Some(MetadataItem::new(
            "XMP",
            Some(format!("{} bytes", payload.len())),
            false,
        )),
        0xed if payload.starts_with(b"Photoshop 3.0") => Some(MetadataItem::new(
            "IPTC / Photoshop",
            Some(format!("{} bytes", payload.len())),
            false,
        )),
        0xfe => Some(MetadataItem::new(
            "JPEG comment",
            Some(lossy_detail(payload)),
            false,
        )),
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
    let keyword = lossy_detail(keyword_bytes);
    let sensitive = png_keyword_sensitive(&keyword);
    let detail = match chunk_type {
        b"tEXt" => Some(lossy_detail(rest)),
        b"zTXt" => Some("(compressed)".to_owned()),
        b"iTXt" => {
            let compression_flag = rest.first().copied();
            let text = rest.get(2..).and_then(|language_and_text| {
                let (_, translated_and_text) = png_keyword(language_and_text);
                let (_, text) = png_keyword(translated_and_text);
                (!text.is_empty()).then_some(text)
            });
            if compression_flag == Some(0) {
                text.map(lossy_detail)
            } else {
                Some("(compressed)".to_owned())
            }
        }
        _ => None,
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
    if metadata_count > 0 {
        items.push(MetadataItem::new(
            "XMP metadata",
            Some(format!(
                "{metadata_count} {}",
                if metadata_count == 1 {
                    "block"
                } else {
                    "blocks"
                }
            )),
            false,
        ));
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
        assert!(report.contains("(compressed)"));
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
