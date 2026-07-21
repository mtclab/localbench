use jpeg_encoder::{ColorType, Encoder};
use lopdf::{dictionary, Object};
use wasm_bindgen::prelude::*;
use zune_jpeg::{
    zune_core::{colorspace::ColorSpace, options::DecoderOptions},
    JpegDecoder,
};

mod archive_ops;
mod image_ops;
mod metadata_ops;

pub use archive_ops::{create_zip, extract_zip_entry, list_zip};
pub use image_ops::{compress_image, convert_image, resize_image};
pub use metadata_ops::{inspect_metadata, scrub_metadata};

const ENCRYPTED_PDF_ERROR: &str = "This PDF is password-protected, so its pages can't be read.";
const MAX_REENCODED_DIMENSION: u16 = 4_096;
const MAX_DECODED_PIXELS: usize = 64_000_000;

/// Return the exact version of the compiled core.
#[wasm_bindgen]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Pure core: parse a PDF from memory and return its page count. Returns a
/// String error (never panics on malformed input) so it is testable natively —
/// JsValue only exists inside wasm, so the error type must not cross into tests.
fn page_count(bytes: &[u8]) -> Result<u32, String> {
    let document = load_pdf(bytes)?;

    u32::try_from(document.get_pages().len())
        .map_err(|_| "This PDF has too many pages to count.".to_owned())
}

/// Parse a PDF and reject encryption before any operation reads its page tree.
fn load_pdf(bytes: &[u8]) -> Result<lopdf::Document, String> {
    let document = lopdf::Document::load_mem(bytes)
        .map_err(|error| format!("Could not read this PDF: {error}"))?;

    // Encrypted PDFs have an unreadable page tree -> lopdf reports 0 pages, which
    // would be a silently wrong answer. Detect it and say so honestly instead.
    if document.trailer.get(b"Encrypt").is_ok() {
        return Err(ENCRYPTED_PDF_ERROR.to_owned());
    }

    Ok(document)
}

/// Find a page attribute, following its parent chain when the value is inherited.
fn inherited_page_attribute(
    document: &lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Option<lopdf::Object> {
    let mut current_id = page_id;
    let mut seen = std::collections::HashSet::new();

    while seen.insert(current_id) {
        let dictionary = document.get_dictionary(current_id).ok()?;
        if let Ok(value) = dictionary.get(key) {
            return Some(value.clone());
        }

        current_id = dictionary
            .get(b"Parent")
            .and_then(lopdf::Object::as_reference)
            .ok()?;
    }

    None
}

/// Pure core: combine PDFs in the supplied order and return the serialized PDF.
fn merge(mut docs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
    if docs.is_empty() {
        return Err("Choose at least one PDF to merge.".to_owned());
    }

    if docs.len() == 1 {
        let bytes = docs
            .pop()
            .ok_or_else(|| "Choose at least one PDF to merge.".to_owned())?;
        load_pdf(&bytes)?;
        return Ok(bytes);
    }

    let mut source_documents = docs
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            load_pdf(&bytes).map_err(|error| format!("PDF {}: {error}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let output_version = source_documents
        .iter()
        .map(|document| document.version.as_str())
        .max()
        .unwrap_or("1.5")
        .max("1.5")
        .to_owned();
    let mut output = lopdf::Document::with_version(output_version);

    // Reserve the two active root IDs, then give every source a disjoint,
    // contiguous range. lopdf rewrites references while it renumbers objects.
    let pages_id = output.new_object_id();
    let catalog_id = output.new_object_id();
    let mut next_object_id = output
        .max_id
        .checked_add(1)
        .ok_or_else(|| "These PDFs contain too many objects to merge.".to_owned())?;
    let mut page_ids = Vec::new();

    for document in &mut source_documents {
        let object_count = u32::try_from(document.objects.len())
            .map_err(|_| "These PDFs contain too many objects to merge.".to_owned())?;
        let following_object_id = next_object_id
            .checked_add(object_count)
            .ok_or_else(|| "These PDFs contain too many objects to merge.".to_owned())?;

        document.renumber_objects_with(next_object_id);
        next_object_id = following_object_id;

        let document_page_ids = document.get_pages().into_values().collect::<Vec<_>>();
        for page_id in &document_page_ids {
            // Resources and page boxes may live on an old Pages ancestor. Copy
            // inherited values onto the leaf before replacing its Parent.
            let inherited = [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"]
                .into_iter()
                .filter_map(|key| {
                    inherited_page_attribute(document, *page_id, key)
                        .map(|value| (key.to_vec(), value))
                })
                .collect::<Vec<_>>();

            let page = document
                .get_object_mut(*page_id)
                .and_then(lopdf::Object::as_dict_mut)
                .map_err(|error| format!("Could not read a PDF page: {error}"))?;
            for (key, value) in inherited {
                if !page.has(&key) {
                    page.set(key, value);
                }
            }
            page.set("Parent", pages_id);
        }

        let document_page_id_set = document_page_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        for (object_id, object) in std::mem::take(&mut document.objects) {
            let is_page = document_page_id_set.contains(&object_id);
            let is_old_root = matches!(object.type_name(), Ok(b"Catalog" | b"Pages"));
            if is_page || !is_old_root {
                output.objects.insert(object_id, object);
            }
        }
        page_ids.extend(document_page_ids);
    }

    let page_count = i64::try_from(page_ids.len())
        .map_err(|_| "These PDFs contain too many pages to merge.".to_owned())?;
    output.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.into_iter().map(lopdf::Object::Reference).collect::<Vec<_>>(),
            "Count" => page_count,
        }),
    );
    output.objects.insert(
        catalog_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }),
    );
    output.max_id = next_object_id - 1;
    output.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    output
        .save_to(&mut bytes)
        .map_err(|error| format!("Could not create the merged PDF: {error}"))?;
    Ok(bytes)
}

