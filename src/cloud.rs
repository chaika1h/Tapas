//! Tapo cloud: account sign-in (with two-step verification), the camera list
//! and the request for a camera's video relay, which is how the Tapo app shows
//! live video when the phone isn't on the cameras' network.
//!
//! The API is private and undocumented; TP-Link can change it at any time.
//! This is a port of `lib/api/tapo_cloud.dart` from Topo TV
//! (<https://github.com/abdulmominsakib/tapo-android-tv>, MIT licence), which
//! is itself based on Topo (<https://github.com/filirnd/topo>, MIT licence).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use chrono::NaiveDate;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest, Md5};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;

/// The app this client presents itself as. If the cloud starts refusing old
/// app versions, raising `APP_VERSION` is the first thing to try.
pub const APP_NAME: &str = "TP-Link_Tapo_Android";
pub const APP_VERSION: &str = "3.21.114";
const PLATFORM: &str = "Android 14";
/// Shown in the Tapo app's list of signed-in devices.
const TERMINAL_NAME: &str = "Tapas (Windows)";

const LOGIN_URL: &str = "https://n-wap-gw.tplinkcloud.com";

// The Tapo Android app's own API key pair. It identifies the app, not the
// user's account, and is published by both projects this module is based on.
const ACCESS_KEY: &str = "4d11b6b9d5ea4d19a829adbb9714b057";
const ACCESS_SECRET: &str = "6ed7d97f3e73467f8a5bab90b577ba4c";
/// What both reference clients send as the signature timestamp.
const TIMESTAMP: &str = "9999999999";

/// TP-Link's own root certificates (`tp-link-CA` and `TP-Link Cloud Root CA`).
/// Some cloud servers use them and they aren't in the Windows trust store.
const TP_LINK_ROOTS: &[u8] = include_bytes!("../certs/tapo-cloud-roots.pem");

/// Service ids for the cloud's service lookup.
const CIPC_SERVICE: &str = "cipc.api.cloud";
const CAMERA_TYPE: &str = "SMART.IPCAMERA";
const HUB_TYPE: &str = "SMART.TAPOHUB";
/// Relay stream types (`StreamType` in the Tapo app).
const STREAM_PREVIEW: i64 = 0;
const STREAM_PLAYBACK: i64 = 1;
const STREAM_DOWNLOAD: i64 = 3;
const STREAM_IMAGE: i64 = 6;

/// The kinds of relay the Tapo app opens for a hub's recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipStream {
    /// Stream type 3 with this sub-type: "video" (the app's download of a
    /// recording) or "cover" (its download type 3, which looks to be for
    /// thumbnails: an H500 accepts it and then sends nothing).
    Download(&'static str),
    /// Stream type 1: the app's playback, sent in real time.
    Playback,
}
const IOT_SERVICE: &str = "nbu.iot-app-server.app";

/// Pan/tilt steps the camera accepts in one move.
pub const MAX_MOVE: i32 = 200;

/// The saved sign-in is no longer accepted.
const AUTH_EXPIRED: i64 = -20651;
/// Login answered by the wrong region; retry at `appServerUrl`.
const WRONG_REGION: i64 = -20212;

#[derive(Debug, Clone)]
pub struct CloudError {
    message: String,
    code: Option<i64>,
}

impl CloudError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None }
    }

    fn with_code(message: impl Into<String>, code: i64) -> Self {
        Self { message: message.into(), code: Some(code) }
    }

    /// The cloud no longer accepts the session token: sign in again.
    pub fn is_auth(&self) -> bool {
        self.code == Some(AUTH_EXPIRED)
    }

    pub fn is_offline(&self) -> bool {
        matches!(self.code, Some(-40401 | -40406))
    }

    /// The cloud answered but refused the request for another reason.
    pub fn is_refused(&self) -> bool {
        self.code.is_some() && !self.is_auth() && !self.is_offline()
    }
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CloudError {}

pub type Result<T> = std::result::Result<T, CloudError>;

/// A signed-in account: the regional API server and the session token.
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    pub base_url: String,
    pub token: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("base_url", &self.base_url).field("token", &"<redacted>").finish()
    }
}

/// Where the cloud sends the two-step verification code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaMethod {
    /// A notification in the Tapo app on the phone.
    App,
    Email,
}

impl MfaMethod {
    fn code(self) -> i64 {
        match self {
            MfaMethod::App => 1,
            MfaMethod::Email => 2,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        match code {
            1 => Some(MfaMethod::App),
            2 => Some(MfaMethod::Email),
            _ => None,
        }
    }
}

/// A sign-in that needs a verification code before it completes.
#[derive(Debug, Clone)]
pub struct MfaChallenge {
    pub base_url: String,
    pub process_id: String,
    pub methods: Vec<MfaMethod>,
}

pub enum Login {
    Done(Session),
    NeedsMfa(MfaChallenge),
}

/// A camera on the Tapo account, as stored in the settings when a camera is
/// linked to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudCamera {
    pub device_id: String,
    pub name: String,
    pub model: String,
    pub device_type: String,
    /// Upper-case hex digits only.
    pub mac: String,
}

/// A camera paired with a hub, as the hub lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubCamera {
    pub device_id: String,
    pub name: String,
    pub model: String,
    /// As the hub reports it (the Tapo app passes it on unchanged).
    pub mac: String,
    /// Whether the hub stores this camera's recordings.
    pub hub_storage: bool,
}

/// One recording on a hub, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clip {
    pub start: i64,
    pub end: i64,
    /// What triggered it (the API's `video_type`); see [`clip_kind`].
    pub kind: i64,
}

impl Clip {
    pub fn seconds(&self) -> i64 {
        (self.end - self.start).max(0)
    }
}

/// What a hub says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubInfo {
    pub firmware: String,
    pub ip: String,
    /// Its disks, largest first: e.g. "HDD 812 GB free of 931 GB".
    pub storage: Vec<String>,
    /// A disk's state when it isn't "normal", e.g. "HDD: unformatted".
    pub storage_status: String,
    /// The same disks, largest first, for the storage bar.
    pub disks: Vec<Disk>,
    /// Whether the newest recordings replace the oldest when it's full.
    pub loop_recording: Option<bool>,
}

/// One of a hub's disks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Disk {
    /// "HDD", "Internal", "SD card"...
    pub label: String,
    /// As the hub says them, spaced: "931.5 GB".
    pub total: String,
    pub free: String,
    /// The same in bytes, 0 when unknown.
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// "normal", "unformatted"...
    pub status: String,
    /// Drive temperature in Celsius when reported by the hub (e.g. SATA HDD).
    pub temperature_c: Option<i64>,
    /// Power-on hours when reported by SMART on the hub's drive.
    pub power_on_hours: Option<u64>,
}

impl Disk {
    /// How full it is, 0 to 1, if known.
    pub fn used(&self) -> Option<f32> {
        (self.total_bytes > 0 && !self.free.is_empty())
            .then(|| 1.0 - (self.free_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32)
    }
}

/// Camera night-vision / day-night mode (Tapo C520WS & compatible cameras).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NightVisionMode {
    #[default]
    Auto,
    Day,
    Infrared,
    Color,
    Smart,
}

impl NightVisionMode {
    pub fn index(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Day => 1,
            Self::Infrared => 2,
            Self::Color => 3,
            Self::Smart => 4,
        }
    }

    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Day,
            2 => Self::Infrared,
            3 => Self::Color,
            4 => Self::Smart,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => crate::i18n::pick("Auto", "Otomatik"),
            Self::Day => crate::i18n::pick("Day", "Gündüz"),
            Self::Infrared => crate::i18n::pick("Infrared", "Kızılötesi"),
            Self::Color => crate::i18n::pick("Color", "Renkli"),
            Self::Smart => crate::i18n::pick("Smart", "Akıllı"),
        }
    }
}

/// Motor patrol (cruise) direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatrolAxis {
    Horizontal,
    Vertical,
}

impl PatrolAxis {
    pub fn coord(self) -> &'static str {
        match self {
            Self::Horizontal => "x",
            Self::Vertical => "y",
        }
    }
}

/// Snapshot of a camera's live capabilities and toggle states, fetched in one
/// batched request when opening the camera view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CameraControls {
    pub ptz: bool,
    pub privacy: Option<bool>,
    pub spotlight: Option<bool>,
    pub night_vision: Option<NightVisionMode>,
    pub auto_track: Option<bool>,
    pub siren_on: Option<bool>,
}

/// Category filter for the Playback / AI Events list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventFilter {
    #[default]
    All,
    Person,
    Vehicle,
    Pet,
    Motion,
    Alerts,
}

impl EventFilter {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Person,
            2 => Self::Vehicle,
            3 => Self::Pet,
            4 => Self::Motion,
            5 => Self::Alerts,
            _ => Self::All,
        }
    }

    pub fn matches(self, d: &Detection) -> bool {
        let has = |pred: fn(i64) -> bool| pred(d.kind) || d.kinds.iter().copied().any(pred);
        match self {
            Self::All => true,
            Self::Person => has(|k| matches!(k, 6 | 20..=22 | 27)),
            Self::Vehicle => has(|k| k == 8),
            Self::Pet => has(|k| matches!(k, 9 | 11 | 12)),
            Self::Motion => has(|k| k == 2),
            Self::Alerts => has(|k| matches!(k, 3..=5 | 13..=16 | 19)),
        }
    }
}

/// The Tapo app's homes ("families") and rooms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Homes {
    /// Home names, in the account's order.
    pub names: Vec<String>,
    /// Room name by device id, for devices placed in a room.
    pub rooms: HashMap<String, String>,
    /// The account's only room, when it has just one: where every device is.
    pub only_room: Option<String>,
}

impl Homes {
    pub fn room_of(&self, device_id: &str) -> Option<&String> {
        self.rooms.get(device_id).or(self.only_room.as_ref())
    }
}

/// Something a camera detected, from the hub's event list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub start: i64,
    pub end: i64,
    /// What it was (`alarm_type`); see [`clip_kind`].
    pub kind: i64,
    /// Everything detected meanwhile (`events_1`, a bit per kind).
    pub kinds: Vec<i64>,
}

/// Classification tag assigned by the H500 HomeBase face recognition engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaceTag {
    Familiar,
    Stranger,
    #[default]
    Unknown,
}

impl FaceTag {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "familiar" => Self::Familiar,
            "stranger" => Self::Stranger,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Familiar => "familiar",
            Self::Stranger => "stranger",
            Self::Unknown => "",
        }
    }

    pub fn is_familiar(self) -> bool {
        self == Self::Familiar
    }
}

/// A face profile stored on the H500 HomeBase (`searchFacesInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceInfo {
    pub face_id: i64,
    pub name: String,
    pub tag: FaceTag,
    pub create_time: i64,
    pub fresh_time: i64,
    pub modify_time: i64,
    pub user_verified: bool,
}

impl FaceInfo {
    /// Most recent timestamp associated with this face (fresh, else modify, else create).
    pub fn last_seen(&self) -> i64 {
        [self.fresh_time, self.modify_time, self.create_time]
            .into_iter()
            .find(|&t| t > 0)
            .unwrap_or(0)
    }
}

/// Filter pill for the People / Faces page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaceFilter {
    #[default]
    All,
    Familiar,
    Stranger,
}

impl FaceFilter {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Familiar,
            2 => Self::Stranger,
            _ => Self::All,
        }
    }

    pub fn matches(self, face: &FaceInfo) -> bool {
        match self {
            Self::All => true,
            Self::Familiar => face.tag == FaceTag::Familiar,
            Self::Stranger => face.tag != FaceTag::Familiar,
        }
    }

    pub fn tag_param(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Familiar => Some("familiar"),
            Self::Stranger => Some("stranger"),
        }
    }
}

/// One cross-camera face tracking trajectory (`doSearchFaceTrackingList`) or
/// face-filtered recording clip (`doSearchFaceDetectionVideo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceTrackingEntry {
    pub face_id: i64,
    pub start_time: i64,
    pub end_time: i64,
    /// Normalized uppercase hex MAC (no separators) of the camera that captured the snapshot/clip.
    pub snapshot_mac: String,
    /// Optional hub child camera `device_id` (populated by `doSearchFaceDetectionVideo`).
    pub dev_id: String,
    pub channel_id: i32,
    pub clip_count: i32,
    pub device_count: i32,
}

/// One camera clip belonging to a face tracking trajectory (`doSearchFaceTrackingClips`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceTrackingClip {
    pub start_time: i64,
    pub end_time: i64,
    /// Normalized uppercase hex MAC resolved from `traj_macs[mac_idx]`.
    pub mac: String,
    pub channel_id: i32,
    pub codec: String,
}

/// Whether a `SMART.TAPOHUB` model stores camera video (the H500 HomeBase and
/// the H200), unlike the smart-home hubs (H100, H110).
pub fn is_camera_hub(model: &str) -> bool {
    let model = model.trim().to_ascii_uppercase();
    model.starts_with("H2") || model.starts_with("H5")
}

/// A recording's trigger, from its `video_type`.
pub fn clip_kind(code: i64) -> String {
    match code {
        1 => "Continuous",
        2 => "Motion",
        3 => "Camera tampering",
        4 => "Line crossing",
        5 => "Area intrusion",
        6 => "Person",
        7 => "Baby crying",
        8 => "Vehicle",
        9 => "Pet",
        10 => "Doorbell ring",
        11 => "Dog barking",
        12 => "Cat meowing",
        13 => "Glass breaking",
        14 => "Smoke alarm",
        15 => "Package delivered",
        16 => "Package picked up",
        17 => "Missed ring",
        18 => "Answered ring",
        19 => "Anti-theft",
        20 => "Face",
        21 => "Unfamiliar face",
        22 => "Unfamiliar person",
        23..=26 | 30 => "Baby monitor",
        27 => "Face covered",
        28 => "Left safe zone",
        31 => "Panoramic",
        33 => "Animal",
        _ => return format!("Event {code}"),
    }
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Hd,
    /// The sub-stream; the Tapo app uses it for previews.
    Vga,
}

impl Resolution {
    fn as_str(self) -> &'static str {
        match self {
            Resolution::Hd => "HD",
            Resolution::Vga => "VGA",
        }
    }
}

/// Where and how to open one camera's video relay.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayInfo {
    pub host: String,
    pub port: u16,
    /// Path and signed query, kept byte for byte.
    pub path: String,
    pub token: String,
    pub player_id: String,
    pub elb_cookie: Option<String>,
}

