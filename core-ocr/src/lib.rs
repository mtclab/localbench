use std::cell::RefCell;
use std::io::Cursor;
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::{DynamicImage, ImageFormat, ImageReader, RgbImage};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten::Model;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const MAX_DECODED_PIXELS: u64 = 64_000_000;
const MAX_ENCODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 32 * 1024 * 1024;

thread_local! {
    static ENGINE: RefCell<Option<OcrEngine>> = const { RefCell::new(None) };
}

#[derive(Debug)]
struct PositionedWord {
    text: String,
    left: f32,
    bottom: f32,
    width: f32,
    height: f32,
}

fn supported_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::WebP
            | ImageFormat::Gif
            | ImageFormat::Bmp
    )
}

fn guard_decoded_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("This image has invalid zero-sized dimensions.".to_owned());
    }

    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "This image's dimensions are too large to decode.".to_owned())?;
    if pixels > MAX_DECODED_PIXELS {
        return Err(format!(
            "This image is too large to decode (maximum {MAX_DECODED_PIXELS} pixels)."
        ));
    }
    Ok(())
}

/// Inspect the encoded header and dimensions before allocating decoded pixels.
fn decode_image(bytes: &[u8]) -> Result<RgbImage, String> {
    if bytes.is_empty() {
        return Err("The image is empty.".to_owned());
    }
    if bytes.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(format!(
            "The encoded image is too large (maximum {} MiB).",
            MAX_ENCODED_IMAGE_BYTES / (1024 * 1024)
        ));
    }

    let format = image::guess_format(bytes)
        .map_err(|error| format!("Could not determine this image's format: {error}"))?;
    if !supported_format(format) {
        return Err(
            "This image format is unsupported; use JPEG, PNG, WebP, GIF, or BMP.".to_owned(),
        );
    }

    let dimensions_reader = ImageReader::with_format(Cursor::new(bytes), format);
    let (width, height) = dimensions_reader
        .into_dimensions()
        .map_err(|error| format!("Could not read this image's dimensions: {error}"))?;
    guard_decoded_dimensions(width, height)?;

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    // Allow enough headroom for formats that decode through wide channel types,
    // while still bounding allocation before a decoder receives the input.
    limits.max_alloc = Some(MAX_DECODED_PIXELS.saturating_mul(8));
    reader.limits(limits);
    let decoded: DynamicImage = reader
        .decode()
        .map_err(|error| format!("Could not decode this image: {error}"))?;
    guard_decoded_dimensions(decoded.width(), decoded.height())?;

    Ok(decoded.into_rgb8())
}

