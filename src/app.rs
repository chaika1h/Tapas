//! Application state and the glue between the Slint UI and the background
//! work (streams, PTZ, discovery, hub). All of it lives on the UI thread;
//! background tasks report back through [`on_ui`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate, TimeZone, Utc};
use futures_util::StreamExt;
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use tapo::responses::{GeneralDeviceHubResult, RecordingHubResult};
use tapo::{ApiClient, CameraHubHandler, DeviceType};

use crate::cloud::{self, CloudCamera, CloudClient, Login, MfaChallenge, MfaMethod, Session};
use crate::config::{self, AppConfig, CameraConfig, HubConfig, Quality, Renderer};
use crate::ptz::{Ptz, PtzCmd};
use crate::secret::Secret;
use crate::stream::{StreamHandle, StreamSpec};
use crate::{
    Bridge, CameraRow, DownloadRow, FoundRow, HubRow, MainWindow, Page, PresetRow, RecRow, Tile,
};

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI thread with the app state. Callable from any thread.
pub fn on_ui(f: impl FnOnce(&Rc<App>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = APP.with(|a| a.borrow().clone()) {
            f(&app);
        }
    });
}

/// Stops all streams and background work. Call after the event loop exits.
pub fn shutdown() {
    if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
        app.timer.stop();
        app.streams.borrow_mut().clear();
        app.ptz.borrow_mut().clear();
    }
}

type StreamKey = (u32, Quality);

struct Live {
    handle: StreamHandle,
    shown_version: u64,
    shown_status: String,
}

#[derive(Clone, Copy, PartialEq)]
enum FoundKind {
    Camera,
    CameraPtz,
    Hub,
    Other,
}

#[derive(Clone)]
struct Found {
    name: String,
    model: String,
    host: String,
    kind: FoundKind,
}

#[derive(Default)]
struct HubSession {
    /// Bumped on every (re)connect so stale results are ignored.
    generation: u64,
    handler: Option<Arc<CameraHubHandler>>,
    paired: Vec<GeneralDeviceHubResult>,
    recordings: Vec<RecordingHubResult>,
    rec_request: u64,
}

/// A Tapo cloud sign-in waiting for its verification code.
struct PendingMfa {
    challenge: MfaChallenge,
    user: String,
    pass: String,
    /// Where the code was sent, once it has been.
    method: Option<MfaMethod>,
}

pub struct App {
    ui: slint::Weak<MainWindow>,
    rt: tokio::runtime::Handle,
    cfg: RefCell<AppConfig>,
    ffmpeg: RefCell<Option<PathBuf>>,
    streams: RefCell<HashMap<StreamKey, Live>>,
    tiles: Rc<VecModel<Tile>>,
    single: RefCell<Option<StreamKey>>,
    single_shown: RefCell<(u64, String)>,
    ptz: RefCell<HashMap<u32, Ptz>>,
    found: RefCell<Vec<Found>>,
    hub: RefCell<HubSession>,
    rec_date: RefCell<NaiveDate>,
    downloads: Rc<VecModel<DownloadRow>>,
    status_until: RefCell<Option<Instant>>,
    last_grid_size: RefCell<(usize, i32, i32)>,
    timer: slint::Timer,
    /// The Tapo cloud client, or why it couldn't be set up.
    cloud: Result<Arc<CloudClient>, String>,
    /// Cameras on the Tapo account, from the last list.
    cloud_cameras: RefCell<Vec<CloudCamera>>,
    /// The account cameras behind the camera form's "Tapo camera" list.
    cloud_choices: RefCell<Vec<CloudCamera>>,
    pending_mfa: RefCell<Option<PendingMfa>>,
    last_relogin: Cell<Option<Instant>>,
}

/// Wraps a method call in a closure that holds only a weak reference.
macro_rules! cb {
    ($app:expr, |$a:ident $(, $arg:ident)*| $body:expr) => {{
        let weak = Rc::downgrade($app);
        move |$($arg),*| {
            if let Some($a) = weak.upgrade() {
                $body
            }
        }
    }};
}

pub fn install(ui: &MainWindow, mut cfg: AppConfig, rt: tokio::runtime::Handle) {
    let ffmpeg = crate::ffmpeg::find(&cfg.ffmpeg_path);
    let new_terminal = cfg.cloud.terminal_uuid.is_empty();
    if new_terminal {
        cfg.cloud.terminal_uuid = uuid::Uuid::new_v4().to_string();
    }
    let cloud = CloudClient::new(cfg.cloud.terminal_uuid.clone()).map(Arc::new).map_err(|e| e.to_string());
    match &cloud {
        Ok(client) => {
            let token = cfg.cloud.token.reveal();
            if !token.is_empty() && !cfg.cloud.base_url.is_empty() {
                client.set_session(Some(Session { base_url: cfg.cloud.base_url.clone(), token }));
            }
        }
        Err(e) => log::error!("Tapo cloud unavailable: {e}"),
    }
    let app = Rc::new(App {
        ui: ui.as_weak(),
        rt,
        cfg: RefCell::new(cfg),
        ffmpeg: RefCell::new(ffmpeg),
        streams: RefCell::default(),
        tiles: Rc::new(VecModel::default()),
        single: RefCell::default(),
        single_shown: RefCell::default(),
        ptz: RefCell::default(),
        found: RefCell::default(),
        hub: RefCell::default(),
        rec_date: RefCell::new(Local::now().date_naive()),
        downloads: Rc::new(VecModel::default()),
        status_until: RefCell::default(),
        last_grid_size: RefCell::default(),
        timer: slint::Timer::default(),
        cloud,
        cloud_cameras: RefCell::default(),
        cloud_choices: RefCell::default(),
        pending_mfa: RefCell::default(),
        last_relogin: Cell::default(),
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    if new_terminal {
        app.persist();
    }

    let b = ui.global::<Bridge>();
    b.set_tiles(app.tiles.clone().into());
    b.set_downloads(app.downloads.clone().into());
    b.set_quality_labels(string_model(Quality::LABELS.iter().copied()));

    b.on_navigate(cb!(&app, |a, page| a.navigate(page)));
    b.on_toggle_fullscreen(cb!(&app, |a| a.toggle_fullscreen()));
    b.on_escape(cb!(&app, |a| a.escape()));
    b.on_open_camera(cb!(&app, |a, id| a.open_camera(id as u32)));
    b.on_quality_changed(cb!(&app, |a, i| a.quality_changed(i)));
    b.on_snapshot(cb!(&app, |a| a.snapshot()));
    b.on_ptz_move(cb!(&app, |a, pan, tilt| a.ptz_send(PtzCmd::Move(pan, tilt))));
    b.on_ptz_goto(cb!(&app, |a, id| a.ptz_send(PtzCmd::Goto(id.into()))));
    b.on_ptz_save(cb!(&app, |a, name| a.ptz_send(PtzCmd::Save(name.trim().into()))));
    b.on_ptz_refresh(cb!(&app, |a| a.ptz_send(PtzCmd::Refresh)));

    b.on_save_settings(cb!(&app, |a| a.save_settings()));
    b.on_cloud_sign_in(cb!(&app, |a| a.cloud_sign_in()));
    b.on_cloud_send_code(cb!(&app, |a, m| a.cloud_send_code(m)));
    b.on_cloud_verify(cb!(&app, |a| a.cloud_verify()));
    b.on_cloud_sign_out(cb!(&app, |a| a.cloud_sign_out()));
    b.on_cloud_refresh(cb!(&app, |a| a.cloud_refresh()));
    b.on_new_camera(cb!(&app, |a| a.new_camera(CameraConfig::default())));
    b.on_edit_camera(cb!(&app, |a, id| a.edit_camera(id as u32)));
    b.on_save_camera(cb!(&app, |a| a.save_camera()));
    b.on_delete_camera(cb!(&app, |a, id| a.delete_camera(id as u32)));
    b.on_move_camera(cb!(&app, |a, id, dir| a.move_camera(id as u32, dir)));
    b.on_new_hub(cb!(&app, |a| a.new_hub()));
    b.on_edit_hub(cb!(&app, |a, id| a.edit_hub(id as u32)));
    b.on_save_hub(cb!(&app, |a| a.save_hub()));
    b.on_delete_hub(cb!(&app, |a, id| a.delete_hub(id as u32)));
    b.on_scan(cb!(&app, |a| a.scan()));
    b.on_add_found(cb!(&app, |a, i| a.add_found(i as usize)));

    b.on_connect_hub(cb!(&app, |a| a.connect_hub()));
    b.on_rec_camera_changed(cb!(&app, |a| a.load_recordings()));
    b.on_change_day(cb!(&app, |a, d| a.change_day(d)));
    b.on_download(cb!(&app, |a, i| a.download(i as usize)));
    b.on_open_file(|p| open_path(Path::new(p.as_str())));
    b.on_show_in_folder(|p| show_in_folder(Path::new(p.as_str())));

    app.load_settings_into_ui();
    app.refresh_cloud_ui();
    app.refresh_camera_lists();
    app.refresh_hub_lists();
    app.update_rec_date_label();
    if app.cfg.borrow().cameras.is_empty() {
        b.set_page(Page::Settings);
    }
    app.sync_streams();

    // First start with a saved account but no cloud session (a new install, or
    // settings from the RTSP version): sign in, which also links the cameras.
    // Not after "Sign out", which leaves the cameras linked.
    let signed_in = app.cloud_client().is_some_and(|c| c.session().is_some());
    let none_linked = app.cfg.borrow().cameras.iter().all(|c| c.cloud.is_none());
    if !signed_in && none_linked && app.cloud_creds().is_some_and(|(_, p)| !p.is_empty()) {
        app.cloud_sign_in();
    }

    app.timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), cb!(&app, |a| a.pump()));
}

