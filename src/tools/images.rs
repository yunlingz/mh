//! Image normalization and resizing, executed outside the terminal task.

use std::io::{Cursor, Read};
use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, imageops::FilterType};
use tokio_util::sync::CancellationToken;

use crate::message::{Content, ToolResult};

const INLINE_BASE64_LIMIT: usize = 4_718_592;
const COMPRESSED_LIMIT: u64 = 64 * 1024 * 1024;

pub fn read_image(
    path: &Path,
    format: ImageFormat,
    cancel: &CancellationToken,
) -> Result<ToolResult, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let file =
        crate::regular_file::open_read(path).map_err(|_| "Cannot open a regular image file")?;
    if file.metadata().map_err(|_| "Cannot inspect image")?.len() > COMPRESSED_LIMIT {
        return Ok(ToolResult::text(
            "[Image omitted: compressed image exceeds the 64 MiB processing limit.]",
        ));
    }
    let mut bytes = Vec::new();
    file.take(COMPRESSED_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read image")?;
    if bytes.len() as u64 > COMPRESSED_LIMIT {
        return Ok(ToolResult::text(
            "[Image omitted: compressed image exceeds the 64 MiB processing limit.]",
        ));
    }
    let result = process(&bytes, format, cancel);
    match result {
        Ok((data, mime, note)) => Ok(ToolResult {
            content: vec![
                Content::text(note),
                Content::Image {
                    data,
                    mime_type: mime,
                },
            ],
            details: serde_json::Value::Null,
            is_error: false,
            structured_content: None,
        }),
        Err(_) if cancel.is_cancelled() => Err("Operation aborted".into()),
        Err(_) => Ok(ToolResult::text(
            "[Image omitted: could not be converted or resized below the inline image size limit.]",
        )),
    }
}

/// Normalize user clipboard/file bytes at the image-input boundary.
pub fn attachment(bytes: &[u8], cancel: &CancellationToken) -> Result<Content, String> {
    if bytes.len() as u64 > COMPRESSED_LIMIT {
        return Err("Image exceeds the 64 MiB processing limit".into());
    }
    let format = image::guess_format(bytes).map_err(|_| "Unsupported clipboard image")?;
    let (data, mime_type, _) = process(bytes, format, cancel)?;
    Ok(Content::Image { data, mime_type })
}

fn process(
    bytes: &[u8],
    format: ImageFormat,
    cancel: &CancellationToken,
) -> Result<(String, String, String), String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "Cannot decode image")?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| "Cannot decode image")?;
    image.apply_orientation(orientation);
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let (original_width, original_height) = (image.width(), image.height());
    let mut source = bytes.to_owned();
    let mut mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
    .to_owned();
    let mut conversion = String::new();
    if format == ImageFormat::Bmp {
        source = encode_png(&image)?;
        conversion = "\n[Image converted from image/bmp to image/png.]".into();
    }
    if original_width <= 2000
        && original_height <= 2000
        && source.len().div_ceil(3) * 4 < INLINE_BASE64_LIMIT
    {
        return Ok((
            STANDARD.encode(source),
            mime.clone(),
            format!("Read image file [{mime}]{conversion}"),
        ));
    }
    let mut width = original_width;
    let mut height = original_height;
    if width > 2000 {
        height = ((height as f64 * 2000.0 / width as f64).round() as u32).max(1);
        width = 2000;
    }
    if height > 2000 {
        width = ((width as f64 * 2000.0 / height as f64).round() as u32).max(1);
        height = 2000;
    }
    loop {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let resized = image.resize_exact(width, height, FilterType::Lanczos3);
        let png = encode_png(&resized)?;
        // Alpha can encode the visible shape itself. JPEG discards it, so an
        // alpha-bearing source must remain PNG rather than silently lose content.
        let candidates = if image.color().has_alpha() {
            vec![(png, "image/png")]
        } else {
            let jpeg = encode_jpeg(&resized, 80)?;
            let mut candidates = if png.len() <= jpeg.len() {
                vec![(png, "image/png"), (jpeg, "image/jpeg")]
            } else {
                vec![(jpeg, "image/jpeg"), (png, "image/png")]
            };
            for quality in [85, 70, 55, 40] {
                candidates.push((encode_jpeg(&resized, quality)?, "image/jpeg"));
            }
            candidates
        };
        if let Some((bytes, kind)) = candidates
            .into_iter()
            .find(|(bytes, _)| bytes.len().div_ceil(3) * 4 < INLINE_BASE64_LIMIT)
        {
            mime = kind.into();
            let scale_x = original_width as f64 / width as f64;
            let scale_y = original_height as f64 / height as f64;
            let note = format!(
                "Read image file [{mime}]{conversion}\n[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply x coordinates by {scale_x:.2} and y coordinates by {scale_y:.2} to map to the original image.]"
            );
            return Ok((STANDARD.encode(bytes), mime, note));
        }
        if width == 1 && height == 1 {
            return Err("Cannot fit inline image".into());
        }
        width = (width * 3 / 4).max(1);
        height = (height * 3 / 4).max(1);
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| "Cannot encode image")?;
    Ok(output.into_inner())
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality)
        .encode_image(&image.to_rgb8())
        .map_err(|_| "Cannot encode image")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resized_alpha_images_keep_opacity_defined_content() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(2100, 20, |x, _| {
            image::Rgba([0, 0, 0, if x < 1050 { 0 } else { 255 }])
        }));
        let original = encode_png(&image).unwrap();
        let (encoded, mime, _) =
            process(&original, ImageFormat::Png, &CancellationToken::new()).unwrap();
        assert_eq!(mime, "image/png");
        let resized = image::load_from_memory(&STANDARD.decode(encoded).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(resized.get_pixel(0, 0)[3], 0);
        assert_eq!(resized.get_pixel(resized.width() - 1, 0)[3], 255);
    }

    #[test]
    fn extreme_aspect_ratios_report_both_coordinate_scales() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::new(1, 32_000));
        let original = encode_png(&image).unwrap();
        let (_, _, note) = process(&original, ImageFormat::Png, &CancellationToken::new()).unwrap();
        assert!(note.contains("displayed at 1x2000"));
        assert!(note.contains("x coordinates by 1.00 and y coordinates by 16.00"));
    }
}