fn validate_model_blob(name: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err(format!("The {name} model is empty."));
    }
    if bytes.len() > MAX_MODEL_BYTES {
        return Err(format!(
            "The {name} model is too large (maximum {} MiB).",
            MAX_MODEL_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

fn load_engine_inner(detection: &[u8], recognition: &[u8]) -> Result<(), String> {
    validate_model_blob("detection", detection)?;
    validate_model_blob("recognition", recognition)?;

    let detection_model = Model::load(detection.to_vec())
        .map_err(|error| format!("Could not load the detection model: {error}"))?;
    let recognition_model = Model::load(recognition.to_vec())
        .map_err(|error| format!("Could not load the recognition model: {error}"))?;
    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(detection_model),
        recognition_model: Some(recognition_model),
        ..Default::default()
    })
    .map_err(|error| format!("Could not build the OCR engine: {error}"))?;

    // Replace only after both models and the new engine are valid, preserving a
    // working engine if a later reload attempt fails.
    ENGINE.with(|slot| {
        let mut slot = slot
            .try_borrow_mut()
            .map_err(|_| "The OCR engine is already in use.".to_owned())?;
        *slot = Some(engine);
        Ok(())
    })
}

fn run_ocr_inner(image: &[u8]) -> Result<String, String> {
    let decoded = decode_image(image)?;
    let source = ImageSource::from_bytes(decoded.as_raw(), decoded.dimensions())
        .map_err(|error| format!("Could not prepare the image pixels: {error}"))?;

    ENGINE.with(|slot| {
        let slot = slot
            .try_borrow()
            .map_err(|_| "The OCR engine is already in use.".to_owned())?;
        let engine = slot
            .as_ref()
            .ok_or_else(|| "The OCR engine is not loaded.".to_owned())?;
        let input = engine
            .prepare_input(source)
            .map_err(|error| format!("Could not prepare the image for OCR: {error}"))?;
        engine
            .get_text(&input)
            .map_err(|error| format!("OCR failed: {error}"))
    })
}

fn recognize_positioned_words(
    engine: &OcrEngine,
    source: ImageSource,
) -> Result<Vec<PositionedWord>, String> {
    let input = engine
        .prepare_input(source)
        .map_err(|error| format!("Could not prepare the image for OCR: {error}"))?;
    let detected_words = engine
        .detect_words(&input)
        .map_err(|error| format!("Could not detect words: {error}"))?;
    let text_line_regions = engine.find_text_lines(&input, &detected_words);
    let recognized_lines = engine
        .recognize_text(&input, &text_line_regions)
        .map_err(|error| format!("Could not recognize text: {error}"))?;

    let mut words = Vec::new();
    for line in recognized_lines.into_iter().flatten() {
        for word in line.words() {
            let text = word.to_string();
            if text.trim().is_empty() {
                continue;
            }

            let rect = word.bounding_rect();
            words.push(PositionedWord {
                text,
                left: rect.left() as f32,
                bottom: rect.bottom() as f32,
                width: rect.width() as f32,
                height: rect.height() as f32,
            });
        }
    }
    Ok(words)
}

fn flate_compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(bytes)
        .map_err(|error| format!("Could not compress the PDF image: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("Could not finish compressing the PDF image: {error}"))
}

fn pdf_real(value: f32) -> Object {
    Object::Real(value)
}

fn append_escaped_pdf_literal(output: &mut Vec<u8>, text: &[u8]) {
    for &byte in text {
        match byte {
            b'(' | b')' | b'\\' => {
                output.push(b'\\');
                output.push(byte);
            }
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x08 => output.extend_from_slice(b"\\b"),
            0x0c => output.extend_from_slice(b"\\f"),
            _ => output.push(byte),
        }
    }
}

fn build_searchable_pdf(
    width: u32,
    height: u32,
    rgb: &[u8],
    words: &[PositionedWord],
) -> Result<Vec<u8>, String> {
    let page_width = width as f32;
    let page_height = height as f32;
    let mut document = Document::with_version("1.5");

    // Keep object creation order fixed so identical inputs produce identical bytes.
    let pages_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let image_id = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => i64::from(width),
            "Height" => i64::from(height),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "FlateDecode",
        },
        flate_compress(rgb)?,
    ));

    let image_operations = vec![
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                pdf_real(page_width),
                0.into(),
                0.into(),
                pdf_real(page_height),
                0.into(),
                0.into(),
            ],
        ),
        Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
        Operation::new("Q", vec![]),
    ];
    let mut content = Content {
        operations: image_operations,
    }
    .encode()
    .map_err(|error| format!("Could not encode the PDF page content: {error}"))?;

    for word in words {
        let font_size = word.height.max(1.0);
        // Fit Helvetica's approximate advance to the detected word rectangle.
        // This makes selection highlights follow the image text more closely.
        let char_count = word.text.chars().count().max(1) as f32;
        let estimated_advance = 0.5 * font_size * char_count;
        let horizontal_scale = (100.0 * word.width / estimated_advance).clamp(10.0, 1_000.0);
        let baseline_y = page_height - word.bottom;

        let word_operations = [
            Operation::new("BT", vec![]),
            // Text rendering mode 3 makes both fill and stroke invisible.
            Operation::new("Tr", vec![3.into()]),
            Operation::new(
                "Tf",
                vec![Object::Name(b"F0".to_vec()), pdf_real(font_size)],
            ),
            Operation::new("Tz", vec![pdf_real(horizontal_scale)]),
            Operation::new("Td", vec![pdf_real(word.left), pdf_real(baseline_y)]),
        ];
        content.push(b'\n');
        content.extend_from_slice(
            &Content {
                operations: word_operations,
            }
            .encode()
            .map_err(|error| format!("Could not encode the PDF text layer: {error}"))?,
        );
        content.extend_from_slice(b"\n(");
        append_escaped_pdf_literal(&mut content, word.text.as_bytes());
        content.extend_from_slice(b") Tj\nET");
    }

    let content_id = document.add_object(Stream::new(dictionary! {}, content));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), i64::from(width).into(), i64::from(height).into()],
        "Contents" => content_id,
        "Resources" => dictionary! {
            "Font" => dictionary! { "F0" => font_id },
            "XObject" => dictionary! { "Im0" => image_id },
        },
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    // Be explicit even though a new lopdf document does not create metadata.
    document.trailer.remove(b"Info");

    let mut bytes = Vec::new();
    document
        .save_to(&mut bytes)
        .map_err(|error| format!("Could not serialize the searchable PDF: {error}"))?;
    Ok(bytes)
}