/// Pure core: keep source pages in the requested output order, adding a
/// clockwise rotation to each one, and return the serialized PDF.
fn organize(bytes: &[u8], pages: Vec<u32>, rotations: Vec<i32>) -> Result<Vec<u8>, String> {
    if pages.is_empty() {
        return Err("Choose at least one page for the output PDF.".to_owned());
    }
    if pages.len() != rotations.len() {
        return Err("Every output page must have one rotation value.".to_owned());
    }
    for (index, rotation) in rotations.iter().enumerate() {
        if !matches!(rotation, 0 | 90 | 180 | 270) {
            return Err(format!(
                "Rotation for output page {} must be 0, 90, 180, or 270 degrees.",
                index + 1
            ));
        }
    }

    let mut source = load_pdf(bytes)?;
    let source_page_count = u32::try_from(source.get_pages().len())
        .map_err(|_| "This PDF has too many pages to organize.".to_owned())?;
    if source_page_count == 0 {
        return Err("This PDF has no pages to organize.".to_owned());
    }
    for page_number in &pages {
        if !(1..=source_page_count).contains(page_number) {
            return Err(format!(
                "Page {page_number} is outside this PDF's page range (1–{source_page_count})."
            ));
        }
    }

    let output_version = source.version.as_str().max("1.5").to_owned();
    let mut output = lopdf::Document::with_version(output_version);
    let pages_id = output.new_object_id();
    let catalog_id = output.new_object_id();
    let first_source_id = output
        .max_id
        .checked_add(1)
        .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;

    // Move every source object into a range above the two new root objects.
    // lopdf updates references, including those inside page resources/streams.
    source.renumber_objects_with(first_source_id);
    let source_pages = source.get_pages();
    let selected_source_ids = pages
        .iter()
        .map(|page_number| {
            source_pages
                .get(page_number)
                .copied()
                .ok_or_else(|| format!("Could not find page {page_number} in this PDF."))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Once a page is attached to the new flat page tree it loses its old Pages
    // ancestors. Preserve all attributes that the PDF spec allows it to inherit.
    let unique_source_ids = selected_source_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    for page_id in &unique_source_ids {
        let inherited = [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"]
            .into_iter()
            .filter_map(|key| {
                inherited_page_attribute(&source, *page_id, key).map(|value| (key.to_vec(), value))
            })
            .collect::<Vec<_>>();
        let page = source
            .get_object_mut(*page_id)
            .and_then(lopdf::Object::as_dict_mut)
            .map_err(|error| format!("Could not read a PDF page: {error}"))?;
        for (key, value) in inherited {
            if !page.has(&key) {
                page.set(key, value);
            }
        }
    }

    let mut next_object_id = source
        .max_id
        .checked_add(1)
        .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;
    let mut used_source_ids = std::collections::HashSet::new();
    let mut output_page_ids = Vec::with_capacity(pages.len());
    let mut output_pages = Vec::with_capacity(pages.len());

    for (source_page_id, added_rotation) in selected_source_ids.into_iter().zip(rotations) {
        let mut page = source
            .get_dictionary(source_page_id)
            .map_err(|error| format!("Could not read a PDF page: {error}"))?
            .clone();
        let existing_rotation = match page.get(b"Rotate") {
            Ok(value) => value
                .as_i64()
                .map_err(|_| "A page has an invalid existing rotation value.".to_owned())?,
            Err(_) => 0,
        };
        let rotation = existing_rotation
            .checked_add(i64::from(added_rotation))
            .ok_or_else(|| "A page rotation value is too large.".to_owned())?
            .rem_euclid(360);
        page.set("Parent", pages_id);
        page.set("Rotate", rotation);

        // Reuse a selected leaf the first time. A repeated page gets a fresh
        // leaf ID while continuing to share immutable content/resources.
        let output_page_id = if used_source_ids.insert(source_page_id) {
            source_page_id
        } else {
            let duplicate_id = (next_object_id, 0);
            next_object_id = next_object_id
                .checked_add(1)
                .ok_or_else(|| "This PDF contains too many objects to organize.".to_owned())?;
            duplicate_id
        };
        output_page_ids.push(output_page_id);
        output_pages.push((output_page_id, lopdf::Object::Dictionary(page)));
    }

    // Start with the source object graph so shared resource references remain
    // intact, replace the selected leaves, then prune everything unreachable
    // from the new catalog (including omitted pages and the old page tree).
    for (object_id, object) in std::mem::take(&mut source.objects) {
        let is_old_root = matches!(object.type_name(), Ok(b"Catalog" | b"Pages"));
        if !is_old_root {
            output.objects.insert(object_id, object);
        }
    }
    for (page_id, page) in output_pages {
        output.objects.insert(page_id, page);
    }

    let page_count = i64::try_from(output_page_ids.len())
        .map_err(|_| "This PDF has too many output pages.".to_owned())?;
    output.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Pages",
            "Kids" => output_page_ids.into_iter().map(lopdf::Object::Reference).collect::<Vec<_>>(),
            "Count" => page_count,
        }),
    );
    output.objects.insert(
        catalog_id,
        lopdf::Object::Dictionary(lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        }),
    );
    output.max_id = next_object_id - 1;
    output.trailer.set("Root", catalog_id);
    output.prune_objects();

    let mut organized = Vec::new();
    output
        .save_to(&mut organized)
        .map_err(|error| format!("Could not create the organized PDF: {error}"))?;
    Ok(organized)
}

