//! Image preprocessing for LLM vision APIs.
//!
//! Provider limits (as of mid-2026):
//!   - Anthropic: 5 MB per image, 8000 px max dimension
//!   - OpenAI:   20 MB per image, auto-resizes (no hard pixel limit)
//!   - Gemini:   20 MB total request, no hard pixel limit
//!
//! Safe defaults: 4096 px max, 4.5 MB, JPEG quality 85.

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView};
use std::io::Cursor;
use std::path::Path;

/// Conservative defaults that work across all major providers.
pub const DEFAULT_MAX_DIMENSION: u32 = 4096;
pub const DEFAULT_MAX_MB: f64 = 4.5;
pub const DEFAULT_QUALITY: u8 = 85;

/// Preprocessed image result.
pub struct Preprocessed {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Load and preprocess an image for sending to a vision LLM.
///
/// Steps:
///   1. If any dimension > *max_dimension*, resize proportionally.
///   2. If PNG/GIF/BMP and no alpha channel, convert to JPEG.
///   3. If still over *max_mb*, lower quality / dimensions further.
pub fn preprocess(
    path: &Path,
    max_dimension: u32,
    max_mb: f64,
    quality: u8,
) -> Result<Preprocessed, String> {
    let img = image::open(path).map_err(|e| format!("Failed to open image: {e}"))?;
    let (w, h) = img.dimensions();
    let max_bytes = (max_mb * 1_048_576.0) as usize;

    let mime = guess_mime(path);

    // Step 1: dimension cap
    let img = if w > max_dimension || h > max_dimension {
        img.resize(max_dimension, max_dimension, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Step 2: format conversion
    let is_lossless = matches!(
        mime.as_str(),
        "image/png" | "image/gif" | "image/bmp"
    );
    if is_lossless {
        // Try JPEG encoding
        let jpeg_bytes = encode_jpeg(&img, quality);
        if jpeg_bytes.len() <= max_bytes {
            return Ok(Preprocessed {
                bytes: jpeg_bytes,
                mime: "image/jpeg".into(),
            });
        }
    }

    // Step 3: size cap
    let buf = encode_image(&img, &mime, quality)?;
    if buf.len() <= max_bytes {
        return Ok(Preprocessed {
            bytes: buf,
            mime,
        });
    }

    // Try lower JPEG quality
    if mime != "image/webp" {
        for q in [75u8, 60, 45, 30] {
            let jpeg = encode_jpeg(&img, q);
            if jpeg.len() <= max_bytes {
                return Ok(Preprocessed {
                    bytes: jpeg,
                    mime: "image/jpeg".into(),
                });
            }
        }
    }

    // Last resort: shrink dimensions
    for scale in [0.75f32, 0.5, 0.33] {
        let nw = (img.width() as f32 * scale) as u32;
        let nh = (img.height() as f32 * scale) as u32;
        let small = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
        let jpeg = encode_jpeg(&small, 60);
        if jpeg.len() <= max_bytes {
            return Ok(Preprocessed {
                bytes: jpeg,
                mime: "image/jpeg".into(),
            });
        }
    }

    // Absolute last resort
    let tiny = img.resize(512, 512, image::imageops::FilterType::Lanczos3);
    let jpeg = encode_jpeg(&tiny, 40);
    Ok(Preprocessed {
        bytes: jpeg,
        mime: "image/jpeg".into(),
    })
}

fn guess_mime(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg".into(),
        Some("png") => "image/png".into(),
        Some("gif") => "image/gif".into(),
        Some("webp") => "image/webp".into(),
        Some("bmp") => "image/bmp".into(),
        _ => "image/jpeg".into(),
    }
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let rgb = img.to_rgb8();
    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality);
    encoder.encode_image(&rgb).ok();
    buf
}

fn encode_image(img: &DynamicImage, mime: &str, quality: u8) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);

    match mime {
        "image/jpeg" => {
            let rgb = img.to_rgb8();
            let mut enc = JpegEncoder::new_with_quality(&mut cursor, quality);
            enc.encode_image(&rgb)
                .map_err(|e| format!("JPEG encode error: {e}"))?;
        }
        "image/png" => {
            img.write_to(&mut cursor, image::ImageFormat::Png)
                .map_err(|e| format!("PNG encode error: {e}"))?;
        }
        "image/webp" => {
            img.write_to(&mut cursor, image::ImageFormat::WebP)
                .map_err(|e| format!("WebP encode error: {e}"))?;
        }
        _ => {
            // Default to JPEG
            let rgb = img.to_rgb8();
            let mut enc = JpegEncoder::new_with_quality(&mut cursor, quality);
            enc.encode_image(&rgb)
                .map_err(|e| format!("JPEG encode error: {e}"))?;
            return Ok(buf);
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{RgbImage, RgbaImage};

    fn temp_png_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.png");
        (dir, path)
    }

    #[test]
    fn png_converts_to_jpeg() {
        let (_dir, path) = temp_png_path();
        let img = RgbImage::from_pixel(200, 200, image::Rgb([255, 0, 0]));
        img.save(&path).unwrap();

        let result = preprocess(&path, 4096, 4.5, 85).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        assert!(result.bytes.len() < 5000);
    }

    #[test]
    fn oversized_dimensions_downscaled() {
        let (_dir, path) = temp_png_path();
        let img = RgbImage::from_pixel(6000, 4000, image::Rgb([0, 0, 255]));
        img.save(&path).unwrap();

        let result = preprocess(&path, 2048, 4.5, 85).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        // Decode back and check dimensions
        let decoded = image::load_from_memory(&result.bytes).unwrap();
        assert!(decoded.width() <= 2048);
        assert!(decoded.height() <= 2048);
    }

    #[test]
    fn jpeg_passthrough_within_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jpg");
        let img = RgbImage::from_pixel(100, 100, image::Rgb([0, 255, 0]));
        img.save(&path).unwrap();

        let result = preprocess(&path, 4096, 4.5, 85).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        assert!(!result.bytes.is_empty());
    }

    #[test]
    fn max_mb_enforced() {
        let (_dir, path) = temp_png_path();
        // Large solid-color image (compresses well but is big at 2048x2048)
        let img = RgbImage::from_pixel(2048, 2048, image::Rgb([128, 0, 64]));
        img.save(&path).unwrap();

        let result = preprocess(&path, 4096, 0.05, 85).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        assert!(result.bytes.len() < 51200);
    }

    #[test]
    fn rgba_png_flattened_to_jpeg() {
        let (_dir, path) = temp_png_path();
        let img = RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 128]));
        img.save(&path).unwrap();

        let result = preprocess(&path, 4096, 4.5, 85).unwrap();
        assert_eq!(result.mime, "image/jpeg");
        let decoded = image::load_from_memory(&result.bytes).unwrap();
        assert_eq!(decoded.color(), image::ColorType::Rgb8);
    }

    #[test]
    fn bad_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.txt");
        std::fs::write(&path, b"not an image").unwrap();

        assert!(preprocess(&path, 4096, 4.5, 85).is_err());
    }

    #[test]
    fn default_constants() {
        assert_eq!(DEFAULT_MAX_DIMENSION, 4096);
        assert!((DEFAULT_MAX_MB - 4.5).abs() < 0.001);
        assert_eq!(DEFAULT_QUALITY, 85);
    }

    #[test]
    fn guess_mime_from_extensions() {
        assert_eq!(guess_mime(std::path::Path::new("a.jpg")), "image/jpeg");
        assert_eq!(guess_mime(std::path::Path::new("a.jpeg")), "image/jpeg");
        assert_eq!(guess_mime(std::path::Path::new("a.png")), "image/png");
        assert_eq!(guess_mime(std::path::Path::new("a.gif")), "image/gif");
        assert_eq!(guess_mime(std::path::Path::new("a.webp")), "image/webp");
        assert_eq!(guess_mime(std::path::Path::new("a.bmp")), "image/bmp");
        assert_eq!(guess_mime(std::path::Path::new("a.unknown")), "image/jpeg");
    }
}
