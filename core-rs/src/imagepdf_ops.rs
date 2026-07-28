//! Deterministic, metadata-clean image-to-PDF assembly using pure-Rust codecs.

use std::io::{Cursor, Write};

use flate2::{write::ZlibEncoder, Compression};
use image::{metadata::Orientation, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use lopdf::{dictionary, Document, Object, Stream};
use wasm_bindgen::prelude::*;

use super::{
    archive_ops::MAX_ARCHIVE_TOTAL_BYTES, is_baseline_jpeg, metadata_ops::strip_jpeg_metadata,
    push_notice, FileResult, Notice, MAX_DECODED_PIXELS,
};

const MAX_IMAGES: usize = 500;
const PAGE_MARGIN: f64 = 36.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageMode {
    Fit,
    A4,
    Letter,
}

impl PageMode {
    /// Unknown public strings deliberately fall back to the least surprising
    /// mode: one PDF point per source pixel.
    fn from_public(value: &str) -> Self {
        match value {
            "a4" => Self::A4,
            "letter" => Self::Letter,
            _ => Self::Fit,
        }
    }

    fn fixed_dimensions(self) -> Option<(f64, f64)> {
        match self {
            Self::Fit => None,
            Self::A4 => Some((595.0, 842.0)),
            Self::Letter => Some((612.0, 792.0)),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct JpegFrame {
    width: u32,
    height: u32,
    components: u8,
}

struct PreparedImage {
    width: u32,
    height: u32,
    color_space: &'static str,
    filter: &'static str,
    content: Vec<u8>,
    /// How the source says the stored pixels must be turned to be seen the
    /// right way up. Applied on the page, not to the pixels, so a passed-
    /// through JPEG stays byte-identical.
    orientation: Orientation,
    /// The source's embedded colour profile, kept so the PDF shows the same
    /// colours the image viewer showed.
    icc_profile: Option<Vec<u8>>,
}

impl PreparedImage {
    /// The size the picture occupies once its orientation is applied.
    fn display_size(&self) -> (u32, u32) {
        if orientation_swaps_axes(self.orientation) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    fn components(&self) -> usize {
        if self.color_space == "DeviceGray" {
            1
        } else {
            3
        }
    }
}

fn orientation_swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// The transform that maps the image's unit square onto itself so the stored
/// pixels appear the way the camera recorded them, as PDF matrix components
/// `[a b c d e f]`.
fn orientation_matrix(orientation: Orientation) -> [f64; 6] {
    match orientation {
        Orientation::NoTransforms => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        Orientation::FlipHorizontal => [-1.0, 0.0, 0.0, 1.0, 1.0, 0.0],
        Orientation::Rotate180 => [-1.0, 0.0, 0.0, -1.0, 1.0, 1.0],
        Orientation::FlipVertical => [1.0, 0.0, 0.0, -1.0, 0.0, 1.0],
        Orientation::Rotate90 => [0.0, -1.0, 1.0, 0.0, 0.0, 1.0],
        Orientation::Rotate270 => [0.0, 1.0, -1.0, 0.0, 1.0, 0.0],
        Orientation::Rotate90FlipH => [0.0, -1.0, -1.0, 0.0, 1.0, 1.0],
        Orientation::Rotate270FlipH => [0.0, 1.0, 1.0, 0.0, 0.0, 0.0],
    }
}

/// Read the orientation tag and colour profile without decoding pixels.
fn source_metadata(bytes: &[u8], format: ImageFormat) -> (Orientation, Option<Vec<u8>>) {
    let Ok(mut decoder) = ImageReader::with_format(Cursor::new(bytes), format).into_decoder()
    else {
        return (Orientation::NoTransforms, None);
    };
    let profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|profile| !profile.is_empty());
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    (orientation, profile)
}

/// The colour space an ICC profile describes, read from its 128-byte header.
fn icc_profile_components(profile: &[u8]) -> Option<usize> {
    let space = profile.get(16..20)?;
    match space {
        b"GRAY" => Some(1),
        b"RGB " => Some(3),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct PageGeometry {
    page_width: f64,
    page_height: f64,
    draw_width: f64,
    draw_height: f64,
    offset_x: f64,
    offset_y: f64,
}

fn guard_dimensions(width: u32, height: u32) -> Result<(), String> {
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

/// Parse the baseline SOF0 marker without decoding image pixels.
fn parse_sof0(bytes: &[u8]) -> Option<JpegFrame> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }

    let mut position = 2usize;
    while position < bytes.len() {
        if bytes.get(position) != Some(&0xff) {
            return None;
        }
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *bytes.get(position)?;
        position += 1;

        if matches!(marker, 0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7) {
            continue;
        }
        if marker == 0xda {
            return None;
        }

        let length_bytes = bytes.get(position..position + 2)?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return None;
        }
        let segment_end = position.checked_add(length)?;
        if segment_end > bytes.len() {
            return None;
        }

        if marker == 0xc0 {
            if length < 8 || bytes.get(position + 2) != Some(&8) {
                return None;
            }
            let height = u32::from(u16::from_be_bytes([
                *bytes.get(position + 3)?,
                *bytes.get(position + 4)?,
            ]));
            let width = u32::from(u16::from_be_bytes([
                *bytes.get(position + 5)?,
                *bytes.get(position + 6)?,
            ]));
            let components = *bytes.get(position + 7)?;
            let expected_length = 8usize.checked_add(usize::from(components).checked_mul(3)?)?;
            if length != expected_length {
                return None;
            }
            return Some(JpegFrame {
                width,
                height,
                components,
            });
        }

        position = segment_end;
    }
    None
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

/// Inspect dimensions under a bounded reader before allocating decoded pixels.
fn decode_image(bytes: &[u8]) -> Result<DynamicImage, String> {
    let format = image::guess_format(bytes)
        .map_err(|error| format!("Could not determine this image's format: {error}"))?;
    if !supported_format(format) {
        return Err(
            "This image format is unsupported; use JPEG, PNG, GIF, BMP, or WebP.".to_owned(),
        );
    }

    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|error| format!("Could not read this image's dimensions: {error}"))?;
    guard_dimensions(width, height)?;

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(
        u64::try_from(MAX_DECODED_PIXELS)
            .unwrap_or(u64::MAX)
            .saturating_mul(8),
    );
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("Could not decode this image: {error}"))?;
    guard_dimensions(image.width(), image.height())?;
    Ok(image)
}

/// Composite transparency onto white with the same integer rounding used by
/// the image transform core, then return tightly packed RGB pixels.
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

fn deflate_rgb(rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(rgb)
        .map_err(|error| format!("Could not compress image pixels: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("Could not finish compressing image pixels: {error}"))
}

fn prepare_image(bytes: &[u8]) -> Result<PreparedImage, String> {
    // Lossless passthrough only when the JPEG could actually be stripped AND the
    // result passed structural verification. If it could not, fall through to the
    // decode path below, which re-encodes the pixels and so carries no metadata —
    // embedding the original bytes instead would put the unscrubbed file in the PDF.
    if is_baseline_jpeg(bytes) {
        if let Some(stripped) = strip_jpeg_metadata(bytes) {
            if let Some(frame) = parse_sof0(&stripped) {
                if matches!(frame.components, 1 | 3) {
                    guard_dimensions(frame.width, frame.height)?;
                    // Stripping removes the orientation tag along with the rest
                    // of the EXIF, so the page has to carry the rotation
                    // instead. Read it from the original bytes.
                    let (orientation, icc_profile) = source_metadata(bytes, ImageFormat::Jpeg);
                    return Ok(PreparedImage {
                        width: frame.width,
                        height: frame.height,
                        color_space: if frame.components == 1 {
                            "DeviceGray"
                        } else {
                            "DeviceRGB"
                        },
                        filter: "DCTDecode",
                        content: stripped,
                        orientation,
                        icc_profile,
                    });
                }
            }
        }
    }

    let format = image::guess_format(bytes)
        .map_err(|error| format!("Could not determine this image's format: {error}"))?;
    let (orientation, icc_profile) = source_metadata(bytes, format);
    let mut image = decode_image(bytes)?;
    // This path rewrites the pixels anyway, so the rotation is baked in and the
    // page needs no further transform.
    image.apply_orientation(orientation);
    let width = image.width();
    let height = image.height();
    let rgb = flatten_alpha_onto_white(&image);
    let content = deflate_rgb(&rgb)?;
    Ok(PreparedImage {
        width,
        height,
        color_space: "DeviceRGB",
        filter: "FlateDecode",
        content,
        orientation: Orientation::NoTransforms,
        icc_profile,
    })
}

fn page_geometry(mode: PageMode, width: u32, height: u32) -> PageGeometry {
    let image_width = f64::from(width);
    let image_height = f64::from(height);
    let Some((page_width, page_height)) = mode.fixed_dimensions() else {
        return PageGeometry {
            page_width: image_width,
            page_height: image_height,
            draw_width: image_width,
            draw_height: image_height,
            offset_x: 0.0,
            offset_y: 0.0,
        };
    };

    let available_width = page_width - PAGE_MARGIN * 2.0;
    let available_height = page_height - PAGE_MARGIN * 2.0;
    let scale = (available_width / image_width).min(available_height / image_height);
    let draw_width = image_width * scale;
    let draw_height = image_height * scale;
    PageGeometry {
        page_width,
        page_height,
        draw_width,
        draw_height,
        offset_x: (page_width - draw_width) / 2.0,
        offset_y: (page_height - draw_height) / 2.0,
    }
}

fn pdf_number(value: f64) -> String {
    if value.fract().abs() < 0.000_000_1 {
        return format!("{value:.0}");
    }
    let mut value = format!("{value:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

/// Place the image on the page, folding the source's recorded orientation into
/// the same matrix. Rotating on the page keeps a passed-through JPEG's bytes
/// untouched while still showing the picture the right way up.
fn content_stream(geometry: PageGeometry, orientation: Orientation) -> Vec<u8> {
    let [a, b, c, d, e, f] = orientation_matrix(orientation);
    format!(
        "q\n{} {} {} {} {} {} cm\n/Im0 Do\nQ",
        pdf_number(a * geometry.draw_width),
        pdf_number(b * geometry.draw_height),
        pdf_number(c * geometry.draw_width),
        pdf_number(d * geometry.draw_height),
        pdf_number(e * geometry.draw_width + geometry.offset_x),
        pdf_number(f * geometry.draw_height + geometry.offset_y),
    )
    .into_bytes()
}

fn validate_input_limits(images: &[Vec<u8>]) -> Result<(), String> {
    if images.is_empty() {
        return Err("Choose at least one image to combine.".to_owned());
    }
    if images.len() > MAX_IMAGES {
        return Err(format!(
            "Choose no more than {MAX_IMAGES} images at a time."
        ));
    }

    let total_bytes = images.iter().try_fold(0_u64, |total, image| {
        let length = u64::try_from(image.len())
            .map_err(|_| "These images are too large to combine safely.".to_owned())?;
        total
            .checked_add(length)
            .ok_or_else(|| "These images are too large to combine safely.".to_owned())
    })?;
    if total_bytes > MAX_ARCHIVE_TOTAL_BYTES {
        return Err("These images are too large to combine safely.".to_owned());
    }
    Ok(())
}

/// Build a flat, one-image-per-page PDF. Object allocation, dictionary order,
/// compression settings, and trailer contents are all stable, with no clock,
/// random value, document ID, or `/Info` metadata.
fn build_images_pdf(
    images: Vec<Vec<u8>>,
    page: PageMode,
) -> Result<(Vec<u8>, Vec<Notice>), String> {
    validate_input_limits(&images)?;

    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let mut page_ids = Vec::with_capacity(images.len());
    let mut notices = Vec::new();

    for (index, bytes) in images.iter().enumerate() {
        let image = prepare_image(bytes)
            .map_err(|error| format!("Image {} could not be read: {error}", index + 1))?;
        let (display_width, display_height) = image.display_size();
        let geometry = page_geometry(page, display_width, display_height);

        // Embed the source colour profile when it matches the image's channels,
        // so a Display-P3 or Adobe RGB photo is not silently reinterpreted as
        // sRGB. A profile that does not match is reported instead of guessed at.
        let color_space = match &image.icc_profile {
            Some(profile) if icc_profile_components(profile) == Some(image.components()) => {
                let profile_id = document.add_object(Stream::new(
                    dictionary! {
                        "N" => i64::try_from(image.components()).unwrap_or(3),
                        "Alternate" => image.color_space,
                    },
                    profile.clone(),
                ));
                Object::Array(vec![
                    Object::Name(b"ICCBased".to_vec()),
                    Object::Reference(profile_id),
                ])
            }
            Some(_) => {
                push_notice(&mut notices, Notice::new(
                    "image-color-profile-dropped",
                    "One image carried an embedded colour profile this tool could not read, so its colours are treated as plain sRGB in the PDF and may look slightly different.",
                ));
                Object::Name(image.color_space.as_bytes().to_vec())
            }
            None => Object::Name(image.color_space.as_bytes().to_vec()),
        };

        let image_id = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => i64::from(image.width),
                "Height" => i64::from(image.height),
                "ColorSpace" => color_space,
                "BitsPerComponent" => 8,
                "Filter" => image.filter,
            },
            image.content,
        ));
        let contents_id = document.add_object(Stream::new(
            dictionary! {},
            content_stream(geometry, image.orientation),
        ));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Real(geometry.page_width as f32),
                Object::Real(geometry.page_height as f32),
            ],
            "Resources" => dictionary! {
                "XObject" => dictionary! {
                    "Im0" => image_id,
                },
            },
            "Contents" => contents_id,
        });
        page_ids.push(page_id);
    }

    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => i64::try_from(page_ids.len())
                .map_err(|_| "This PDF has too many pages.".to_owned())?,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);

    let mut output = Vec::new();
    document
        .save_to(&mut output)
        .map_err(|error| format!("Could not create the image PDF: {error}"))?;

    let parsed = Document::load_mem(&output)
        .map_err(|error| format!("Could not validate the image PDF: {error}"))?;
    if parsed.get_pages().len() != images.len() {
        return Err("Could not validate every page in the image PDF.".to_owned());
    }
    if parsed.trailer.get(b"Info").is_ok() {
        return Err("Could not create a metadata-clean image PDF.".to_owned());
    }
    Ok((output, notices))
}