impl fmt::Debug for RelayInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayInfo").field("host", &self.host).field("port", &self.port).finish_non_exhaustive()
    }
}

/// One TLS setup for every cloud connection: the operating system's trust
/// store plus TP-Link's two roots. Certificate and host-name checks stay on.
pub fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<std::result::Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| build_tls().map(Arc::new).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| CloudError::new(format!("Secure connection setup failed: {e}")))
}

/// The account API server to sign in at. Debug builds can point it at a local
/// fake cloud with `TAPAS_FAKE_CLOUD` (see `tools/fake_cloud.py`).
fn login_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("TAPAS_FAKE_CLOUD") {
        return url;
    }
    LOGIN_URL.to_string()
}

fn build_tls() -> anyhow::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    #[allow(unused_mut)]
    let mut roots = CertificateDer::pem_slice_iter(TP_LINK_ROOTS).collect::<std::result::Result<Vec<_>, _>>()?;
    // Debug builds only: trust the fake cloud's test CA.
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("TAPAS_FAKE_CLOUD_CA") {
        let pem = std::fs::read(path)?;
        roots.extend(CertificateDer::pem_slice_iter(&pem).collect::<std::result::Result<Vec<_>, _>>()?);
    }
    let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(roots, provider.clone())?;
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        // rustls calls any custom verifier "dangerous"; this one is the
        // standard platform check, with TP-Link's roots added.
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

pub struct CloudClient {
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    terminal_uuid: String,
    session: Mutex<Option<Session>>,
    /// Service URLs by (account API server, service id). Held during a
    /// lookup so streams starting together share one.
    services: tokio::sync::Mutex<HashMap<(String, String), String>>,
    /// Set when the camera-control gateway only has the v1 API.
    iot_v1: AtomicBool,
}