#[derive(Clone, Copy)]
enum JpegColor {
    Gray,
    Rgb,
}

impl JpegColor {
    fn components(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
        }
    }

    fn decoder_color_space(self) -> ColorSpace {
        match self {
            Self::Gray => ColorSpace::Luma,
            Self::Rgb => ColorSpace::RGB,
        }
    }

    fn encoder_color_type(self) -> ColorType {
        match self {
            Self::Gray => ColorType::Luma,
            Self::Rgb => ColorType::Rgb,
        }
    }
}

/// Return whether the byte stream announces a baseline sequential DCT frame.
/// Other JPEG modes can also use PDF's DCTDecode filter, but S3 deliberately
/// leaves them untouched rather than changing a mode we have not qualified.
fn is_baseline_jpeg(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return false;
    }

    let mut position = 2;
    while position < bytes.len() {
        if bytes[position] != 0xff {
            return false;
        }
        while position < bytes.len() && bytes[position] == 0xff {
            position += 1;
        }
        let Some(&marker) = bytes.get(position) else {
            return false;
        };
        position += 1;

        match marker {
            0xc0 => return true,
            // Any other start-of-frame mode is outside this spike's safe scope.
            0xc1..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => return false,
            0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7 => continue,
            0xda => return false,
            _ => {}
        }

        let Some(length_bytes) = bytes.get(position..position + 2) else {
            return false;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return false;
        }
        let Some(next_position) = position.checked_add(length) else {
            return false;
        };
        if next_position > bytes.len() {
            return false;
        }
        position = next_position;
    }

    false
}

