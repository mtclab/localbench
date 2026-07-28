//! Deterministic image transforms backed only by permissively licensed,
//! pure-Rust codecs.
//!
//! Every transform states what it did. When the core cannot carry something
//! through (a colour profile a format cannot hold, animation frames a target
//! format has no room for, bit depth a codec cannot express), it returns a
//! notice instead of quietly changing the user's picture.
//!
//! ## Deferred
//!
//! - Lossy WebP compression: the pure-Rust `image` encoder is lossless-only;
//!   using libwebp would add a forbidden C dependency. Lossless WebP output is
//!   supported for resize and convert.
//! - PNG optimization beyond the `png` crate's best compression: stronger
//!   optimizers are outside the permitted dependency and wasm portability floor.

use std::io::Cursor;

use image::{
    codecs::{
        bmp::BmpEncoder,
        gif::{GifDecoder, GifEncoder},
        png::{CompressionType, FilterType, PngEncoder},
        webp::WebPEncoder,
    },
    imageops::FilterType as ResizeFilter,
    metadata::Orientation,
    AnimationDecoder, DynamicImage, ExtendedColorType, Frame, GenericImageView, ImageDecoder,
    ImageEncoder, ImageFormat, ImageReader,
};
use jpeg_encoder::{ColorType, Encoder};
use wasm_bindgen::prelude::*;

use super::{push_notice, FileResult, Notice, MAX_DECODED_PIXELS};

const DEFAULT_JPEG_QUALITY: u8 = 90;
/// The JPEG container stores dimensions in 16 bits, so nothing wider or taller
/// than this can be written as a JPEG at all.
const MAX_JPEG_DIMENSION: u32 = 65_535;
/// A guard on animated input: frames are decoded fully, so the whole animation
/// has to fit the same pixel budget as a still image.
const MAX_ANIMATION_FRAMES: usize = 1_024;

/// Everything read from the source file that a transform has to carry across,
/// separate from the pixels themselves.
struct SourceImage {
    format: ImageFormat,
    image: DynamicImage,
    icc_profile: Option<Vec<u8>>,
    orientation: Orientation,
    /// Every decoded frame, present only for animations this core can rebuild.
    frames: Option<Vec<Frame>>,
    /// True when the source holds more than one frame, whether or not this
    /// core can rebuild the animation in the requested output format.
    animated: bool,
}

impl SourceImage {
    fn is_sixteen_bit(&self) -> bool {
        is_sixteen_bit(&self.image)
    }
}

/// Whether these pixels carry more precision than an 8-bit format can hold.
fn is_sixteen_bit(image: &DynamicImage) -> bool {
    matches!(
        image,
        DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
    )
}

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

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(
        u64::try_from(MAX_DECODED_PIXELS)
            .unwrap_or(u64::MAX)
            .saturating_mul(8),
    );
    limits
}

/// Return whether a PNG byte stream declares an animation control chunk, which
/// makes it an APNG whose extra frames a still decode would silently drop.
fn png_is_animated(bytes: &[u8]) -> bool {
    let mut position = 8;
    while let Some(header) = bytes.get(position..position + 8) {
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = &header[4..8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        let Some(next) = length
            .checked_add(12)
            .and_then(|step| position.checked_add(step))
        else {
            return false;
        };
        position = next;
    }
    false
}

/// Decode every frame of an animated GIF, bounded by the same pixel budget a
/// still image gets.
fn decode_gif_frames(bytes: &[u8], width: u32, height: u32) -> Result<Vec<Frame>, String> {
    let pixels_per_frame = u64::from(width).saturating_mul(u64::from(height)).max(1);
    let frame_budget = (MAX_DECODED_PIXELS as u64 / pixels_per_frame).max(1) as usize;
    let mut decoder = GifDecoder::new(Cursor::new(bytes))
        .map_err(|error| format!("Could not read this GIF: {error}"))?;
    decoder
        .set_limits(decode_limits())
        .map_err(|error| format!("Could not read this GIF safely: {error}"))?;

    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|error| format!("Could not decode a GIF frame: {error}"))?;
        frames.push(frame);
        if frames.len() > frame_budget.min(MAX_ANIMATION_FRAMES) {
            return Err("This animation has too many frames to process safely.".to_owned());
        }
    }
    Ok(frames)
}