fn searchable_pdf_inner(image: &[u8]) -> Result<Vec<u8>, String> {
    let decoded = decode_image(image)?;
    let (width, height) = decoded.dimensions();
    let source = ImageSource::from_bytes(decoded.as_raw(), decoded.dimensions())
        .map_err(|error| format!("Could not prepare the image pixels: {error}"))?;

    let words = ENGINE.with(|slot| {
        let slot = slot
            .try_borrow()
            .map_err(|_| "The OCR engine is already in use.".to_owned())?;
        let engine = slot
            .as_ref()
            .ok_or_else(|| "The OCR engine is not loaded.".to_owned())?;
        recognize_positioned_words(engine, source)
    })?;

    build_searchable_pdf(width, height, decoded.as_raw(), &words)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn load_engine(detection: &[u8], recognition: &[u8]) -> Result<(), JsValue> {
    load_engine_inner(detection, recognition).map_err(|error| JsValue::from_str(&error))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_engine(detection: &[u8], recognition: &[u8]) -> Result<(), String> {
    load_engine_inner(detection, recognition)
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn run_ocr(image: &[u8]) -> Result<String, JsValue> {
    run_ocr_inner(image).map_err(|error| JsValue::from_str(&error))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn run_ocr(image: &[u8]) -> Result<String, String> {
    run_ocr_inner(image)
}

/// Build a searchable PDF from one image: a single page showing the image with an
/// invisible Helvetica text layer positioned over each recognized word. Requires the
/// engine to be loaded (same as run_ocr). Deterministic; no /Info; metadata-clean.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn searchable_pdf(image: &[u8]) -> Result<Vec<u8>, JsValue> {
    searchable_pdf_inner(image).map_err(|error| JsValue::from_str(&error))
}

/// Native counterpart of the WebAssembly searchable-PDF export.
#[cfg(not(target_arch = "wasm32"))]
pub fn searchable_pdf(image: &[u8]) -> Result<Vec<u8>, String> {
    searchable_pdf_inner(image)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub fn engine_ready() -> bool {
    ENGINE.with(|slot| {
        slot.try_borrow()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_is_not_ready_before_load() {
        assert!(!engine_ready());
    }

    #[test]
    fn run_ocr_rejects_garbage_without_panicking() {
        let error = run_ocr(b"not an image").expect_err("garbage must be rejected");
        assert!(error.contains("format"));
    }

    #[test]
    fn searchable_pdf_requires_a_loaded_engine() {
        let image =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255])));
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("tiny PNG fixture should encode");

        let error = searchable_pdf(&encoded.into_inner())
            .expect_err("searchable PDF should require a loaded OCR engine");
        assert!(error.contains("engine is not loaded"));
    }

    #[test]
    fn pdf_builder_is_deterministic_and_metadata_clean() {
        let rgb = [255, 255, 255, 0, 0, 0];
        let words = [PositionedWord {
            text: "a(b)\\c".to_owned(),
            left: 0.0,
            bottom: 1.0,
            width: 2.0,
            height: 1.0,
        }];

        let first =
            build_searchable_pdf(2, 1, &rgb, &words).expect("model-free PDF fixture should build");
        let second = build_searchable_pdf(2, 1, &rgb, &words)
            .expect("model-free PDF fixture should rebuild");
        assert!(first.starts_with(b"%PDF"));
        assert_eq!(first, second);

        let parsed = Document::load_mem(&first).expect("generated PDF should parse");
        assert!(parsed.trailer.get(b"Info").is_err());
        let image_stream = parsed
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| stream.dict.has_type(b"XObject"))
            .expect("generated PDF should contain an image XObject");
        assert_eq!(
            image_stream
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("image should declare a filter"),
            b"FlateDecode"
        );
        assert_eq!(
            image_stream
                .decompressed_content()
                .expect("image should Flate-decompress"),
            rgb
        );

        let content = parsed
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| !stream.dict.has_type(b"XObject"))
            .expect("generated PDF should contain a page content stream")
            .get_plain_content()
            .expect("page content should decode");
        assert!(content.windows(4).any(|window| window == b"3 Tr"));
        assert!(content
            .windows(b"a\\(b\\)\\\\c".len())
            .any(|window| window == b"a\\(b\\)\\\\c"));
        assert!(parsed
            .extract_text(&[1])
            .expect("searchable text should extract")
            .contains("a(b)\\c"));
    }

    #[test]
    fn decoded_pixel_guard_rejects_oversized_dimensions() {
        let error = guard_decoded_dimensions(8_001, 8_000)
            .expect_err("more than 64 million pixels must be rejected");
        assert!(error.contains("too large"));
        assert!(guard_decoded_dimensions(8_000, 8_000).is_ok());
    }

    #[test]
    fn empty_model_blob_is_rejected_without_replacing_engine() {
        let error = load_engine(&[], &[1]).expect_err("empty model must be rejected");
        assert!(error.contains("detection model is empty"));
        assert!(!engine_ready());
    }
}
