//! Tapo cloud: account sign-in (with two-step verification), the camera list
//! and the request for a camera's video relay, which is how the Tapo app shows
//! live video when the phone isn't on the cameras' network.
//!
//! The API is private and undocumented; TP-Link can change it at any time.
//! This is a port of `lib/api/tapo_cloud.dart` from Topo TV
//! (<https://github.com/abdulmominsakib/tapo-android-tv>, MIT licence), which
//! is itself based on Topo (<https://github.com/filirnd/topo>, MIT licence).

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
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
const TERMINAL_NAME: &str = "Tapo Viewer (Windows)";

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
/// fake cloud with `TAPO_VIEWER_FAKE_CLOUD` (see `tools/fake_cloud.py`).
fn login_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("TAPO_VIEWER_FAKE_CLOUD") {
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
    if let Some(path) = std::env::var_os("TAPO_VIEWER_FAKE_CLOUD_CA") {
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
    /// (account API server, CIPC URL) from the last service lookup. Held
    /// during a lookup so streams starting together share one.
    cipc: tokio::sync::Mutex<Option<(String, String)>>,
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
        Ok(Self { http, tls, terminal_uuid, session: Mutex::default(), cipc: Default::default() })
    }

    pub fn tls(&self) -> Arc<rustls::ClientConfig> {
        self.tls.clone()
    }

    pub fn session(&self) -> Option<Session> {
        self.session.lock().unwrap().clone()
    }

    /// The CIPC URL is cached per regional server, so it needn't be cleared.
    pub fn set_session(&self, session: Option<Session>) {
        *self.session.lock().unwrap() = session;
    }

    fn require_session(&self) -> Result<Session> {
        self.session().ok_or_else(|| CloudError::new("Not signed in to the Tapo cloud: sign in under Settings"))
    }

    // ------------------------------------------------------------------ HTTP

    async fn post_raw(&self, url: &str, body: Vec<u8>, headers: Vec<(&'static str, String)>) -> Result<(u16, Value)> {
        let mut request = self.http.post(url).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(network_error)?;
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
        const PAGE: usize = 20;
        let session = self.require_session()?;
        let mut cameras = Vec::new();
        for index in (0..200).step_by(PAGE) {
            let payload = json!({ "deviceTypeList": ["SMART.IPCAMERA"], "index": index, "limit": PAGE });
            let response = self
                .post(&session.base_url, "/api/v2/common/getDeviceListByPage", &payload, Some(&session.token))
                .await?;
            check_outer(&response, "Could not load the cameras on your Tapo account")?;
            let result = result_of(&response);
            let Some(page) = result.get("deviceList").and_then(Value::as_array) else { break };
            cameras.extend(page.iter().filter_map(camera_from));
            let total = result.get("totalNum").and_then(int_value).unwrap_or(i64::MAX);
            if page.len() < PAGE || cameras.len() as i64 >= total {
                break;
            }
        }
        Ok(cameras)
    }

    // ----------------------------------------------------------------- relay

    /// The CIPC (camera video) service URL for the session's region.
    async fn cipc_url(&self, session: &Session) -> Result<String> {
        let mut cached = self.cipc.lock().await;
        if let Some((base, url)) = &*cached
            && *base == session.base_url
        {
            return Ok(url.clone());
        }
        let payload = json!({ "serviceIds": ["cipc.api.cloud"] });
        let response = self
            .post(&session.base_url, "/api/v2/common/getAppServiceUrl", &payload, Some(&session.token))
            .await?;
        check_outer(&response, "Video service lookup refused")?;
        let url = result_of(&response)
            .get("serviceUrls")
            .map(|s| str_field(s, "cipc.api.cloud"))
            .unwrap_or_default();
        if url.is_empty() {
            return Err(CloudError::new("The Tapo cloud did not return its video service address"));
        }
        let url = url.replacen("http://", "https://", 1);
        *cached = Some((session.base_url.clone(), url.clone()));
        Ok(url)
    }

    async fn post_cipc(&self, session: &Session, endpoint: &str, payload: &Value, player_id: &str) -> Result<Value> {
        let url = format!("{}{endpoint}", self.cipc_url(session).await?.trim_end_matches('/'));
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
                *self.cipc.lock().await = None; // look the service up again next time
                Err(e)
            }
        }
    }

    /// Asks the cloud for a relay that streams this camera's live video.
    pub async fn request_relay(&self, camera: &CloudCamera, resolution: Resolution) -> Result<RelayInfo> {
        if camera.device_id.is_empty() {
            return Err(CloudError::new("This camera isn't linked to a camera on your Tapo account"));
        }
        let session = self.require_session()?;
        let player_id = new_uuid();
        let device_type = if camera.device_type.is_empty() { "SMART.IPCAMERA" } else { &camera.device_type };
        let payload = json!({
            "deviceId": camera.device_id,
            "dataTimeoutCount": 0,
            "cloudType": 2,
            "deviceType": device_type,
            "playerId": player_id,
            "rootCaVer": "1",
            "streamType": 0,
            "preConnection": 0,
            "resolution": resolution.as_str(),
        });
        let response = self.post_cipc(&session, "/v2/relay/request", &payload, &player_id).await?;
        match error_code(&response) {
            0 => {}
            code @ (-40401 | -40406) => return Err(CloudError::with_code("Camera is offline", code)),
            AUTH_EXPIRED => return Err(CloudError::with_code("The Tapo cloud sign-in has expired", AUTH_EXPIRED)),
            code => {
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
        .map(|n| n.trim().to_string())
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
    fn tls_config_loads_tp_link_roots() {
        assert_eq!(CertificateDer::pem_slice_iter(TP_LINK_ROOTS).count(), 2);
        tls_config().unwrap();
    }
}
