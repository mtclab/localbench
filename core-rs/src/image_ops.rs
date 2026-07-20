//! Deterministic image transforms backed only by permissively licensed,
//! pure-Rust codecs.
//!
//! ## Deferred
//!
//! - Lossy WebP compression: the pure-Rust `image` encoder is lossless-only;
//!   using libwebp would add a forbidden C dependency. Lossless WebP output is
//!   supported for resize and convert.
//! - PNG optimization beyond the `png` crate's best compression: stronger
//!   optimizers are outside the permitted dependency and wasm portability floor.
//! - Multi-frame animation transforms: v1's single-image operations decode the
//!   first frame; preserving frame timing and disposal is a separate operation.

use std::io::Cursor;

use image::{
    codecs::{
        bmp::BmpEncoder,
        gif::GifEncoder,
        png::{CompressionType, FilterType, PngEncoder},
        webp::WebPEncoder,
    },
    imageops::FilterType as ResizeFilter,
    DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, ImageFormat, ImageReader,
};
use jpeg_encoder::{ColorType, Encoder};
use wasm_bindgen::prelude::*;

use super::{MAX_DECODED_PIXELS, MAX_REENCODED_DIMENSION};

const DEFAULT_JPEG_QUALITY: u8 = 90;

fn supported_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::Bmp
            | ImageFormat::WebP
    )
}

fn guard_decoded_dimensions(width: u32, height: u32) -> Result<(), String> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "This image's dimensions are too large to decode.".to_owned())?;
    if width == 0 || height == 0 {
        return Err("This image has invalid zero-sized dimensions.".to_owned());
    }
    if pixels > MAX_DECODED_PIXELS as u64 {
        return Err(format!(
            "This image is too large to decode (maximum {MAX_DECODED_PIXELS} pixels)."
        ));
    }

    Ok(())
}

/// Inspect dimensions before allocating the decoded pixel buffer, then decode.
fn decode(bytes: &[u8]) -> Result<(ImageFormat, DynamicImage), String> {
    let format = image::guess_format(bytes)
        .map_err(|error| format!("Could not determine this image's format: {error}"))?;
    if !supported_format(format) {
        return Err(
            "This image format is unsupported; use JPEG, PNG, GIF, BMP, or WebP.".to_owned(),
        );
    }

    let dimensions_reader = ImageReader::with_format(Cursor::new(bytes), format);
    let (width, height) = dimensions_reader
        .into_dimensions()
        .map_err(|error| format!("Could not read this image's dimensions: {error}"))?;
    guard_decoded_dimensions(width, height)?;

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(
        u64::try_from(MAX_DECODED_PIXELS)
            .unwrap_or(u64::MAX)
            .saturating_mul(8),
    );
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| format!("Could not decode this image: {error}"))?;
    guard_decoded_dimensions(decoded.width(), decoded.height())?;

    Ok((format, decoded))
}

fn cap_reencoded_dimensions(image: DynamicImage) -> DynamicImage {
    let maximum = u32::from(MAX_REENCODED_DIMENSION);
    if image.width() <= maximum && image.height() <= maximum {
        image
    } else {
        image.resize(maximum, maximum, ResizeFilter::Lanczos3)
    }
}

fn flatten_alpha_onto_white(image: &DynamicImage) -> Vec<u8> {
    let rgba = image.to_rgba8();
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.pixels() {
        let alpha = u16::from(pixel[3]);
        let inverse_alpha = 255 - alpha;
        for channel in &pixel.0[..3] {
            let flattened = (u16::from(*channel) * alpha + 255 * inverse_alpha + 127) / 255;
            rgb.push(flattened as u8);
        }
    }
    rgb
}