/// Inspect dimensions before allocating the decoded pixel buffer, then decode
/// the pixels together with the colour profile, the recorded orientation, and
/// any animation frames, so a transform can carry all of them across.
fn decode(bytes: &[u8]) -> Result<SourceImage, String> {
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
    reader.limits(decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("Could not read this image: {error}"))?;
    // Read the profile and the orientation before the decoder is consumed:
    // both live in metadata that decoding throws away.
    let icc_profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("Could not decode this image: {error}"))?;
    guard_decoded_dimensions(image.width(), image.height())?;

    // A phone photo is stored sideways with an orientation tag. Output carries
    // no EXIF, so the rotation has to be baked into the pixels or the picture
    // comes out lying on its side.
    image.apply_orientation(orientation);

    let frames = match format {
        ImageFormat::Gif => {
            let frames = decode_gif_frames(bytes, width, height)?;
            (frames.len() > 1).then_some(frames)
        }
        _ => None,
    };
    let animated = frames.is_some() || (format == ImageFormat::Png && png_is_animated(bytes));

    Ok(SourceImage {
        format,
        image,
        icc_profile,
        orientation,
        frames,
        animated,
    })
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

/// The exact channel layout and bit depth a PNG can store this image in.
/// Preserving it is what makes "losslessly re-encoded" a true statement: a
/// 16-bit scan stays 16-bit, and an opaque image does not gain an alpha
/// channel it never had.
fn png_layout(image: &DynamicImage) -> Option<(ExtendedColorType, &[u8])> {
    let color = match image {
        DynamicImage::ImageLuma8(_) => ExtendedColorType::L8,
        DynamicImage::ImageLumaA8(_) => ExtendedColorType::La8,
        DynamicImage::ImageRgb8(_) => ExtendedColorType::Rgb8,
        DynamicImage::ImageRgba8(_) => ExtendedColorType::Rgba8,
        DynamicImage::ImageLuma16(_) => ExtendedColorType::L16,
        DynamicImage::ImageLumaA16(_) => ExtendedColorType::La16,
        DynamicImage::ImageRgb16(_) => ExtendedColorType::Rgb16,
        DynamicImage::ImageRgba16(_) => ExtendedColorType::Rgba16,
        _ => return None,
    };
    Some((color, image.as_bytes()))
}

