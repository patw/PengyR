//! Durable content-addressed attachment storage (schema v1).
use crate::config::pengy_config_dir;
use crate::image_utils;
use base64::Engine;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_SOURCE_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 40_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub v: u32,
    pub id: String,
    pub kind: String,
    pub name: String,
    pub media_type: String,
    pub byte_size: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageMetadata>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub width: u32,
    pub height: u32,
}

fn digest(id: &str) -> Result<&str, String> {
    let Some(hex) = id.strip_prefix("sha256:") else {
        return Err("invalid attachment id".into());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid attachment id".into());
    }
    Ok(hex)
}
pub fn object_path(id: &str) -> Result<PathBuf, String> {
    let d = digest(id)?;
    Ok(pengy_config_dir()
        .join("attachments/objects/sha256")
        .join(&d[..2])
        .join(d))
}
pub fn derivative_path(id: &str, name: &str) -> Result<PathBuf, String> {
    if !matches!(name, "image-display-v1.jpg" | "thumbnail-256-v1.jpg") {
        return Err("invalid derivative name".into());
    }
    let d = digest(id)?;
    Ok(pengy_config_dir()
        .join("attachments/derivatives/sha256")
        .join(&d[..2])
        .join(d)
        .join(name))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("missing parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}
fn mime(fmt: image::ImageFormat) -> &'static str {
    match fmt {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::Tiff => "image/tiff",
        _ => "application/octet-stream",
    }
}
pub fn import_image(
    path: &Path,
    name: &str,
    max_dimension: u32,
    max_mb: f64,
    quality: u8,
) -> Result<AttachmentRef, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_BYTES {
        return Err("image exceeds the 25 MB attachment source limit".into());
    }
    let format = image::guess_format(&bytes).map_err(|_| "attachment is not a valid image")?;
    let img = image::load_from_memory(&bytes).map_err(|_| "attachment is not a valid image")?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err("image exceeds decoded pixel limit".into());
    }
    let hex = format!("{:x}", Sha256::digest(&bytes));
    let id = format!("sha256:{hex}");
    let source = object_path(&id)?;
    if !source.exists() {
        atomic_write(&source, &bytes)?
    }
    ensure_image_derivatives(&id, max_dimension, max_mb, quality)?;
    Ok(AttachmentRef {
        v: 1,
        id,
        kind: "image".into(),
        name: name.replace('\0', "").trim().chars().take(240).collect(),
        media_type: mime(format).into(),
        byte_size: bytes.len() as u64,
        created_at: chrono::Utc::now().to_rfc3339(),
        image: Some(ImageMetadata {
            width: w,
            height: h,
        }),
        extra: BTreeMap::new(),
    })
}
pub fn ensure_image_derivatives(
    id: &str,
    max_dimension: u32,
    max_mb: f64,
    quality: u8,
) -> Result<(), String> {
    let display = derivative_path(id, "image-display-v1.jpg")?;
    let thumb = derivative_path(id, "thumbnail-256-v1.jpg")?;
    if display.exists() && thumb.exists() {
        return Ok(());
    };
    let source = object_path(id)?;
    let p = image_utils::preprocess(&source, max_dimension, max_mb, quality)?;
    let img = image::load_from_memory(&p.bytes)
        .map_err(|e| e.to_string())?
        .to_rgb8();
    let mut d = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut d, quality)
        .encode_image(&img)
        .map_err(|e| e.to_string())?;
    let small = image::DynamicImage::ImageRgb8(img)
        .thumbnail(256, 256)
        .to_rgb8();
    let mut t = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut t, 82)
        .encode_image(&small)
        .map_err(|e| e.to_string())?;
    if !display.exists() {
        atomic_write(&display, &d)?
    };
    if !thumb.exists() {
        atomic_write(&thumb, &t)?
    };
    Ok(())
}
pub fn image_data_url(
    r: &AttachmentRef,
    max_dimension: u32,
    max_mb: f64,
    quality: u8,
) -> Option<String> {
    if r.kind != "image" {
        return None;
    };
    ensure_image_derivatives(&r.id, max_dimension, max_mb, quality).ok()?;
    let b = fs::read(derivative_path(&r.id, "image-display-v1.jpg").ok()?).ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(b)
    ))
}
pub fn storage_report(referenced: &std::collections::BTreeSet<String>) -> serde_json::Value {
    let root = pengy_config_dir().join("attachments");
    let mut objects = 0u64;
    let mut bytes = 0u64;
    let mut reclaimable = 0u64;
    let base = root.join("objects/sha256");
    if let Ok(prefixes) = fs::read_dir(&base) {
        for prefix in prefixes.flatten() {
            if let Ok(entries) = fs::read_dir(prefix.path()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() { continue; }
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue; };
                    if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { continue; }
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    objects += 1; bytes += size;
                    if !referenced.contains(&format!("sha256:{name}")) { reclaimable += size; }
                }
            }
        }
    }
    serde_json::json!({"objects": objects, "object_bytes": bytes, "referenced": referenced.len(), "reclaimable_bytes": reclaimable, "delete_performed": false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_traversal_and_bad_ids() {
        assert!(object_path("../../bad").is_err());
        assert!(derivative_path("sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "thumbnail-256-v1.jpg").is_err());
        assert!(derivative_path("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "other.bin").is_err());
    }

    #[test]
    fn unknown_attachment_fields_round_trip() {
        let value = serde_json::json!({"v":1,"id":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","kind":"future-widget","name":"x","media_type":"application/x-future","byte_size":1,"created_at":"2026-09-03T00:00:00Z","future":{"keep":true}});
        let reference: AttachmentRef = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(reference).unwrap(), value);
    }

    #[test]
    fn missing_and_corrupt_objects_do_not_resolve() {
        let dir = tempdir().unwrap();
        crate::config::set_config_dir(dir.path().to_str().unwrap());
        let reference = AttachmentRef { v:1, id:"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(), kind:"image".into(), name:"missing.png".into(), media_type:"image/png".into(), byte_size:1, created_at:"now".into(), image:None, extra:BTreeMap::new() };
        assert!(image_data_url(&reference, 4096, 4.5, 85).is_none());
    }
}

pub fn label(r: &AttachmentRef) -> String {
    if r.kind == "image" {
        let d = r
            .image
            .as_ref()
            .map(|m| format!(" · {}×{}", m.width, m.height))
            .unwrap_or_default();
        format!("[image: {}{}]", r.name, d)
    } else {
        format!("[attachment: {}]", r.name)
    }
}