impl CloudClient {
    /// `terminal_uuid` identifies this PC to the cloud. Keep it stable so a
    /// verified sign-in is remembered.
    pub fn new(terminal_uuid: String) -> Result<Self> {
        let tls = tls_config()?;
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured((*tls).clone())
            .http1_title_case_headers()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| CloudError::new(format!("HTTP client setup failed: {e}")))?;
        Ok(Self {
            http,
            tls,
            terminal_uuid,
            session: Mutex::default(),
            services: Default::default(),
            iot_v1: AtomicBool::new(false),
        })
    }

    pub fn tls(&self) -> Arc<rustls::ClientConfig> {
        self.tls.clone()
    }

    pub fn session(&self) -> Option<Session> {
        self.session.lock().unwrap().clone()
    }

    /// Service URLs are cached per regional server, so they needn't be cleared.
    pub fn set_session(&self, session: Option<Session>) {
        *self.session.lock().unwrap() = session;
    }

    fn require_session(&self) -> Result<Session> {
        self.session().ok_or_else(|| CloudError::new("Not signed in to the Tapo cloud: sign in under Settings"))
    }

    // ------------------------------------------------------------------ HTTP

    async fn post_bytes(&self, url: &str, body: Vec<u8>, headers: Vec<(&'static str, String)>) -> Result<(u16, Vec<u8>)> {
        let mut request = self.http.post(url).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(network_error)?;
        Ok((status, bytes.to_vec()))
    }

    async fn post_raw(&self, url: &str, body: Vec<u8>, headers: Vec<(&'static str, String)>) -> Result<(u16, Value)> {
        let (status, bytes) = self.post_bytes(url, body, headers).await?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value @ Value::Object(_)) => Ok((status, value)),
            _ if status == 401 => Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED)),
            _ if status >= 400 => Err(CloudError::new(format!("The Tapo cloud refused the request (HTTP {status})"))),
            _ => Err(CloudError::new("Unexpected answer from the Tapo cloud")),
        }
    }

    /// A signed call to the account API.
    async fn post(&self, base_url: &str, endpoint: &str, payload: &Value, token: Option<&str>) -> Result<Value> {
        let body = serde_json::to_vec(payload).expect("JSON values always serialise");
        let mut url = format!("{}{endpoint}", base_url.trim_end_matches('/'));
        if let Some(token) = token {
            url = format!("{url}?token={}", url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>());
        }
        let headers = signed_headers(&body, endpoint, &new_uuid());
        Ok(self.post_raw(&url, body, headers).await?.1)
    }

    // ----------------------------------------------------------------- login

    fn login_payload(&self, username: &str, password: &str) -> Value {
        json!({
            "appType": APP_NAME,
            "appVersion": APP_VERSION,
            "cloudPassword": password,
            "cloudUserName": username,
            "platform": PLATFORM,
            "refreshTokenNeeded": false,
            "supportBindAccount": false,
            "terminalMeta": "1",
            "terminalName": TERMINAL_NAME,
            "terminalUUID": self.terminal_uuid,
        })
    }

    /// Signs in. On success the session is also kept in this client.
    pub async fn login(&self, username: &str, password: &str) -> Result<Login> {
        let payload = self.login_payload(username, password);
        let mut base_url = login_url();
        let mut response = self.post(&base_url, "/api/v2/account/login", &payload, None).await?;
        check_outer(&response, "Sign-in refused")?;
        let mut result = result_of(&response);
        // Accounts outside the default region are sent to their regional server.
        if error_code(&result) == WRONG_REGION {
            let regional = str_field(&result, "appServerUrl");
            if !regional.is_empty() {
                base_url = regional;
                response = self.post(&base_url, "/api/v2/account/login", &payload, None).await?;
                check_outer(&response, "Sign-in refused")?;
                result = result_of(&response);
            }
        }
        let token = str_field(&result, "token");
        if !token.is_empty() {
            let session = Session { base_url, token };
            self.set_session(Some(session.clone()));
            return Ok(Login::Done(session));
        }
        let process_id = str_field(&result, "MFAProcessId");
        if !process_id.is_empty() {
            let mut methods: Vec<MfaMethod> = result
                .get("supportedMFATypes")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(|v| int_value(v).and_then(MfaMethod::from_code)).collect())
                .unwrap_or_default();
            if methods.is_empty() {
                methods.push(MfaMethod::Email);
            }
            return Ok(Login::NeedsMfa(MfaChallenge { base_url, process_id, methods }));
        }
        match error_code(&result) {
            0 => Err(CloudError::new("The Tapo cloud did not return a session")),
            code => Err(CloudError::with_code(describe("Sign-in refused", code), code)),
        }
    }

    /// Asks the cloud to send a verification code.
    pub async fn request_mfa(
        &self,
        challenge: &MfaChallenge,
        username: &str,
        password: &str,
        method: MfaMethod,
    ) -> Result<()> {
        let endpoint = match method {
            MfaMethod::App => "/api/v2/account/getPushVC4TerminalMFA",
            MfaMethod::Email => "/api/v2/account/getEmailVC4TerminalMFA",
        };
        let payload = json!({
            "appType": APP_NAME,
            "cloudPassword": password,
            "cloudUserName": username,
            "terminalUUID": self.terminal_uuid,
        });
        let response = self.post(&challenge.base_url, endpoint, &payload, None).await?;
        check_both(&response, "Could not send the verification code")
    }

    /// Completes the sign-in with the code. This PC is then remembered, so
    /// later sign-ins normally don't ask for a code.
    pub async fn verify_mfa(
        &self,
        challenge: &MfaChallenge,
        username: &str,
        code: &str,
        method: MfaMethod,
    ) -> Result<Session> {
        let payload = json!({
            "appType": APP_NAME,
            "cloudUserName": username,
            "code": code,
            "MFAProcessId": challenge.process_id,
            "MFAType": method.code(),
            "terminalBindEnabled": true,
        });
        let response = self.post(&challenge.base_url, "/api/v2/account/checkMFACodeAndLogin", &payload, None).await?;
        check_both(&response, "Verification code rejected")?;
        let token = str_field(&result_of(&response), "token");
        if token.is_empty() {
            return Err(CloudError::new("The Tapo cloud did not return a session"));
        }
        let session = Session { base_url: challenge.base_url.clone(), token };
        self.set_session(Some(session.clone()));
        Ok(session)
    }

    // --------------------------------------------------------------- cameras

    pub async fn list_cameras(&self) -> Result<Vec<CloudCamera>> {
        self.list_devices(CAMERA_TYPE, "Could not load the cameras on your Tapo account").await
    }

    /// The camera hubs (H500 HomeBase, H200) on the account. Smart-home hubs
    /// such as the H100 are the same device type but store no video.
    pub async fn list_hubs(&self) -> Result<Vec<CloudCamera>> {
        let hubs = self.list_devices(HUB_TYPE, "Could not load the hubs on your Tapo account").await?;
        Ok(hubs.into_iter().filter(|h| is_camera_hub(&h.model)).collect())
    }

    async fn list_devices(&self, device_type: &str, what: &str) -> Result<Vec<CloudCamera>> {
        const PAGE: usize = 20;
        let session = self.require_session()?;
        let mut devices = Vec::new();
        for index in (0..200).step_by(PAGE) {
            let payload = json!({ "deviceTypeList": [device_type], "index": index, "limit": PAGE });
            let response = self
                .post(&session.base_url, "/api/v2/common/getDeviceListByPage", &payload, Some(&session.token))
                .await?;
            check_outer(&response, what)?;
            let result = result_of(&response);
            let Some(page) = result.get("deviceList").and_then(Value::as_array) else { break };
            devices.extend(page.iter().filter_map(camera_from).filter(|d| d.device_type.is_empty() || d.device_type == device_type));
            let total = result.get("totalNum").and_then(int_value).unwrap_or(i64::MAX);
            if page.len() < PAGE || devices.len() as i64 >= total {
                break;
            }
        }
        Ok(devices)
    }

    // ----------------------------------------------------------------- relay

    /// A service's URL for the session's region, e.g. the CIPC (camera video)
    /// service.
    async fn service_url(&self, session: &Session, service: &str) -> Result<String> {
        let mut cache = self.services.lock().await;
        let key = (session.base_url.clone(), service.to_string());
        if let Some(url) = cache.get(&key) {
            return Ok(url.clone());
        }
        let payload = json!({ "serviceIds": [service] });
        let response = self
            .post(&session.base_url, "/api/v2/common/getAppServiceUrl", &payload, Some(&session.token))
            .await?;
        check_outer(&response, "Service lookup refused")?;
        let url = result_of(&response).get("serviceUrls").map(|s| str_field(s, service)).unwrap_or_default();
        if url.is_empty() {
            return Err(CloudError::new(format!("The Tapo cloud did not return the address of {service}")));
        }
        let url = url.replacen("http://", "https://", 1);
        cache.insert(key, url.clone());
        Ok(url)
    }

    /// Looks the service up again next time, e.g. after a network error.
    async fn forget_service(&self, session: &Session, service: &str) {
        self.services.lock().await.remove(&(session.base_url.clone(), service.to_string()));
    }

    async fn post_cipc(&self, session: &Session, endpoint: &str, payload: &Value, player_id: &str) -> Result<Value> {
        let url = format!("{}{endpoint}", self.service_url(session, CIPC_SERVICE).await?.trim_end_matches('/'));
        let body = serde_json::to_vec(payload).expect("JSON values always serialise");
        let headers = vec![
            // CIPC takes the bare token (the account API's `ut|` prefix is not used).
            ("Authorization", session.token.clone()),
            ("Content-Type", "application/json; charset=UTF-8".into()),
            ("User-Agent", format!("Tapo_APP/Linux/{APP_VERSION}_Android/{PLATFORM}")),
            ("X-Client-Id", player_id.to_string()),
            ("X-Source", "tapo-app".into()),
            ("X-Brand", "TPLINK".into()),
            ("X-Ca-Type", "cloud-self".into()),
            // A per-request trace id, not a credential.
            ("X-Request-Signature", uuid::Uuid::new_v4().simple().to_string()[..10].to_string()),
        ];
        match self.post_raw(&url, body, headers).await {
            Ok((_, value)) => Ok(value),
            Err(e) => {
                self.forget_service(session, CIPC_SERVICE).await;
                Err(e)
            }
        }
    }

    /// Asks the cloud for a relay that streams this camera's live video.
    pub async fn request_relay(&self, camera: &CloudCamera, resolution: Resolution) -> Result<RelayInfo> {
        if camera.device_id.is_empty() {
            return Err(CloudError::new("This camera isn't linked to a camera on your Tapo account"));
        }
        let player_id = new_uuid();
        let device_type = if camera.device_type.is_empty() { CAMERA_TYPE } else { &camera.device_type };
        let payload = json!({
            "deviceId": camera.device_id,
            "dataTimeoutCount": 0,
            "cloudType": 2,
            "deviceType": device_type,
            "playerId": player_id,
            "rootCaVer": "1",
            "streamType": STREAM_PREVIEW,
            "preConnection": 0,
            "resolution": resolution.as_str(),
        });
        self.relay_request(&payload, player_id, "Camera").await
    }

    /// Asks the cloud for a relay that brings one of `camera`'s recordings
    /// from `hub`. Shaped like the Tapo app's: the relay goes to the hub, and
    /// `customParams` names the camera.
    pub async fn request_clip_relay(
        &self,
        hub: &CloudCamera,
        camera: &HubCamera,
        player_id: &str,
        stream: ClipStream,
    ) -> Result<RelayInfo> {
        let device_type = if hub.device_type.is_empty() { HUB_TYPE } else { &hub.device_type };
        let mut custom = serde_json::Map::new();
        if !camera.device_id.is_empty() {
            custom.insert("camera_device_id".into(), camera.device_id.clone().into());
        }
        if !camera.mac.is_empty() {
            custom.insert("camera_mac".into(), camera.mac.clone().into());
        }
        custom.insert("audio_config".into(), json!({ "encode_type": "G711ulaw", "sample_rate": "16" }));
        let mut payload = json!({
            "deviceId": hub.device_id,
            "dataTimeoutCount": 0,
            "cloudType": 2,
            "deviceType": device_type,
            "playerId": player_id,
            "rootCaVer": "1",
            "preConnection": 0,
            "customParams": Value::Object(custom).to_string(),
        });
        match stream {
            ClipStream::Download(sub_type) => {
                payload["streamType"] = STREAM_DOWNLOAD.into();
                payload["subType"] = sub_type.into();
            }
            ClipStream::Playback => payload["streamType"] = STREAM_PLAYBACK.into(),
        }
        log::info!("clip relay request: {}", redact_relay_request(&payload));
        self.relay_request(&payload, player_id.to_string(), "Hub").await
    }

    /// Asks the cloud for a relay session to download face profile JPEGs from
    /// an H500 hub (`STREAM_TYPE_IMAGE = 6`, `subType = "get_face"` in the Tapo app).
    pub async fn request_face_image_relay(&self, hub: &CloudCamera, player_id: &str) -> Result<RelayInfo> {
        let device_type = if hub.device_type.is_empty() { HUB_TYPE } else { &hub.device_type };
        let custom = json!({
            "image_type": "get_face",
            "image_format": "jpeg",
        });
        let payload = json!({
            "deviceId": hub.device_id,
            "dataTimeoutCount": 0,
            "cloudType": 2,
            "deviceType": device_type,
            "playerId": player_id,
            "rootCaVer": "1",
            "preConnection": 0,
            "streamType": STREAM_IMAGE,
            "subType": "get_face",
            "customParams": custom.to_string(),
        });
        log::info!("face image relay request: {}", redact_relay_request(&payload));
        self.relay_request(&payload, player_id.to_string(), "Hub").await
    }

    async fn relay_request(&self, payload: &Value, player_id: String, device: &str) -> Result<RelayInfo> {
        let session = self.require_session()?;
        let response = self.post_cipc(&session, "/v2/relay/request", payload, &player_id).await?;
        match error_code(&response) {
            0 => {}
            code @ (-40401 | -40406) => return Err(CloudError::with_code(format!("{device} is offline"), code)),
            AUTH_EXPIRED => return Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED)),
            code @ -64324 => return Err(CloudError::with_code("Privacy mode is on", code)),
            code => {
                log::warn!("relay request refused: {response}");
                return Err(CloudError::with_code(format!("The cloud relay refused the stream (code {code})"), code));
            }
        }
        let info = result_of(&response);
        let relay_url = str_field(&info, "relayUrl");
        let mut token = str_field(&info, "relayToken");
        if token.is_empty() {
            token = str_field(&info, "sessionToken");
        }
        if relay_url.is_empty() || token.is_empty() {
            return Err(CloudError::new("Incomplete relay answer from the Tapo cloud"));
        }
        let (host, port, path) = parse_relay_url(&relay_url)?;
        let elb_cookie = Some(str_field(&info, "elbCookie")).filter(|c| !c.is_empty());
        Ok(RelayInfo { host, port, path, token, player_id, elb_cookie })
    }

    // -------------------------------------------------------- camera control

    fn iot_headers(&self, session: &Session) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", format!("ut|{}", session.token)),
            ("app-cid", format!("app:{APP_NAME}:{}", self.terminal_uuid)),
            ("x-app-name", APP_NAME.into()),
            ("x-app-version", APP_VERSION.into()),
            ("x-term-id", self.terminal_uuid.clone()),
            ("x-ospf", PLATFORM.into()),
            ("x-net-type", "wifi".into()),
            ("x-strict", "0".into()),
            ("x-locale", "en_US".into()),
            ("Content-Type", "application/json; charset=UTF-8".into()),
            ("User-Agent", format!("{APP_NAME}/{APP_VERSION}(cloud-only;Windows)")),
        ]
    }

    /// Sends requests in the camera's own API (the same ones it accepts on the
    /// local network) through the cloud, as the Tapo app does for pan/tilt and
    /// settings. Returns one response per request.
    pub async fn camera_requests(&self, camera: &CloudCamera, requests: Vec<Value>) -> Result<Vec<Value>> {
        if camera.device_id.is_empty() {
            return Err(CloudError::new("This camera isn't linked to a camera on your Tapo account"));
        }
        let session = self.require_session()?;
        let gateway = self.service_url(&session, IOT_SERVICE).await?;
        let body = serde_json::to_vec(&json!({
            "inputParams": {
                "requestData": { "method": "multipleRequest", "params": { "requests": requests } },
            },
            "serviceId": "passthrough",
        }))
        .expect("JSON values always serialise");
        let device: String = url::form_urlencoded::byte_serialize(camera.device_id.as_bytes()).collect();
        let url = |version: &str| format!("{}/{version}/things/{device}/services-sync", gateway.trim_end_matches('/'));

        let v1 = self.iot_v1.load(Ordering::Relaxed);
        let sent = self.post_bytes(&url(if v1 { "v1" } else { "v2" }), body.clone(), self.iot_headers(&session)).await;
        let (mut status, mut bytes) = match sent {
            Ok(answer) => answer,
            Err(e) => {
                self.forget_service(&session, IOT_SERVICE).await;
                return Err(e);
            }
        };
        if !v1 && matches!(status, 404 | 405 | 501) {
            // Some regional gateways only have the older API.
            self.iot_v1.store(true, Ordering::Relaxed);
            (status, bytes) = self.post_bytes(&url("v1"), body, self.iot_headers(&session)).await?;
        }
        parse_camera_answer(status, &bytes)
    }

    /// Fetches a camera's capabilities and live toggle states (PTZ, privacy,
    /// spotlight, night vision, auto-tracking, manual alarm) in one batched
    /// cloud round trip.
    pub async fn camera_controls(&self, camera: &CloudCamera) -> Result<CameraControls> {
        let requests = vec![
            json!({
                "method": "getAppComponentList",
                "params": { "app_component": { "name": ["app_component_list"] } },
            }),
            json!({ "method": "getLensMaskConfig", "params": { "lens_mask": { "name": ["lens_mask_info"] } } }),
            json!({ "method": "getForceWhitelampState", "params": { "image": { "name": ["switch"] } } }),
            json!({ "method": "getNightVisionModeConfig", "params": { "image": { "name": ["switch"] } } }),
            json!({ "method": "getDayNightModeConfig", "params": { "image": { "name": ["common"] } } }),
            json!({ "method": "getTargetTrackConfig", "params": { "target_track": { "name": ["target_track_info"] } } }),
            json!({ "method": "getManualAlarmStatus", "params": { "msg_alarm": { "get_manual_status": {} } } }),
        ];
        let responses = self.camera_requests(camera, requests).await?;
        Ok(parse_camera_controls(&responses))
    }

    pub async fn privacy_mode(&self, camera: &CloudCamera) -> Result<bool> {
        let request = json!({ "method": "getLensMaskConfig", "params": { "lens_mask": { "name": ["lens_mask_info"] } } });
        let result = first_result(&self.camera_requests(camera, vec![request]).await?, "getLensMaskConfig")?;
        parse_lens_mask(&result).ok_or_else(|| CloudError::new("The camera didn't say whether privacy mode is on"))
    }

    /// Turns privacy mode on or off and waits until the camera reports it.
    pub async fn set_privacy_mode(&self, camera: &CloudCamera, on: bool) -> Result<bool> {
        let value = if on { "on" } else { "off" };
        let request = json!({
            "method": "setLensMaskConfig",
            "params": { "lens_mask": { "lens_mask_info": { "enabled": value } } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "setLensMaskConfig")?;
        // The camera applies it asynchronously.
        let mut last_error = None;
        for delay_ms in [0, 400, 800, 1200, 1600] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            match self.privacy_mode(camera).await {
                Ok(state) if state == on => return Ok(state),
                Ok(_) => last_error = None,
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| CloudError::new("Privacy mode didn't change; try again")))
    }

    /// Turns the camera's white spotlight on or off (falls back to
    /// `reverseWhitelampStatus` on firmware that toggles instead of setting).
    pub async fn set_spotlight(&self, camera: &CloudCamera, on: bool) -> Result<bool> {
        let value = if on { "on" } else { "off" };
        let request = json!({
            "method": "setForceWhitelampState",
            "params": { "image": { "switch": { "force_wtl_state": value } } },
        });
        if first_result(&self.camera_requests(camera, vec![request]).await?, "setForceWhitelampState").is_ok() {
            return Ok(on);
        }
        let toggle = json!({
            "method": "reverseWhitelampStatus",
            "params": { "image": { "reverse_wtl_status": {} } },
        });
        first_result(&self.camera_requests(camera, vec![toggle]).await?, "reverseWhitelampStatus")?;
        Ok(on)
    }

    /// Changes the camera's day/night and night-vision mode.
    pub async fn set_night_vision(&self, camera: &CloudCamera, mode: NightVisionMode) -> Result<NightVisionMode> {
        let requests = match mode {
            NightVisionMode::Auto => vec![json!({
                "method": "setDayNightModeConfig",
                "params": { "image": { "common": { "inf_type": "auto" } } },
            })],
            NightVisionMode::Day => vec![json!({
                "method": "setDayNightModeConfig",
                "params": { "image": { "common": { "inf_type": "off" } } },
            })],
            NightVisionMode::Infrared => vec![
                json!({
                    "method": "setNightVisionModeConfig",
                    "params": { "image": { "switch": { "night_vision_mode": "inf_night_vision" } } },
                }),
                json!({
                    "method": "setDayNightModeConfig",
                    "params": { "image": { "common": { "inf_type": "on" } } },
                }),
            ],
            NightVisionMode::Color => vec![
                json!({
                    "method": "setNightVisionModeConfig",
                    "params": { "image": { "switch": { "night_vision_mode": "wtl_night_vision" } } },
                }),
                json!({
                    "method": "setDayNightModeConfig",
                    "params": { "image": { "common": { "inf_type": "on" } } },
                }),
            ],
            NightVisionMode::Smart => vec![
                json!({
                    "method": "setNightVisionModeConfig",
                    "params": { "image": { "switch": { "night_vision_mode": "md_night_vision" } } },
                }),
                json!({
                    "method": "setDayNightModeConfig",
                    "params": { "image": { "common": { "inf_type": "auto" } } },
                }),
            ],
        };
        let responses = self.camera_requests(camera, requests).await?;
        let any_ok = responses.iter().any(|r| r.get("error_code").and_then(Value::as_i64).unwrap_or(0) == 0);
        if !any_ok {
            let first = responses.first().cloned().unwrap_or(Value::Null);
            let code = first.get("error_code").and_then(Value::as_i64).unwrap_or(-1);
            return Err(CloudError::new(format!("The camera refused the night vision change (code {code})")));
        }
        Ok(mode)
    }

    /// Enables or disables pan/tilt motion auto-tracking.
    pub async fn set_auto_track(&self, camera: &CloudCamera, on: bool) -> Result<bool> {
        let value = if on { "on" } else { "off" };
        let request = json!({
            "method": "setTargetTrackConfig",
            "params": { "target_track": { "target_track_info": { "enabled": value } } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "setTargetTrackConfig")?;
        Ok(on)
    }

    /// Starts horizontal or vertical motor patrol (cruise).
    pub async fn start_patrol(&self, camera: &CloudCamera, axis: PatrolAxis) -> Result<()> {
        let request = json!({
            "method": "cruiseMove",
            "params": { "motor": { "cruise": { "coord": axis.coord() } } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "cruiseMove").map(drop)
    }

    /// Stops any active motor patrol (cruise).
    pub async fn stop_patrol(&self, camera: &CloudCamera) -> Result<()> {
        let request = json!({
            "method": "cruiseStop",
            "params": { "motor": { "cruise_stop": {} } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "cruiseStop").map(drop)
    }

    /// Starts or stops the camera's manual siren/alarm.
    pub async fn set_camera_alarm(&self, camera: &CloudCamera, on: bool) -> Result<bool> {
        let action = if on { "start" } else { "stop" };
        let manual = json!({
            "method": "doManualAlarm",
            "params": { "msg_alarm": { "manual_alarm": { "action": action } } },
        });
        if first_result(&self.camera_requests(camera, vec![manual]).await?, "doManualAlarm").is_ok() {
            return Ok(on);
        }
        let status = if on { "on" } else { "off" };
        let siren = json!({
            "method": "setSirenStatus",
            "params": { "siren": { "status": status } },
        });
        first_result(&self.camera_requests(camera, vec![siren]).await?, "setSirenStatus")?;
        Ok(on)
    }

    /// Reboots a camera or hub.
    pub async fn reboot_device(&self, device: &CloudCamera) -> Result<()> {
        let request = json!({
            "method": "rebootDevice",
            "params": { "system": { "reboot": { "delay": 1 } } },
        });
        first_result(&self.camera_requests(device, vec![request]).await?, "rebootDevice").map(drop)
    }

    /// Moves the camera relative to where it is: `x > 0` pans right, `y > 0`
    /// tilts up.
    pub async fn move_motor(&self, camera: &CloudCamera, x: i32, y: i32) -> Result<()> {
        let (x, y) = (x.clamp(-MAX_MOVE, MAX_MOVE), y.clamp(-MAX_MOVE, MAX_MOVE));
        let request = json!({
            "method": "motorMove",
            "params": { "motor": { "move": { "x_coord": x.to_string(), "y_coord": y.to_string() } } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "motorMove").map(drop)
    }

    /// The saved positions, as (id, name).
    pub async fn presets(&self, camera: &CloudCamera) -> Result<Vec<(String, String)>> {
        let request = json!({ "method": "getPresetConfig", "params": { "preset": { "name": ["preset"] } } });
        let result = first_result(&self.camera_requests(camera, vec![request]).await?, "getPresetConfig")?;
        Ok(parse_presets(&result))
    }

    pub async fn goto_preset(&self, camera: &CloudCamera, id: &str) -> Result<()> {
        let request = json!({ "method": "motorMoveToPreset", "params": { "preset": { "goto_preset": { "id": id } } } });
        first_result(&self.camera_requests(camera, vec![request]).await?, "motorMoveToPreset").map(drop)
    }

    /// Saves the current position under `name`.
    pub async fn save_preset(&self, camera: &CloudCamera, name: &str) -> Result<()> {
        // "Postion" is the API's spelling.
        let request = json!({ "method": "addMotorPostion", "params": { "preset": { "set_preset": { "name": name } } } });
        first_result(&self.camera_requests(camera, vec![request]).await?, "addMotorPostion").map(drop)
    }

    /// Deletes a saved pan/tilt preset by id.
    pub async fn delete_preset(&self, camera: &CloudCamera, id: &str) -> Result<()> {
        let request = json!({
            "method": "deletePreset",
            "params": { "preset": { "remove_preset": { "id": [id] } } },
        });
        first_result(&self.camera_requests(camera, vec![request]).await?, "deletePreset").map(drop)
    }

    // ------------------------------------------------------------------ hubs

    /// The cameras paired with `hub`, from the hub itself (through the cloud).
    pub async fn hub_cameras(&self, hub: &CloudCamera) -> Result<Vec<HubCamera>> {
        let request = json!({
            "method": "getGeneralDeviceList",
            "params": { "general_camera_manage": { "paired_general_device_list": {} } },
        });
        let result = first_result(&self.camera_requests(hub, vec![request]).await?, "getGeneralDeviceList")?;
        Ok(parse_hub_cameras(&result))
    }

    /// The days from `from` to `to` (the hub's calendar) on which the hub
    /// recorded the camera.
    pub async fn hub_recording_days(
        &self,
        hub: &CloudCamera,
        camera_id: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<NaiveDate>> {
        let request = json!({
            "method": "searchDateWithVideo",
            "params": { "playback": { "search_year_utility": {
                "channel": [0],
                "start_date": from.format("%Y%m%d").to_string(),
                "end_date": to.format("%Y%m%d").to_string(),
                "child_device_id": camera_id,
            } } },
        });
        let result = first_result(&self.camera_requests(hub, vec![request]).await?, "searchDateWithVideo")?;
        let days = parse_recording_days(&result);
        if days.is_empty() {
            log::info!("searchDateWithVideo {from}..{to} found no days; the hub said {result}");
        }
        Ok(days)
    }

    /// What the camera detected between `start` and `end` (Unix seconds, at
    /// most a day), oldest first: the hub's event search, naming the camera
    /// by id and MAC address as the Tapo app does (an H500 finds nothing
    /// without the MAC).
    pub async fn hub_detections(&self, hub: &CloudCamera, camera: &HubCamera, start: i64, end: i64) -> Result<Vec<Detection>> {
        const PAGE: i64 = 100;
        let mut found = Vec::new();
        let mut index = 0;
        while index <= 12288 {
            let request = json!({
                "method": "searchDetectionList",
                "params": { "playback": { "search_detection_list": {
                    "channel": 0,
                    "start_time": start,
                    "end_time": end,
                    "start_index": index,
                    "end_index": index + PAGE - 1,
                    "child_device_id": camera.device_id,
                    "child_device_mac": camera.mac,
                } } },
            });
            let responses = self.camera_requests(hub, vec![request]).await?;
            let result = match first_result(&responses, "searchDetectionList") {
                Ok(r) => r,
                Err(e) => {
                    log::info!("searchDetectionList: {e}; the hub said {}", Value::Array(responses));
                    return Err(e);
                }
            };
            let page = parse_detections(&result);
            if index == 0 && page.is_empty() {
                log::info!("searchDetectionList found nothing; the hub said {result}");
            }
            let more = result.pointer("/playback/to_be_continued").and_then(int_value) == Some(1)
                || page.len() as i64 >= PAGE;
            found.extend(page);
            if !more {
                break;
            }
            index += PAGE;
        }
        found.sort_by_key(|d| d.start);
        found.dedup();
        Ok(found)
    }

    /// The hub's firmware, address and storage, asked of the hub itself.
    pub async fn hub_info(&self, hub: &CloudCamera) -> Result<HubInfo> {
        let requests = vec![
            json!({ "method": "getDeviceInfo", "params": { "device_info": { "name": ["basic_info"] } } }),
            json!({ "method": "getSdCardStatus", "params": { "harddisk_manage": { "table": ["hd_info"] } } }),
            // A hub with a hard disk (an H500) reports it here, as the Tapo
            // app asks; getSdCardStatus has only its built-in storage.
            json!({ "method": "getHardDiskStatus", "params": { "harddisk_manage": { "table": ["hd_info"] } } }),
            json!({ "method": "getCircularRecordingConfig", "params": { "harddisk_manage": { "name": ["harddisk"] } } }),
        ];
        let responses = self.camera_requests(hub, requests).await?;
        // An H500 doesn't answer in the order asked: match by method.
        let info = result_for(&responses, "getDeviceInfo", 0).unwrap_or_default();
        let disks: Vec<Value> = [("getHardDiskStatus", 2), ("getSdCardStatus", 1)]
            .iter()
            .filter_map(|(method, i)| result_for(&responses, method, *i).ok())
            .collect();
        let looping = result_for(&responses, "getCircularRecordingConfig", 3).unwrap_or_default();
        let mut parsed = parse_hub_info(&info, &disks);
        parsed.loop_recording = match looping.pointer("/harddisk_manage/harddisk/loop").and_then(Value::as_str) {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        };
        log::info!("hub {}: {parsed:?}", hub.name);
        let storage: Vec<&Value> = responses
            .iter()
            .filter(|r| r.get("method").and_then(Value::as_str).is_some_and(|m| m != "getDeviceInfo"))
            .collect();
        let answer = serde_json::to_string(&storage).unwrap_or_default();
        log::info!("hub {}: its storage answers: {}", hub.name, answer.chars().take(3000).collect::<String>());
        Ok(parsed)
    }

    /// Enables or disables circular (loop) recording on a hub's storage.
    pub async fn set_loop_recording(&self, hub: &CloudCamera, on: bool) -> Result<bool> {
        let value = if on { "on" } else { "off" };
        let request = json!({
            "method": "setCircularRecordingConfig",
            "params": { "harddisk_manage": { "harddisk": { "loop": value } } },
        });
        first_result(&self.camera_requests(hub, vec![request]).await?, "setCircularRecordingConfig")?;
        Ok(on)
    }

    /// Triggers or silences the hub's built-in alarm/siren (tries the H500
    /// camera-hub siren API first, falling back to the IoT hub `play_alarm` /
    /// `stop_alarm` method).
    pub async fn set_hub_alarm(&self, hub: &CloudCamera, on: bool) -> Result<bool> {
        let status = if on { "on" } else { "off" };
        let siren = json!({
            "method": "setSirenStatus",
            "params": { "siren": { "status": status } },
        });
        if first_result(&self.camera_requests(hub, vec![siren]).await?, "setSirenStatus").is_ok() {
            return Ok(on);
        }
        let hub_ring = json!({
            "method": "setHubRingStatus",
            "params": { "hub_ring": { "config": { "status": status } } },
        });
        if first_result(&self.camera_requests(hub, vec![hub_ring]).await?, "setHubRingStatus").is_ok() {
            return Ok(on);
        }
        let method = if on { "play_alarm" } else { "stop_alarm" };
        let fallback = json!({ "method": method, "params": {} });
        first_result(&self.camera_requests(hub, vec![fallback]).await?, method)?;
        Ok(on)
    }

    /// The account's homes and rooms (as in the Tapo app), from the IoT
    /// service: `/v1/families`, then each home's `/rooms`.
    pub async fn homes(&self) -> Result<Homes> {
        let families = self.iot_get("/v1/families").await?;
        log::info!("homes on the account: {families}");
        let mut homes = Homes::default();
        let mut ids = Vec::new();
        for family in objects_with(&families, &["familyId", "id"]) {
            if let Some(name) = name_of(family, &["familyName", "name"]) {
                homes.names.push(name);
            }
            if let Some(id) = ["familyId", "id"].iter().map(|k| str_field(family, k)).find(|v| !v.is_empty()) {
                ids.push(id);
            }
        }
        let escape = |id: &str| -> String { url::form_urlencoded::byte_serialize(id.as_bytes()).collect() };
        // Room ids and names; the rooms don't list their devices, but each
        // room's "thing order" does (the order the Tapo app shows them in).
        let mut all_rooms: Vec<String> = Vec::new();
        for family in &ids {
            let rooms = match self.iot_get(&format!("/v1/families/{}/rooms", escape(family))).await {
                Ok(rooms) => rooms,
                Err(e) => {
                    log::info!("no rooms for a home: {e}");
                    continue;
                }
            };
            log::info!("rooms: {rooms}");
            homes.rooms.extend(parse_rooms(&rooms));
            for (room, name) in room_list(&rooms) {
                all_rooms.push(name.clone());
                let path = format!("/v1/families/{}/rooms/{}/thing-order", escape(family), escape(&room));
                match self.iot_get(&path).await {
                    Ok(things) => {
                        log::info!("devices in {name}: {things}");
                        for device in device_ids(&things) {
                            homes.rooms.insert(device, name.clone());
                        }
                    }
                    Err(e) => log::info!("no device list for {name}: {e}"),
                }
            }
        }
        if let [only] = all_rooms.as_slice() {
            homes.only_room = Some(only.clone());
        }
        Ok(homes)
    }

    async fn iot_get(&self, path: &str) -> Result<Value> {
        let session = self.require_session()?;
        let gateway = self.service_url(&session, IOT_SERVICE).await?;
        let url = format!("{}{path}", gateway.trim_end_matches('/'));
        let mut request = self.http.get(&url);
        for (name, value) in self.iot_headers(&session) {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(network_error)?;
        if status == 401 {
            return Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| CloudError::new(format!("Unexpected answer from the Tapo cloud (HTTP {status})")))?;
        if status >= 400 {
            return Err(CloudError::new(format!("The Tapo cloud refused {path} (HTTP {status}): {value}")));
        }
        Ok(value)
    }

    /// The id the hub knows this account by, which its playback requests
    /// carry as `client_id`.
    pub async fn hub_user_id(&self, hub: &CloudCamera) -> Result<i64> {
        let request = json!({ "method": "getUserID", "params": { "system": { "get_user_id": "null" } } });
        let result = first_result(&self.camera_requests(hub, vec![request]).await?, "getUserID")?;
        result
            .get("user_id")
            .and_then(int_value)
            .ok_or_else(|| CloudError::new(format!("The hub gave no user id: {result}")))
    }

    /// The camera's recordings that overlap `start`..`end` (Unix seconds),
    /// oldest first. `player_id` must be the one the clips are downloaded with.
    pub async fn hub_clips(
        &self,
        hub: &CloudCamera,
        camera_id: &str,
        start: i64,
        end: i64,
        player_id: &str,
    ) -> Result<Vec<Clip>> {
        // Like the Tapo app: pages of 100, up to a start index of 12288.
        const PAGE: i64 = 100;
        let mut clips = Vec::new();
        let mut index = 0;
        while index <= 12288 {
            let request = json!({
                "method": "searchVideoWithUTC",
                "params": { "playback": { "search_video_with_utc": {
                    "channel": 0,
                    "start_time": start,
                    "end_time": end,
                    "start_index": index,
                    "end_index": index + PAGE - 1,
                    "child_device_id": camera_id,
                    "player_id": player_id,
                } } },
            });
            let result = first_result(&self.camera_requests(hub, vec![request]).await?, "searchVideoWithUTC")?;
            let page = parse_clips(&result);
            let full = page.len() as i64 >= PAGE;
            clips.extend(page);
            if !full {
                break;
            }
            index += PAGE;
        }
        clips.sort_by_key(|c| (c.start, c.end));
        clips.dedup();
        Ok(clips)
    }

    // --------------------------------------------------- hub face recognition

    /// Lists all Familiar & Stranger face profiles stored on the H500 HomeBase.
    /// Matches the Tapo app's `CameraHubFaceDetectionRepository`: fetches
    /// Familiars (`tag: ["stranger"], strategy: "reject"`) and Strangers
    /// (`tag: ["stranger"], strategy: "select"`) in one batched hub call,
    /// falling back to an unfiltered `"select"` query if both come back empty.
    pub async fn hub_faces(&self, hub: &CloudCamera) -> Result<Vec<FaceInfo>> {
        let requests = vec![
            search_faces_request(&["stranger"], "reject", 48),
            search_faces_request(&["stranger"], "select", 48),
        ];
        let responses = self.camera_requests(hub, requests).await?;
        let mut faces = Vec::new();
        for (i, resp) in responses.iter().enumerate() {
            let label = if i == 0 { "familiar" } else { "stranger" };
            match first_result(std::slice::from_ref(resp), "searchFacesInfo") {
                Ok(res) => {
                    let raw = serde_json::to_string(&res).unwrap_or_default();
                    log::info!(
                        "hub {} faces ({label}): {}",
                        hub.name,
                        raw.chars().take(1200).collect::<String>()
                    );
                    faces.extend(parse_faces_info(&res));
                }
                Err(e) if i == 0 => return Err(e),
                Err(e) => log::info!("hub {} faces ({label}) failed: {e}", hub.name),
            }
        }
        if faces.is_empty() {
            let fallback = search_faces_request(&[], "select", 48);
            if let Ok(res) = first_result(&self.camera_requests(hub, vec![fallback]).await?, "searchFacesInfo") {
                let raw = serde_json::to_string(&res).unwrap_or_default();
                log::info!("hub {} faces (all fallback): {}", hub.name, raw.chars().take(1200).collect::<String>());
                faces.extend(parse_faces_info(&res));
            }
        }
        sort_and_dedup_faces(&mut faces);
        Ok(faces)
    }

    /// Renames a recognized face and marks it as Familiar or Stranger on the H500.
    pub async fn set_face_info(&self, hub: &CloudCamera, face_id: i64, name: &str, familiar: bool) -> Result<()> {
        let request = set_face_info_request(face_id, name, familiar);
        first_result(&self.camera_requests(hub, vec![request]).await?, "setFaceInfo")?;
        Ok(())
    }

    /// Cross-camera face tracking trajectories overlapping `start..=end` (Unix seconds),
    /// newest first. Optionally narrowed to one `face_id` or `tag` (`"familiar"` / `"stranger"`).
    pub async fn hub_face_tracking(
        &self,
        hub: &CloudCamera,
        start: i64,
        end: i64,
        face_id: Option<i64>,
        tag: Option<&str>,
    ) -> Result<Vec<FaceTrackingEntry>> {
        const PAGE: i32 = 50;
        let mut entries = Vec::new();
        let mut index = 0;
        while index < 250 {
            let request = search_face_tracking_request(start, end, index, index + PAGE - 1, face_id, tag);
            let result = first_result(&self.camera_requests(hub, vec![request]).await?, "doSearchFaceTrackingList")?;
            if index == 0 {
                let raw = serde_json::to_string(&result).unwrap_or_default();
                log::info!("hub {} face tracking: {}", hub.name, raw.chars().take(1200).collect::<String>());
            }
            let page = parse_face_tracking_list(&result);
            let full = page.len() as i32 >= PAGE;
            entries.extend(page);
            if !full {
                break;
            }
            index += PAGE;
        }
        entries.sort_by(|a, b| b.start_time.cmp(&a.start_time).then_with(|| b.end_time.cmp(&a.end_time)));
        entries.dedup();
        Ok(entries)
    }

    /// Face-filtered recordings (`doSearchFaceDetectionVideo` / `SearchVideoWithFilterRequest`)
    /// across `camera_macs` for each face in `face_ids`, newest first. Works on H500 hubs even
    /// when multi-camera Face Tracking is turned off.
    pub async fn hub_face_videos(
        &self,
        hub: &CloudCamera,
        start: i64,
        end: i64,
        face_ids: &[i64],
        camera_macs: &[String],
    ) -> Result<Vec<FaceTrackingEntry>> {
        if face_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for chunk in face_ids.chunks(8) {
            let requests: Vec<Value> = chunk
                .iter()
                .map(|&fid| search_face_videos_request(fid, camera_macs, start, end, 0, 49))
                .collect();
            let responses = self.camera_requests(hub, requests).await?;
            for (i, (&fid, resp)) in chunk.iter().zip(responses.iter()).enumerate() {
                if let Ok(res) = first_result(std::slice::from_ref(resp), "doSearchFaceDetectionVideo") {
                    if entries.is_empty() && i == 0 {
                        let raw = serde_json::to_string(&res).unwrap_or_default();
                        log::info!(
                            "hub {} face videos (face {fid}): {}",
                            hub.name,
                            raw.chars().take(1200).collect::<String>()
                        );
                    }
                    entries.extend(parse_face_videos(&res, fid));
                }
            }
        }
        entries.sort_by(|a, b| b.start_time.cmp(&a.start_time).then_with(|| b.end_time.cmp(&a.end_time)));
        entries.dedup();
        Ok(entries)
    }

    /// Detailed per-camera clips inside a single cross-camera face trajectory.
    #[allow(dead_code)]
    pub async fn hub_face_tracking_clips(
        &self,
        hub: &CloudCamera,
        face_id: i64,
        start_time: i64,
    ) -> Result<Vec<FaceTrackingClip>> {
        let request = search_face_tracking_clips_request(face_id, start_time, 0, 49);
        let result = first_result(&self.camera_requests(hub, vec![request]).await?, "doSearchFaceTrackingClips")?;
        Ok(parse_face_tracking_clips(&result))
    }
}


/// Unwraps the gateway's answer to [`CloudClient::camera_requests`].
fn parse_camera_answer(status: u16, bytes: &[u8]) -> Result<Vec<Value>> {
    let value: Option<Value> = serde_json::from_slice(bytes).ok();
    if status == 401 {
        return Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED));
    }
    if status >= 400 {
        let detail = value
            .as_ref()
            .and_then(|v| v.get("message").or_else(|| v.get("msg")))
            .and_then(Value::as_str)
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        return Err(CloudError::new(format!("Camera control refused (HTTP {status}){detail}")));
    }
    let data = value
        .as_ref()
        .and_then(|v| v.pointer("/outputParams/responseData"))
        .filter(|d| d.is_object())
        .ok_or_else(|| CloudError::new("The camera did not answer"))?;
    match error_code(data) {
        0 => {}
        AUTH_EXPIRED => return Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED)),
        code @ (-40401 | -40406) => return Err(CloudError::with_code("Camera is offline", code)),
        code => return Err(CloudError::with_code(device_error("Camera control refused", code), code)),
    }
    data.pointer("/result/responses")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| CloudError::new("The camera returned no answer"))
}

/// The result of the first response, or the camera's error.
/// The result of `method` in a multiple request's answers: the answer that
/// names it, else the one at `index`.
fn result_for(responses: &[Value], method: &str, index: usize) -> Result<Value> {
    let named = responses.iter().position(|r| r.get("method").and_then(Value::as_str) == Some(method));
    first_result(responses.get(named.unwrap_or(index)..).unwrap_or_default(), method)
}

fn first_result(responses: &[Value], method: &str) -> Result<Value> {
    let response = responses.first().ok_or_else(|| CloudError::new(format!("No answer to {method}")))?;
    match error_code(response) {
        0 => Ok(response.get("result").cloned().unwrap_or_else(|| json!({}))),
        code => Err(CloudError::with_code(device_error(&format!("The camera refused {method}"), code), code)),
    }
}

fn device_error(what: &str, code: i64) -> String {
    match code {
        -64303 => "Motor busy: wait for the movement to finish".into(),
        -64304 => "Reached the end of the pan/tilt range".into(),
        -64324 => "Privacy mode is on".into(),
        -64336 => "Movement blocked by a privacy zone".into(),
        _ => format!("{what} (code {code})"),
    }
}

/// `lens_mask.lens_mask_info.enabled` is "on" or "off".
fn parse_lens_mask(result: &Value) -> Option<bool> {
    match result.pointer("/lens_mask/lens_mask_info/enabled").and_then(Value::as_str) {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    }
}

/// Zips the parallel `id`/`name` arrays of `getPresetConfig`.
fn parse_presets(result: &Value) -> Vec<(String, String)> {
    let section = result.pointer("/preset/preset").unwrap_or(&Value::Null);
    let list = |key: &str| -> Vec<String> {
        section
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect())
            .unwrap_or_default()
    };
    list("id").into_iter().zip(list("name")).collect()
}

/// `general_camera_manage.paired_general_device_list` of `getGeneralDeviceList`.
fn parse_hub_cameras(result: &Value) -> Vec<HubCamera> {
    let list = result.pointer("/general_camera_manage/paired_general_device_list").and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|c| {
            let device_id = str_field(c, "device_id");
            if device_id.is_empty() {
                return None;
            }
            let name = Some(str_field(c, "alias").trim().to_string()).filter(|n| !n.is_empty());
            let model = str_field(c, "device_model");
            Some(HubCamera {
                name: name.unwrap_or_else(|| if model.is_empty() { "Camera".into() } else { model.clone() }),
                device_id,
                model,
                mac: str_field(c, "mac"),
                hub_storage: c.get("hub_storage_enabled").is_none_or(|v| v.as_bool().unwrap_or(true)),
            })
        })
        .collect()
}

/// The hub wraps every list entry in an object of its own:
/// `[{"search_results_1": {...}}, {"search_results_2": {...}}]`.
fn section_entries(list: Option<&Value>) -> impl Iterator<Item = &Value> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .flat_map(|o| o.values())
}

/// `playback.search_results` of `searchDateWithVideo`: days as `YYYYMMDD`.
fn parse_recording_days(result: &Value) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = section_entries(result.pointer("/playback/search_results"))
        .filter_map(|e| NaiveDate::parse_from_str(&str_field(e, "date"), "%Y%m%d").ok())
        .collect();
    days.sort();
    days.dedup();
    days
}

/// `playback.search_video_results` of `searchVideoWithUTC`.
fn parse_clips(result: &Value) -> Vec<Clip> {
    let time = |e: &Value, keys: [&str; 2]| keys.iter().find_map(|k| e.get(*k).and_then(int_value));
    section_entries(result.pointer("/playback/search_video_results"))
        .filter_map(|e| {
            let start = time(e, ["startTime", "start_time"])?;
            let end = time(e, ["endTime", "end_time"])?;
            let kind = e.get("video_type").and_then(int_value).unwrap_or(0);
            (end > start).then_some(Clip { start, end, kind })
        })
        .collect()
}

/// `playback.search_detection_list` of `searchDetectionList`: a plain list,
/// or wrapped like the other hub lists.
fn parse_detections(result: &Value) -> Vec<Detection> {
    let list = result.pointer("/playback/search_detection_list");
    let plain = list.and_then(Value::as_array).into_iter().flatten().filter(|e| e.get("start_time").is_some());
    let wrapped = section_entries(list).filter(|e| e.get("start_time").is_some());
    plain
        .chain(wrapped)
        .filter_map(|e| {
            let start = e.get("start_time").and_then(int_value)?;
            let end = e.get("end_time").and_then(int_value).unwrap_or(start);
            let kind = e.get("alarm_type").and_then(int_value).unwrap_or(0);
            let mask = e.get("events_1").and_then(int_value).unwrap_or(0);
            let kinds = (0..63).filter(|bit| mask & (1 << bit) != 0).map(|bit| bit + 1).collect();
            Some(Detection { start, end: end.max(start), kind, kinds })
        })
        .collect()
}

fn search_faces_request(tags: &[&str], strategy: &str, limit: i32) -> Value {
    json!({
        "method": "searchFacesInfo",
        "params": {
            "face_detection": {
                "search_faces_info": {
                    "update_time": 0,
                    "face_id": [],
                    "tag": tags,
                    "strategy": strategy,
                    "sort_by": "fresh_time",
                    "trans_limit": limit,
                }
            }
        }
    })
}

fn set_face_info_request(face_id: i64, name: &str, familiar: bool) -> Value {
    let tag = if familiar { FaceTag::Familiar } else { FaceTag::Stranger }.as_str();
    json!({
        "method": "setFaceInfo",
        "params": {
            "face_detection": {
                "set_face_info": {
                    "commands": [{
                        "operate": "set",
                        "ori_face_id": face_id,
                        "cover": {
                            "face_id": face_id,
                            "name": name.trim(),
                            "tag": tag,
                        }
                    }]
                }
            }
        }
    })
}

fn search_face_tracking_request(
    start: i64,
    end: i64,
    start_index: i32,
    end_index: i32,
    face_id: Option<i64>,
    tag: Option<&str>,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("start_time".into(), json!(start));
    body.insert("end_time".into(), json!(end));
    body.insert("start_index".into(), json!(start_index));
    body.insert("end_index".into(), json!(end_index));
    body.insert("mode".into(), json!("detailed"));
    body.insert("order".into(), json!("reverse"));
    body.insert("version".into(), json!(2));
    if let Some(id) = face_id {
        body.insert("face_id".into(), json!([id]));
    } else {
        body.insert("face_id".into(), json!([]));
    }
    if let Some(t) = tag.filter(|t| !t.is_empty()) {
        body.insert("tag".into(), json!([t]));
    } else if face_id.is_none() {
        body.insert("tag".into(), json!(["familiar", "stranger"]));
    }
    json!({
        "method": "doSearchFaceTrackingList",
        "params": {
            "face_detection": {
                "action_search_face_tracking_list": Value::Object(body),
            }
        }
    })
}

/// Builds `doSearchFaceDetectionVideo` (`SearchVideoWithFilterRequest` in the Tapo app).
fn search_face_videos_request(
    face_id: i64,
    camera_macs: &[String],
    start: i64,
    end: i64,
    start_index: i32,
    end_index: i32,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("id".into(), json!(1));
    body.insert("face_id".into(), json!(face_id));
    body.insert("start_time".into(), json!(start));
    body.insert("end_time".into(), json!(end));
    body.insert("start_index".into(), json!(start_index));
    body.insert("end_index".into(), json!(end_index));
    if !camera_macs.is_empty() {
        body.insert("mac".into(), json!(camera_macs));
    }
    json!({
        "method": "doSearchFaceDetectionVideo",
        "params": {
            "playback": {
                "action_search_face_detection_video": Value::Object(body),
            }
        }
    })
}

fn search_face_tracking_clips_request(face_id: i64, start_time: i64, start_index: i32, end_index: i32) -> Value {
    json!({
        "method": "doSearchFaceTrackingClips",
        "params": {
            "face_detection": {
                "action_search_face_tracking_clips": {
                    "face_id": face_id,
                    "start_time": start_time,
                    "start_index": start_index,
                    "end_index": end_index,
                    "version": 2,
                }
            }
        }
    })
}

fn sort_and_dedup_faces(faces: &mut Vec<FaceInfo>) {
    // Familiars first, then most recently seen.
    faces.sort_by(|a, b| {
        b.tag
            .is_familiar()
            .cmp(&a.tag.is_familiar())
            .then_with(|| b.last_seen().cmp(&a.last_seen()))
            .then_with(|| a.face_id.cmp(&b.face_id))
    });
    let mut seen = std::collections::HashSet::new();
    faces.retain(|f| seen.insert(f.face_id));
}

/// Parses `face_detection.search_faces_info_result` (`FacesInfo`:
/// `{"total_num": N, "search_time": T, "faces_infos": [...]}`), as well as
/// direct array or hub-wrapped section entries.
fn parse_faces_info(result: &Value) -> Vec<FaceInfo> {
    let node = result
        .pointer("/face_detection/search_faces_info_result")
        .or_else(|| result.pointer("/face_detection/search_faces_info"))
        .or_else(|| result.get("search_faces_info_result"))
        .or_else(|| result.get("face_detection"))
        .or(Some(result));
    let list = node.and_then(|n| {
        if n.is_array() {
            Some(n)
        } else {
            n.get("faces_infos")
                .or_else(|| n.get("face_infos"))
                .or_else(|| n.get("face_info_list"))
                .or_else(|| n.get("face_list"))
                .or_else(|| n.get("faces"))
                .or_else(|| n.get("list"))
        }
    });
    let plain = list.and_then(Value::as_array).into_iter().flatten().filter(|e| e.get("face_id").is_some());
    let wrapped = section_entries(list).filter(|e| e.get("face_id").is_some());
    let mut faces: Vec<FaceInfo> = plain
        .chain(wrapped)
        .filter_map(|e| {
            let face_id = e.get("face_id").and_then(int_value)?;
            let raw_name = str_field(e, "name");
            let name = decode_name(raw_name.trim());
            let mut tag = FaceTag::parse(&str_field(e, "tag"));
            // H500 stores named non-stranger profiles with either "familiar" or "".
            if tag == FaceTag::Unknown && !name.is_empty() {
                tag = FaceTag::Familiar;
            }
            let create_time = e.get("create_time").and_then(int_value).unwrap_or(0);
            let fresh_time = e.get("fresh_time").and_then(int_value).unwrap_or(0);
            let modify_time = e.get("modify_time").and_then(int_value).unwrap_or(0);
            let user_verified = e.get("user_verified").and_then(Value::as_bool).unwrap_or(false);
            Some(FaceInfo { face_id, name, tag, create_time, fresh_time, modify_time, user_verified })
        })
        .collect();
    sort_and_dedup_faces(&mut faces);
    faces
}

/// Parses `FaceTrackingResult` of `doSearchFaceTrackingList`:
/// `face_detection.action_search_face_tracking_list.search_face_tracking_list_result.tracking_traj_list`
/// (as well as flatter firmware variants).
fn parse_face_tracking_list(result: &Value) -> Vec<FaceTrackingEntry> {
    let container = result
        .pointer("/face_detection/action_search_face_tracking_list/search_face_tracking_list_result")
        .or_else(|| result.pointer("/face_detection/search_face_tracking_list_result"))
        .or_else(|| result.pointer("/face_detection/action_search_face_tracking_list"))
        .or_else(|| result.get("search_face_tracking_list_result"))
        .or(Some(result));
    let list = container.and_then(|c| {
        if c.is_array() {
            Some(c)
        } else {
            c.get("tracking_traj_list")
                .or_else(|| c.get("face_tracking_list"))
                .or_else(|| c.pointer("/search_face_tracking_list_result/tracking_traj_list"))
        }
    });
    let plain = list
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|e| e.get("traj_start_time").is_some() || e.get("start_time").is_some());
    let wrapped = section_entries(list)
        .filter(|e| e.get("traj_start_time").is_some() || e.get("start_time").is_some());
    plain
        .chain(wrapped)
        .filter_map(|e| {
            let start_time = e
                .get("traj_start_time")
                .or_else(|| e.get("start_time"))
                .and_then(int_value)?;
            let end_time = e
                .get("traj_end_time")
                .or_else(|| e.get("end_time"))
                .and_then(int_value)
                .unwrap_or(start_time)
                .max(start_time);
            let face_id = e.get("face_id").and_then(int_value).unwrap_or(0);
            let snapshot_mac = normalize_mac(&str_field(e, "snapshot_mac"));
            let dev_id = str_field(e, "dev_id");
            let channel_id = e.get("chn_id").and_then(int_value).unwrap_or(0) as i32;
            let clip_count = e.get("clip_cnt").and_then(int_value).unwrap_or(1) as i32;
            let device_count = e.get("dev_cnt").and_then(int_value).unwrap_or(1) as i32;
            Some(FaceTrackingEntry {
                face_id,
                start_time,
                end_time,
                snapshot_mac,
                dev_id,
                channel_id,
                clip_count,
                device_count,
            })
        })
        .collect()
}