fn downsample_if_huge(
    pixels: Vec<u8>,
    width: u16,
    height: u16,
    components: usize,
) -> Option<(Vec<u8>, u16, u16)> {
    let longest = width.max(height);
    if longest <= MAX_REENCODED_DIMENSION {
        return Some((pixels, width, height));
    }

    let longest = u32::from(longest);
    let new_width =
        ((u32::from(width) * u32::from(MAX_REENCODED_DIMENSION)) / longest).max(1) as u16;
    let new_height =
        ((u32::from(height) * u32::from(MAX_REENCODED_DIMENSION)) / longest).max(1) as u16;
    let output_length = usize::from(new_width)
        .checked_mul(usize::from(new_height))?
        .checked_mul(components)?;
    let mut downsampled = vec![0; output_length];

    // Nearest-neighbour resampling is deterministic, dependency-free, and only
    // applies to unusually large embedded images. JPEG quantization handles the
    // ordinary quality reduction without changing dimensions.
    for output_y in 0..usize::from(new_height) {
        let source_y = output_y * usize::from(height) / usize::from(new_height);
        for output_x in 0..usize::from(new_width) {
            let source_x = output_x * usize::from(width) / usize::from(new_width);
            let source_start = (source_y * usize::from(width) + source_x) * components;
            let output_start = (output_y * usize::from(new_width) + output_x) * components;
            downsampled[output_start..output_start + components]
                .copy_from_slice(&pixels[source_start..source_start + components]);
        }
    }

    Some((downsampled, new_width, new_height))
}

/// Decode and re-encode one qualified PDF DCT image. Any unsupported or
/// inconsistent image returns None so its original stream remains untouched.
fn reencode_dct_image(stream: &lopdf::Stream, quality: u8) -> Option<(Vec<u8>, u16, u16)> {
    if stream.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Image")
        || stream.dict.get(b"Filter").and_then(Object::as_name).ok() != Some(b"DCTDecode")
        || !is_baseline_jpeg(&stream.content)
    {
        return None;
    }

    let color = match stream
        .dict
        .get(b"ColorSpace")
        .and_then(Object::as_name)
        .ok()?
    {
        b"DeviceGray" => JpegColor::Gray,
        b"DeviceRGB" => JpegColor::Rgb,
        _ => return None,
    };
    if stream
        .dict
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .ok()
        != Some(8)
    {
        return None;
    }

    let declared_width =
        u16::try_from(stream.dict.get(b"Width").and_then(Object::as_i64).ok()?).ok()?;
    let declared_height =
        u16::try_from(stream.dict.get(b"Height").and_then(Object::as_i64).ok()?).ok()?;
    let pixel_count = usize::from(declared_width).checked_mul(usize::from(declared_height))?;
    if declared_width == 0 || declared_height == 0 || pixel_count > MAX_DECODED_PIXELS {
        return None;
    }

    let options = DecoderOptions::default().jpeg_set_out_colorspace(color.decoder_color_space());
    let mut decoder = JpegDecoder::new_with_options(stream.content.as_slice(), options);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    if info.width != declared_width || info.height != declared_height {
        return None;
    }
    let expected_length = pixel_count.checked_mul(color.components())?;
    if pixels.len() != expected_length {
        return None;
    }

    let (pixels, width, height) =
        downsample_if_huge(pixels, info.width, info.height, color.components())?;
    let mut encoded = Vec::new();
    Encoder::new(&mut encoded, quality)
        .encode(&pixels, width, height, color.encoder_color_type())
        .ok()?;
    Some((encoded, width, height))
}

/// Pure core: recompress qualified baseline-JPEG image XObjects, remove
/// nonessential metadata, compress otherwise-unfiltered streams, and serialize.
/// A no-growth fallback guarantees that a successful result is never larger.
fn compress(bytes: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let mut document = load_pdf(bytes)?;
    let source_page_count = document.get_pages().len();
    let quality = quality.clamp(1, 100);

    for object in document.objects.values_mut() {
        let Object::Stream(stream) = object else {
            continue;
        };
        let Some((encoded, width, height)) = reencode_dct_image(stream, quality) else {
            continue;
        };
        if encoded.len() >= stream.content.len() {
            continue;
        }

        stream.set_content(encoded);
        stream.dict.remove(b"DecodeParms");
        stream.dict.set("Width", i64::from(width));
        stream.dict.set("Height", i64::from(height));
    }

    // Metadata can appear on the catalog, pages, or other dictionaries. Info
    // is optional document metadata, so removing its trailer reference is safe.
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
    document.compress();
    let mut compressed = Vec::new();
    document
        .save_to(&mut compressed)
        .map_err(|error| format!("Could not create the compressed PDF: {error}"))?;

    // Re-parse before returning transformed bytes. If serialization changed the
    // page tree or did not reduce total size, preserve the known-good input.
    let valid_and_smaller = compressed.len() < bytes.len()
        && load_pdf(&compressed)
            .map(|output| output.get_pages().len() == source_page_count)
            .unwrap_or(false);
    Ok(if valid_and_smaller {
        compressed
    } else {
        bytes.to_vec()
    })
}