/// Encode only raw pixels, deliberately omitting EXIF, XMP, ICC, and all other
/// source metadata. JPEG output composites transparent pixels onto white.
fn encode(image: &DynamicImage, format: ImageFormat, jpeg_quality: u8) -> Result<Vec<u8>, String> {
    let (width, height) = image.dimensions();
    let mut encoded = Vec::new();

    match format {
        ImageFormat::Jpeg => {
            let width = u16::try_from(width)
                .map_err(|_| "This image is too wide to encode as JPEG.".to_owned())?;
            let height = u16::try_from(height)
                .map_err(|_| "This image is too tall to encode as JPEG.".to_owned())?;
            let rgb = flatten_alpha_onto_white(image);
            Encoder::new(&mut encoded, jpeg_quality)
                .encode(&rgb, width, height, ColorType::Rgb)
                .map_err(|error| format!("Could not encode the JPEG image: {error}"))?;
        }
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive)
                .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the PNG image: {error}"))?;
        }
        ImageFormat::Gif => {
            let rgba = image.to_rgba8();
            GifEncoder::new(&mut encoded)
                .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the GIF image: {error}"))?;
        }
        ImageFormat::Bmp => {
            let rgba = image.to_rgba8();
            BmpEncoder::new(&mut encoded)
                .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the BMP image: {error}"))?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            WebPEncoder::new_lossless(&mut encoded)
                .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the WebP image: {error}"))?;
        }
        _ => {
            return Err("This image format cannot be encoded safely.".to_owned());
        }
    }

    Ok(encoded)
}

fn target_format(target: &str) -> Result<ImageFormat, String> {
    match target {
        "png" => Ok(ImageFormat::Png),
        "jpeg" => Ok(ImageFormat::Jpeg),
        "webp" => Ok(ImageFormat::WebP),
        _ => Err("Target format must be \"png\", \"jpeg\", or \"webp\".".to_owned()),
    }
}

/// Pure core: resize without upscaling and re-encode in the detected source
/// format. `keep_aspect` fits inside the box; otherwise each axis is clamped
/// independently before stretching.
fn resize(bytes: &[u8], max_w: u32, max_h: u32, keep_aspect: bool) -> Result<Vec<u8>, String> {
    if max_w == 0 || max_h == 0 {
        return Err("Resize width and height must both be greater than zero.".to_owned());
    }

    let (format, image) = decode(bytes)?;
    let maximum = u32::from(MAX_REENCODED_DIMENSION);
    let target_width = max_w.min(maximum).min(image.width());
    let target_height = max_h.min(maximum).min(image.height());
    let resized = if keep_aspect {
        image.resize(target_width, target_height, ResizeFilter::Lanczos3)
    } else {
        image.resize_exact(target_width, target_height, ResizeFilter::Lanczos3)
    };

    encode(&resized, format, DEFAULT_JPEG_QUALITY)
}

/// Pure core: decode pixels and encode them into the requested format. PNG and
/// WebP preserve alpha; JPEG composites alpha onto white.
fn convert(bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let target = target_format(target)?;
    let (_, image) = decode(bytes)?;
    let image = cap_reencoded_dimensions(image);
    encode(&image, target, DEFAULT_JPEG_QUALITY)
}

fn validate_encoded_image(bytes: &[u8], expected_format: ImageFormat) -> Result<(), String> {
    let (actual_format, _) = decode(bytes)
        .map_err(|error| format!("Could not validate the re-encoded image: {error}"))?;
    if actual_format != expected_format {
        return Err("The re-encoded image has an unexpected format.".to_owned());
    }
    Ok(())
}

/// Pure core: recompress JPEG or PNG pixels, verify the encoded image, and
/// preserve the original bytes whenever the re-encode does not reduce size.
fn compress(bytes: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    if !(1..=100).contains(&quality) {
        return Err("Image quality must be between 1 and 100.".to_owned());
    }

    let (format, image) = decode(bytes)?;
    if !matches!(format, ImageFormat::Jpeg | ImageFormat::Png) {
        return Err("Only JPEG and PNG images can be compressed in this version.".to_owned());
    }
    let image = cap_reencoded_dimensions(image);
    let encoded = encode(&image, format, quality)?;
    validate_encoded_image(&encoded, format)?;

    Ok(if encoded.len() < bytes.len() {
        encoded
    } else {
        bytes.to_vec()
    })
}

/// Resize an image and return bytes in its detected source format.
#[wasm_bindgen]
pub fn resize_image(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    keep_aspect: bool,
) -> Result<Vec<u8>, JsValue> {
    resize(bytes, max_w, max_h, keep_aspect).map_err(|error| JsValue::from_str(&error))
}