/// Parses `VideoResultList` of `doSearchFaceDetectionVideo`:
/// `playback.action_search_face_detection_video.search_face_detection_video_result`
/// (or `playback.search_face_detection_video_result`).
fn parse_face_videos(result: &Value, face_id: i64) -> Vec<FaceTrackingEntry> {
    let list = result
        .pointer("/playback/action_search_face_detection_video/search_face_detection_video_result")
        .or_else(|| result.pointer("/playback/search_face_detection_video_result"))
        .or_else(|| result.get("search_face_detection_video_result"))
        .or_else(|| result.pointer("/playback/action_search_face_detection_video"));
    let plain = list
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|e| e.get("startTime").is_some() || e.get("start_time").is_some());
    let wrapped = section_entries(list)
        .filter(|e| e.get("startTime").is_some() || e.get("start_time").is_some());
    plain
        .chain(wrapped)
        .filter_map(|e| {
            let start_time = e
                .get("startTime")
                .or_else(|| e.get("start_time"))
                .and_then(int_value)?;
            let end_time = e
                .get("endTime")
                .or_else(|| e.get("end_time"))
                .and_then(int_value)
                .unwrap_or(start_time)
                .max(start_time);
            let snapshot_mac = normalize_mac(&str_field(e, "mac"));
            let dev_id = str_field(e, "dev_id");
            Some(FaceTrackingEntry {
                face_id,
                start_time,
                end_time,
                snapshot_mac,
                dev_id,
                channel_id: 0,
                clip_count: 1,
                device_count: 1,
            })
        })
        .collect()
}