impl App {
    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("UI is alive while the event loop runs")
    }

    fn status(&self, msg: impl Into<SharedString>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.ui().global::<Bridge>().set_status_text(msg);
        *self.status_until.borrow_mut() = Some(Instant::now() + Duration::from_secs(8));
    }

    fn cloud_creds(&self) -> Option<(String, String)> {
        let cfg = self.cfg.borrow();
        let user = cfg.cloud_username.trim().to_string();
        (!user.is_empty()).then(|| (user, cfg.cloud_password.reveal()))
    }

    fn persist(&self) {
        if let Err(e) = config::save(&self.cfg.borrow()) {
            self.status(format!("Could not save settings: {e:#}"));
        }
    }

    // ---------------------------------------------------------------- frames

    /// Runs every ~30 ms: moves new frames and status into the UI.
    fn pump(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let b = ui.global::<Bridge>();
        match b.get_page() {
            Page::Live => {
                self.update_columns(&ui);
                self.pump_tiles();
            }
            Page::Single => self.pump_single(&b),
            _ => {}
        }
        let expired = self.status_until.borrow().is_some_and(|t| Instant::now() >= t);
        if expired {
            *self.status_until.borrow_mut() = None;
            b.set_status_text(SharedString::new());
        }
    }

    fn pump_tiles(&self) {
        let cfg = self.cfg.borrow();
        let mut streams = self.streams.borrow_mut();
        for (i, cam) in cfg.cameras.iter().enumerate() {
            let Some(live) = streams.get_mut(&(cam.id, cam.grid_quality)) else { continue };
            let (frame, version, status, is_live) = {
                let slot = live.handle.slot.lock().unwrap();
                let frame = (slot.version != live.shown_version).then(|| slot.frame.clone()).flatten();
                (frame, slot.version, slot.status_text(), slot.is_live())
            };
            if frame.is_none() && status == live.shown_status {
                continue;
            }
            let Some(mut tile) = self.tiles.row_data(i) else { continue };
            if let Some(frame) = frame {
                tile.frame = Image::from_rgb8(frame);
                tile.has_frame = true;
                live.shown_version = version;
            }
            tile.status = status.as_str().into();
            tile.live = is_live;
            live.shown_status = status;
            self.tiles.set_row_data(i, tile);
        }
    }

    fn pump_single(&self, b: &Bridge) {
        let Some(key) = *self.single.borrow() else { return };
        let streams = self.streams.borrow();
        let Some(live) = streams.get(&key) else { return };
        let mut shown = self.single_shown.borrow_mut();
        let slot = live.handle.slot.lock().unwrap();
        if slot.version != shown.0 {
            if let Some(frame) = slot.frame.clone() {
                b.set_single_frame(Image::from_rgb8(frame));
                b.set_single_has_frame(true);
            }
            shown.0 = slot.version;
        }
        let status = slot.status_text();
        if status != shown.1 {
            b.set_single_status(status.as_str().into());
            shown.1 = status;
        }
    }

    /// Picks the column count that gives the biggest 16:9 tiles.
    fn update_columns(&self, ui: &MainWindow) {
        let n = self.tiles.row_count();
        let window = ui.window();
        let size = window.size().to_logical(window.scale_factor());
        let fullscreen = ui.global::<Bridge>().get_fullscreen();
        let chrome = if fullscreen { 8.0 } else { 50.0 + 26.0 + 20.0 };
        let (w, h) = (size.width - 20.0, size.height - chrome);
        let key = (n, w as i32, h as i32);
        if *self.last_grid_size.borrow() == key || n == 0 {
            return;
        }
        *self.last_grid_size.borrow_mut() = key;
        let gap = 10.0;
        let best = (1..=n)
            .max_by(|&a, &b| {
                let cell = |cols: usize| {
                    let rows = n.div_ceil(cols);
                    let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
                    let ch = (h - gap * (rows as f32 - 1.0)) / rows as f32;
                    cw.min(ch * 16.0 / 9.0)
                };
                cell(a).total_cmp(&cell(b)).then(b.cmp(&a)) // prefer fewer columns on ties
            })
            .unwrap_or(1);
        ui.global::<Bridge>().set_columns(best as i32);
    }

    /// Starts the streams the current page needs and stops the rest.
    fn sync_streams(&self) {
        let page = self.ui().global::<Bridge>().get_page();
        let cfg = self.cfg.borrow();
        let wanted: Vec<StreamKey> = match page {
            Page::Live => cfg.cameras.iter().map(|c| (c.id, c.grid_quality)).collect(),
            Page::Single => self.single.borrow().iter().copied().collect(),
            _ => Vec::new(),
        };
        let mut streams = self.streams.borrow_mut();
        streams.retain(|k, _| wanted.contains(k));
        for key in wanted {
            if streams.contains_key(&key) {
                continue;
            }
            // Cameras not linked to the account have nothing to stream; their
            // tile says so (see `tile_status`).
            if let Some(spec) = cfg.camera(key.0).and_then(|cam| self.spec(cam, key.1)) {
                let handle = StreamHandle::spawn(&self.rt, spec);
                streams.insert(key, Live { handle, shown_version: 0, shown_status: String::new() });
            }
        }
    }

    fn spec(&self, cam: &CameraConfig, quality: Quality) -> Option<StreamSpec> {
        Some(StreamSpec {
            quality,
            ffmpeg: self.ffmpeg.borrow().clone(),
            client: self.cloud_client()?,
            camera: cam.cloud.clone()?,
        })
    }

    /// What a camera's tile shows before its stream reports anything.
    fn tile_status(&self, cam: &CameraConfig) -> String {
        match (&self.cloud, &cam.cloud) {
            (Err(e), _) => format!("Tapo cloud unavailable: {e}"),
            (Ok(client), _) if client.session().is_none() => "Not signed in: sign in with your Tapo account in Settings".into(),
            (Ok(_), None) => "Not linked to a camera on your Tapo account: edit it in Settings".into(),
            (Ok(_), Some(_)) => "Connecting…".into(),
        }
    }

    fn cloud_client(&self) -> Option<Arc<CloudClient>> {
        self.cloud.as_ref().ok().cloned()
    }

    /// Reconnects every camera, e.g. after signing in or out.
    fn restart_streams(&self) {
        self.streams.borrow_mut().clear();
        self.sync_streams();
    }

    fn restart_camera(&self, id: u32) {
        self.streams.borrow_mut().retain(|k, _| k.0 != id);
        self.ptz.borrow_mut().remove(&id);
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&self, page: Page) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_fullscreen() && page != b.get_page() {
            self.set_fullscreen(false);
        }
        if page != Page::Single {
            *self.single.borrow_mut() = None;
        }
        if page == Page::Live {
            // Force tiles to pick up the latest frame again.
            for live in self.streams.borrow_mut().values_mut() {
                live.shown_version = 0;
                live.shown_status.clear();
            }
        }
        b.set_page(page);
        self.sync_streams();
    }

    fn set_fullscreen(&self, on: bool) {
        let ui = self.ui();
        ui.window().set_fullscreen(on);
        ui.global::<Bridge>().set_fullscreen(on);
    }

    fn toggle_fullscreen(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if matches!(b.get_page(), Page::Live | Page::Single) || b.get_fullscreen() {
            self.set_fullscreen(!b.get_fullscreen());
        }
    }

    fn escape(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_fullscreen() {
            self.set_fullscreen(false);
        } else if b.get_page() == Page::Single {
            self.navigate(Page::Live);
        }
    }

    fn open_camera(&self, id: u32) {
        let Some(cam) = self.cfg.borrow().camera(id).cloned() else { return };
        let quality = Quality::Hd;
        *self.single.borrow_mut() = Some((id, quality));
        *self.single_shown.borrow_mut() = (0, String::new());

        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_single_name(cam.name.as_str().into());
        b.set_single_quality(quality.index());
        b.set_single_frame(Image::default());
        b.set_single_has_frame(false);
        b.set_single_status(self.tile_status(&cam).into());
        // Pan/tilt goes through the camera's local API, so it needs an IP.
        let ptz = cam.ptz && cam.has_ip();
        b.set_single_ptz(ptz);
        b.set_presets(ModelRc::default());
        b.set_ptz_message(SharedString::new());
        b.set_ptz_busy(false);
        b.set_page(Page::Single);
        self.sync_streams();

        if ptz {
            if self.ptz.borrow().contains_key(&id) {
                self.ptz_changed(id);
            } else {
                self.ptz_send(PtzCmd::Refresh);
            }
        }
    }

    fn quality_changed(&self, index: i32) {
        let Some((id, _)) = *self.single.borrow() else { return };
        let quality = Quality::from_index(index);
        *self.single.borrow_mut() = Some((id, quality));
        *self.single_shown.borrow_mut() = (0, String::new());
        self.ui().global::<Bridge>().set_single_has_frame(false);
        self.sync_streams();
    }

    fn snapshot(&self) {
        let Some(key) = *self.single.borrow() else { return };
        let frame = self.streams.borrow().get(&key).and_then(|l| l.handle.latest_frame());
        let Some(frame) = frame else {
            self.status("No picture yet.");
            return;
        };
        let name = self.cfg.borrow().camera(key.0).map(|c| c.name.clone()).unwrap_or_default();
        let path = config::snapshot_dir()
            .join(format!("{}_{}.png", file_safe(&name), Local::now().format("%Y-%m-%d_%H-%M-%S")));
        self.rt.spawn_blocking(move || {
            let result = (|| -> anyhow::Result<()> {
                std::fs::create_dir_all(path.parent().unwrap())?;
                let img = image::RgbImage::from_raw(frame.width(), frame.height(), frame.as_bytes().to_vec())
                    .ok_or_else(|| anyhow::anyhow!("bad frame"))?;
                img.save(&path)?;
                Ok(())
            })();
            on_ui(move |app| match result {
                Ok(()) => app.status(format!("Snapshot saved to {}", path.display())),
                Err(e) => app.status(format!("Snapshot failed: {e:#}")),
            });
        });
    }

    // ------------------------------------------------------------------- PTZ

    fn ptz_send(&self, cmd: PtzCmd) {
        let Some((id, _)) = *self.single.borrow() else { return };
        let mut ptz = self.ptz.borrow_mut();
        let controller = match ptz.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let Some(cam) = self.cfg.borrow().camera(id).cloned().filter(CameraConfig::has_ip) else {
                    return;
                };
                let (user, pass) = self.cloud_creds().unwrap_or_default();
                slot.insert(Ptz::spawn(&self.rt, id, cam.host, user, pass))
            }
        };
        controller.send(cmd);
    }

    pub fn ptz_changed(&self, camera_id: u32) {
        if self.single.borrow().map(|k| k.0) != Some(camera_id) {
            return;
        }
        let ptz = self.ptz.borrow();
        let Some(p) = ptz.get(&camera_id) else { return };
        let state = p.state.lock().unwrap();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_ptz_busy(state.busy);
        b.set_ptz_message(state.message.as_str().into());
        let rows: Vec<PresetRow> = state
            .presets
            .iter()
            .map(|(id, name)| PresetRow { id: id.into(), name: name.into() })
            .collect();
        b.set_presets(ModelRc::new(VecModel::from(rows)));
    }

    // -------------------------------------------------------------- settings

    fn load_settings_into_ui(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let cfg = self.cfg.borrow();
        b.set_cloud_user(cfg.cloud_username.as_str().into());
        b.set_cloud_pass(cfg.cloud_password.reveal().into());
        b.set_ffmpeg_path(cfg.ffmpeg_path.as_str().into());
        b.set_renderer_index(cfg.renderer.index());
        b.set_config_path(config::config_path().display().to_string().into());
        b.set_ffmpeg_found(
            match &*self.ffmpeg.borrow() {
                Some(p) if crate::ffmpeg::is_bundled(p) => "Using the built-in ffmpeg.".to_string(),
                Some(p) => format!("Using {}", p.display()),
                None => "ffmpeg not found: live view needs it. Download ffmpeg (e.g. gyan.dev or BtbN builds) \
                         and put ffmpeg.exe next to TapoViewer.exe."
                    .into(),
            }
            .into(),
        );
    }

    fn save_settings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        {
            let mut cfg = self.cfg.borrow_mut();
            cfg.cloud_username = b.get_cloud_user().trim().to_string();
            cfg.cloud_password = Secret::new(&b.get_cloud_pass());
            cfg.ffmpeg_path = b.get_ffmpeg_path().trim().to_string();
            let renderer = Renderer::from_index(b.get_renderer_index());
            if renderer != cfg.renderer {
                cfg.renderer = renderer;
                self.status("Renderer will change after restarting the app.");
            }
        }
        self.persist();
        let account_changed = {
            let cfg = self.cfg.borrow();
            !cfg.cloud.account.is_empty() && !cfg.cloud.account.eq_ignore_ascii_case(&cfg.cloud_username)
        };
        if account_changed {
            self.clear_cloud_session();
            b.set_cloud_message("Signed out of the Tapo cloud because the account changed.".into());
        }
        *self.ffmpeg.borrow_mut() = crate::ffmpeg::find(&self.cfg.borrow().ffmpeg_path);
        // New credentials or ffmpeg: reconnect everything that uses them.
        self.ptz.borrow_mut().clear();
        *self.hub.borrow_mut() = HubSession::default();
        b.set_hub_connected(false);
        b.set_hub_message(SharedString::new());
        self.streams.borrow_mut().clear();
        self.load_settings_into_ui();
        self.sync_streams();
        self.status("Settings saved.");
        b.set_account_message("✓ Saved".into());

        // Saving the account signs in, which also lists its cameras.
        let signed_in = self.cloud_client().is_some_and(|c| c.session().is_some());
        if self.cloud_creds().is_some() && !signed_in && !b.get_cloud_busy() && self.pending_mfa.borrow().is_none() {
            self.cloud_sign_in();
        }
    }

    fn refresh_camera_lists(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let cfg = self.cfg.borrow();
        let rows: Vec<CameraRow> = cfg
            .cameras
            .iter()
            .map(|c| {
                let mut detail = Vec::new();
                match &c.cloud {
                    Some(link) if link.model.is_empty() => detail.push(format!("Tapo: {}", link.name)),
                    Some(link) => detail.push(format!("Tapo: {} ({})", link.name, link.model)),
                    None => detail.push("⚠ not linked to your Tapo account".into()),
                }
                detail.push(format!("grid: {}", Quality::LABELS[c.grid_quality.index() as usize]));
                if c.ptz && c.has_ip() {
                    detail.push(format!("pan/tilt via {}", c.host));
                }
                CameraRow { id: c.id as i32, name: c.name.as_str().into(), detail: detail.join("  ·  ").into() }
            })
            .collect();
        b.set_cameras(ModelRc::new(VecModel::from(rows)));

        let tiles: Vec<Tile> = cfg
            .cameras
            .iter()
            .map(|c| Tile {
                id: c.id as i32,
                name: c.name.as_str().into(),
                frame: Image::default(),
                has_frame: false,
                status: self.tile_status(c).into(),
                live: false,
            })
            .collect();
        self.tiles.set_vec(tiles);
        *self.last_grid_size.borrow_mut() = Default::default();
        for live in self.streams.borrow_mut().values_mut() {
            live.shown_version = 0;
            live.shown_status.clear();
        }
    }

    fn new_camera(&self, cam: CameraConfig) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_cam_editing(if cam.id == 0 { 0 } else { cam.id as i32 });
        b.set_cam_name(cam.name.as_str().into());
        b.set_cam_host(cam.host.as_str().into());
        b.set_cam_ptz(cam.ptz);
        b.set_cam_quality(cam.grid_quality.index());
        self.set_cloud_choices(cam.cloud.as_ref().map(|l| l.device_id.as_str()));
        b.set_cam_error(SharedString::new());
    }

    /// Every camera the form can link to: the account's cameras from the last
    /// list, plus any already-linked camera not in it.
    fn known_cloud_cameras(&self) -> Vec<CloudCamera> {
        let mut list = self.cloud_cameras.borrow().clone();
        for cam in &self.cfg.borrow().cameras {
            if let Some(link) = &cam.cloud
                && !list.iter().any(|k| k.device_id == link.device_id)
            {
                list.push(link.clone());
            }
        }
        list
    }

    /// Fills the camera form's "Tapo camera" list and selects `selected`.
    fn set_cloud_choices(&self, selected: Option<&str>) {
        let list = self.known_cloud_cameras();
        let labels = std::iter::once("Not linked".to_string()).chain(list.iter().map(|c| match c.model.as_str() {
            "" => c.name.clone(),
            model => format!("{} ({model})", c.name),
        }));
        let index = selected.and_then(|id| list.iter().position(|c| c.device_id == id)).map_or(0, |i| i as i32 + 1);
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_cloud_camera_labels(string_model(labels));
        b.set_cam_cloud(index);
        *self.cloud_choices.borrow_mut() = list;
    }

    fn selected_cloud_camera(&self) -> Option<CloudCamera> {
        let index = self.ui().global::<Bridge>().get_cam_cloud();
        usize::try_from(index - 1).ok().and_then(|i| self.cloud_choices.borrow().get(i).cloned())
    }

    fn edit_camera(&self, id: u32) {
        if let Some(cam) = self.cfg.borrow().camera(id).cloned() {
            self.new_camera(cam);
        }
    }

    fn save_camera(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let host = b.get_cam_host().trim().to_string();
        let Some(cloud) = self.selected_cloud_camera() else {
            b.set_cam_error(
                "Pick the camera on your Tapo account. Sign in under “Tapo account” to list your cameras.".into(),
            );
            return;
        };
        let name = match b.get_cam_name().trim() {
            "" => cloud.name.clone(),
            n => n.to_string(),
        };
        let edit = b.get_cam_editing();
        let id = {
            let mut cfg = self.cfg.borrow_mut();
            let id = if edit > 0 { edit as u32 } else { cfg.next_camera_id() };
            let cam = CameraConfig {
                id,
                name,
                cloud: Some(cloud),
                host,
                ptz: b.get_cam_ptz(),
                grid_quality: Quality::from_index(b.get_cam_quality()),
            };
            match cfg.cameras.iter_mut().find(|c| c.id == id) {
                Some(existing) => *existing = cam,
                None => cfg.cameras.push(cam),
            }
            id
        };
        self.persist();
        self.restart_camera(id);
        b.set_cam_editing(-1);
        self.refresh_camera_lists();
        self.refresh_found();
        self.sync_streams();
        self.status("Camera saved.");
    }

    fn delete_camera(&self, id: u32) {
        self.cfg.borrow_mut().cameras.retain(|c| c.id != id);
        self.persist();
        self.restart_camera(id);
        let b_edit = self.ui().global::<Bridge>().get_cam_editing();
        if b_edit == id as i32 {
            self.ui().global::<Bridge>().set_cam_editing(-1);
        }
        self.refresh_camera_lists();
        self.refresh_found();
        self.sync_streams();
    }

    fn move_camera(&self, id: u32, dir: i32) {
        {
            let mut cfg = self.cfg.borrow_mut();
            let Some(i) = cfg.cameras.iter().position(|c| c.id == id) else { return };
            let j = i as i32 + dir;
            if j < 0 || j as usize >= cfg.cameras.len() {
                return;
            }
            cfg.cameras.swap(i, j as usize);
        }
        self.persist();
        self.refresh_camera_lists();
    }

    fn refresh_hub_lists(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let cfg = self.cfg.borrow();
        let rows: Vec<HubRow> = cfg
            .hubs
            .iter()
            .map(|h| HubRow { id: h.id as i32, name: h.name.as_str().into(), host: h.host.as_str().into() })
            .collect();
        b.set_hubs(ModelRc::new(VecModel::from(rows)));
        b.set_hub_names(string_model(cfg.hubs.iter().map(|h| format!("{} ({})", h.name, h.host))));
        if b.get_rec_hub() as usize >= cfg.hubs.len() {
            b.set_rec_hub(0);
        }
    }

    fn new_hub(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_hub_editing(0);
        b.set_hub_name(SharedString::new());
        b.set_hub_host(SharedString::new());
        b.set_hub_error(SharedString::new());
    }

    fn edit_hub(&self, id: u32) {
        let Some(hub) = self.cfg.borrow().hubs.iter().find(|h| h.id == id).cloned() else { return };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_hub_editing(id as i32);
        b.set_hub_name(hub.name.into());
        b.set_hub_host(hub.host.into());
        b.set_hub_error(SharedString::new());
    }

    fn save_hub(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let host = b.get_hub_host().trim().to_string();
        if host.is_empty() {
            b.set_hub_error("Enter the hub's IP address.".into());
            return;
        }
        let name = match b.get_hub_name().trim() {
            "" => "H500".to_string(),
            n => n.to_string(),
        };
        let edit = b.get_hub_editing();
        {
            let mut cfg = self.cfg.borrow_mut();
            let id = if edit > 0 { edit as u32 } else { cfg.next_hub_id() };
            let hub = HubConfig { id, name, host };
            match cfg.hubs.iter_mut().find(|h| h.id == id) {
                Some(existing) => *existing = hub,
                None => cfg.hubs.push(hub),
            }
        }
        self.persist();
        b.set_hub_editing(-1);
        self.refresh_hub_lists();
        self.refresh_found();
        self.status("Hub saved.");
    }

    fn delete_hub(&self, id: u32) {
        self.cfg.borrow_mut().hubs.retain(|h| h.id != id);
        self.persist();
        self.refresh_hub_lists();
        self.refresh_found();
    }

    // ------------------------------------------------------------ Tapo cloud

    fn cloud_message(&self, msg: impl Into<SharedString>) {
        self.ui().global::<Bridge>().set_cloud_message(msg.into());
    }

    fn set_cloud_busy(&self, busy: bool) {
        self.ui().global::<Bridge>().set_cloud_busy(busy);
    }

    fn refresh_cloud_ui(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let session = self.cloud_client().and_then(|c| c.session());
        b.set_cloud_signed_in(session.is_some());
        let status = match (&self.cloud, &session) {
            (Err(e), _) => format!("Tapo cloud unavailable: {e}"),
            (Ok(_), Some(s)) => {
                let region = url::Url::parse(&s.base_url).ok().and_then(|u| u.host_str().map(str::to_string));
                let account = self.cfg.borrow().cloud.account.clone();
                match region {
                    Some(region) => format!("Signed in as {account} ({region})"),
                    None => format!("Signed in as {account}"),
                }
            }
            (Ok(_), None) => "Not signed in".to_string(),
        };
        b.set_cloud_status(status.into());
        let pending = self.pending_mfa.borrow();
        let methods = pending.as_ref().map(|p| p.challenge.methods.clone()).unwrap_or_default();
        b.set_cloud_mfa(pending.is_some());
        b.set_cloud_mfa_app(methods.contains(&MfaMethod::App));
        b.set_cloud_mfa_email(methods.contains(&MfaMethod::Email));
        b.set_cloud_code_sent(pending.as_ref().is_some_and(|p| p.method.is_some()));
    }

    fn cloud_sign_in(&self) {
        let Some(client) = self.cloud_client() else {
            self.refresh_cloud_ui();
            return;
        };
        let Some((user, pass)) = self.cloud_creds().filter(|(_, p)| !p.is_empty()) else {
            self.cloud_message("Enter your Tapo email and password under “Tapo account” and press Save account first.");
            return;
        };
        *self.pending_mfa.borrow_mut() = None;
        self.set_cloud_busy(true);
        self.cloud_message("Signing in…");
        self.refresh_cloud_ui();
        self.rt.spawn(async move {
            let result = client.login(&user, &pass).await;
            on_ui(move |app| app.cloud_login_done(user, pass, result));
        });
    }

    fn cloud_login_done(&self, user: String, pass: String, result: cloud::Result<Login>) {
        self.set_cloud_busy(false);
        match result {
            Ok(Login::Done(session)) => self.cloud_signed_in(user, session),
            Ok(Login::NeedsMfa(challenge)) => {
                *self.pending_mfa.borrow_mut() = Some(PendingMfa { challenge, user, pass, method: None });
                self.ui().global::<Bridge>().set_cloud_code(SharedString::new());
                self.cloud_message("Choose where Tapo should send the verification code.");
                // It may have been started automatically: show where to answer.
                if self.ui().global::<Bridge>().get_page() != Page::Settings {
                    self.navigate(Page::Settings);
                    self.status("Tapo needs a verification code to sign in: see Settings › Tapo account.");
                }
            }
            Err(e) => self.cloud_message(format!("Sign-in failed: {e}")),
        }
        self.refresh_cloud_ui();
    }

    fn cloud_send_code(&self, method: i32) {
        let method = if method == 1 { MfaMethod::App } else { MfaMethod::Email };
        let Some(client) = self.cloud_client() else { return };
        let Some((challenge, user, pass)) =
            self.pending_mfa.borrow().as_ref().map(|p| (p.challenge.clone(), p.user.clone(), p.pass.clone()))
        else {
            return;
        };
        self.set_cloud_busy(true);
        self.cloud_message("Sending the code…");
        self.rt.spawn(async move {
            let result = client.request_mfa(&challenge, &user, &pass, method).await;
            on_ui(move |app| app.cloud_code_sent(method, result));
        });
    }

    fn cloud_code_sent(&self, method: MfaMethod, result: cloud::Result<()>) {
        self.set_cloud_busy(false);
        match result {
            Ok(()) => {
                if let Some(pending) = self.pending_mfa.borrow_mut().as_mut() {
                    pending.method = Some(method);
                }
                self.cloud_message(match method {
                    MfaMethod::App => "Code sent: open the Tapo app on your phone, then enter the code here.",
                    MfaMethod::Email => "Code sent to your email: enter it here.",
                });
            }
            Err(e) => self.cloud_message(format!("Could not send the code: {e}")),
        }
        self.refresh_cloud_ui();
    }

    fn cloud_verify(&self) {
        let code = self.ui().global::<Bridge>().get_cloud_code().trim().to_string();
        if code.is_empty() {
            self.cloud_message("Enter the verification code.");
            return;
        }
        let Some(client) = self.cloud_client() else { return };
        let pending = self.pending_mfa.borrow().as_ref().map(|p| (p.challenge.clone(), p.user.clone(), p.method));
        let Some((challenge, user, Some(method))) = pending else {
            self.cloud_message("Send a code first.");
            return;
        };
        self.set_cloud_busy(true);
        self.cloud_message("Checking the code…");
        self.rt.spawn(async move {
            let result = client.verify_mfa(&challenge, &user, &code, method).await;
            on_ui(move |app| app.cloud_verified(user, result));
        });
    }

    fn cloud_verified(&self, user: String, result: cloud::Result<Session>) {
        self.set_cloud_busy(false);
        match result {
            Ok(session) => self.cloud_signed_in(user, session),
            Err(e) => self.cloud_message(format!("Verification failed: {e}")),
        }
        self.refresh_cloud_ui();
    }

    fn save_cloud_session(&self, user: String, session: &Session) {
        {
            let mut cfg = self.cfg.borrow_mut();
            cfg.cloud.account = user;
            cfg.cloud.base_url = session.base_url.clone();
            cfg.cloud.token = Secret::new(&session.token);
        }
        self.persist();
    }

    fn cloud_signed_in(&self, user: String, session: Session) {
        self.save_cloud_session(user, &session);
        *self.pending_mfa.borrow_mut() = None;
        self.ui().global::<Bridge>().set_cloud_code(SharedString::new());
        self.refresh_cloud_ui();
        self.refresh_camera_lists();
        self.restart_streams();
        self.cloud_refresh();
    }

    /// Forgets the cloud session (camera links are kept).
    fn clear_cloud_session(&self) {
        if let Some(client) = self.cloud_client() {
            client.set_session(None);
        }
        {
            let mut cfg = self.cfg.borrow_mut();
            cfg.cloud.account.clear();
            cfg.cloud.base_url.clear();
            cfg.cloud.token = Secret::default();
        }
        self.persist();
        *self.pending_mfa.borrow_mut() = None;
        self.refresh_cloud_ui();
    }

    fn cloud_sign_out(&self) {
        self.clear_cloud_session();
        self.cloud_message("Signed out of the Tapo cloud.");
        self.refresh_camera_lists();
        self.restart_streams();
    }

    fn cloud_refresh(&self) {
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            self.cloud_message("Sign in first.");
            return;
        };
        self.set_cloud_busy(true);
        self.cloud_message("Loading the cameras on your Tapo account…");
        self.rt.spawn(async move {
            let result = client.list_cameras().await;
            on_ui(move |app| app.cloud_cameras_loaded(result));
        });
    }

    fn cloud_cameras_loaded(&self, result: cloud::Result<Vec<CloudCamera>>) {
        self.set_cloud_busy(false);
        let list = match result {
            Ok(list) => list,
            Err(e) if e.is_auth() => {
                self.cloud_session_lost();
                return;
            }
            Err(e) => {
                self.cloud_message(format!("Could not load the cameras on your Tapo account: {e}"));
                return;
            }
        };
        let (linked, added) = link_cloud_cameras(&mut self.cfg.borrow_mut().cameras, &list);
        *self.cloud_cameras.borrow_mut() = list.clone();
        self.persist();
        for &id in &linked {
            self.restart_camera(id);
        }
        // Keep the camera form's selection if it is open.
        let selected = self.selected_cloud_camera().map(|c| c.device_id);
        self.set_cloud_choices(selected.as_deref());
        self.refresh_camera_lists();
        self.sync_streams();

        let mut msg = format!("{} camera(s) on your Tapo account.", list.len());
        if !linked.is_empty() {
            msg += &format!(" Linked {} camera(s) you already had, matched by name.", linked.len());
        }
        if added > 0 {
            msg += &format!(" Added {added} camera(s).");
            let unlinked = self.cfg.borrow().cameras.iter().any(|c| c.cloud.is_none());
            if unlinked {
                msg += " If one of them duplicates a camera you already have, remove it and pick its \
                        Tapo camera in that camera's settings instead.";
            }
        }
        self.cloud_message(msg);
    }

    /// A stream found the session token rejected. Signs in again with the
    /// saved account; if the cloud wants a verification code, the user has to
    /// sign in under Settings.
    pub fn cloud_auth_failed(&self) {
        if self.pending_mfa.borrow().is_some() {
            return;
        }
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else { return };
        // Every cloud stream reports the same failure; act once a minute at most.
        if self.last_relogin.get().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        self.last_relogin.set(Some(Instant::now()));
        let Some((user, pass)) = self.cloud_creds().filter(|(_, p)| !p.is_empty()) else {
            self.cloud_session_lost();
            return;
        };
        log::info!("Tapo cloud session expired; signing in again");
        self.rt.spawn(async move {
            let result = client.login(&user, &pass).await;
            on_ui(move |app| app.cloud_relogin_done(user, result));
        });
    }

    fn cloud_relogin_done(&self, user: String, result: cloud::Result<Login>) {
        match result {
            Ok(Login::Done(session)) => {
                self.save_cloud_session(user, &session);
                self.refresh_cloud_ui();
                self.status("Signed in to the Tapo cloud again.");
            }
            Ok(Login::NeedsMfa(_)) => self.cloud_session_lost(),
            Err(e) if e.is_auth() || e.is_refused() => self.cloud_session_lost(),
            // A network problem: keep the session; the streams retry.
            Err(e) => log::warn!("Tapo cloud sign-in failed: {e}"),
        }
    }

    fn cloud_session_lost(&self) {
        self.clear_cloud_session();
        let msg = "Your Tapo cloud sign-in has expired: sign in again under Settings › Tapo account.";
        self.cloud_message(msg);
        self.status(msg);
    }

    // ------------------------------------------------------------- discovery

    fn scan(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let Some((user, pass)) = self.cloud_creds() else {
            b.set_scan_message("Enter and save your Tapo account first.".into());
            return;
        };
        let target = b.get_scan_target().trim().to_string();
        b.set_scanning(true);
        b.set_scan_message("Scanning…".into());
        self.found.borrow_mut().clear();
        self.refresh_found();
        self.rt.spawn(async move {
            let result = discover(user, pass, target).await;
            on_ui(move |app| app.scan_done(result));
        });
    }

    fn scan_done(&self, result: Result<(Vec<Found>, usize), String>) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_scanning(false);
        match result {
            Ok((mut found, failed)) => {
                found.sort_by_key(|f| match f.kind {
                    FoundKind::Camera | FoundKind::CameraPtz => 0,
                    FoundKind::Hub => 1,
                    FoundKind::Other => 2,
                });
                let useful = found.iter().filter(|f| f.kind != FoundKind::Other).count();
                let mut msg = format!("Found {useful} camera(s)/hub(s), {} device(s) in total.", found.len());
                if failed > 0 {
                    msg += &format!(" {failed} device(s) answered but could not be logged in to.");
                }
                b.set_scan_message(msg.into());
                *self.found.borrow_mut() = found;
            }
            Err(e) => b.set_scan_message(format!("Scan failed: {e}").into()),
        }
        self.refresh_found();
    }

    fn refresh_found(&self) {
        let cfg = self.cfg.borrow();
        let rows: Vec<FoundRow> = self
            .found
            .borrow()
            .iter()
            .map(|f| {
                let added = cfg.cameras.iter().any(|c| c.host == f.host) || cfg.hubs.iter().any(|h| h.host == f.host);
                let kind = match f.kind {
                    FoundKind::Camera => "camera",
                    FoundKind::CameraPtz => "pan/tilt camera",
                    FoundKind::Hub => "hub",
                    FoundKind::Other => "other device",
                };
                FoundRow {
                    name: f.name.as_str().into(),
                    model: f.model.as_str().into(),
                    host: f.host.as_str().into(),
                    kind: if added { format!("{kind} (added)") } else { kind.to_string() }.into(),
                    can_add: !added && f.kind != FoundKind::Other,
                }
            })
            .collect();
        self.ui().global::<Bridge>().set_found(ModelRc::new(VecModel::from(rows)));
    }

    fn add_found(&self, index: usize) {
        let Some(f) = self.found.borrow().get(index).cloned() else { return };
        match f.kind {
            FoundKind::Camera | FoundKind::CameraPtz => {
                // Usually the camera is already here from the Tapo account:
                // fill in its IP (for pan/tilt) instead of adding it twice.
                let existing = self.cfg.borrow().cameras.iter().find(|c| same_name(&c.name, &f.name)).cloned();
                let cloud = self.cloud_cameras.borrow().iter().find(|c| same_name(&c.name, &f.name)).cloned();
                let cam = match existing {
                    Some(cam) => CameraConfig { host: f.host.clone(), ptz: f.kind == FoundKind::CameraPtz, ..cam },
                    None => CameraConfig {
                        name: f.name.clone(),
                        host: f.host.clone(),
                        ptz: f.kind == FoundKind::CameraPtz,
                        cloud,
                        ..Default::default()
                    },
                };
                self.new_camera(cam);
                self.status(format!("Check {} in the camera form above, then press Save camera.", f.name));
            }
            FoundKind::Hub => {
                {
                    let mut cfg = self.cfg.borrow_mut();
                    let id = cfg.next_hub_id();
                    cfg.hubs.push(HubConfig { id, name: f.name.clone(), host: f.host.clone() });
                }
                self.persist();
                self.refresh_hub_lists();
                self.refresh_found();
                self.status(format!("Added hub {}.", f.name));
            }
            FoundKind::Other => {}
        }
    }

    // ------------------------------------------------------------ recordings

    fn connect_hub(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let Some(hub) = self.cfg.borrow().hubs.get(b.get_rec_hub().max(0) as usize).cloned() else { return };
        let Some((user, pass)) = self.cloud_creds() else {
            b.set_hub_message("Enter and save your Tapo account in Settings first.".into());
            return;
        };
        let generation = {
            let mut hub_session = self.hub.borrow_mut();
            *hub_session = HubSession { generation: hub_session.generation + 1, ..Default::default() };
            hub_session.generation
        };
        b.set_hub_busy(true);
        b.set_hub_connected(false);
        b.set_hub_message(format!("Connecting to {}…", hub.host).into());
        b.set_recordings(ModelRc::default());
        self.rt.spawn(async move {
            let result = async {
                let handler = ApiClient::new(user, pass)
                    .with_timeout(Duration::from_secs(20))
                    .h500(hub.host.clone())
                    .await?;
                let paired = handler.get_general_device_list().await?;
                Ok::<_, tapo::Error>((Arc::new(handler), paired))
            }
            .await
            .map_err(|e| e.to_string());
            on_ui(move |app| app.hub_connected(generation, result));
        });
    }

    fn hub_connected(
        &self,
        generation: u64,
        result: Result<(Arc<CameraHubHandler>, Vec<GeneralDeviceHubResult>), String>,
    ) {
        if self.hub.borrow().generation != generation {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_hub_busy(false);
        match result {
            Ok((handler, paired)) => {
                b.set_hub_message(format!("Connected · {} camera(s) paired", paired.len()).into());
                b.set_paired_names(string_model(paired.iter().map(|p| format!("{} ({})", p.alias, p.device_model))));
                b.set_rec_camera(0);
                b.set_hub_connected(true);
                let mut hub = self.hub.borrow_mut();
                hub.handler = Some(handler);
                hub.paired = paired;
            }
            Err(e) => b.set_hub_message(format!("Could not connect: {e}").into()),
        }
        self.load_recordings();
    }

    fn change_day(&self, delta: i32) {
        let today = Local::now().date_naive();
        let mut date = self.rec_date.borrow_mut();
        *date = match delta {
            0 => today,
            d => (*date + chrono::Duration::days(d as i64)).min(today),
        };
        drop(date);
        self.update_rec_date_label();
        self.load_recordings();
    }

    fn update_rec_date_label(&self) {
        let date = *self.rec_date.borrow();
        let label = if date == Local::now().date_naive() {
            format!("Today, {}", date.format("%d %b"))
        } else {
            date.format("%a %d %b %Y").to_string()
        };
        self.ui().global::<Bridge>().set_rec_date(label.into());
    }

    fn load_recordings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let (handler, device_id, request) = {
            let mut hub = self.hub.borrow_mut();
            let Some(handler) = hub.handler.clone() else { return };
            let Some(cam) = hub.paired.get(b.get_rec_camera().max(0) as usize) else {
                b.set_recordings(ModelRc::default());
                return;
            };
            let device_id = cam.device_id.clone();
            hub.rec_request += 1;
            hub.recordings.clear();
            (handler, device_id, hub.rec_request)
        };
        let date = *self.rec_date.borrow();
        let start = Local
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap_or_else(Local::now)
            .with_timezone(&Utc);
        let end = start + chrono::Duration::days(1) - chrono::Duration::seconds(1);

        b.set_rec_loading(true);
        b.set_recordings(ModelRc::default());
        self.rt.spawn(async move {
            let result = handler.get_recordings(device_id, start, end).await.map_err(|e| e.to_string());
            on_ui(move |app| app.recordings_loaded(request, result));
        });
    }

    fn recordings_loaded(&self, request: u64, result: Result<Vec<RecordingHubResult>, String>) {
        if self.hub.borrow().rec_request != request {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_rec_loading(false);
        match result {
            Ok(mut recs) => {
                recs.sort_by_key(|r| std::cmp::Reverse(r.start_time)); // newest first
                let rows: Vec<RecRow> = recs
                    .iter()
                    .map(|r| RecRow {
                        time: format!(
                            "{} – {}",
                            r.start_time.with_timezone(&Local).format("%H:%M:%S"),
                            r.end_time.with_timezone(&Local).format("%H:%M:%S")
                        )
                        .into(),
                        kind: humanize(&format!("{:?}", r.video_type)).into(),
                        duration: fmt_duration((r.end_time - r.start_time).num_seconds().max(0) as u64).into(),
                    })
                    .collect();
                b.set_recordings(ModelRc::new(VecModel::from(rows)));
                b.set_hub_message(format!("{} recording(s)", recs.len()).into());
                self.hub.borrow_mut().recordings = recs;
            }
            Err(e) => b.set_hub_message(format!("Could not list recordings: {e}").into()),
        }
    }

    fn download(&self, index: usize) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let (handler, cam, rec) = {
            let hub = self.hub.borrow();
            let (Some(handler), Some(cam), Some(rec)) = (
                hub.handler.clone(),
                hub.paired.get(b.get_rec_camera().max(0) as usize).cloned(),
                hub.recordings.get(index).cloned(),
            ) else {
                return;
            };
            (handler, cam, rec)
        };
        let local_start = rec.start_time.with_timezone(&Local);
        let path = config::recordings_dir()
            .join(format!("{}_{}.ts", file_safe(&cam.alias), local_start.format("%Y-%m-%d_%H-%M-%S")));
        let label = format!("{} · {}", cam.alias, local_start.format("%d %b %H:%M:%S"));

        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            self.downloads.push(DownloadRow {
                label: label.into(),
                status: "Already downloaded".into(),
                path: path.display().to_string().into(),
                done: true,
            });
            open_path(&path);
            return;
        }

        let row = self.downloads.row_count();
        self.downloads.push(DownloadRow {
            label: label.into(),
            status: "Downloading…".into(),
            path: path.display().to_string().into(),
            done: false,
        });
        self.rt.spawn(async move {
            let result = async {
                use tokio::io::AsyncWriteExt;
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                let mut file = tokio::fs::File::create(&path).await?;
                let r = handler.download_recording(cam.device_id, rec.start_time, rec.end_time, &mut file).await?;
                file.flush().await?;
                Ok::<_, anyhow::Error>(r)
            }
            .await;
            if result.is_err() {
                let _ = tokio::fs::remove_file(&path).await;
            }
            on_ui(move |app| {
                let Some(mut d) = app.downloads.row_data(row) else { return };
                match result {
                    Ok(r) => {
                        let mb = r.byte_count as f64 / 1_048_576.0;
                        d.status = match r.duration_s {
                            Some(s) => format!("Done · {mb:.1} MB · {}", fmt_duration(s as u64)),
                            None => format!("Done · {mb:.1} MB"),
                        }
                        .into();
                        d.done = true;
                        open_path(&path);
                    }
                    Err(e) => d.status = format!("Failed: {e:#}").into(),
                }
                app.downloads.set_row_data(row, d);
            });
        });
    }
}

