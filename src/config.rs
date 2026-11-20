//! Settings saved to `%APPDATA%\TapoViewer\config\config.json`.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// Which camera stream to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Quality {
    /// The camera's MJPEG stream (`stream8`, ~640×360), decoded in-process.
    /// Light on CPU, no extra tools needed: ideal for the grid.
    #[default]
    Mjpeg,
    /// The H.264/H.265 sub-stream (`stream2`), decoded by ffmpeg.
    Sd,
    /// The H.264/H.265 main stream (`stream1`), decoded by ffmpeg.
    Hd,
}

impl Quality {
    pub const LABELS: [&str; 3] = ["Fast (built-in)", "SD (ffmpeg)", "HD (ffmpeg)"];

    pub fn index(self) -> i32 {
        match self {
            Quality::Mjpeg => 0,
            Quality::Sd => 1,
            Quality::Hd => 2,
        }
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Quality::Sd,
            2 => Quality::Hd,
            _ => Quality::Mjpeg,
        }
    }

    pub fn needs_ffmpeg(self) -> bool {
        self != Quality::Mjpeg
    }
}

/// Graphics backend, picked at start-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Renderer {
    /// FemtoVG on wgpu, forced onto Direct3D 12. Avoids the OpenGL
    /// exclusive-fullscreen flicker some Windows drivers produce.
    #[default]
    Direct3D,
    Vulkan,
    /// CPU rendering; slowest, but works everywhere (VMs, RDP, old GPUs).
    Software,
    OpenGL,
}

impl Renderer {
    pub fn index(self) -> i32 {
        match self {
            Renderer::Direct3D => 0,
            Renderer::Vulkan => 1,
            Renderer::Software => 2,
            Renderer::OpenGL => 3,
        }
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Renderer::Vulkan,
            2 => Renderer::Software,
            3 => Renderer::OpenGL,
            _ => Renderer::Direct3D,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    pub id: u32,
    pub name: String,
    pub host: String,
    pub rtsp_port: u16,
    /// The camera account set in the Tapo app
    /// (Camera Settings › Advanced Settings › Camera Account).
    pub username: String,
    pub password: Secret,
    pub ptz: bool,
    pub grid_quality: Quality,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            host: String::new(),
            rtsp_port: 554,
            username: String::new(),
            password: Secret::default(),
            ptz: false,
            grid_quality: Quality::Mjpeg,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    pub id: u32,
    pub name: String,
    pub host: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// TP-Link / Tapo app account. Used for discovery, pan/tilt and the hub.
    pub cloud_username: String,
    pub cloud_password: Secret,
    pub cameras: Vec<CameraConfig>,
    pub hubs: Vec<HubConfig>,
    /// Empty = look next to the exe, then on PATH.
    pub ffmpeg_path: String,
    pub renderer: Renderer,
}

impl AppConfig {
    pub fn next_camera_id(&self) -> u32 {
        self.cameras.iter().map(|c| c.id).max().unwrap_or(0) + 1
    }

    pub fn next_hub_id(&self) -> u32 {
        self.hubs.iter().map(|h| h.id).max().unwrap_or(0) + 1
    }

    pub fn camera(&self, id: u32) -> Option<&CameraConfig> {
        self.cameras.iter().find(|c| c.id == id)
    }
}

fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "TapoViewer")
}

pub fn config_path() -> PathBuf {
    dirs()
        .map(|d| d.config_dir().join("config.json"))
        .unwrap_or_else(|| PathBuf::from("tapo-viewer-config.json"))
}

pub fn load() -> AppConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            log::error!("{} is invalid ({e}); starting with empty settings", path.display());
            let _ = std::fs::copy(&path, path.with_extension("json.bad"));
            AppConfig::default()
        }),
        Err(_) => AppConfig::default(),
    }
}

pub fn save(cfg: &AppConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// `%LOCALAPPDATA%\TapoViewer\data` (the unpacked built-in ffmpeg).
pub fn data_dir() -> PathBuf {
    dirs()
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

/// `Pictures\TapoViewer` (snapshots).
pub fn snapshot_dir() -> PathBuf {
    user_subdir(|u| u.picture_dir().map(Into::into))
}

/// `Videos\TapoViewer` (downloaded recordings).
pub fn recordings_dir() -> PathBuf {
    user_subdir(|u| u.video_dir().map(Into::into))
}

fn user_subdir(pick: impl Fn(&directories::UserDirs) -> Option<PathBuf>) -> PathBuf {
    let base = directories::UserDirs::new()
        .and_then(|u| pick(&u).or_else(|| Some(u.home_dir().to_path_buf())))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("TapoViewer")
}