/// Parses `face_detection.action_search_face_tracking_clips` /
/// `search_face_tracking_clips_result` and resolves each clip's `mac_idx`
/// through `traj_macs`.
fn parse_face_tracking_clips(result: &Value) -> Vec<FaceTrackingClip> {
    let container = result
        .pointer("/face_detection/action_search_face_tracking_clips")
        .or_else(|| result.pointer("/face_detection/search_face_tracking_clips_result"))
        .or_else(|| result.get("face_detection"))
        .unwrap_or(result);
    let macs: Vec<String> = container
        .get("traj_macs")
        .or_else(|| result.pointer("/face_detection/traj_macs"))
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(normalize_mac).collect())
        .unwrap_or_default();
    let list = if container.is_array() {
        Some(container)
    } else {
        container
            .get("search_face_tracking_clips_result")
            .or_else(|| result.pointer("/face_detection/search_face_tracking_clips_result"))
    };
    let plain = list.and_then(Value::as_array).into_iter().flatten().filter(|e| e.get("start_time").is_some());
    let wrapped = section_entries(list).filter(|e| e.get("start_time").is_some());
    plain
        .chain(wrapped)
        .filter_map(|e| {
            let start_time = e.get("start_time").and_then(int_value)?;
            let end_time = e.get("end_time").and_then(int_value).unwrap_or(start_time).max(start_time);
            let mac_idx = e.get("mac_idx").and_then(int_value).unwrap_or(-1);
            let mac = usize::try_from(mac_idx)
                .ok()
                .and_then(|i| macs.get(i).cloned())
                .unwrap_or_default();
            let channel_id = e.get("chn_id").and_then(int_value).unwrap_or(0) as i32;
            let codec = str_field(e, "codec_type");
            Some(FaceTrackingClip { start_time, end_time, mac, channel_id, codec })
        })
        .collect()
}