/// Build a one-image-per-page PDF from the images in order. `page` is `"fit"`
/// (page equals image size), `"a4"`, or `"letter"`; unknown values use fit.
/// The output is deterministic and carries no source image metadata.
#[wasm_bindgen]
pub fn images_to_pdf(buffers: js_sys::Array, page: &str) -> Result<FileResult, JsValue> {
    let images = buffers
        .iter()
        .map(|bytes| js_sys::Uint8Array::new(&bytes).to_vec())
        .collect();
    build_images_pdf(images, PageMode::from_public(page))
        .map(|(bytes, notices)| FileResult::new(bytes, notices))
        .map_err(|error| JsValue::from_str(&error))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::ZlibDecoder;
    use image::{
        codecs::png::PngEncoder, ExtendedColorType, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage,
    };
    use jpeg_encoder::{ColorType, Encoder};
    use lopdf::{Document, Object};

    use super::{
        build_images_pdf, page_geometry, parse_sof0, Notice, PageMode, MAX_IMAGES, PAGE_MARGIN,
    };

    /// Byte-only wrapper: the notice-carrying signature is exercised by the
    /// notice gates below.
    fn build_images_pdf_bytes(images: Vec<Vec<u8>>, page: PageMode) -> Result<Vec<u8>, String> {
        build_images_pdf(images, page).map(|(bytes, _)| bytes)
    }

    fn build_images_pdf_notices(images: Vec<Vec<u8>>, page: PageMode) -> Vec<Notice> {
        build_images_pdf(images, page)
            .expect("images should combine")
            .1
    }
    use crate::metadata_ops::strip_jpeg_metadata;

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

    fn grayscale_jpeg_fixture(width: u16, height: u16) -> Vec<u8> {
        let pixels = (0..usize::from(width) * usize::from(height))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        Encoder::new(&mut bytes, 90)
            .encode(&pixels, width, height, ColorType::Luma)
            .expect("grayscale JPEG should encode");
        bytes
    }

    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 251) as u8, (y % 251) as u8, 90, 255])
        });
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .expect("fixture PNG should encode");
        bytes
    }

    fn inject_exif(jpeg: &[u8]) -> Vec<u8> {
        let payload = b"Exif\0\0II*\0\x08\0\0\0GPS latitude and longitude";
        let length = u16::try_from(payload.len() + 2).expect("APP1 fixture should fit");
        let mut output = Vec::new();
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn page_image(document: &Document, page_number: u32) -> &lopdf::Stream {
        let page_id = document.get_pages()[&page_number];
        let page = document
            .get_dictionary(page_id)
            .expect("page dictionary should exist");
        let resources = page
            .get(b"Resources")
            .and_then(Object::as_dict)
            .expect("page resources should exist");
        let xobjects = resources
            .get(b"XObject")
            .and_then(Object::as_dict)
            .expect("XObject resources should exist");
        let image_id = xobjects
            .get(b"Im0")
            .and_then(Object::as_reference)
            .expect("image reference should exist");
        document
            .get_object(image_id)
            .and_then(Object::as_stream)
            .expect("image stream should exist")
    }

    fn page_box(document: &Document, page_number: u32) -> Vec<f64> {
        let page_id = document.get_pages()[&page_number];
        document
            .get_dictionary(page_id)
            .and_then(|page| page.get(b"MediaBox"))
            .and_then(Object::as_array)
            .expect("MediaBox should exist")
            .iter()
            .map(|value| match value {
                Object::Integer(value) => *value as f64,
                Object::Real(value) => f64::from(*value),
                _ => panic!("MediaBox must contain numbers"),
            })
            .collect()
    }

    /// A JPEG that says its stored pixels must be turned 90 degrees clockwise,
    /// exactly as a phone writes a portrait photo.
    fn jpeg_with_orientation(width: u16, height: u16, exif_orientation: u8) -> Vec<u8> {
        let jpeg = jpeg_fixture(width, height);
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
        let mut output = Vec::new();
        output.extend_from_slice(&jpeg[..2]);
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&payload);
        output.extend_from_slice(&jpeg[2..]);
        output
    }

    fn sample_icc_profile() -> Vec<u8> {
        let mut profile = vec![0_u8; 132];
        profile[0..4].copy_from_slice(&132_u32.to_be_bytes());
        profile[12..16].copy_from_slice(b"mntr");
        profile[16..20].copy_from_slice(b"RGB ");
        profile[20..24].copy_from_slice(b"XYZ ");
        profile[36..40].copy_from_slice(b"acsp");
        profile
    }

    fn jpeg_with_profile(width: u16, height: u16) -> Vec<u8> {
        let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
            Rgb([(x % 251) as u8, (y % 251) as u8, 33])
        });
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, 90);
        encoder
            .add_icc_profile(&sample_icc_profile())
            .expect("profile should be accepted");
        encoder
            .encode(image.as_raw(), width, height, ColorType::Rgb)
            .expect("fixture JPEG should encode");
        bytes
    }

    fn page_content(document: &Document, page_number: u32) -> String {
        let page_id = document.get_pages()[&page_number];
        let contents_id = document
            .get_dictionary(page_id)
            .and_then(|page| page.get(b"Contents"))
            .and_then(Object::as_reference)
            .expect("page contents should exist");
        let stream = document
            .get_object(contents_id)
            .and_then(Object::as_stream)
            .expect("content stream should exist");
        String::from_utf8_lossy(&stream.content).into_owned()
    }

    // The single most common input to this tool is a portrait phone photo,
    // which is stored landscape with an orientation tag. A landscape page for
    // a portrait photo is the bug the preview thumbnail never shows.
    #[test]
    fn a_portrait_photo_produces_a_portrait_page() {
        let source = jpeg_with_orientation(40, 20, 6);
        let output = build_images_pdf_bytes(vec![source], PageMode::Fit)
            .expect("rotated photo should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");

        assert_eq!(
            page_box(&document, 1),
            vec![0.0, 0.0, 20.0, 40.0],
            "a photo tagged as rotated must get a page shaped the way it is seen"
        );

        // The stored JPEG is untouched: the rotation is done by the page's
        // placement matrix, so the passthrough stays byte-for-byte lossless.
        let image = page_image(&document, 1);
        assert_eq!(
            image
                .dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("width should exist"),
            40
        );
        assert_eq!(
            image
                .dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("filter should exist"),
            b"DCTDecode"
        );
        assert_eq!(
            page_content(&document, 1),
            "q\n0 -40 20 0 0 40 cm\n/Im0 Do\nQ",
            "the page must rotate the image it draws"
        );
    }

    #[test]
    fn an_unrotated_photo_is_placed_without_a_transform() {
        let output = build_images_pdf_bytes(vec![jpeg_fixture(40, 20)], PageMode::Fit)
            .expect("photo should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(
            page_content(&document, 1),
            "q\n40 0 0 20 0 0 cm\n/Im0 Do\nQ"
        );
        assert_eq!(page_box(&document, 1), vec![0.0, 0.0, 40.0, 20.0]);
    }

    // A Display-P3 or Adobe RGB photo reinterpreted as sRGB shifts every
    // colour on the page, so the profile has to travel with the pixels.
    // A profile the core cannot match to the image's channels is not embedded,
    // and the user is told the colours will be read as plain sRGB.
    #[test]
    fn an_unusable_colour_profile_is_reported_rather_than_guessed_at() {
        let mut profile = sample_icc_profile();
        profile[16..20].copy_from_slice(b"CMYK");
        let image = RgbImage::from_fn(8, 4, |x, y| Rgb([(x * 30) as u8, (y * 30) as u8, 7]));
        let mut source = Vec::new();
        let mut encoder = Encoder::new(&mut source, 90);
        encoder
            .add_icc_profile(&profile)
            .expect("profile should be accepted");
        encoder
            .encode(image.as_raw(), 8, 4, ColorType::Rgb)
            .expect("fixture JPEG should encode");

        let notices = build_images_pdf_notices(vec![source], PageMode::Fit);
        assert!(notices
            .iter()
            .any(|notice| notice.code() == "image-color-profile-dropped"));
    }

    #[test]
    fn an_embedded_colour_profile_travels_into_the_pdf() {
        let output = build_images_pdf_bytes(vec![jpeg_with_profile(24, 12)], PageMode::Fit)
            .expect("profiled photo should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");

        let color_space = page_image(&document, 1)
            .dict
            .get(b"ColorSpace")
            .and_then(Object::as_array)
            .expect("an ICC colour space should be an array");
        assert_eq!(
            color_space.first().and_then(|value| value.as_name().ok()),
            Some(b"ICCBased".as_slice()),
            "the page image must reference the source colour profile"
        );

        let profile_id = color_space[1]
            .as_reference()
            .expect("the profile should be an indirect stream");
        let profile = document
            .get_object(profile_id)
            .and_then(Object::as_stream)
            .expect("the profile stream should exist");
        assert_eq!(profile.content, sample_icc_profile());
        assert_eq!(
            profile
                .dict
                .get(b"N")
                .and_then(Object::as_i64)
                .expect("component count should be declared"),
            3
        );
    }

    #[test]
    fn embeds_jpeg_and_png_with_expected_filters_and_dimensions() {
        let gps_jpeg = inject_exif(&jpeg_fixture(40, 20));
        let output = build_images_pdf_bytes(vec![gps_jpeg, png_fixture(12, 24)], PageMode::Fit)
            .expect("images should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(document.get_pages().len(), 2);
        assert!(!output.windows(6).any(|window| window == b"Exif\0\0"));
        eprintln!(
            "real example: 2 images -> {} PDF pages / {} bytes; GPS EXIF present: false",
            document.get_pages().len(),
            output.len()
        );

        let jpeg = page_image(&document, 1);
        assert_eq!(
            jpeg.dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("JPEG filter should exist"),
            b"DCTDecode"
        );
        assert_eq!(
            jpeg.dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("JPEG width should exist"),
            40
        );
        assert_eq!(
            jpeg.dict
                .get(b"Height")
                .and_then(Object::as_i64)
                .expect("JPEG height should exist"),
            20
        );

        let png = page_image(&document, 2);
        assert_eq!(
            png.dict
                .get(b"Filter")
                .and_then(Object::as_name)
                .expect("PNG filter should exist"),
            b"FlateDecode"
        );
        assert_eq!(
            png.dict
                .get(b"Width")
                .and_then(Object::as_i64)
                .expect("PNG width should exist"),
            12
        );
        assert_eq!(
            png.dict
                .get(b"Height")
                .and_then(Object::as_i64)
                .expect("PNG height should exist"),
            24
        );
    }

    #[test]
    fn strips_exif_gps_before_embedding_jpeg() {
        let source = inject_exif(&jpeg_fixture(32, 16));
        assert!(source.windows(6).any(|window| window == b"Exif\0\0"));
        let output =
            build_images_pdf_bytes(vec![source], PageMode::Fit).expect("JPEG should combine");
        assert!(!output.windows(6).any(|window| window == b"Exif\0\0"));
        assert!(!output.windows(3).any(|window| window == b"GPS"));
    }

    #[test]
    fn baseline_jpeg_passthrough_keeps_stripped_bytes_exactly() {
        let source = inject_exif(&jpeg_fixture(48, 24));
        let expected = strip_jpeg_metadata(&source).expect("baseline JPEG should strip");
        let output =
            build_images_pdf_bytes(vec![source], PageMode::Fit).expect("JPEG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(page_image(&document, 1).content, expected);
    }

    #[test]
    fn output_is_deterministic_and_has_no_info_dictionary() {
        let images = vec![jpeg_fixture(32, 20), png_fixture(17, 9)];
        let first = build_images_pdf_bytes(images.clone(), PageMode::A4).expect("first run");
        let second = build_images_pdf_bytes(images, PageMode::A4).expect("second run");
        assert_eq!(first, second);
        let document = Document::load_mem(&first).expect("output PDF should parse");
        assert!(document.trailer.get(b"Info").is_err());
    }

    #[test]
    fn fit_pages_match_pixel_dimensions() {
        let output = build_images_pdf_bytes(vec![png_fixture(123, 45)], PageMode::Fit)
            .expect("PNG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(page_box(&document, 1), vec![0.0, 0.0, 123.0, 45.0]);
    }

    #[test]
    fn a4_pages_scale_within_margins_and_preserve_aspect() {
        let geometry = page_geometry(PageMode::A4, 400, 200);
        assert_eq!((geometry.page_width, geometry.page_height), (595.0, 842.0));
        assert!(geometry.draw_width <= 595.0 - PAGE_MARGIN * 2.0);
        assert!(geometry.draw_height <= 842.0 - PAGE_MARGIN * 2.0);
        assert!((geometry.draw_width / geometry.draw_height - 2.0).abs() < 0.000_001);
        assert!((geometry.offset_x - PAGE_MARGIN).abs() < 0.000_001);

        let output = build_images_pdf_bytes(vec![png_fixture(400, 200)], PageMode::A4)
            .expect("PNG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(page_box(&document, 1), vec![0.0, 0.0, 595.0, 842.0]);
    }

    #[test]
    fn letter_pages_have_fixed_dimensions() {
        let output = build_images_pdf_bytes(vec![png_fixture(20, 40)], PageMode::Letter)
            .expect("PNG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(page_box(&document, 1), vec![0.0, 0.0, 612.0, 792.0]);
    }

    #[test]
    fn unknown_page_mode_defaults_to_fit() {
        assert_eq!(PageMode::from_public("poster"), PageMode::Fit);
    }

    #[test]
    fn empty_input_is_rejected() {
        assert_eq!(
            build_images_pdf_bytes(Vec::new(), PageMode::Fit).expect_err("empty input must fail"),
            "Choose at least one image to combine."
        );
    }

    #[test]
    fn undecodable_image_error_names_its_position() {
        let error = build_images_pdf_bytes(
            vec![png_fixture(2, 2), b"\x89PNG\r\n\x1a\ntruncated".to_vec()],
            PageMode::Fit,
        )
        .expect_err("truncated PNG must fail");
        assert!(error.starts_with("Image 2 could not be read:"));
    }

    #[test]
    fn non_image_blob_is_rejected_without_panicking() {
        let error = build_images_pdf_bytes(vec![b"not an image".to_vec()], PageMode::Fit)
            .expect_err("non-image must fail");
        assert!(error.starts_with("Image 1 could not be read:"));
    }

    #[test]
    fn too_many_images_are_rejected_before_decoding() {
        let images = vec![Vec::new(); MAX_IMAGES + 1];
        let error = build_images_pdf_bytes(images, PageMode::Fit).expect_err("limit must fail");
        assert!(error.contains("500"));
    }

    #[test]
    fn transparent_pixels_are_flattened_to_white_rgb() {
        let mut image = RgbaImage::new(1, 1);
        image.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        let mut source = Vec::new();
        PngEncoder::new(&mut source)
            .write_image(image.as_raw(), 1, 1, ExtendedColorType::Rgba8)
            .expect("transparent PNG should encode");
        let output =
            build_images_pdf_bytes(vec![source], PageMode::Fit).expect("PNG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        let stream = page_image(&document, 1);
        let mut decoded = Vec::new();
        ZlibDecoder::new(stream.content.as_slice())
            .read_to_end(&mut decoded)
            .expect("Flate image should inflate");
        assert_eq!(decoded, vec![255, 255, 255]);
    }

    #[test]
    fn grayscale_baseline_jpeg_uses_device_gray() {
        let source = grayscale_jpeg_fixture(8, 4);
        assert_eq!(
            parse_sof0(&source).expect("SOF0 should parse").components,
            1
        );
        let output =
            build_images_pdf_bytes(vec![source], PageMode::Fit).expect("JPEG should combine");
        let document = Document::load_mem(&output).expect("output PDF should parse");
        assert_eq!(
            page_image(&document, 1)
                .dict
                .get(b"ColorSpace")
                .and_then(Object::as_name)
                .expect("color space should exist"),
            b"DeviceGray"
        );
    }
}
