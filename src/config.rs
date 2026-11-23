//! Settings saved to `%APPDATA%\TapoViewer\config\config.json`.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::cloud::CloudCamera;
use crate::secret::Secret;

/// The Tapo cloud sign-in. The password is not stored here: the session token
/// is reused until the cloud stops accepting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    /// Random ID that identifies this PC to the Tapo cloud. Kept so a
    /// verified sign-in is remembered and doesn't ask for a code again.
    pub terminal_uuid: String,
    /// The email the session belongs to.
    pub account: String,
    /// The account's regional API server.
    pub base_url: String,
    pub token: Secret,
}

/// Which of the camera's streams to request through the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Quality {
    /// The sub-stream (~640×360): light on bandwidth and CPU, for the grid.
    /// Settings from the RTSP version saved its built-in stream as `Mjpeg`.
    #[default]
    #[serde(alias = "Mjpeg")]
    Sd,
    /// The main stream at full resolution.
    Hd,
}

impl Quality {
    pub const LABELS: [&str; 2] = ["SD", "HD"];

    pub fn index(self) -> i32 {
        match self {
            Quality::Sd => 0,
            Quality::Hd => 1,
        }
    }

    pub fn from_index(i: i32) -> Self {
        if i == 1 { Quality::Hd } else { Quality::Sd }
    }
}

/// Light or dark look; `System` follows Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn index(self) -> i32 {
        match self {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        }
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::System,
        }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    pub id: u32,
    pub name: String,
    /// The camera on the Tapo account; video and pan/tilt go through the cloud.
    pub cloud: Option<CloudCamera>,
    pub grid_quality: Quality,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// The TP-Link / Tapo app account.
    pub cloud_username: String,
    pub cloud_password: Secret,
    pub cameras: Vec<CameraConfig>,
    /// The camera hubs (H500, H200) on the account, from its last list.
    /// Settings from the local-network version listed hubs by IP address;
    /// those have no device id and are dropped.
    #[serde(deserialize_with = "account_hubs")]
    pub hubs: Vec<CloudCamera>,
    /// Empty = look next to the exe, then on PATH.
    pub ffmpeg_path: String,
    pub renderer: Renderer,
    pub theme: Theme,
    pub cloud: CloudConfig,
    /// Don't look for a new version at start-up.
    pub skip_update_checks: bool,
    /// The GitHub repository ("owner/name") whose releases are the updates;
    /// empty for the default.
    pub update_repo: String,
}

impl AppConfig {
    pub fn next_camera_id(&self) -> u32 {
        self.cameras.iter().map(|c| c.id).max().unwrap_or(0) + 1
    }

    pub fn camera(&self, id: u32) -> Option<&CameraConfig> {
        self.cameras.iter().find(|c| c.id == id)
    }
}

fn account_hubs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<CloudCamera>, D::Error> {
    let hubs: Vec<CloudCamera> = Vec::deserialize(d)?;
    Ok(hubs
        .into_iter()
        .filter(|h| !h.device_id.is_empty())
        // Saved before names were decoded (an H500 as "VGFwbyBINTAw").
        .map(|h| CloudCamera { name: crate::cloud::decode_name(&h.name), ..h })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hubs_from_the_local_version_are_dropped() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"hubs": [{"id": 1, "name": "HomeBase", "host": "192.168.1.40"},
                         {"device_id": "HUB1", "name": "VGFwbyBINTAw", "model": "H500"}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.hubs.len(), 1);
        assert_eq!(cfg.hubs[0].device_id, "HUB1");
        assert_eq!(cfg.hubs[0].name, "Tapo H500");
    }
}