/// `device_info.basic_info` and `harddisk_manage.hd_info` (a list wrapped
/// like the other hub lists).
fn parse_hub_info(info: &Value, disk_answers: &[Value]) -> HubInfo {
    let basic = info.pointer("/device_info/basic_info").unwrap_or(&Value::Null);
    let first = |keys: &[&str]| keys.iter().map(|k| str_field(basic, k)).find(|v| !v.is_empty()).unwrap_or_default();
    let mut parsed = HubInfo {
        firmware: first(&["sw_version", "fw_ver"]),
        ip: first(&["local_ip", "ip"]),
        ..Default::default()
    };
    // The disks: a hub's own storage and, in an H500, the hard disk, from
    // both answers. An H500 reports its built-in eMMC in both getSdCardStatus
    // and getHardDiskStatus (sometimes with different reported sizes, e.g.
    // 10.00 GB vs 9.00 GB with 0 GB free), so deduplicate built-in storage
    // by label and external disks by (label, disk_name), keeping the entry
    // with the larger capacity (or more complete free-space info on ties).
    let entries = disk_answers.iter().flat_map(|answer| {
        let list = answer.pointer("/harddisk_manage/hd_info");
        section_entries(list).chain(list.and_then(Value::as_array).into_iter().flatten()).collect::<Vec<_>>()
    });
    let mut by_key: HashMap<(String, String), (f64, String, Disk)> = HashMap::new();
    for disk in entries {
        let total = str_field(disk, "total_space");
        if total.is_empty() {
            continue;
        }
        let disk_name = str_field(disk, "disk_name");
        let label = disk_label(&str_field(disk, "type"), &disk_name);
        let free = str_field(disk, "free_space");
        let total_f = size_bytes(&total);
        let total_bytes = total_f as u64;
        let free_bytes = size_bytes(&free) as u64;
        let text = match free.is_empty() {
            false => format!("{label} {} free of {}", spaced(&free), spaced(&total)),
            true => format!("{label} {}", spaced(&total)),
        };
        let status = str_field(disk, "status");
        let temperature_c = disk
            .get("temperature")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
        let power_on_hours = disk
            .get("power_on_hours")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
        let entry = Disk {
            label: label.clone(),
            total: spaced(&total),
            free: if free.is_empty() { String::new() } else { spaced(&free) },
            total_bytes,
            free_bytes,
            status,
            temperature_c,
            power_on_hours,
        };
        let key = if label == "Internal" {
            (label, String::new())
        } else if !disk_name.is_empty() {
            (label, disk_name)
        } else {
            (label, total_bytes.to_string())
        };
        let replace = match by_key.get(&key) {
            None => true,
            Some((_, _, existing)) => {
                // Prefer the entry with the larger reported total size, or
                // non-empty / larger free space when total sizes match.
                (entry.total_bytes, !entry.free.is_empty(), entry.free_bytes)
                    > (existing.total_bytes, !existing.free.is_empty(), existing.free_bytes)
            }
        };
        if replace {
            by_key.insert(key, (total_f, text, entry));
        }
    }
    let mut found: Vec<(f64, String, Disk)> = by_key.into_values().collect();
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    if let Some((_, _, d)) = found.iter().find(|(_, _, d)| !d.status.is_empty() && !d.status.eq_ignore_ascii_case("normal")) {
        parsed.storage_status = format!("{}: {}", d.label, d.status);
    }
    parsed.storage = found.iter().map(|(_, t, _)| t.clone()).collect();
    parsed.disks = found.into_iter().map(|(_, _, d)| d).collect();
    parsed
}

fn on_off(v: Option<&Value>) -> Option<bool> {
    match v {
        Some(Value::String(s)) if s.eq_ignore_ascii_case("on") || s == "1" || s.eq_ignore_ascii_case("true") => Some(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("off") || s == "0" || s.eq_ignore_ascii_case("false") => Some(false),
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => n.as_i64().map(|x| x != 0),
        _ => None,
    }
}

/// Parses the batched `camera_controls` response array.
fn parse_camera_controls(responses: &[Value]) -> CameraControls {
    let ptz = result_for(responses, "getAppComponentList", 0)
        .ok()
        .and_then(|c| c.pointer("/app_component/app_component_list").and_then(Value::as_array).cloned())
        .is_some_and(|l| l.iter().any(|c| c.get("name").and_then(Value::as_str) == Some("ptz")));
    let privacy = result_for(responses, "getLensMaskConfig", 1).ok().and_then(|r| parse_lens_mask(&r));
    let spotlight = result_for(responses, "getForceWhitelampState", 2)
        .ok()
        .and_then(|r| {
            on_off(r.pointer("/image/switch/force_wtl_state"))
                .or_else(|| on_off(r.pointer("/image/get_wtl_status/status")))
        });
    let nv_str = result_for(responses, "getNightVisionModeConfig", 3)
        .ok()
        .and_then(|r| r.pointer("/image/switch/night_vision_mode").and_then(Value::as_str).map(str::to_string));
    let inf_str = result_for(responses, "getDayNightModeConfig", 4)
        .ok()
        .and_then(|r| r.pointer("/image/common/inf_type").and_then(Value::as_str).map(str::to_string));
    let night_vision = match (nv_str.as_deref(), inf_str.as_deref()) {
        (Some("wtl_night_vision") | Some("color"), _) => Some(NightVisionMode::Color),
        (Some("md_night_vision"), _) => Some(NightVisionMode::Smart),
        (_, Some("off")) => Some(NightVisionMode::Day),
        (_, Some("on")) | (Some("inf_night_vision"), _) => Some(NightVisionMode::Infrared),
        (_, Some("auto")) => Some(NightVisionMode::Auto),
        _ => None,
    };
    let auto_track = result_for(responses, "getTargetTrackConfig", 5)
        .ok()
        .and_then(|r| on_off(r.pointer("/target_track/target_track_info/enabled")));
    let siren_on = result_for(responses, "getManualAlarmStatus", 6)
        .ok()
        .and_then(|r| {
            on_off(r.pointer("/msg_alarm/get_manual_status/status"))
                .or_else(|| on_off(r.pointer("/siren/status")))
        });
    CameraControls { ptz, privacy, spotlight, night_vision, auto_track, siren_on }
}

/// "HDD", "SSD", "Internal", "SD card" from a disk's `type`, else its name.
fn disk_label(kind: &str, name: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.contains("ssd") {
        "SSD".into()
    } else if k.contains("hdd") || k.contains("sata") || k.contains("hard") {
        "HDD".into()
    } else if k.contains("emmc") || k.contains("local") || k.contains("internal") || k.contains("flash") {
        "Internal".into()
    } else if k.contains("sd") {
        "SD card".into()
    } else if !kind.is_empty() {
        kind.to_string()
    } else if !name.is_empty() {
        format!("Disk {name}")
    } else {
        "Disk".into()
    }
}

/// "931.5GB" → "931.5 GB".
fn spaced(size: &str) -> String {
    let at = size.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(size.len());
    format!("{} {}", size[..at].trim(), size[at..].trim()).trim().to_string()
}

/// "1.8TB" → bytes, for putting the largest disk first.
fn size_bytes(size: &str) -> f64 {
    let at = size.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(size.len());
    let number: f64 = size[..at].trim().parse().unwrap_or(0.0);
    let scale = match size[at..].trim().to_ascii_uppercase().chars().next() {
        Some('K') => 1e3,
        Some('M') => 1e6,
        Some('G') => 1e9,
        Some('T') => 1e12,
        _ => 1.0,
    };
    number * scale
}

/// Objects anywhere in `value` that have one of `id_keys`.
fn objects_with<'a>(value: &'a Value, id_keys: &[&str]) -> Vec<&'a Value> {
    let mut found = Vec::new();
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(o) => {
                if id_keys.iter().any(|k| o.contains_key(*k)) {
                    found.push(v);
                } else {
                    stack.extend(o.values());
                }
            }
            Value::Array(a) => stack.extend(a.iter().rev()),
            _ => {}
        }
    }
    found
}

fn name_of(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().map(|k| decode_name(str_field(v, k).trim())).find(|n| !n.is_empty())
}

/// Rooms and the devices in them, where the answer lists them: each room
/// object with an id and a name, and any device ids under it.
fn parse_rooms(value: &Value) -> HashMap<String, String> {
    let mut rooms = HashMap::new();
    for room in objects_with(value, &["roomId"]) {
        let Some(name) = name_of(room, &["roomName", "name"]) else { continue };
        for device in device_ids(room) {
            rooms.insert(device, name.clone());
        }
    }
    rooms
}

/// (id, name) of each room in a rooms answer: `{"data": [{"id", "name"}]}`.
fn room_list(value: &Value) -> Vec<(String, String)> {
    objects_with(value, &["roomId", "id"])
        .into_iter()
        .filter_map(|room| {
            let id = ["roomId", "id"].iter().map(|k| str_field(room, k)).find(|v| !v.is_empty())?;
            Some((id, name_of(room, &["roomName", "name"])?))
        })
        .collect()
}