/// Convert an image to PNG, JPEG, or lossless WebP.
#[wasm_bindgen]
pub fn convert_image(bytes: &[u8], target: &str) -> Result<Vec<u8>, JsValue> {
    convert(bytes, target).map_err(|error| JsValue::from_str(&error))
}

/// Compress a JPEG or PNG without ever returning more bytes than the input.
#[wasm_bindgen]
pub fn compress_image(bytes: &[u8], quality: u8) -> Result<Vec<u8>, JsValue> {
    compress(bytes, quality).map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{
        compress, convert, encode, guard_decoded_dimensions, resize, CompressionType, Encoder,
        ExtendedColorType, FilterType, ImageEncoder, ImageFormat, PngEncoder, MAX_DECODED_PIXELS,
    };
    use image::{DynamicImage, GenericImageView, Rgb, RgbImage, Rgba, RgbaImage};
    use jpeg_encoder::ColorType;

    fn sample_rgba(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([
                ((x * 29 + y * 7) % 256) as u8,
                ((x * 11 + y * 41) % 256) as u8,
                ((x * 53 + y * 17) % 256) as u8,
                ((x * 3 + y * 5) % 256) as u8,
            ])
        })
    }

    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = sample_rgba(width, height);
        let mut encoded = Vec::new();
        PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .expect("fixture PNG should encode");
        encoded
    }

    fn jpeg_fixture(width: u16, height: u16, quality: u8) -> Vec<u8> {
        let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
            let noise = ((x * 73 + y * 151 + x * y * 17) % 251) as u8;
            Rgb([
                (x % 256) as u8 ^ noise,
                (y % 256) as u8 ^ noise.rotate_left(2),
                ((x + y) % 256) as u8 ^ noise.rotate_left(4),
            ])
        });
        let mut encoded = Vec::new();
        Encoder::new(&mut encoded, quality)
            .encode(image.as_raw(), width, height, ColorType::Rgb)
            .expect("fixture JPEG should encode");
        encoded
    }

    fn dimensions(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes)
            .expect("output image should decode")
            .dimensions()
    }

    #[test]
    fn resize_downscale_fits_the_box_and_preserves_aspect() {
        let resized = resize(&png_fixture(400, 200), 100, 100, true)
            .expect("PNG should resize within the box");

        assert_eq!(
            image::guess_format(&resized).expect("output format should be detectable"),
            ImageFormat::Png
        );
        assert_eq!(dimensions(&resized), (100, 50));
    }

    #[test]
    fn resize_refuses_zero_dimensions() {
        let source = png_fixture(20, 10);
        assert!(resize(&source, 0, 10, true).is_err());
        assert!(resize(&source, 10, 0, false).is_err());
    }

    #[test]
    fn resize_never_upscales() {
        let resized = resize(&png_fixture(40, 20), 400, 400, true)
            .expect("a larger box should not upscale the image");

        assert_eq!(dimensions(&resized), (40, 20));
    }

    #[test]
    fn resize_without_aspect_stretches_to_the_clamped_box() {
        let resized = resize(&png_fixture(80, 40), 30, 20, false)
            .expect("PNG should stretch to the requested box");

        assert_eq!(dimensions(&resized), (30, 20));
    }

    #[test]
    fn resize_preserves_each_additional_supported_format() {
        let image = DynamicImage::ImageRgba8(sample_rgba(32, 16));
        for format in [ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::WebP] {
            let source = encode(&image, format, 90).expect("fixture should encode");
            let resized = resize(&source, 10, 10, true).expect("fixture should resize");

            assert_eq!(
                image::guess_format(&resized).expect("output format should be detectable"),
                format
            );
            assert_eq!(dimensions(&resized), (10, 5));
        }
    }

    #[test]
    fn convert_png_to_jpeg_and_jpeg_to_png() {
        let jpeg = convert(&png_fixture(64, 32), "jpeg").expect("PNG should convert to JPEG");
        assert_eq!(
            image::guess_format(&jpeg).expect("output format should be detectable"),
            ImageFormat::Jpeg
        );
        assert_eq!(dimensions(&jpeg), (64, 32));

        let png = convert(&jpeg_fixture(48, 24, 90), "png").expect("JPEG should convert to PNG");
        assert_eq!(
            image::guess_format(&png).expect("output format should be detectable"),
            ImageFormat::Png
        );
        assert_eq!(dimensions(&png), (48, 24));
    }

    #[test]
    fn jpeg_conversion_flattens_alpha_onto_white() {
        let mut transparent = RgbaImage::new(1, 1);
        transparent.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        let mut source = Vec::new();
        PngEncoder::new(&mut source)
            .write_image(transparent.as_raw(), 1, 1, ExtendedColorType::Rgba8)
            .expect("transparent fixture should encode");

        let jpeg = convert(&source, "jpeg").expect("transparent PNG should convert");
        let decoded = image::load_from_memory(&jpeg)
            .expect("JPEG should decode")
            .to_rgb8();
        let pixel = decoded.get_pixel(0, 0);
        assert!(pixel.0.iter().all(|channel| *channel >= 250));
    }

    #[test]
    fn compresses_a_jpeg_at_low_quality_and_keeps_it_decodable() {
        let source = jpeg_fixture(320, 240, 100);
        let compressed = compress(&source, 10).expect("JPEG should compress");

        assert!(compressed.len() < source.len());
        assert_eq!(
            image::guess_format(&compressed).expect("output format should be detectable"),
            ImageFormat::Jpeg
        );
        assert_eq!(dimensions(&compressed), (320, 240));
    }

    #[test]
    fn compress_never_returns_more_bytes_than_the_input() {
        let source = png_fixture(1, 1);
        let compressed = compress(&source, 100).expect("tiny PNG should still be accepted");

        assert!(compressed.len() <= source.len());
        assert_eq!(dimensions(&compressed), (1, 1));
    }

    #[test]
    fn malformed_input_returns_errors_without_panicking() {
        for garbage in [
            b"not an image".as_slice(),
            b"\x89PNG\r\n\x1a\ntruncated".as_slice(),
            b"".as_slice(),
        ] {
            assert!(resize(garbage, 10, 10, true).is_err());
            assert!(convert(garbage, "png").is_err());
            assert!(compress(garbage, 50).is_err());
        }
    }

    #[test]
    fn pixel_guard_rejects_claimed_dimensions_before_allocation() {
        assert!(guard_decoded_dimensions(8_001, 8_000).is_err());
        assert!(guard_decoded_dimensions(u32::MAX, u32::MAX).is_err());
        assert!(guard_decoded_dimensions(8_000, 8_000).is_ok());
        assert_eq!(8_000_u64 * 8_000, MAX_DECODED_PIXELS as u64);
    }

    #[test]
    fn reencoding_removes_exif_gps_app1_metadata() {
        let jpeg = jpeg_fixture(64, 32, 90);
        let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x25\x88\x04\0\x01\0\0\0\x1a\0\0\0\0\0\0\0\x01\0\x01\0\x02\0\x02\0\0\0N\0\0\0\0\0\0\0".to_vec();
        exif.extend_from_slice(b"GPS latitude");
        let segment_length = u16::try_from(exif.len() + 2).expect("fixture APP1 should fit");
        let mut with_exif = Vec::with_capacity(jpeg.len() + exif.len() + 4);
        with_exif.extend_from_slice(&jpeg[..2]);
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&segment_length.to_be_bytes());
        with_exif.extend_from_slice(&exif);
        with_exif.extend_from_slice(&jpeg[2..]);
        assert!(with_exif.windows(6).any(|window| window == b"Exif\0\0"));
        assert!(with_exif.windows(2).any(|window| window == [0xff, 0xe1]));

        let stripped = convert(&with_exif, "jpeg").expect("EXIF JPEG should re-encode");
        assert!(!stripped.windows(6).any(|window| window == b"Exif\0\0"));
        assert!(!stripped.windows(2).any(|window| window == [0xff, 0xe1]));
        assert_eq!(dimensions(&stripped), (64, 32));
    }

    #[test]
    fn repeated_transforms_are_byte_identical() {
        let source = png_fixture(80, 40);
        assert_eq!(
            resize(&source, 30, 30, true).expect("first resize should work"),
            resize(&source, 30, 30, true).expect("second resize should work")
        );
        assert_eq!(
            convert(&source, "webp").expect("first conversion should work"),
            convert(&source, "webp").expect("second conversion should work")
        );
    }
}