async fn discover(user: String, pass: String, target: String) -> Result<(Vec<Found>, usize), String> {
    let mut discovery = ApiClient::new(user, pass)
        .discover_devices(target, 5)
        .await
        .map_err(|e| e.to_string())?;
    let mut found = Vec::new();
    let mut failed = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while let Ok(Some(item)) = tokio::time::timeout_at(deadline, discovery.next()).await {
        match item {
            Ok(r) => {
                let kind = match r.device_type() {
                    DeviceType::Camera => FoundKind::Camera,
                    DeviceType::CameraPtz => FoundKind::CameraPtz,
                    DeviceType::CameraHub => FoundKind::Hub,
                    _ => FoundKind::Other,
                };
                let (host, _port) = r.address();
                let name = match r.nickname() {
                    "" => r.model().to_string(),
                    n => n.to_string(),
                };
                found.push(Found { name, model: r.model().to_string(), host, kind });
            }
            Err(e) => {
                log::warn!("discovery: {e}");
                failed += 1;
            }
        }
    }
    Ok((found, failed))
}

/// Camera names compare loosely: the Tapo app's nickname is what both
/// "Find devices" and the cloud list use.
fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Links the account's cameras to the configured ones: by cloud id, then by
/// a unique name match; any left over are added as cloud-only cameras.
/// Returns the ids of newly linked cameras and how many were added.
fn link_cloud_cameras(cameras: &mut Vec<CameraConfig>, account: &[CloudCamera]) -> (Vec<u32>, usize) {
    let mut linked = Vec::new();
    let mut added = 0;
    for cloud_cam in account {
        let id_match = cameras.iter_mut().find(|c| c.cloud.as_ref().is_some_and(|l| l.device_id == cloud_cam.device_id));
        if let Some(cam) = id_match {
            cam.cloud = Some(cloud_cam.clone()); // refresh its name and model
            continue;
        }
        let by_name: Vec<usize> = cameras
            .iter()
            .enumerate()
            .filter(|(_, c)| c.cloud.is_none() && same_name(&c.name, &cloud_cam.name))
            .map(|(i, _)| i)
            .collect();
        if let [i] = by_name[..] {
            cameras[i].cloud = Some(cloud_cam.clone());
            linked.push(cameras[i].id);
            continue;
        }
        let id = cameras.iter().map(|c| c.id).max().unwrap_or(0) + 1;
        cameras.push(CameraConfig {
            id,
            name: cloud_cam.name.clone(),
            grid_quality: Quality::Sd,
            cloud: Some(cloud_cam.clone()),
            ..Default::default()
        });
        added += 1;
    }
    (linked, added)
}