/// Encode raw pixels plus the source colour profile, deliberately omitting
/// EXIF, XMP, and all other source metadata. JPEG output composites
/// transparent pixels onto white.
///
/// Anything this encoder cannot carry across (a profile a format has no room
/// for, a bit depth a codec cannot express) is reported through `notices`.
fn encode(
    image: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    icc_profile: Option<&[u8]>,
    notices: &mut Vec<Notice>,
) -> Result<Vec<u8>, String> {
    let (width, height) = image.dimensions();
    let mut encoded = Vec::new();
    let profile_dropped = |notices: &mut Vec<Notice>| {
        push_notice(notices, Notice::new(
            "image-color-profile-dropped",
            "This image carried an embedded colour profile that the chosen output format cannot store, so its colours will be shown as plain sRGB and may look slightly different.",
        ));
    };
    let depth_reduced = |notices: &mut Vec<Notice>| {
        push_notice(notices, Notice::new(
            "image-bit-depth-reduced",
            "This image stores 16 bits per colour channel; the chosen output format holds only 8, so fine tonal steps were rounded.",
        ));
    };

    match format {
        ImageFormat::Jpeg => {
            if width > MAX_JPEG_DIMENSION {
                return Err("This image is too wide to encode as JPEG.".to_owned());
            }
            if height > MAX_JPEG_DIMENSION {
                return Err("This image is too tall to encode as JPEG.".to_owned());
            }
            let rgb = flatten_alpha_onto_white(image);
            let mut encoder = Encoder::new(&mut encoded, jpeg_quality);
            if let Some(profile) = icc_profile {
                encoder
                    .add_icc_profile(profile)
                    .map_err(|error| format!("Could not keep the colour profile: {error}"))?;
            }
            encoder
                .encode(&rgb, width as u16, height as u16, ColorType::Rgb)
                .map_err(|error| format!("Could not encode the JPEG image: {error}"))?;
        }
        ImageFormat::Png => {
            let mut encoder = PngEncoder::new_with_quality(
                &mut encoded,
                CompressionType::Best,
                FilterType::Adaptive,
            );
            if let Some(profile) = icc_profile {
                encoder
                    .set_icc_profile(profile.to_vec())
                    .map_err(|error| format!("Could not keep the colour profile: {error}"))?;
            }
            match png_layout(image) {
                Some((color, pixels)) => encoder
                    .write_image(pixels, width, height, color)
                    .map_err(|error| format!("Could not encode the PNG image: {error}"))?,
                None => {
                    let rgba = image.to_rgba8();
                    encoder
                        .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                        .map_err(|error| format!("Could not encode the PNG image: {error}"))?;
                }
            }
        }
        ImageFormat::Gif => {
            if icc_profile.is_some() {
                profile_dropped(notices);
            }
            if is_sixteen_bit(image) {
                depth_reduced(notices);
            }
            let rgba = image.to_rgba8();
            GifEncoder::new(&mut encoded)
                .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the GIF image: {error}"))?;
        }
        ImageFormat::Bmp => {
            if icc_profile.is_some() {
                profile_dropped(notices);
            }
            if is_sixteen_bit(image) {
                depth_reduced(notices);
            }
            let rgba = image.to_rgba8();
            BmpEncoder::new(&mut encoded)
                .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
                .map_err(|error| format!("Could not encode the BMP image: {error}"))?;
        }
        ImageFormat::WebP => {
            if is_sixteen_bit(image) {
                depth_reduced(notices);
            }
            let rgba = image.to_rgba8();
            let mut encoder = WebPEncoder::new_lossless(&mut encoded);
            if let Some(profile) = icc_profile {
                encoder
                    .set_icc_profile(profile.to_vec())
                    .map_err(|error| format!("Could not keep the colour profile: {error}"))?;
            }
            encoder
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

/// Work out the output size. `keep_aspect` fits the picture inside the box.
/// Without it the requested box is used exactly, except that a box larger than
/// the source is scaled down as a whole: clamping the axes independently would
/// silently hand back a different shape than the one that was asked for.
fn resize_target(
    width: u32,
    height: u32,
    max_w: u32,
    max_h: u32,
    notices: &mut Vec<Notice>,
) -> (u32, u32) {
    if max_w <= width && max_h <= height {
        return (max_w, max_h);
    }

    let scale = (f64::from(width) / f64::from(max_w))
        .min(f64::from(height) / f64::from(max_h))
        .min(1.0);
    let target_width = ((f64::from(max_w) * scale).round() as u32).max(1);
    let target_height = ((f64::from(max_h) * scale).round() as u32).max(1);
    push_notice(notices, Notice::new(
        "image-resize-clamped",
        format!(
            "The requested {max_w}x{max_h} is larger than this image, and enlarging would invent detail. It was scaled to {target_width}x{target_height}, which keeps the shape you asked for."
        ),
    ));
    (target_width, target_height)
}

/// Pure core: resize without upscaling and re-encode in the detected source
/// format, preserving animation, colour profile, and bit depth wherever the
/// output format can hold them.
fn resize(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    keep_aspect: bool,
) -> Result<(Vec<u8>, Vec<Notice>), String> {
    if max_w == 0 || max_h == 0 {
        return Err("Resize width and height must both be greater than zero.".to_owned());
    }

    let source = decode(bytes)?;
    let mut notices = Vec::new();
    let (width, height) = (source.image.width(), source.image.height());

    if let Some(frames) = &source.frames {
        // An animation resized down to one still frame is not the file the
        // user asked for. Rebuild every frame instead.
        let (target_width, target_height) = if keep_aspect {
            (max_w.min(width), max_h.min(height))
        } else {
            resize_target(width, height, max_w, max_h, &mut notices)
        };
        if source.icc_profile.is_some() {
            push_notice(&mut notices, Notice::new(
                "image-color-profile-dropped",
                "This image carried an embedded colour profile that the chosen output format cannot store, so its colours will be shown as plain sRGB and may look slightly different.",
            ));
        }

        let mut encoded = Vec::new();
        let mut encoder = GifEncoder::new(&mut encoded);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|error| format!("Could not set the GIF loop count: {error}"))?;
        for frame in frames {
            let delay = frame.delay();
            let mut image = DynamicImage::ImageRgba8(frame.buffer().clone());
            image.apply_orientation(source.orientation);
            let resized = if keep_aspect {
                image.resize(target_width, target_height, ResizeFilter::Lanczos3)
            } else {
                image.resize_exact(target_width, target_height, ResizeFilter::Lanczos3)
            };
            encoder
                // Frames arrive fully composited on the canvas, so the
                // rebuilt frame covers the canvas too: offset 0, 0.
                .encode_frame(Frame::from_parts(resized.to_rgba8(), 0, 0, delay))
                .map_err(|error| format!("Could not encode a GIF frame: {error}"))?;
        }
        drop(encoder);
        push_notice(&mut notices, Notice::new(
            "image-animation-recoded",
            format!(
                "All {} frames of this animation were kept, and its colours were re-selected frame by frame, which can shift them slightly.",
                frames.len()
            ),
        ));
        return Ok((encoded, notices));
    }

    if source.animated {
        push_notice(&mut notices, Notice::new(
            "image-animation-dropped",
            "This file contains an animation. Only its first frame was used; the result is a still picture.",
        ));
    }

    let resized = if keep_aspect {
        source
            .image
            .resize(max_w.min(width), max_h.min(height), ResizeFilter::Lanczos3)
    } else {
        let (target_width, target_height) =
            resize_target(width, height, max_w, max_h, &mut notices);
        source
            .image
            .resize_exact(target_width, target_height, ResizeFilter::Lanczos3)
    };

    let encoded = encode(
        &resized,
        source.format,
        DEFAULT_JPEG_QUALITY,
        source.icc_profile.as_deref(),
        &mut notices,
    )?;
    Ok((encoded, notices))
}

/// Pure core: decode pixels and encode them into the requested format at their
/// original resolution. PNG and WebP preserve alpha; JPEG composites alpha
/// onto white.
fn convert(bytes: &[u8], target: &str) -> Result<(Vec<u8>, Vec<Notice>), String> {
    let target = target_format(target)?;
    let source = decode(bytes)?;
    let mut notices = Vec::new();

    if source.animated {
        push_notice(&mut notices, Notice::new(
            "image-animation-dropped",
            "This file contains an animation. Only its first frame was converted; the result is a still picture.",
        ));
    }
    if source.is_sixteen_bit() && target != ImageFormat::Png {
        push_notice(&mut notices, Notice::new(
            "image-bit-depth-reduced",
            "This image stores 16 bits per colour channel; the chosen output format holds only 8, so fine tonal steps were rounded.",
        ));
    }

    let encoded = encode(
        &source.image,
        target,
        DEFAULT_JPEG_QUALITY,
        source.icc_profile.as_deref(),
        &mut notices,
    )?;
    Ok((encoded, notices))
}

fn validate_encoded_image(bytes: &[u8], expected_format: ImageFormat) -> Result<(), String> {
    let actual_format = image::guess_format(bytes)
        .map_err(|error| format!("Could not validate the re-encoded image: {error}"))?;
    if actual_format != expected_format {
        return Err("The re-encoded image has an unexpected format.".to_owned());
    }
    image::load_from_memory_with_format(bytes, expected_format)
        .map_err(|error| format!("Could not validate the re-encoded image: {error}"))?;
    Ok(())
}

/// Pure core: recompress JPEG or PNG pixels at their original resolution,
/// verify the encoded image, and preserve the original bytes whenever the
/// re-encode does not reduce size.
fn compress(bytes: &[u8], quality: u8) -> Result<(Vec<u8>, Vec<Notice>), String> {
    if !(1..=100).contains(&quality) {
        return Err("Image quality must be between 1 and 100.".to_owned());
    }

    let source = decode(bytes)?;
    if !matches!(source.format, ImageFormat::Jpeg | ImageFormat::Png) {
        return Err("Only JPEG and PNG images can be compressed in this version.".to_owned());
    }
    let mut notices = Vec::new();
    if source.animated {
        push_notice(&mut notices, Notice::new(
            "image-animation-dropped",
            "This file contains an animation. Only its first frame was compressed; the result is a still picture.",
        ));
    }

    let encoded = encode(
        &source.image,
        source.format,
        quality,
        source.icc_profile.as_deref(),
        &mut notices,
    )?;
    validate_encoded_image(&encoded, source.format)?;

    if encoded.len() < bytes.len() {
        return Ok((encoded, notices));
    }

    // Handing back the input means none of this ran on the file the user
    // downloads. Only the notice can say so.
    notices.retain(|notice| notice.code() != "image-animation-dropped");
    push_notice(&mut notices, Notice::new(
        "image-returned-unchanged",
        "This image is already as small as this tool can make it, so your original file was returned exactly as it was, metadata included.",
    ));
    Ok((bytes.to_vec(), notices))
}

/// Resize an image and return bytes in its detected source format, plus every
/// notice the interface must show.
#[wasm_bindgen]
pub fn resize_image(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    keep_aspect: bool,
) -> Result<FileResult, JsValue> {
    resize(bytes, max_w, max_h, keep_aspect)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

/// Convert an image to PNG, JPEG, or lossless WebP, plus every notice the
/// interface must show.
#[wasm_bindgen]
pub fn convert_image(bytes: &[u8], target: &str) -> Result<FileResult, JsValue> {
    convert(bytes, target)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

/// Compress a JPEG or PNG without ever returning more bytes than the input,
/// plus every notice the interface must show.
#[wasm_bindgen]
pub fn compress_image(bytes: &[u8], quality: u8) -> Result<FileResult, JsValue> {
    compress(bytes, quality)
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use super::{
        compress, convert, encode, guard_decoded_dimensions, resize, CompressionType, Encoder,
        ExtendedColorType, FilterType, ImageEncoder, ImageFormat, Notice, PngEncoder,
        MAX_DECODED_PIXELS,
    };

    /// Byte-only wrappers: most tests care about the picture that comes out,
    /// while the notice-carrying signature is exercised by the notice gates.
    fn resize_bytes(
        bytes: &[u8],
        max_w: u32,
        max_h: u32,
        keep_aspect: bool,
    ) -> Result<Vec<u8>, String> {
        resize(bytes, max_w, max_h, keep_aspect).map(|(bytes, _)| bytes)
    }

    fn convert_bytes(bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
        convert(bytes, target).map(|(bytes, _)| bytes)
    }

    fn compress_bytes(bytes: &[u8], quality: u8) -> Result<Vec<u8>, String> {
        compress(bytes, quality).map(|(bytes, _)| bytes)
    }

    fn notice_codes(notices: &[Notice]) -> Vec<&'static str> {
        notices.iter().map(|notice| notice.code()).collect()
    }

    fn encode_still(
        image: &image::DynamicImage,
        format: ImageFormat,
        quality: u8,
    ) -> Result<Vec<u8>, String> {
        encode(image, format, quality, None, &mut Vec::new())
    }
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

    /// A JPEG that says, the way every phone camera says it, that the stored
    /// pixels have to be turned before they are shown.
    fn jpeg_with_orientation(width: u16, height: u16, exif_orientation: u8) -> Vec<u8> {
        let jpeg = jpeg_fixture(width, height, 90);
        let mut payload = b"Exif\0\0II*\0".to_vec();
        payload.extend_from_slice(&8_u32.to_le_bytes());
        payload.extend_from_slice(&1_u16.to_le_bytes());
        payload.extend_from_slice(&0x0112_u16.to_le_bytes());
        payload.extend_from_slice(&3_u16.to_le_bytes());
        payload.extend_from_slice(&1_u32.to_le_bytes());
        payload.extend_from_slice(&u16::from(exif_orientation).to_le_bytes());
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(&0_u32.to_le_bytes());

        let length = u16::try_from(payload.len() + 2).expect("APP1 fixture should fit");
        let mut output = Vec::with_capacity(jpeg.len() + payload.len() + 4);
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    /// A plausible ICC profile header: enough for a decoder to hand it back
    /// and for the PDF core to read which colour space it describes.
    fn sample_icc_profile() -> Vec<u8> {
        let mut profile = vec![0_u8; 132];
        profile[0..4].copy_from_slice(&132_u32.to_be_bytes());
        profile[12..16].copy_from_slice(b"mntr");
        profile[16..20].copy_from_slice(b"RGB ");
        profile[20..24].copy_from_slice(b"XYZ ");
        profile[36..40].copy_from_slice(b"acsp");
        profile
    }

    fn png_with_profile(width: u32, height: u32) -> Vec<u8> {
        let image = sample_rgba(width, height);
        let mut encoded = Vec::new();
        let mut encoder =
            PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive);
        encoder
            .set_icc_profile(sample_icc_profile())
            .expect("PNG should accept a profile");
        encoder
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .expect("fixture PNG should encode");
        encoded
    }

    fn png16_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push((x * 257) as u16);
                pixels.push((y * 259) as u16);
                pixels.push(((x + y) * 263) as u16);
            }
        }
        let bytes = pixels
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect::<Vec<u8>>();
        let mut encoded = Vec::new();
        PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive)
            .write_image(&bytes, width, height, ExtendedColorType::Rgb16)
            .expect("16-bit fixture PNG should encode");
        encoded
    }

    fn animated_gif_fixture(frame_count: u8) -> Vec<u8> {
        let mut encoded = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .expect("repeat should be set");
            for index in 0..frame_count {
                let mut frame = RgbaImage::new(8, 8);
                for pixel in frame.pixels_mut() {
                    *pixel = Rgba([index * 40, 255 - index * 40, 90, 255]);
                }
                encoder
                    .encode_frame(image::Frame::from_parts(
                        frame,
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(100, 1),
                    ))
                    .expect("fixture frame should encode");
            }
        }
        encoded
    }

    fn frame_count(bytes: &[u8]) -> usize {
        use image::AnimationDecoder;
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .expect("GIF should decode")
            .into_frames()
            .collect_frames()
            .expect("frames should decode")
            .len()
    }

    fn profile_of(bytes: &[u8]) -> Option<Vec<u8>> {
        use image::ImageDecoder;
        let format = image::guess_format(bytes).expect("format should be detectable");
        let mut decoder = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
            .into_decoder()
            .expect("decoder should build");
        decoder.icc_profile().ok().flatten()
    }

    // Every phone photo is bigger than 4096 pixels on its long edge. Silently
    // resampling it is the difference between "converted" and "damaged".
    #[test]
    fn convert_and_compress_keep_every_pixel_of_a_large_image() {
        let source = png_fixture(5_000, 4);

        let converted = convert_bytes(&source, "png").expect("large PNG should convert");
        assert_eq!(
            dimensions(&converted),
            (5_000, 4),
            "converting must not resample a large image"
        );

        let compressed = compress_bytes(&source, 60).expect("large PNG should compress");
        assert_eq!(
            dimensions(&compressed),
            (5_000, 4),
            "compressing must not resample a large image"
        );
    }

    // "Losslessly re-encoded" has to be true: a 16-bit scan must not come back
    // rounded down to 8 bits per channel.
    #[test]
    fn sixteen_bit_pngs_keep_their_precision() {
        let source = png16_fixture(32, 16);
        assert!(matches!(
            image::load_from_memory(&source).expect("fixture should decode"),
            DynamicImage::ImageRgb16(_)
        ));

        let resized = resize_bytes(&source, 16, 8, true).expect("16-bit PNG should resize");
        assert!(
            matches!(
                image::load_from_memory(&resized).expect("output should decode"),
                DynamicImage::ImageRgb16(_)
            ),
            "a 16-bit PNG must stay 16-bit"
        );

        let converted = convert_bytes(&source, "png").expect("16-bit PNG should convert");
        assert!(matches!(
            image::load_from_memory(&converted).expect("output should decode"),
            DynamicImage::ImageRgb16(_)
        ));
    }

    #[test]
    fn a_format_that_cannot_hold_sixteen_bits_says_so() {
        let (_, notices) = convert(&png16_fixture(8, 8), "jpeg").expect("conversion should work");
        assert!(notice_codes(&notices).contains(&"image-bit-depth-reduced"));
    }

    // An embedded colour profile is what makes a Display-P3 photo look right.
    // Dropping it silently shifts every colour in the picture.
    #[test]
    fn colour_profiles_survive_every_transform_that_can_hold_them() {
        let source = png_with_profile(40, 20);
        let expected = sample_icc_profile();

        let resized = resize_bytes(&source, 20, 10, true).expect("PNG should resize");
        assert_eq!(
            profile_of(&resized).as_deref(),
            Some(expected.as_slice()),
            "resizing dropped the colour profile"
        );

        let jpeg = convert_bytes(&source, "jpeg").expect("PNG should convert");
        assert_eq!(
            profile_of(&jpeg).as_deref(),
            Some(expected.as_slice()),
            "converting to JPEG dropped the colour profile"
        );
    }

    #[test]
    fn a_format_that_cannot_hold_a_profile_says_so() {
        let source = png_with_profile(16, 8);
        let gif = encode(
            &image::load_from_memory(&source).expect("fixture should decode"),
            ImageFormat::Gif,
            90,
            Some(&sample_icc_profile()),
            &mut Vec::new(),
        );
        assert!(gif.is_ok());

        let mut notices = Vec::new();
        encode(
            &image::load_from_memory(&source).expect("fixture should decode"),
            ImageFormat::Gif,
            90,
            Some(&sample_icc_profile()),
            &mut notices,
        )
        .expect("GIF should still encode");
        assert!(notice_codes(&notices).contains(&"image-color-profile-dropped"));
    }

    // A portrait phone photo is stored landscape plus an orientation tag. The
    // output carries no tag, so the pixels themselves have to be turned.
    #[test]
    fn a_sideways_photo_comes_out_upright() {
        let source = jpeg_with_orientation(40, 20, 6);
        assert_eq!(
            dimensions(&source),
            (40, 20),
            "the fixture is stored landscape"
        );

        for output in [
            convert_bytes(&source, "png").expect("photo should convert"),
            compress_bytes(&source, 50).expect("photo should compress"),
        ] {
            assert_eq!(
                dimensions(&output),
                (20, 40),
                "a photo tagged as rotated must come out upright"
            );
        }

        let resized = resize_bytes(&source, 10, 10, true).expect("photo should resize");
        let (width, height) = dimensions(&resized);
        assert!(
            height > width,
            "a portrait photo must resize to a portrait picture, got {width}x{height}"
        );
    }

    // Resizing an animation must not hand back a single still frame.
    #[test]
    fn resizing_an_animation_keeps_every_frame() {
        let source = animated_gif_fixture(4);
        assert_eq!(frame_count(&source), 4);

        let (resized, notices) = resize(&source, 4, 4, true).expect("animation should resize");
        assert_eq!(
            frame_count(&resized),
            4,
            "every frame of the animation must survive the resize"
        );
        assert_eq!(dimensions(&resized), (4, 4));
        assert!(notice_codes(&notices).contains(&"image-animation-recoded"));
    }

    #[test]
    fn converting_an_animation_says_the_other_frames_were_left_behind() {
        let (_, notices) =
            convert(&animated_gif_fixture(4), "png").expect("animation should convert");
        assert!(notice_codes(&notices).contains(&"image-animation-dropped"));
    }

    // Asking for a 200x200 box must not quietly produce a 200x100 picture.
    #[test]
    fn a_stretch_box_larger_than_the_image_keeps_the_requested_shape() {
        let (resized, notices) =
            resize(&png_fixture(4_000, 100), 200, 200, false).expect("image should resize");

        assert_eq!(
            dimensions(&resized),
            (100, 100),
            "the requested square must stay square"
        );
        assert!(notice_codes(&notices).contains(&"image-resize-clamped"));
    }

    #[test]
    fn compress_says_when_it_hands_back_the_original_untouched() {
        let source = png_fixture(1, 1);
        let (bytes, notices) = compress(&source, 100).expect("tiny PNG should be accepted");

        assert_eq!(bytes, source);
        assert!(notice_codes(&notices).contains(&"image-returned-unchanged"));
    }

    #[test]
    fn resize_downscale_fits_the_box_and_preserves_aspect() {
        let resized = resize_bytes(&png_fixture(400, 200), 100, 100, true)
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
        assert!(resize_bytes(&source, 0, 10, true).is_err());
        assert!(resize_bytes(&source, 10, 0, false).is_err());
    }

    #[test]
    fn resize_never_upscales() {
        let resized = resize_bytes(&png_fixture(40, 20), 400, 400, true)
            .expect("a larger box should not upscale the image");

        assert_eq!(dimensions(&resized), (40, 20));
    }

    #[test]
    fn resize_without_aspect_stretches_to_the_clamped_box() {
        let resized = resize_bytes(&png_fixture(80, 40), 30, 20, false)
            .expect("PNG should stretch to the requested box");

        assert_eq!(dimensions(&resized), (30, 20));
    }

    #[test]
    fn resize_preserves_each_additional_supported_format() {
        let image = DynamicImage::ImageRgba8(sample_rgba(32, 16));
        for format in [ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::WebP] {
            let source = encode_still(&image, format, 90).expect("fixture should encode");
            let resized = resize_bytes(&source, 10, 10, true).expect("fixture should resize");

            assert_eq!(
                image::guess_format(&resized).expect("output format should be detectable"),
                format
            );
            assert_eq!(dimensions(&resized), (10, 5));
        }
    }

    #[test]
    fn convert_png_to_jpeg_and_jpeg_to_png() {
        let jpeg = convert_bytes(&png_fixture(64, 32), "jpeg").expect("PNG should convert to JPEG");
        assert_eq!(
            image::guess_format(&jpeg).expect("output format should be detectable"),
            ImageFormat::Jpeg
        );
        assert_eq!(dimensions(&jpeg), (64, 32));

        let png =
            convert_bytes(&jpeg_fixture(48, 24, 90), "png").expect("JPEG should convert to PNG");
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

        let jpeg = convert_bytes(&source, "jpeg").expect("transparent PNG should convert");
        let decoded = image::load_from_memory(&jpeg)
            .expect("JPEG should decode")
            .to_rgb8();
        let pixel = decoded.get_pixel(0, 0);
        assert!(pixel.0.iter().all(|channel| *channel >= 250));
    }

    #[test]
    fn compresses_a_jpeg_at_low_quality_and_keeps_it_decodable() {
        let source = jpeg_fixture(320, 240, 100);
        let compressed = compress_bytes(&source, 10).expect("JPEG should compress");

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
        let compressed = compress_bytes(&source, 100).expect("tiny PNG should still be accepted");

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
            assert!(resize_bytes(garbage, 10, 10, true).is_err());
            assert!(convert_bytes(garbage, "png").is_err());
            assert!(compress_bytes(garbage, 50).is_err());
        }
    }

    #[test]
    fn pixel_guard_rejects_claimed_dimensions_before_allocation() {
        // Derived from the shipped constant, so the guard is checked against
        // the limit it actually enforces rather than against a copy of it.
        let limit = u32::try_from(MAX_DECODED_PIXELS).expect("the pixel cap fits in a u32");
        assert!(guard_decoded_dimensions(1, limit).is_ok());
        assert!(guard_decoded_dimensions(1, limit + 1).is_err());
        assert!(guard_decoded_dimensions(2, limit / 2 + 1).is_err());
        assert!(guard_decoded_dimensions(u32::MAX, u32::MAX).is_err());
        assert!(guard_decoded_dimensions(0, 10).is_err());
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

        let stripped = convert_bytes(&with_exif, "jpeg").expect("EXIF JPEG should re-encode");
        assert!(!stripped.windows(6).any(|window| window == b"Exif\0\0"));
        assert!(!stripped.windows(2).any(|window| window == [0xff, 0xe1]));
        assert_eq!(dimensions(&stripped), (64, 32));
    }

    #[test]
    fn repeated_transforms_are_byte_identical() {
        let source = png_fixture(80, 40);
        assert_eq!(
            resize_bytes(&source, 30, 30, true).expect("first resize should work"),
            resize_bytes(&source, 30, 30, true).expect("second resize should work")
        );
        assert_eq!(
            convert_bytes(&source, "webp").expect("first conversion should work"),
            convert_bytes(&source, "webp").expect("second conversion should work")
        );
    }
}
