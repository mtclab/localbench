use std::cell::RefCell;
use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader, RgbImage};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const MAX_DECODED_PIXELS: u64 = 64_000_000;
const MAX_ENCODED_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 32 * 1024 * 1024;

thread_local! {
    static ENGINE: RefCell<Option<OcrEngine>> = const { RefCell::new(None) };
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