/// Device ids anywhere in `value`: objects with a device or thing id, and
/// strings that look like one (40 hex digits).
fn device_ids(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack: Vec<&Value> = match value {
        Value::Object(o) => o.values().collect(),
        other => vec![other],
    };
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(a) => stack.extend(a),
            Value::Object(o) => {
                match ["deviceId", "thingName", "thingId"].iter().map(|k| str_field(v, k)).find(|x| !x.is_empty()) {
                    Some(id) => found.push(id),
                    None => stack.extend(o.values()),
                }
            }
            Value::String(s) if s.len() >= 20 && s.bytes().all(|b| b.is_ascii_hexdigit()) => found.push(s.clone()),
            _ => {}
        }
    }
    found
}

/// The clip relay request for the log, without the player id.
fn redact_relay_request(payload: &Value) -> Value {
    let mut copy = payload.clone();
    if let Some(o) = copy.as_object_mut() {
        o.remove("playerId");
    }
    copy
}

// ----------------------------------------------------------------- helpers

/// Headers for the signed account API: an HMAC-SHA1 over the body's MD5, the
/// timestamp, a nonce and the endpoint path.
fn signed_headers(body: &[u8], endpoint: &str, nonce: &str) -> Vec<(&'static str, String)> {
    let content_md5 = B64.encode(Md5::digest(body));
    let message = format!("{content_md5}\n{TIMESTAMP}\n{nonce}\n{endpoint}");
    let mut mac = Hmac::<Sha1>::new_from_slice(ACCESS_SECRET.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    let signature: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    vec![
        ("Content-Type", "application/json; charset=UTF-8".into()),
        ("Content-MD5", content_md5),
        (
            "X-Authorization",
            format!("Timestamp={TIMESTAMP}, Nonce={nonce}, AccessKey={ACCESS_KEY}, Signature={signature}"),
        ),
        ("User-Agent", "okhttp/3.12.13".into()),
    ]
}

/// Splits the relay URL into host, port and path. The query is an opaque
/// signed string, so it's kept exactly as given, plus `retryTime=0`.
pub fn parse_relay_url(relay_url: &str) -> Result<(String, u16, String)> {
    let rest = relay_url.split_once("://").map_or(relay_url, |(_, rest)| rest);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = format!("{path}{}retryTime=0", if path.contains('?') { "&" } else { "?" });
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host, port.parse().unwrap_or(443)),
        _ => (authority, 443),
    };
    if host.is_empty() {
        return Err(CloudError::new("The Tapo cloud returned a relay without a host"));
    }
    Ok((host.to_string(), port, path))
}

fn camera_from(device: &Value) -> Option<CloudCamera> {
    let device_id = str_field(device, "deviceId");
    if device_id.is_empty() {
        return None;
    }
    let name = [str_field(device, "alias"), str_field(device, "deviceName")]
        .into_iter()
        .map(|n| decode_name(n.trim()))
        .find(|n| !n.is_empty())
        .unwrap_or_else(|| "Camera".into());
    Some(CloudCamera {
        device_id,
        name,
        model: str_field(device, "deviceModel"),
        device_type: str_field(device, "deviceType"),
        mac: normalize_mac(&str_field(device, "deviceMac")),
    })
}

/// Some device names come base64-encoded (an H500 is listed as
/// "VGFwbyBINTAw", "Tapo H500"). A name is decoded only if it is valid base64
/// of readable UTF-8 text, so plain names are left alone.
pub fn decode_name(name: &str) -> String {
    let looks_encoded = name.len() >= 4
        && name.len().is_multiple_of(4)
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if looks_encoded
        && let Ok(bytes) = B64.decode(name)
        && let Ok(text) = String::from_utf8(bytes)
    {
        let trimmed = text.trim();
        let has_padding = name.ends_with('=');
        let plausible = !trimmed.is_empty()
            && !text.chars().any(char::is_control)
            // Unpadded 4-char ASCII strings like "T200" or "P100" decode to
            // short gibberish ("Om4", "?]4"); require clean name characters
            // when there is no base64 `=` padding.
            && (has_padding
                || (name.len() > 4
                    && trimmed.chars().all(|c| {
                        c.is_alphanumeric() || c.is_whitespace() || matches!(c, '-' | '_' | '\'' | '(' | ')' | '.' | '&' | '/' | ',' | ':')
                    })));
        if plausible {
            return trimmed.to_string();
        }
    }
    name.to_string()
}

pub fn normalize_mac(mac: &str) -> String {
    mac.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_uppercase()).collect()
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn network_error(e: reqwest::Error) -> CloudError {
    let what = if e.is_timeout() { "The Tapo cloud did not answer in time" } else { "Could not reach the Tapo cloud" };
    // reqwest's Display omits the cause (DNS, TLS, refused…); include it.
    let mut detail = String::new();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        detail = s.to_string();
        source = s.source();
    }
    if detail.is_empty() { CloudError::new(what) } else { CloudError::new(format!("{what}: {detail}")) }
}