/// Parse a PDF from memory and return its number of pages.
#[wasm_bindgen]
pub fn pdf_page_count(bytes: &[u8]) -> Result<u32, JsValue> {
    page_count(bytes).map_err(|error| JsValue::from_str(&error))
}

/// Combine PDFs in array order and return the serialized PDF bytes.
#[wasm_bindgen]
pub fn merge_pdfs(docs: js_sys::Array) -> Result<Vec<u8>, JsValue> {
    let docs = docs
        .iter()
        .map(|bytes| js_sys::Uint8Array::new(&bytes).to_vec())
        .collect();
    merge(docs).map_err(|error| JsValue::from_str(&error))
}

/// Keep pages in the requested order, adding the parallel rotation values.
#[wasm_bindgen]
pub fn organize_pdf(
    bytes: &[u8],
    pages: Vec<u32>,
    rotations: Vec<i32>,
) -> Result<Vec<u8>, JsValue> {
    organize(bytes, pages, rotations).map_err(|error| JsValue::from_str(&error))
}

/// Reduce a PDF's size using only qualified, local pure-Rust codecs.
#[wasm_bindgen]
pub fn compress_pdf(bytes: &[u8], quality: u8) -> Result<Vec<u8>, JsValue> {
    compress(bytes, quality).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{
        compress, downsample_if_huge, merge, organize, page_count, ENCRYPTED_PDF_ERROR,
        MAX_REENCODED_DIMENSION,
    };
    use jpeg_encoder::{ColorType, Encoder};
    use lopdf::{dictionary, Document, Object, Stream};

    fn multi_page_pdf(page_count: u32) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_ids = (1..=page_count)
            .map(|page_number| {
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
                    "Resources" => dictionary! {},
                    "LocalbenchPageNumber" => page_number,
                })
            })
            .collect::<Vec<_>>();

        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.into_iter().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => page_count,
                "Rotate" => 90,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("in-memory PDF should serialize");
        bytes
    }

    fn one_page_pdf() -> Vec<u8> {
        multi_page_pdf(1)
    }

    fn jpeg_image_pdf() -> (Vec<u8>, usize) {
        const WIDTH: u16 = 640;
        const HEIGHT: u16 = 480;
        let mut pixels = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 3);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                // Fine deterministic detail makes the high-quality fixture a
                // meaningful compression target without checking in a binary.
                let noise = ((u32::from(x) * 73 + u32::from(y) * 151) % 251) as u8;
                pixels.extend_from_slice(&[
                    (x % 256) as u8 ^ noise,
                    (y % 256) as u8 ^ noise.rotate_left(2),
                    ((u32::from(x) + u32::from(y)) % 256) as u8 ^ noise.rotate_left(4),
                ]);
            }
        }

        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, 96)
            .encode(&pixels, WIDTH, HEIGHT, ColorType::Rgb)
            .expect("fixture JPEG should encode");
        let original_jpeg_size = jpeg.len();

        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => i64::from(WIDTH),
                "Height" => i64::from(HEIGHT),
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg,
        ));
        let content_id = document.add_object(Stream::new(
            dictionary! {},
            format!("q\n{} 0 0 {} 0 0 cm\n/Im0 Do\nQ\n", WIDTH, HEIGHT).into_bytes(),
        ));
        let metadata_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "Metadata",
                "Subtype" => "XML",
            },
            vec![b'm'; 4_096],
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), i64::from(WIDTH).into(), i64::from(HEIGHT).into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! {
                    "Im0" => image_id,
                },
            },
            "Contents" => content_id,
            "Metadata" => metadata_id,
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
            "Metadata" => metadata_id,
        });
        let info_id = document.add_object(dictionary! {
            "Creator" => Object::string_literal("localbench compression fixture"),
            "Producer" => Object::string_literal("localbench tests"),
        });
        document.trailer.set("Root", catalog_id);
        document.trailer.set("Info", info_id);

        let mut bytes = Vec::new();
        document
            .save_to(&mut bytes)
            .expect("JPEG fixture PDF should serialize");
        (bytes, original_jpeg_size)
    }

    #[test]
    fn counts_pages_in_a_valid_pdf() {
        let pdf = one_page_pdf();
        assert_eq!(page_count(&pdf).expect("PDF should parse"), 1);
    }

    // Malformed input must return Err, never panic: a panic aborts the whole
    // wasm instance and would hang the worker. Guards against garbage PDFs.
    #[test]
    fn rejects_garbage_without_panicking() {
        for bytes in [
            b"not a pdf at all".as_slice(),
            b"%PDF-1.7\ngarbage".as_slice(),
            b"".as_slice(),
        ] {
            assert!(
                page_count(bytes).is_err(),
                "garbage must be Err, not panic/Ok"
            );
        }
    }

    #[test]
    fn merges_two_one_page_pdfs_in_order() {
        let merged = merge(vec![one_page_pdf(), one_page_pdf()]).expect("PDFs should merge");

        assert_eq!(page_count(&merged).expect("merged PDF should parse"), 2);
        let merged_document = Document::load_mem(&merged).expect("merged PDF should load");
        let page_ids = merged_document
            .get_pages()
            .into_values()
            .collect::<Vec<_>>();
        assert_ne!(page_ids[0], page_ids[1], "source page IDs must not collide");
    }

    #[test]
    fn rejects_an_empty_merge() {
        assert!(merge(Vec::new()).is_err());
    }

    #[test]
    fn rejects_an_encrypted_merge_input() {
        let mut encrypted = Document::load_mem(&one_page_pdf()).expect("fixture should parse");
        encrypted.trailer.set("Encrypt", dictionary! {});
        let mut encrypted_bytes = Vec::new();
        encrypted
            .save_to(&mut encrypted_bytes)
            .expect("encrypted marker fixture should serialize");

        let error = merge(vec![one_page_pdf(), encrypted_bytes])
            .expect_err("encrypted input must be rejected");
        assert!(error.contains(ENCRYPTED_PDF_ERROR));
    }

    #[test]
    fn extracts_pages_in_requested_order() {
        let organized = organize(&multi_page_pdf(3), vec![3, 1], vec![0, 0])
            .expect("selected pages should organize");

        assert_eq!(page_count(&organized).expect("output PDF should parse"), 2);
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let source_numbers = document
            .get_pages()
            .into_values()
            .map(|page_id| {
                document
                    .get_dictionary(page_id)
                    .and_then(|page| page.get(b"LocalbenchPageNumber"))
                    .and_then(Object::as_i64)
                    .expect("fixture page number should remain")
            })
            .collect::<Vec<_>>();
        assert_eq!(source_numbers, vec![3, 1]);
    }

    #[test]
    fn adds_rotation_to_an_inherited_page_rotation() {
        let organized =
            organize(&multi_page_pdf(3), vec![2], vec![90]).expect("page should rotate");
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let page_id = document.get_pages()[&1];
        let rotation = document
            .get_dictionary(page_id)
            .and_then(|page| page.get(b"Rotate"))
            .and_then(Object::as_i64)
            .expect("output page should have a rotation");
        assert_eq!(rotation, 180);
    }

    #[test]
    fn duplicates_a_selected_page_with_independent_rotations() {
        let organized = organize(&multi_page_pdf(1), vec![1, 1], vec![0, 90])
            .expect("a source page should be reusable");
        let document = Document::load_mem(&organized).expect("output PDF should load");
        let page_ids = document.get_pages().into_values().collect::<Vec<_>>();
        let rotations = page_ids
            .iter()
            .map(|page_id| {
                document
                    .get_dictionary(*page_id)
                    .and_then(|page| page.get(b"Rotate"))
                    .and_then(Object::as_i64)
                    .expect("output page should have a rotation")
            })
            .collect::<Vec<_>>();
        assert_eq!(page_ids.len(), 2);
        assert_ne!(page_ids[0], page_ids[1]);
        assert_eq!(rotations, vec![90, 180]);
    }

    #[test]
    fn rejects_an_out_of_range_page() {
        assert!(organize(&multi_page_pdf(3), vec![4], vec![0]).is_err());
    }

    #[test]
    fn rejects_mismatched_organize_arrays() {
        assert!(organize(&multi_page_pdf(3), vec![1, 2], vec![0]).is_err());
    }

    #[test]
    fn rejects_an_unsupported_rotation() {
        assert!(organize(&multi_page_pdf(3), vec![1], vec![45]).is_err());
    }

    #[test]
    fn rejects_an_empty_organize_selection() {
        assert!(organize(&multi_page_pdf(3), Vec::new(), Vec::new()).is_err());
    }

    #[test]
    fn compresses_a_baseline_jpeg_without_changing_page_count() {
        let (source, original_jpeg_size) = jpeg_image_pdf();
        let compressed = compress(&source, 25).expect("fixture should compress");

        eprintln!(
            "baseline-JPEG fixture: {} bytes -> {} bytes",
            source.len(),
            compressed.len()
        );
        assert!(compressed.len() < source.len());
        assert_eq!(page_count(&compressed).expect("output should parse"), 1);

        let output = Document::load_mem(&compressed).expect("output PDF should load");
        assert!(output.trailer.get(b"Info").is_err());
        let output_image = output
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("output image should remain");
        assert!(output_image.content.len() < original_jpeg_size);
        assert_eq!(
            output_image
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("image filter should remain"),
            b"DCTDecode"
        );
        assert_eq!(
            output_image
                .dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("image width should remain"),
            640
        );
        assert_eq!(
            output_image
                .dict
                .get(b"Height")
                .and_then(Object::as_i64)
                .expect("image height should remain"),
            480
        );
        assert_eq!(
            output_image
                .dict
                .get(b"ColorSpace")
                .and_then(Object::as_name)
                .expect("image color space should remain"),
            b"DeviceRGB"
        );
        assert_eq!(
            output_image
                .dict
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .expect("image bit depth should remain"),
            8
        );
        assert!(output.objects.values().all(|object| match object {
            Object::Dictionary(dictionary) => !dictionary.has(b"Metadata"),
            Object::Stream(stream) => !stream.dict.has(b"Metadata"),
            _ => true,
        }));
    }

    #[test]
    fn clamps_compression_quality_to_the_public_range() {
        let (source, _) = jpeg_image_pdf();
        assert_eq!(
            compress(&source, 0).expect("quality zero should clamp"),
            compress(&source, 1).expect("quality one should work")
        );
        assert_eq!(
            compress(&source, 101).expect("quality 101 should clamp"),
            compress(&source, 100).expect("quality 100 should work")
        );
    }

    #[test]
    fn a_no_image_pdf_stays_valid_and_never_grows() {
        let source = multi_page_pdf(2);
        let compressed = compress(&source, 30).expect("no-image PDF should still compress");

        assert!(compressed.len() <= source.len());
        assert_eq!(
            page_count(&compressed).expect("output should remain valid"),
            2
        );
    }

    #[test]
    fn leaves_jpeg_2000_image_streams_untouched() {
        let (source, _) = jpeg_image_pdf();
        let mut input = Document::load_mem(&source).expect("fixture should load");
        let input_image = input
            .objects
            .values_mut()
            .filter_map(|object| object.as_stream_mut().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("fixture image should exist");
        input_image.dict.set("Filter", "JPXDecode");
        let original_content = input_image.content.clone();
        let mut input_bytes = Vec::new();
        input
            .save_to(&mut input_bytes)
            .expect("modified fixture should serialize");

        let compressed = compress(&input_bytes, 20).expect("unsupported image should be preserved");
        let output = Document::load_mem(&compressed).expect("output should load");
        let output_image = output
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .expect("unsupported image should remain");
        assert_eq!(
            output_image
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("filter should remain"),
            b"JPXDecode"
        );
        assert_eq!(output_image.content, original_content);
    }

    #[test]
    fn downsamples_an_unusually_large_dimension() {
        let width = MAX_REENCODED_DIMENSION + 1;
        let pixels = vec![42; usize::from(width) * 2 * 3];
        let (downsampled, output_width, output_height) =
            downsample_if_huge(pixels, width, 2, 3).expect("image should downsample");

        assert_eq!(output_width, MAX_REENCODED_DIMENSION);
        assert_eq!(output_height, 1);
        assert_eq!(
            downsampled.len(),
            usize::from(output_width) * usize::from(output_height) * 3
        );
    }
}
