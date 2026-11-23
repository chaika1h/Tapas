//! Pictures kept on disk, so they needn't be decoded again: each camera's
//! last picture (its Home card, and its tile while live view connects) and
//! the hub's event pictures, as small JPEGs under
//! `%LOCALAPPDATA%\Tapas\data\pictures`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use slint::{Rgba8Pixel, SharedPixelBuffer};

pub type Frame = SharedPixelBuffer<Rgba8Pixel>;

/// Event pictures not looked at for this long are deleted.
const EVENT_PICTURES_KEPT: Duration = Duration::from_secs(45 * 24 * 3600);

fn dir() -> PathBuf {
    crate::config::data_dir().join("pictures")
}

/// Device ids are hex, but keep file names safe whatever they are.
fn safe(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn camera_path(device_id: &str) -> PathBuf {
    dir().join("cameras").join(format!("{}.jpg", safe(device_id)))
}

fn event_path(device_id: &str, start: i64) -> PathBuf {
    dir().join("events").join(safe(device_id)).join(format!("{start}.jpg"))
}

/// A camera's last picture and when it was taken.
pub fn load_camera(device_id: &str) -> Option<(Frame, SystemTime)> {
    let path = camera_path(device_id);
    let taken = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    Some((load(&path)?, taken))
}

pub fn save_camera(device_id: &str, frame: &Frame) {
    if let Err(e) = save(&camera_path(device_id), frame, 80) {
        log::info!("could not keep a camera's picture: {e:#}");
    }
}

pub fn load_event(device_id: &str, start: i64) -> Option<Frame> {
    load(&event_path(device_id, start))
}

pub fn save_event(device_id: &str, start: i64, frame: &Frame) {
    if let Err(e) = save(&event_path(device_id, start), frame, 85) {
        log::info!("could not keep an event picture: {e:#}");
    }
}

/// Deletes old event pictures; run once at start-up.
pub fn prune_events() {
    let Ok(cameras) = std::fs::read_dir(dir().join("events")) else { return };
    let mut removed = 0;
    for camera in cameras.flatten() {
        let Ok(files) = std::fs::read_dir(camera.path()) else { continue };
        for file in files.flatten() {
            let old = file
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > EVENT_PICTURES_KEPT);
            if old && std::fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
        let _ = std::fs::remove_dir(camera.path()); // only goes when empty
    }
    if removed > 0 {
        log::info!("deleted {removed} old event pictures");
    }
}

/// An event picture the hub sent as an image (an H500 sends JPEGs), at most
/// `width` wide.
pub fn decode_still(data: &[u8], width: u32) -> Result<Frame> {
    let image = image::load_from_memory(data).context("Could not read the picture")?.to_rgba8();
    let image = if image.width() > width {
        let height = (image.height() as u64 * width as u64 / image.width() as u64).max(1) as u32;
        image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    Ok(SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height()))
}

fn load(path: &std::path::Path) -> Option<Frame> {
    let bytes = std::fs::read(path).ok()?;
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height()))
}

fn save(path: &std::path::Path, frame: &Frame, quality: u8) -> Result<()> {
    use image::ImageEncoder;
    let dir = path.parent().context("no folder")?;
    std::fs::create_dir_all(dir)?;
    let mut rgb = Vec::with_capacity(frame.as_slice().len() * 3);
    for px in frame.as_slice() {
        rgb.extend_from_slice(&[px.r, px.g, px.b]);
    }
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality).write_image(
        &rgb,
        frame.width(),
        frame.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_pictures_as_jpeg() {
        let dir = std::env::temp_dir().join(format!("tapas-pictures-{}", std::process::id()));
        let path = dir.join("a").join("1.jpg");
        let mut frame = Frame::new(64, 36);
        for (i, px) in frame.make_mut_slice().iter_mut().enumerate() {
            *px = Rgba8Pixel { r: (i % 64 * 4) as u8, g: 128, b: 40, a: 255 };
        }
        save(&path, &frame, 85).unwrap();
        let back = load(&path).unwrap();
        assert_eq!((back.width(), back.height()), (64, 36));
        let (a, b) = (frame.as_slice()[100], back.as_slice()[100]);
        assert!((a.r as i32 - b.r as i32).abs() < 24 && (a.g as i32 - b.g as i32).abs() < 24);
        assert_eq!(safe("80F1/..\\x"), "80F1____x");
        let _ = std::fs::remove_dir_all(dir);
    }
}