fn string_model<S: Into<SharedString>>(items: impl IntoIterator<Item = S>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.into_iter().map(Into::into).collect::<Vec<SharedString>>()))
}

fn file_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "camera".into() } else { s }
}

/// "LineCrossing" -> "Line crossing"
fn humanize(variant: &str) -> String {
    let mut out = String::new();
    for (i, c) in variant.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn fmt_duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

fn open_path(path: &Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = r {
        log::warn!("could not open {}: {e}", path.display());
    }
}

fn show_in_folder(path: &Path) {
    #[cfg(windows)]
    let r = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn()
    };
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
    if let Err(e) = r {
        log::warn!("could not show {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(humanize("LineCrossing"), "Line crossing");
        assert_eq!(humanize("Motion"), "Motion");
        assert_eq!(fmt_duration(65), "1m 05s");
        assert_eq!(fmt_duration(3700), "1h 01m");
        assert_eq!(file_safe("Front door/1"), "Front_door_1");
    }

    #[test]
    fn links_account_cameras() {
        let cloud = |id: &str, name: &str| CloudCamera { device_id: id.into(), name: name.into(), ..Default::default() };
        let local = |id: u32, name: &str| CameraConfig { id, name: name.into(), host: "10.0.0.1".into(), ..Default::default() };
        let mut cameras = vec![
            local(1, "Front Door"),
            local(2, "Garage"),
            // Already linked: only its details are refreshed.
            CameraConfig { cloud: Some(cloud("c3", "Old name")), ..local(3, "Yard") },
            // Two cameras share this name, so neither is linked by name.
            local(4, "Hall"),
            local(5, "hall"),
        ];
        let account = [cloud("c1", "front door "), cloud("c3", "Yard"), cloud("c4", "Hall"), cloud("c9", "Office")];
        let (linked, added) = link_cloud_cameras(&mut cameras, &account);
        assert_eq!(linked, vec![1]);
        assert_eq!(added, 2); // "Hall" (ambiguous) and "Office"
        assert_eq!(cameras[0].cloud.as_ref().unwrap().device_id, "c1");
        assert!(cameras[1].cloud.is_none());
        assert_eq!(cameras[2].cloud.as_ref().unwrap().name, "Yard");
        assert!(cameras[3].cloud.is_none() && cameras[4].cloud.is_none());
        let office = cameras.iter().find(|c| c.name == "Office").unwrap();
        assert!(!office.has_ip());
        assert_eq!(office.id, 7);
        // Running it again changes nothing.
        let (linked, added) = link_cloud_cameras(&mut cameras, &account);
        assert!(linked.is_empty() && added == 0);
    }
}