fn int_value(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The API reports failures as `error_code` (or `errorCode`); 0 means OK.
fn error_code(v: &Value) -> i64 {
    ["error_code", "errorCode"].iter().find_map(|k| v.get(*k)).map_or(0, |c| int_value(c).unwrap_or(1))
}

fn result_of(v: &Value) -> Value {
    v.get("result").filter(|r| r.is_object()).cloned().unwrap_or_else(|| json!({}))
}

fn str_field(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn describe(what: &str, code: i64) -> String {
    match code {
        -20601 => format!("Incorrect email or password ({code})"),
        -20600 => format!("Account not found ({code})"),
        AUTH_EXPIRED => "The Tapo cloud sign-in has expired".into(),
        _ => format!("{what} (code {code})"),
    }
}

fn check_outer(response: &Value, what: &str) -> Result<()> {
    match error_code(response) {
        0 => Ok(()),
        code => Err(CloudError::with_code(describe(what, code), code)),
    }
}

fn check_both(response: &Value, what: &str) -> Result<()> {
    check_outer(response, what)?;
    check_outer(&result_of(response), what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_reference_clients() {
        // Expected values computed independently with Python's hashlib/hmac,
        // the same way Topo's tapo_cloud_probe.py signs requests.
        let body = br#"{"serviceIds":["cipc.api.cloud"]}"#;
        let headers = signed_headers(body, "/api/v2/common/getAppServiceUrl", "0f8fad5b-d9cb-469f-a165-70867728950e");
        let get = |name: &str| headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("Content-MD5"), EXPECTED_MD5);
        assert_eq!(
            get("X-Authorization"),
            format!(
                "Timestamp=9999999999, Nonce=0f8fad5b-d9cb-469f-a165-70867728950e, \
                 AccessKey={ACCESS_KEY}, Signature={EXPECTED_SIGNATURE}"
            )
        );
    }

    const EXPECTED_MD5: &str = "F6ZG767UdbygPCZbHUvUFg==";
    const EXPECTED_SIGNATURE: &str = "d3e94189d5158cf4043741add9bebbbacdc9f9c7";

    #[test]
    fn relay_url_keeps_signed_query() {
        let (host, port, path) =
            parse_relay_url("euw1-relay-dcipc.i.tplinkcloud.com/relayservice?deviceid=AB&sig=x%2By==").unwrap();
        assert_eq!(host, "euw1-relay-dcipc.i.tplinkcloud.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/relayservice?deviceid=AB&sig=x%2By==&retryTime=0");
    }

    #[test]
    fn relay_url_with_scheme_port_and_no_query() {
        let (host, port, path) = parse_relay_url("https://relay.example:8443/stream").unwrap();
        assert_eq!((host.as_str(), port, path.as_str()), ("relay.example", 8443, "/stream?retryTime=0"));
        let (host, port, path) = parse_relay_url("relay.example").unwrap();
        assert_eq!((host.as_str(), port, path.as_str()), ("relay.example", 443, "/?retryTime=0"));
        assert!(parse_relay_url("https:///x").is_err());
    }

    #[test]
    fn error_codes() {
        assert_eq!(error_code(&json!({"error_code": 0})), 0);
        assert_eq!(error_code(&json!({"errorCode": "-20651"})), -20651);
        assert_eq!(error_code(&json!({"result": {}})), 0);
        assert!(CloudError::with_code("x", -20651).is_auth());
    }

    #[test]
    fn camera_list_entry() {
        let cam = camera_from(&json!({
            "deviceId": "8021ABC", "alias": " Front door ", "deviceModel": "C200",
            "deviceType": "SMART.IPCAMERA", "deviceMac": "a8-42-a1-00-11-22"
        }))
        .unwrap();
        assert_eq!(cam.name, "Front door");
        assert_eq!(cam.mac, "A842A1001122");
        assert!(camera_from(&json!({"alias": "no id"})).is_none());
        assert_eq!(camera_from(&json!({"deviceId": "1"})).unwrap().name, "Camera");
    }

    #[test]
    fn camera_control_answers() {
        let ok = br#"{"outputParams":{"responseData":{"error_code":0,"result":{"responses":[{"method":"motorMove","result":{},"error_code":0}]}}}}"#;
        assert!(first_result(&parse_camera_answer(200, ok).unwrap(), "motorMove").is_ok());

        let at_limit = br#"{"outputParams":{"responseData":{"error_code":0,"result":{"responses":[{"method":"motorMove","error_code":-64304}]}}}}"#;
        let e = first_result(&parse_camera_answer(200, at_limit).unwrap(), "motorMove").unwrap_err();
        assert_eq!(e.to_string(), "Reached the end of the pan/tilt range");

        assert!(parse_camera_answer(401, b"").unwrap_err().is_auth());
        let expired = br#"{"outputParams":{"responseData":{"error_code":-20651}}}"#;
        assert!(parse_camera_answer(200, expired).unwrap_err().is_auth());
        let offline = br#"{"outputParams":{"responseData":{"error_code":-40401}}}"#;
        assert!(parse_camera_answer(200, offline).unwrap_err().is_offline());
        let e = parse_camera_answer(403, br#"{"message":"forbidden"}"#).unwrap_err().to_string();
        assert!(e.contains("HTTP 403") && e.contains("forbidden"), "{e}");
        assert!(parse_camera_answer(200, b"{}").is_err());
    }

    #[test]
    fn lens_mask_states() {
        assert_eq!(parse_lens_mask(&json!({"lens_mask": {"lens_mask_info": {"enabled": "on"}}})), Some(true));
        assert_eq!(parse_lens_mask(&json!({"lens_mask": {"lens_mask_info": {"enabled": "off"}}})), Some(false));
        assert_eq!(parse_lens_mask(&json!({})), None);
    }

    #[test]
    fn presets_zip_ids_and_names() {
        let result = json!({"preset": {"preset": {"id": ["1", "2"], "name": ["Door", "Gate"], "read_only": ["0", "0"]}}});
        assert_eq!(parse_presets(&result), vec![("1".to_string(), "Door".to_string()), ("2".into(), "Gate".into())]);
        assert!(parse_presets(&json!({})).is_empty());
    }

    #[test]
    fn hub_camera_list() {
        let result = json!({"general_camera_manage": {"max_bound": 4, "paired_general_device_list": [
            {"device_id": "CAM1", "alias": "Front door", "device_model": "C210", "mac": "A8-42-A1-00-00-01",
             "hub_storage_enabled": true, "parent_device_id": "HUB"},
            {"device_id": "CAM2", "alias": "", "device_model": "C120", "mac": "A8-42-A1-00-00-02",
             "hub_storage_enabled": false},
            {"alias": "no id"}
        ]}});
        let cams = parse_hub_cameras(&result);
        assert_eq!(cams.len(), 2);
        assert_eq!(cams[0].name, "Front door");
        assert_eq!(cams[0].mac, "A8-42-A1-00-00-01");
        assert!(cams[0].hub_storage);
        assert_eq!(cams[1].name, "C120");
        assert!(!cams[1].hub_storage);
        // H200 firmware leaves the list out when nothing is paired.
        assert!(parse_hub_cameras(&json!({"general_camera_manage": {"max_bound": 4}})).is_empty());
    }

    #[test]
    fn hub_recording_days_and_clips() {
        let days = json!({"playback": {"search_results": [
            {"search_results_2": {"date": "20261121"}},
            {"search_results_1": {"date": "20261117"}},
            {"search_results_3": {"date": "bad"}}
        ]}});
        let days = parse_recording_days(&days);
        assert_eq!(days, vec![NaiveDate::from_ymd_opt(2026, 11, 17).unwrap(), NaiveDate::from_ymd_opt(2026, 11, 21).unwrap()]);

        let clips = json!({"playback": {"search_video_results": [
            {"search_video_results_1": {"startTime": 1786694400, "endTime": 1786694460, "video_type": "2"}},
            {"search_video_results_2": {"startTime": "1786694500", "endTime": "1786694530", "video_type": 6}},
            {"search_video_results_3": {"startTime": 5, "endTime": 5}}
        ]}});
        let clips = parse_clips(&clips);
        assert_eq!(clips, vec![
            Clip { start: 1786694400, end: 1786694460, kind: 2 },
            Clip { start: 1786694500, end: 1786694530, kind: 6 },
        ]);
        assert_eq!(clips[0].seconds(), 60);
        assert_eq!(clip_kind(2), "Motion");
        assert_eq!(clip_kind(99), "Event 99");
        assert!(parse_clips(&json!({"playback": {}})).is_empty());
    }

    #[test]
    fn hub_detections() {
        let result = json!({"playback": {"search_detection_list": [
            {"alarm_type": 6, "start_time": 1792786149, "end_time": 1792786217, "events_1": 34}
        ], "to_be_continued": 0}});
        assert_eq!(parse_detections(&result), vec![Detection { start: 1792786149, end: 1792786217, kind: 6, kinds: vec![2, 6] }]);
        let wrapped = json!({"playback": {"search_detection_list": [
            {"search_detection_list_1": {"alarm_type": "2", "start_time": "10", "end_time": "12"}}
        ]}});
        assert_eq!(parse_detections(&wrapped), vec![Detection { start: 10, end: 12, kind: 2, kinds: vec![] }]);
    }

    #[test]
    fn base64_names_are_decoded() {
        assert_eq!(decode_name("VGFwbyBINTAw"), "Tapo H500");
        assert_eq!(decode_name("SG9tZUJhc2U="), "HomeBase");
        assert_eq!(decode_name("Üst Kat"), "Üst Kat");
        assert_eq!(decode_name("Door"), "Door");
        assert_eq!(decode_name("T200"), "T200");
        assert_eq!(decode_name("S200"), "S200");
        assert_eq!(decode_name("P100"), "P100");
        assert_eq!(decode_name("Garage"), "Garage");
        assert_eq!(decode_name("Front door"), "Front door");
        let hub = camera_from(&json!({"deviceId": "H", "alias": "VGFwbyBINTAw", "deviceType": "SMART.TAPOHUB"}));
        assert_eq!(hub.unwrap().name, "Tapo H500");
    }

    #[test]
    fn hub_info_and_homes() {
        let info = json!({"device_info": {"basic_info": {"sw_version": "1.2.3 Build 250101", "local_ip": "192.168.1.40"}}});
        // An H500: its own storage, then the hard disk.
        let disks = json!({"harddisk_manage": {"hd_info": [
            {"hd_info_1": {"type": "local", "disk_name": "1", "total_space": "10.00GB", "free_space": "7.44GB", "status": "normal"}},
            {"hd_info_2": {"type": "hdd", "disk_name": "2", "total_space": "931.5GB", "free_space": "812.0GB", "status": "normal"}}
        ]}});
        assert_eq!(parse_hub_info(&info, std::slice::from_ref(&disks)), HubInfo {
            firmware: "1.2.3 Build 250101".into(),
            ip: "192.168.1.40".into(),
            storage: vec!["HDD 812.0 GB free of 931.5 GB".into(), "Internal 7.44 GB free of 10.00 GB".into()],
            storage_status: String::new(),
            disks: vec![
                Disk {
                    label: "HDD".into(),
                    total: "931.5 GB".into(),
                    free: "812.0 GB".into(),
                    total_bytes: 931_500_000_000,
                    free_bytes: 812_000_000_000,
                    status: "normal".into(),
                    temperature_c: None,
                    power_on_hours: None,
                },
                Disk {
                    label: "Internal".into(),
                    total: "10.00 GB".into(),
                    free: "7.44 GB".into(),
                    total_bytes: 10_000_000_000,
                    free_bytes: 7_440_000_000,
                    status: "normal".into(),
                    temperature_c: None,
                    power_on_hours: None,
                },
            ],
            loop_recording: None,
        });
        let used = parse_hub_info(&info, std::slice::from_ref(&disks)).disks[0].used().unwrap();
        // An H500: getSdCardStatus reports built-in storage as 10.00 GB (7.45 GB free),
        // while getHardDiskStatus reports it again with a smaller quota (9.00 GB, 0 GB free)
        // alongside the SATA hard disk. Both collapse to one Built-in storage entry.
        let sd = json!({"harddisk_manage": {"hd_info": [
            {"hd_info_1": {"type": "local", "disk_name": "1", "total_space": "10.00GB", "free_space": "7.45GB", "status": "normal"}}
        ]}});
        let hdd = json!({"harddisk_manage": {"hd_info": [
            {"hd_info_1": {"type": "emmc", "disk_name": "1", "total_space": "9.00GB", "free_space": "0GB", "status": "normal"}},
            {"hd_info_2": {"type": "hdd", "disk_name": "2", "total_space": "931GB", "free_space": "163.00GB", "status": "normal", "temperature": 52, "power_on_hours": 26703}}
        ]}});
        let both = parse_hub_info(&json!({}), &[hdd, sd]);
        assert_eq!(both.disks.len(), 2);
        assert_eq!((both.disks[0].label.as_str(), both.disks[0].total.as_str(), both.disks[0].temperature_c, both.disks[0].power_on_hours), ("HDD", "931 GB", Some(52), Some(26703)));
        assert_eq!((both.disks[1].label.as_str(), both.disks[1].total.as_str(), both.disks[1].free.as_str()), ("Internal", "10.00 GB", "7.45 GB"));
        assert!((used - 0.128).abs() < 0.01, "{used}");
        let broken = json!({"harddisk_manage": {"hd_info": [{"hd_info_1": {"type": "hdd", "total_space": "1TB", "status": "unformatted"}}]}});
        assert_eq!(parse_hub_info(&json!({}), &[broken]).storage_status, "HDD: unformatted");
        assert_eq!(parse_hub_info(&json!({}), &[json!({})]), HubInfo::default());

        let rooms = json!({"data": {"rooms": [
            {"roomId": "r1", "roomName": "Office", "things": [{"thingName": "CAM1"}, {"thingName": "HUB"}]},
            {"roomId": "r2", "name": "Lobby", "deviceIds": ["802DDDB125A9F2705123B328A3947A4B2526D47C"]}
        ]}});
        let rooms = parse_rooms(&rooms);
        assert_eq!(rooms.get("CAM1").map(String::as_str), Some("Office"));
        assert_eq!(rooms.get("802DDDB125A9F2705123B328A3947A4B2526D47C").map(String::as_str), Some("Lobby"));
        // As the cloud answers: rooms without their devices, then a room's
        // thing order.
        let listed = json!({"data": [{"id": "w03QLRwg", "name": "Ofis"}], "page": 0, "pageSize": 20, "total": 1});
        assert_eq!(room_list(&listed), vec![("w03QLRwg".to_string(), "Ofis".to_string())]);
        assert!(parse_rooms(&listed).is_empty());
        let order = json!({"data": ["802DDDB125A9F2705123B328A3947A4B2526D47C", {"thingName": "8021F89910F333382370C451E9E4B624249780DE"}]});
        assert_eq!(device_ids(&order).len(), 2);
        let homes = Homes { only_room: Some("Ofis".into()), ..Default::default() };
        assert_eq!(homes.room_of("anything").map(String::as_str), Some("Ofis"));
        let families = json!({"data": [{"familyId": "f1", "familyName": "T2ZmaWNl"}]});
        let names: Vec<_> = objects_with(&families, &["familyId", "id"]).iter().filter_map(|f| name_of(f, &["familyName", "name"])).collect();
        assert_eq!(names, vec!["Office"]);
    }

    #[test]
    fn camera_controls_and_event_filters() {
        let answers = vec![
            json!({"method": "getAppComponentList", "error_code": 0, "result": {"app_component": {"app_component_list": [{"name": "ptz"}]}}}),
            json!({"method": "getLensMaskConfig", "error_code": 0, "result": {"lens_mask": {"lens_mask_info": {"enabled": "off"}}}}),
            json!({"method": "getForceWhitelampState", "error_code": 0, "result": {"image": {"switch": {"force_wtl_state": "on"}}}}),
            json!({"method": "getNightVisionModeConfig", "error_code": 0, "result": {"image": {"switch": {"night_vision_mode": "md_night_vision"}}}}),
            json!({"method": "getDayNightModeConfig", "error_code": 0, "result": {"image": {"common": {"inf_type": "auto"}}}}),
            json!({"method": "getTargetTrackConfig", "error_code": 0, "result": {"target_track": {"target_track_info": {"enabled": "on"}}}}),
            json!({"method": "getManualAlarmStatus", "error_code": 0, "result": {"msg_alarm": {"get_manual_status": {"status": 0}}}}),
        ];
        assert_eq!(parse_camera_controls(&answers), CameraControls {
            ptz: true,
            privacy: Some(false),
            spotlight: Some(true),
            night_vision: Some(NightVisionMode::Smart),
            auto_track: Some(true),
            siren_on: Some(false),
        });

        let person_motion = Detection { start: 1, end: 2, kind: 6, kinds: vec![2, 6] };
        let vehicle = Detection { start: 3, end: 4, kind: 8, kinds: vec![8] };
        assert!(EventFilter::All.matches(&person_motion));
        assert!(EventFilter::Person.matches(&person_motion));
        assert!(EventFilter::Motion.matches(&person_motion));
        assert!(!EventFilter::Vehicle.matches(&person_motion));
        assert!(EventFilter::Vehicle.matches(&vehicle));
        assert!(!EventFilter::Pet.matches(&vehicle));
    }

    #[test]
    fn camera_hub_models() {
        assert!(is_camera_hub("H500"));
        assert!(is_camera_hub("h200(EU)"));
        assert!(!is_camera_hub("H100"));
        assert!(!is_camera_hub(""));
    }

    #[test]
    fn face_recognition_requests_and_parsers() {
        let fam_req = search_faces_request(&["stranger"], "reject", 48);
        assert_eq!(fam_req["method"], "searchFacesInfo");
        let fam_body = &fam_req["params"]["face_detection"]["search_faces_info"];
        assert_eq!(fam_body["strategy"], "reject");
        assert_eq!(fam_body["tag"], json!(["stranger"]));
        assert_eq!(fam_body["face_id"], json!([]));
        assert_eq!(fam_body["trans_limit"], 48);
        assert_eq!(fam_body["sort_by"], "fresh_time");

        let set_req = set_face_info_request(42, "  Ahmet ", true);
        assert_eq!(set_req["method"], "setFaceInfo");
        let cmd = &set_req["params"]["face_detection"]["set_face_info"]["commands"][0];
        assert_eq!(cmd["operate"], "set");
        assert_eq!(cmd["ori_face_id"], 42);
        assert_eq!(cmd["cover"]["name"], "Ahmet");
        assert_eq!(cmd["cover"]["tag"], "familiar");

        let tr_req = search_face_tracking_request(100, 200, 0, 49, Some(42), Some("familiar"));
        assert_eq!(tr_req["method"], "doSearchFaceTrackingList");
        let tr_body = &tr_req["params"]["face_detection"]["action_search_face_tracking_list"];
        assert_eq!(tr_body["face_id"], json!([42]));
        assert_eq!(tr_body["tag"], json!(["familiar"]));
        assert_eq!(tr_body["version"], 2);

        // Exact H500 `FacesInfo` wire shape: `search_faces_info_result.faces_infos`
        let faces_json = json!({
            "face_detection": {
                "search_faces_info_result": {
                    "total_num": 3,
                    "search_time": 1700000999,
                    "faces_infos": [
                        {"face_id": 2, "name": "", "tag": "stranger", "fresh_time": 1700000500},
                        {"face_id": 1, "name": "Ahmet", "tag": "", "fresh_time": 1700000100, "user_verified": true},
                        {"face_id": 3, "name": "Zeynep", "tag": "familiar", "fresh_time": 1700000900}
                    ]
                }
            }
        });
        let faces = parse_faces_info(&faces_json);
        assert_eq!(faces.len(), 3);
        // Familiars sorted before Strangers, newest first (named tag:"" treated as Familiar)
        assert_eq!(faces[0].face_id, 3);
        assert_eq!(faces[0].name, "Zeynep");
        assert!(faces[0].tag.is_familiar());
        assert_eq!(faces[1].face_id, 1);
        assert!(faces[1].tag.is_familiar());
        assert_eq!(faces[2].face_id, 2);
        assert_eq!(faces[2].tag, FaceTag::Stranger);
        assert!(FaceFilter::Familiar.matches(&faces[0]));
        assert!(!FaceFilter::Familiar.matches(&faces[2]));
        assert!(FaceFilter::Stranger.matches(&faces[2]));

        // Wrapped section format also parses cleanly
        let wrapped_faces = json!({
            "face_detection": {
                "search_faces_info_result": [
                    {"face_1": {"face_id": 7, "name": "Ali", "tag": "familiar", "create_time": 10}}
                ]
            }
        });
        assert_eq!(parse_faces_info(&wrapped_faces)[0].name, "Ali");

        // Exact H500 `FaceTrackingResult` wire shape:
        // `face_detection.action_search_face_tracking_list.search_face_tracking_list_result.tracking_traj_list`
        let tracking_json = json!({
            "face_detection": {
                "action_search_face_tracking_list": {
                    "search_face_tracking_list_result": {
                        "traj_cnt": 1,
                        "tracking_traj_list": [
                            {
                                "face_id": 3,
                                "traj_start_time": 1700001000,
                                "traj_end_time": 1700001045,
                                "snapshot_mac": "a8-42-a1-00-11-22",
                                "chn_id": 0,
                                "clip_cnt": 2,
                                "dev_cnt": 2
                            }
                        ]
                    }
                }
            }
        });
        let tracks = parse_face_tracking_list(&tracking_json);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].snapshot_mac, "A842A1001122");
        assert_eq!(tracks[0].clip_count, 2);
        assert_eq!(tracks[0].device_count, 2);

        let clips_json = json!({
            "face_detection": {
                "action_search_face_tracking_clips": {
                    "traj_macs": ["A8-42-A1-00-11-22", "A8:42:A1:00:11:33"],
                    "search_face_tracking_clips_result": [
                        {"start_time": 1000, "end_time": 1020, "mac_idx": 1, "chn_id": 0, "codec_type": "h264"}
                    ]
                }
            }
        });
        let clips = parse_face_tracking_clips(&clips_json);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].mac, "A842A1001133");
        assert_eq!(clips[0].codec, "h264");

        // Exact H500 `doSearchFaceDetectionVideo` request & `VideoResultList` response shape
        let vid_req = search_face_videos_request(42, &["A8-42-A1-00-11-22".into()], 100, 200, 0, 49);
        assert_eq!(vid_req["method"], "doSearchFaceDetectionVideo");
        let vid_body = &vid_req["params"]["playback"]["action_search_face_detection_video"];
        assert_eq!(vid_body["face_id"], 42);
        assert_eq!(vid_body["id"], 1);
        assert_eq!(vid_body["mac"], json!(["A8-42-A1-00-11-22"]));
        let vid_json = json!({
            "playback": {
                "action_search_face_detection_video": {
                    "search_face_detection_video_result": [
                        {"startTime": 1700002000, "endTime": 1700002030, "mac": "a8-42-a1-00-11-22", "dev_id": "CAM1"}
                    ]
                }
            }
        });
        let vids = parse_face_videos(&vid_json, 42);
        assert_eq!(vids.len(), 1);
        assert_eq!(vids[0].face_id, 42);
        assert_eq!(vids[0].snapshot_mac, "A842A1001122");
        assert_eq!(vids[0].dev_id, "CAM1");
    }

    #[test]
    fn tls_config_loads_tp_link_roots() {
        assert_eq!(CertificateDer::pem_slice_iter(TP_LINK_ROOTS).count(), 2);
        tls_config().unwrap();
    }
}
