//! Application state and the glue between the Slint UI and the background
//! work (streams, PTZ, hub recordings). All of it lives on the UI thread;
//! background tasks report back through [`on_ui`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};

use crate::clip::{self, ClipRequest};
use crate::cloud::{self, Clip, CloudCamera, CloudClient, HubCamera, Login, MfaChallenge, MfaMethod, Session};
use crate::config::{self, AppConfig, CameraConfig, Quality, Renderer, Theme};
use crate::player::{Player, Source};
use crate::ptz::{Ptz, PtzCmd};
use crate::secret::Secret;
use crate::stream::{Frame, StreamHandle, StreamSpec};
use crate::{
    Bridge, CameraRow, DayChip, DownloadRow, HomeCard, HubRow, MainWindow, Page, PresetRow, RecRow, Tile,
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

/// The Playback page: one hub's cameras and one day of a camera's clips.
#[derive(Default)]
struct HubSession {
    /// Bumped on every (re)load so stale results are ignored.
    generation: u64,
    hub: Option<CloudCamera>,
    cameras: Vec<HubCamera>,
    /// Newest first, as listed.
    clips: Vec<Clip>,
    /// Days with recordings around the selected day.
    days: Vec<NaiveDate>,
    rec_request: u64,
    days_request: u64,
}

/// The recording in the player.
struct Playing {
    player: Player,
    camera_id: String,
    clip: Clip,
    path: PathBuf,
    request: ClipRequest,
    shown_version: u64,
    /// When the first frame showed, for the position.
    started: Option<Instant>,
    shown_position: String,
}

/// A Tapo cloud sign-in waiting for its verification code.
struct PendingMfa {
    challenge: MfaChallenge,
    user: String,
    pass: String,
    /// "Remember me" on the log-in card.
    remember: bool,
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
    hub: RefCell<HubSession>,
    /// Identifies this PC's player to the hubs: clip searches and downloads
    /// must use the same one.
    player_id: String,
    playing: RefCell<Option<Playing>>,
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
    /// The Tapo password for this run, when "Remember me" was off.
    password: RefCell<String>,
    /// The latest picture of each camera, for its Home card.
    thumbnails: RefCell<HashMap<u32, Frame>>,
    /// Home card badges, e.g. "Offline".
    home_states: RefCell<HashMap<u32, String>>,
    home_cards: Rc<VecModel<HomeCard>>,
    /// Where the camera view's back button goes.
    return_page: Cell<Page>,
    /// A camera (cloud device id or name) to select once the hub's cameras
    /// are listed.
    pending_rec_camera: RefCell<Option<(String, String)>>,
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
        hub: RefCell::default(),
        player_id: uuid::Uuid::new_v4().to_string(),
        playing: RefCell::default(),
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
        password: RefCell::default(),
        thumbnails: RefCell::default(),
        home_states: RefCell::default(),
        home_cards: Rc::new(VecModel::default()),
        return_page: Cell::new(Page::Home),
        pending_rec_camera: RefCell::default(),
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    if new_terminal {
        app.persist();
    }

    let b = ui.global::<Bridge>();
    b.set_tiles(app.tiles.clone().into());
    b.set_home_cameras(app.home_cards.clone().into());
    ui.invoke_set_theme(app.cfg.borrow().theme.index());
    b.set_downloads(app.downloads.clone().into());
    b.set_quality_labels(string_model(Quality::LABELS.iter().copied()));

    b.on_navigate(cb!(&app, |a, page| a.navigate(page)));
    b.on_toggle_fullscreen(cb!(&app, |a| a.toggle_fullscreen()));
    b.on_escape(cb!(&app, |a| a.escape()));
    b.on_open_camera(cb!(&app, |a, id| a.open_camera(id as u32)));
    b.on_open_hub(cb!(&app, |a, i| a.open_hub(i)));
    b.on_single_back(cb!(&app, |a| a.single_back()));
    b.on_privacy_toggle(cb!(&app, |a| a.privacy_toggle()));
    b.on_open_playback(cb!(&app, |a| a.open_playback()));
    b.on_quality_changed(cb!(&app, |a, i| a.quality_changed(i)));
    b.on_snapshot(cb!(&app, |a| a.snapshot()));
    b.on_ptz_move(cb!(&app, |a, pan, tilt| a.ptz_send(PtzCmd::Move(pan, tilt))));
    b.on_ptz_goto(cb!(&app, |a, id| a.ptz_send(PtzCmd::Goto(id.into()))));
    b.on_ptz_save(cb!(&app, |a, name| {
        a.ptz_send(PtzCmd::Save(name.trim().into()));
        a.ui().invoke_focus_keys(); // so the arrow keys pan again
    }));
    b.on_ptz_refresh(cb!(&app, |a| a.ptz_send(PtzCmd::Refresh)));

    b.on_save_settings(cb!(&app, |a| a.save_settings()));
    b.on_login(cb!(&app, |a| a.login()));
    b.on_mfa_send(cb!(&app, |a, m| a.mfa_send(m)));
    b.on_mfa_code_edited(cb!(&app, |a, text| a.mfa_code_edited(text)));
    b.on_mfa_verify(cb!(&app, |a| a.mfa_verify()));
    b.on_mfa_back(cb!(&app, |a| a.mfa_back()));
    b.on_cloud_sign_out(cb!(&app, |a| a.cloud_sign_out()));
    b.on_cloud_refresh(cb!(&app, |a| a.cloud_refresh()));
    b.on_new_camera(cb!(&app, |a| a.new_camera(CameraConfig::default())));
    b.on_edit_camera(cb!(&app, |a, id| a.edit_camera(id as u32)));
    b.on_save_camera(cb!(&app, |a| a.save_camera()));
    b.on_delete_camera(cb!(&app, |a, id| a.delete_camera(id as u32)));
    b.on_move_camera(cb!(&app, |a, id, dir| a.move_camera(id as u32, dir)));

    b.on_connect_hub(cb!(&app, |a| a.connect_hub()));
    b.on_rec_hub_changed(cb!(&app, |a| a.connect_hub()));
    b.on_rec_camera_changed(cb!(&app, |a| a.camera_picked()));
    b.on_change_day(cb!(&app, |a, d| a.change_day(d)));
    b.on_pick_day(cb!(&app, |a, i| a.pick_day(i)));
    b.on_play_clip(cb!(&app, |a, i| a.play_clip(i as usize)));
    b.on_download(cb!(&app, |a, i| a.download(i as usize)));
    b.on_player_stop(cb!(&app, |a| a.player_stop()));
    b.on_player_replay(cb!(&app, |a| a.player_replay()));
    b.on_open_file(|p| open_path(Path::new(p.as_str())));
    b.on_show_in_folder(|p| show_in_folder(Path::new(p.as_str())));

    app.load_settings_into_ui();
    app.refresh_cloud_ui();
    app.refresh_camera_lists();
    app.refresh_hub_lists();
    app.update_rec_date_label();
    app.sync_streams();

    // Not signed in: the sign-in screens show. With a remembered password
    // (e.g. settings from an earlier version), log in straight away.
    if !app.signed_in_now() {
        let (user, pass) = {
            let cfg = app.cfg.borrow();
            (cfg.cloud_username.trim().to_string(), cfg.cloud_password.reveal())
        };
        b.set_login_email(user.as_str().into());
        if let Err(e) = &app.cloud {
            app.show_sign_in(0, &format!("Tapo cloud unavailable: {e}"));
        } else if !user.is_empty() && !pass.is_empty() {
            b.set_login_password(pass.as_str().into());
            b.set_onboarding_step(1);
            app.start_login(user, pass, true);
        } else {
            app.show_sign_in(if user.is_empty() { 0 } else { 1 }, "");
        }
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

    /// The Tapo account: the saved password, or the one typed in this run.
    fn cloud_creds(&self) -> Option<(String, String)> {
        let cfg = self.cfg.borrow();
        let user = cfg.cloud_username.trim().to_string();
        let mut pass = cfg.cloud_password.reveal();
        if pass.is_empty() {
            pass = self.password.borrow().clone();
        }
        (!user.is_empty()).then_some((user, pass))
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
            Page::Home => self.pump_home(),
            Page::Live => {
                self.update_columns(&ui);
                self.pump_tiles();
            }
            Page::Single => self.pump_single(&b),
            Page::Recordings => self.pump_player(&b),
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
        // Sidebar, page header and status bar around the grid (see app.slint).
        let (chrome_w, chrome_h) = if fullscreen { (8.0, 8.0) } else { (236.0 + 20.0, 76.0 + 26.0 + 20.0) };
        let (w, h) = (size.width - chrome_w, size.height - chrome_h);
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
        let page = if self.signed_in_now() { page } else { Page::Settings }; // nothing to show
        let wanted: Vec<StreamKey> = match page {
            // Home only needs one picture per camera for its card.
            Page::Home => {
                let thumbs = self.thumbnails.borrow();
                cfg.cameras
                    .iter()
                    .filter(|c| c.cloud.is_some() && !thumbs.contains_key(&c.id))
                    .map(|c| (c.id, Quality::Sd))
                    .collect()
            }
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
        let from = b.get_page();
        if b.get_fullscreen() && page != from {
            self.set_fullscreen(false);
        }
        if matches!(from, Page::Live | Page::Single) {
            self.capture_thumbnails();
        }
        if page != Page::Single {
            *self.single.borrow_mut() = None;
        }
        if page == Page::Home {
            self.refresh_home_cards();
        }
        if page == Page::Live {
            // Force tiles to pick up the latest frame again.
            for live in self.streams.borrow_mut().values_mut() {
                live.shown_version = 0;
                live.shown_status.clear();
            }
        }
        b.set_page(page);
        ui.invoke_focus_keys();
        self.sync_streams();
        if page == Page::Recordings && self.hub.borrow().hub.is_none() && !b.get_hub_busy() {
            self.connect_hub();
        }
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
            self.single_back();
        }
    }

    /// Back from the camera view to where it was opened from.
    fn single_back(&self) {
        self.navigate(self.return_page.get());
    }

    fn open_camera(&self, id: u32) {
        let Some(cam) = self.cfg.borrow().camera(id).cloned() else { return };
        let quality = Quality::Hd;
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_page() != Page::Single {
            self.return_page.set(b.get_page());
            if b.get_page() == Page::Live {
                self.capture_thumbnails();
            }
        }
        *self.single.borrow_mut() = Some((id, quality));
        *self.single_shown.borrow_mut() = (0, String::new());

        b.set_single_name(cam.name.as_str().into());
        b.set_single_quality(quality.index());
        b.set_single_frame(Image::default());
        b.set_single_has_frame(false);
        b.set_single_status(self.tile_status(&cam).into());
        // The pan/tilt and privacy tiles work once the camera has answered.
        b.set_single_ptz(false);
        b.set_ptz_open(false);
        b.set_single_privacy(false);
        b.set_single_privacy_known(false);
        b.set_presets(ModelRc::default());
        b.set_ptz_message(SharedString::new());
        b.set_ptz_busy(false);
        b.set_page(Page::Single);
        ui.invoke_focus_keys();
        self.sync_streams();

        if self.ptz.borrow().contains_key(&id) {
            self.ptz_changed(id);
        } else {
            self.ptz_send(PtzCmd::Refresh);
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

    // ------------------------------------------------------------------ home

    /// Rebuilds the Home cards from the camera list.
    fn refresh_home_cards(&self) {
        let cfg = self.cfg.borrow();
        let thumbs = self.thumbnails.borrow();
        let states = self.home_states.borrow();
        let cards: Vec<HomeCard> = cfg
            .cameras
            .iter()
            .map(|c| {
                let thumb = thumbs.get(&c.id).cloned();
                HomeCard {
                    id: c.id as i32,
                    name: c.name.as_str().into(),
                    detail: match &c.cloud {
                        Some(link) if !link.model.is_empty() => link.model.as_str().into(),
                        Some(_) => "Tapo camera".into(),
                        None => "Not linked to your Tapo account".into(),
                    },
                    has_thumb: thumb.is_some(),
                    thumb: thumb.map(Image::from_rgb8).unwrap_or_default(),
                    state: states.get(&c.id).map(String::as_str).unwrap_or("").into(),
                }
            })
            .collect();
        self.home_cards.set_vec(cards);
    }

    /// Keeps the newest picture of each streaming camera for its Home card.
    fn capture_thumbnails(&self) {
        let streams = self.streams.borrow();
        let mut thumbs = self.thumbnails.borrow_mut();
        for (key, live) in streams.iter() {
            if let Some(frame) = live.handle.latest_frame() {
                thumbs.insert(key.0, frame);
            }
        }
    }

    /// On Home, each camera without a picture streams until its first frame,
    /// which becomes its card's picture; cameras that can't be reached get a
    /// badge instead.
    fn pump_home(&self) {
        let mut captured = Vec::new();
        let mut changed = false;
        {
            let streams = self.streams.borrow();
            let mut states = self.home_states.borrow_mut();
            for (key, live) in streams.iter() {
                let slot = live.handle.slot.lock().unwrap();
                if let Some(frame) = slot.frame.clone() {
                    self.thumbnails.borrow_mut().insert(key.0, frame);
                    states.remove(&key.0);
                    captured.push(*key);
                    changed = true;
                    continue;
                }
                let status = slot.status_text();
                let badge = if status.contains("offline") {
                    "Offline"
                } else if status.contains("Privacy mode is on") {
                    "Privacy mode"
                } else {
                    ""
                };
                if states.get(&key.0).map(String::as_str).unwrap_or("") != badge {
                    if badge.is_empty() {
                        states.remove(&key.0);
                    } else {
                        states.insert(key.0, badge.to_string());
                    }
                    changed = true;
                }
            }
        }
        if !captured.is_empty() {
            let mut streams = self.streams.borrow_mut();
            for key in &captured {
                streams.remove(key);
            }
        }
        if changed {
            self.refresh_home_cards();
        }
    }

    /// The Privacy Mode tile.
    fn privacy_toggle(&self) {
        let Some((id, _)) = *self.single.borrow() else { return };
        let current = self.ptz.borrow().get(&id).and_then(|p| p.state.lock().unwrap().privacy);
        if let Some(on) = current {
            self.ptz_send(PtzCmd::Privacy(!on));
        }
    }

    /// Privacy mode changed: reconnect the camera's video.
    pub fn privacy_changed(&self, camera_id: u32) {
        self.streams.borrow_mut().retain(|k, _| k.0 != camera_id);
        if self.single.borrow().is_some_and(|k| k.0 == camera_id) {
            *self.single_shown.borrow_mut() = (0, String::new());
            let ui = self.ui();
            let b = ui.global::<Bridge>();
            b.set_single_has_frame(false);
            b.set_single_frame(Image::default());
        }
        self.sync_streams();
    }

    // ------------------------------------------------------------------- PTZ

    fn ptz_send(&self, cmd: PtzCmd) {
        let Some((id, _)) = *self.single.borrow() else { return };
        let mut ptz = self.ptz.borrow_mut();
        let controller = match ptz.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let link = self.cfg.borrow().camera(id).and_then(|c| c.cloud.clone());
                let (Some(camera), Some(client)) = (link, self.cloud_client()) else { return };
                slot.insert(Ptz::spawn(&self.rt, id, client, camera))
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
        b.set_single_ptz(state.supported == Some(true));
        b.set_single_privacy_known(state.privacy.is_some());
        b.set_single_privacy(state.privacy == Some(true));
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
        b.set_ffmpeg_path(cfg.ffmpeg_path.as_str().into());
        b.set_renderer_index(cfg.renderer.index());
        b.set_theme_index(cfg.theme.index());
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

    /// "Save" under Video & display.
    fn save_settings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        {
            let mut cfg = self.cfg.borrow_mut();
            cfg.ffmpeg_path = b.get_ffmpeg_path().trim().to_string();
            let renderer = Renderer::from_index(b.get_renderer_index());
            if renderer != cfg.renderer {
                cfg.renderer = renderer;
                self.status("Renderer will change after restarting the app.");
            }
            cfg.theme = Theme::from_index(b.get_theme_index());
        }
        self.persist();
        ui.invoke_set_theme(self.cfg.borrow().theme.index());
        *self.ffmpeg.borrow_mut() = crate::ffmpeg::find(&self.cfg.borrow().ffmpeg_path);
        // A different ffmpeg: reconnect the streams.
        self.streams.borrow_mut().clear();
        self.load_settings_into_ui();
        self.sync_streams();
        self.status("Settings saved.");
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
        drop(cfg);
        self.refresh_home_cards();
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
            .enumerate()
            .map(|(i, h)| HubRow { id: i as i32, name: h.name.as_str().into(), detail: hub_detail(h).into() })
            .collect();
        b.set_hubs(ModelRc::new(VecModel::from(rows)));
        b.set_hub_names(string_model(cfg.hubs.iter().map(|h| h.name.clone())));
        if b.get_rec_hub() < 0 || b.get_rec_hub() as usize >= cfg.hubs.len() {
            b.set_rec_hub(0);
        }
    }

    // ------------------------------------------------------------ Tapo cloud

    fn cloud_message(&self, msg: impl Into<SharedString>) {
        self.ui().global::<Bridge>().set_cloud_message(msg.into());
    }

    fn set_cloud_busy(&self, busy: bool) {
        self.ui().global::<Bridge>().set_cloud_busy(busy);
    }

    fn signed_in_now(&self) -> bool {
        self.cloud_client().is_some_and(|c| c.session().is_some())
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
        let account = if session.is_some() { self.cfg.borrow().cloud.account.clone() } else { String::new() };
        b.set_account_name(account.into());
    }

    // --------------------------------------------------------- sign-in screens

    fn login_error(&self, msg: impl Into<SharedString>) {
        self.ui().global::<Bridge>().set_login_error(msg.into());
    }

    /// Shows the sign-in screens at `step` (0 welcome, 1 log in), with the
    /// saved email filled in.
    fn show_sign_in(&self, step: i32, error: &str) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_login_email().is_empty() {
            b.set_login_email(self.cfg.borrow().cloud_username.as_str().into());
        }
        b.set_login_busy(false);
        b.set_login_error(error.into());
        b.set_onboarding_step(step);
    }

    /// "Log In" on the log-in card.
    fn login(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let user = b.get_login_email().trim().to_string();
        let pass = b.get_login_password().to_string();
        if user.is_empty() || pass.is_empty() || b.get_login_busy() {
            return;
        }
        if !user.contains('@') {
            self.login_error("Enter the email address of your TP-Link ID.");
            return;
        }
        self.start_login(user, pass, b.get_login_remember());
    }

    fn start_login(&self, user: String, pass: String, remember: bool) {
        let client = match &self.cloud {
            Ok(client) => client.clone(),
            Err(e) => {
                self.login_error(format!("Tapo cloud unavailable: {e}"));
                return;
            }
        };
        *self.pending_mfa.borrow_mut() = None;
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_login_busy(true);
        b.set_login_error(SharedString::new());
        b.set_login_help(SharedString::new());
        self.rt.spawn(async move {
            let result = client.login(&user, &pass).await;
            on_ui(move |app| app.login_done(user, pass, remember, result));
        });
    }

    fn login_done(&self, user: String, pass: String, remember: bool, result: cloud::Result<Login>) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_login_busy(false);
        match result {
            Ok(Login::Done(session)) => self.signed_in(user, pass, remember, session),
            Ok(Login::NeedsMfa(challenge)) => {
                // Like the Tapo app: send the code to the phone straight away
                // (or by email, if that's the only way).
                let method = if challenge.methods.contains(&MfaMethod::App) { MfaMethod::App } else { MfaMethod::Email };
                *self.pending_mfa.borrow_mut() = Some(PendingMfa { challenge, user, pass, remember, method: None });
                self.set_mfa_code("");
                b.set_mfa_resend_seconds(0);
                b.set_mfa_sent_to(SharedString::new());
                b.set_mfa_switch_label(SharedString::new());
                b.set_onboarding_step(2);
                self.mfa_send(mfa_index(method));
            }
            Err(e) => {
                b.set_onboarding_step(1);
                self.login_error(e.to_string());
            }
        }
    }

    /// Sends a verification code: 1 = Tapo app, 2 = email.
    fn mfa_send(&self, method: i32) {
        let method = if method == 1 { MfaMethod::App } else { MfaMethod::Email };
        let Some(client) = self.cloud_client() else { return };
        let Some((challenge, user, pass)) =
            self.pending_mfa.borrow().as_ref().map(|p| (p.challenge.clone(), p.user.clone(), p.pass.clone()))
        else {
            return;
        };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_login_busy(true);
        b.set_login_error(SharedString::new());
        b.set_mfa_method(mfa_index(method));
        self.rt.spawn(async move {
            let result = client.request_mfa(&challenge, &user, &pass, method).await;
            on_ui(move |app| app.mfa_sent(method, result));
        });
    }

    fn mfa_sent(&self, method: MfaMethod, result: cloud::Result<()>) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_login_busy(false);
        let Some((methods, user)) = self.pending_mfa.borrow().as_ref().map(|p| (p.challenge.methods.clone(), p.user.clone()))
        else {
            return; // went back meanwhile
        };
        match result {
            Ok(()) => {
                if let Some(pending) = self.pending_mfa.borrow_mut().as_mut() {
                    pending.method = Some(method);
                }
                b.set_mfa_resend_seconds(60);
                b.set_mfa_sent_to(
                    match method {
                        MfaMethod::App => {
                            "A verification code has been sent via Tapo push notification to your trusted devices."
                                .to_string()
                        }
                        MfaMethod::Email => format!("A verification code has been sent to {user}."),
                    }
                    .into(),
                );
            }
            Err(e) => self.login_error(format!("Could not send the code: {e}")),
        }
        // Offer the other way of getting a code, if the account has one.
        let other = match method {
            MfaMethod::App => MfaMethod::Email,
            MfaMethod::Email => MfaMethod::App,
        };
        if methods.contains(&other) {
            b.set_mfa_switch_label(
                match other {
                    MfaMethod::Email => "Receive Code via Email",
                    MfaMethod::App => "Receive Code via Tapo App",
                }
                .into(),
            );
            b.set_mfa_switch_method(mfa_index(other));
        } else {
            b.set_mfa_switch_label(SharedString::new());
        }
    }

    /// Keeps the code field to six digits and fills the boxes.
    fn mfa_code_edited(&self, text: SharedString) {
        let digits: String = text.chars().filter(char::is_ascii_digit).take(6).collect();
        self.set_mfa_code(&digits);
        self.login_error("");
    }

    fn set_mfa_code(&self, digits: &str) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_mfa_code() != digits {
            b.set_mfa_code(digits.into());
        }
        let boxes: Vec<SharedString> =
            (0..6).map(|i| digits.chars().nth(i).map(|c| c.to_string()).unwrap_or_default().into()).collect();
        b.set_mfa_digits(ModelRc::new(VecModel::from(boxes)));
        b.set_mfa_filled(digits.chars().count() as i32);
    }

    fn mfa_verify(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let code = b.get_mfa_code().to_string();
        if code.len() != 6 || b.get_login_busy() {
            return;
        }
        let Some(client) = self.cloud_client() else { return };
        let pending = self
            .pending_mfa
            .borrow()
            .as_ref()
            .map(|p| (p.challenge.clone(), p.user.clone(), p.pass.clone(), p.remember, p.method));
        let Some((challenge, user, pass, remember, method)) = pending else { return };
        // The method the code was asked for; if sending seemed to fail, the
        // one last tried.
        let method = method.unwrap_or(if b.get_mfa_method() == 2 { MfaMethod::Email } else { MfaMethod::App });
        b.set_login_busy(true);
        b.set_login_error(SharedString::new());
        self.rt.spawn(async move {
            let result = client.verify_mfa(&challenge, &user, &code, method).await;
            on_ui(move |app| app.mfa_verified(user, pass, remember, result));
        });
    }

    fn mfa_verified(&self, user: String, pass: String, remember: bool, result: cloud::Result<Session>) {
        self.ui().global::<Bridge>().set_login_busy(false);
        match result {
            Ok(session) => self.signed_in(user, pass, remember, session),
            Err(e) => {
                self.set_mfa_code("");
                self.login_error(e.to_string());
            }
        }
    }

    /// "‹" on the verification card: back to the log-in card.
    fn mfa_back(&self) {
        *self.pending_mfa.borrow_mut() = None;
        self.set_mfa_code("");
        self.show_sign_in(1, "");
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

    /// Signed in: keep the account (the password only with "Remember me")
    /// and open the app.
    fn signed_in(&self, user: String, pass: String, remember: bool, session: Session) {
        {
            let mut cfg = self.cfg.borrow_mut();
            cfg.cloud_username = user.clone();
            cfg.cloud_password = if remember { Secret::new(&pass) } else { Secret::default() };
        }
        *self.password.borrow_mut() = pass;
        self.save_cloud_session(user, &session);
        *self.pending_mfa.borrow_mut() = None;

        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_login_password(SharedString::new());
        b.set_login_help(SharedString::new());
        b.set_login_error(SharedString::new());
        b.set_onboarding_step(0);
        self.set_mfa_code("");
        // The account may have changed: reconnect everything that uses it.
        *self.hub.borrow_mut() = HubSession::default();
        self.player_stop();
        b.set_hub_connected(false);
        b.set_hub_message(SharedString::new());
        self.ptz.borrow_mut().clear();
        self.refresh_cloud_ui();
        self.refresh_camera_lists();
        self.navigate(Page::Home);
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
        self.streams.borrow_mut().clear();
        self.ptz.borrow_mut().clear();
        self.set_fullscreen(false);
        self.refresh_cloud_ui();
    }

    /// "Sign out" in Settings: forgets the session and the password, keeps
    /// the email, and goes back to the welcome screen.
    fn cloud_sign_out(&self) {
        self.clear_cloud_session();
        self.cfg.borrow_mut().cloud_password = Secret::default();
        self.password.borrow_mut().clear();
        self.persist();
        self.cloud_message("");
        self.ui().global::<Bridge>().set_login_password(SharedString::new());
        self.show_sign_in(0, "");
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
            let hubs = match &result {
                Ok(_) => Some(client.list_hubs().await),
                Err(_) => None,
            };
            on_ui(move |app| {
                app.cloud_cameras_loaded(result);
                if let Some(hubs) = hubs {
                    app.cloud_hubs_loaded(hubs);
                }
            });
        });
    }

    fn cloud_hubs_loaded(&self, result: cloud::Result<Vec<CloudCamera>>) {
        let hubs = match result {
            Ok(hubs) => hubs,
            Err(e) => {
                log::warn!("could not list the hubs on the account: {e}");
                return;
            }
        };
        if hubs == self.cfg.borrow().hubs {
            return;
        }
        log::info!("{} camera hub(s) on the account", hubs.len());
        let current = self.hub.borrow().hub.clone();
        self.cfg.borrow_mut().hubs = hubs;
        self.persist();
        self.refresh_hub_lists();
        // The page's hub went away or the list changed under it.
        if current.is_some() && self.selected_hub() != current {
            self.connect_hub();
        }
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
    /// saved account; if the cloud wants a verification code, the log-in
    /// card comes back.
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

    /// The session expired and couldn't be renewed without the user: back to
    /// the log-in card, with the email (and a remembered password) filled in.
    fn cloud_session_lost(&self) {
        self.clear_cloud_session();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if let Some((_, pass)) = self.cloud_creds() {
            b.set_login_password(pass.into());
        }
        self.show_sign_in(1, "Your Tapo session has expired. Log in again.");
    }

    // ------------------------------------------------------------ recordings

    fn selected_hub(&self) -> Option<CloudCamera> {
        let index = self.ui().global::<Bridge>().get_rec_hub().max(0) as usize;
        self.cfg.borrow().hubs.get(index).cloned()
    }

    /// Lists the selected hub's cameras, through the cloud.
    fn connect_hub(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let generation = {
            let mut hub = self.hub.borrow_mut();
            *hub = HubSession { generation: hub.generation + 1, ..Default::default() };
            hub.generation
        };
        b.set_hub_connected(false);
        b.set_hub_busy(false);
        b.set_rec_loading(false);
        b.set_recordings(ModelRc::default());
        b.set_paired_names(ModelRc::default());
        self.update_day_chips();
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            b.set_hub_message("Sign in to your Tapo account first.".into());
            return;
        };
        let Some(hub) = self.selected_hub() else {
            b.set_hub_message(
                "There's no H500 or H200 hub on your Tapo account. If you've just added one in the Tapo app, \
                 press ⟳ to look again."
                    .into(),
            );
            return;
        };
        b.set_hub_busy(true);
        b.set_hub_message(format!("Asking {} for its cameras…", hub.name).into());
        self.rt.spawn(async move {
            let result = client.hub_cameras(&hub).await;
            on_ui(move |app| app.hub_connected(generation, hub, result));
        });
    }

    fn hub_connected(&self, generation: u64, hub: CloudCamera, result: cloud::Result<Vec<HubCamera>>) {
        if self.hub.borrow().generation != generation {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_hub_busy(false);
        let cameras = match result {
            Ok(cameras) => cameras,
            Err(e) => {
                if e.is_auth() {
                    self.cloud_auth_failed();
                }
                b.set_hub_message(format!("Could not reach {}: {e}", hub.name).into());
                return;
            }
        };
        b.set_hub_message(match cameras.len() {
            0 => "No cameras are paired with this hub yet (pair them in the Tapo app).".into(),
            1 => "1 camera".into(),
            n => format!("{n} cameras").into(),
        });
        b.set_paired_names(string_model(cameras.iter().map(|c| {
            if c.hub_storage { c.name.clone() } else { format!("{} (not recording to the hub)", c.name) }
        })));
        // A camera that records to the hub, unless one was asked for.
        let first = cameras.iter().position(|c| c.hub_storage).unwrap_or(0);
        b.set_rec_camera(first as i32);
        b.set_hub_connected(!cameras.is_empty());
        {
            let mut session = self.hub.borrow_mut();
            session.hub = Some(hub);
            session.cameras = cameras;
        }
        if self.pending_rec_camera.borrow().is_some() {
            self.select_pending_rec_camera();
        } else {
            self.camera_picked();
        }
    }

    /// The hub card on Home: its recordings.
    fn open_hub(&self, index: i32) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_rec_hub() != index {
            b.set_rec_hub(index);
            self.connect_hub();
        }
        self.navigate(Page::Recordings); // connects, if it hasn't yet
    }

    /// "Playback & Download" in the camera view: this camera's recordings on
    /// the hub.
    fn open_playback(&self) {
        let Some((id, _)) = *self.single.borrow() else { return };
        let wanted = self.cfg.borrow().camera(id).map(|c| {
            (c.cloud.as_ref().map(|l| l.device_id.clone()).unwrap_or_default(), c.name.clone())
        });
        self.navigate(Page::Recordings);
        if self.cfg.borrow().hubs.is_empty() {
            self.status("Recordings are played from the H500 hub, and there's none on your Tapo account.");
            return;
        }
        *self.pending_rec_camera.borrow_mut() = wanted;
        // Like the Tapo app: start from today's recordings.
        *self.rec_date.borrow_mut() = Local::now().date_naive();
        self.update_rec_date_label();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_hub_connected() {
            self.select_pending_rec_camera();
        } else if !b.get_hub_busy() {
            self.connect_hub();
        }
    }

    fn select_pending_rec_camera(&self) {
        let Some((device_id, name)) = self.pending_rec_camera.borrow_mut().take() else { return };
        let index = {
            let hub = self.hub.borrow();
            hub.cameras
                .iter()
                .position(|c| !device_id.is_empty() && c.device_id == device_id)
                .or_else(|| hub.cameras.iter().position(|c| same_name(&c.name, &name)))
        };
        match index {
            Some(i) => self.ui().global::<Bridge>().set_rec_camera(i as i32),
            None => self.status(format!("{name} isn't paired with this hub; pick a camera.")),
        }
        self.camera_picked();
    }

    fn selected_rec_camera(&self) -> Option<HubCamera> {
        let index = self.ui().global::<Bridge>().get_rec_camera().max(0) as usize;
        self.hub.borrow().cameras.get(index).cloned()
    }

    fn camera_picked(&self) {
        self.hub.borrow_mut().days.clear();
        self.load_recordings();
        self.load_days();
    }

    fn change_day(&self, delta: i32) {
        let today = Local::now().date_naive();
        let before = *self.rec_date.borrow();
        let date = match delta {
            0 => today,
            d => (before + chrono::Duration::days(d as i64)).min(today),
        };
        self.set_rec_date(date);
    }

    /// A day in the week strip (0 = Monday).
    fn pick_day(&self, index: i32) {
        let date = week_start(*self.rec_date.borrow()) + chrono::Duration::days(index as i64);
        if date <= Local::now().date_naive() {
            self.set_rec_date(date);
        }
    }

    fn set_rec_date(&self, date: NaiveDate) {
        let before = self.rec_date.replace(date);
        self.update_rec_date_label();
        self.update_day_chips();
        if date != before {
            self.load_recordings();
        }
        // The days fetched cover four weeks before the one shown.
        let covered = self.hub.borrow().days_request > 0 && week_start(date) >= week_start(before) - chrono::Duration::days(21);
        if !covered || week_start(date) > week_start(before) {
            self.load_days();
        }
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

    /// The week strip: the week of the selected day, marking days with
    /// recordings.
    fn update_day_chips(&self) {
        let date = *self.rec_date.borrow();
        let today = Local::now().date_naive();
        let start = week_start(date);
        let days = self.hub.borrow().days.clone();
        let chips: Vec<DayChip> = (0..7)
            .map(|i| {
                let day = start + chrono::Duration::days(i);
                DayChip {
                    weekday: day.format("%a").to_string().into(),
                    day: day.day().to_string().into(),
                    has_video: days.contains(&day),
                    selected: day == date,
                    future: day > today,
                }
            })
            .collect();
        self.ui().global::<Bridge>().set_rec_days(ModelRc::new(VecModel::from(chips)));
    }

    /// Which days around the selected one have recordings.
    fn load_days(&self) {
        let (Some(client), Some(hub), Some(camera)) = (
            self.cloud_client(),
            self.hub.borrow().hub.clone(),
            self.selected_rec_camera(),
        ) else {
            return;
        };
        let request = {
            let mut session = self.hub.borrow_mut();
            session.days_request += 1;
            session.days_request
        };
        let date = *self.rec_date.borrow();
        let from = week_start(date) - chrono::Duration::days(28);
        let to = (week_start(date) + chrono::Duration::days(6)).min(Local::now().date_naive());
        self.rt.spawn(async move {
            let result = client.hub_recording_days(&hub, &camera.device_id, from, to).await;
            on_ui(move |app| app.days_loaded(request, result));
        });
    }

    fn days_loaded(&self, request: u64, result: cloud::Result<Vec<NaiveDate>>) {
        if self.hub.borrow().days_request != request {
            return;
        }
        match result {
            Ok(days) => self.hub.borrow_mut().days = days,
            // The clip list says what's wrong; the strip just has no marks.
            Err(e) => log::warn!("could not list the days with recordings: {e}"),
        }
        self.update_day_chips();
    }

    fn load_recordings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let (Some(client), Some(hub), Some(camera)) = (
            self.cloud_client(),
            self.hub.borrow().hub.clone(),
            self.selected_rec_camera(),
        ) else {
            b.set_recordings(ModelRc::default());
            return;
        };
        let request = {
            let mut session = self.hub.borrow_mut();
            session.rec_request += 1;
            session.clips.clear();
            session.rec_request
        };
        let date = *self.rec_date.borrow();
        let (start, end) = local_day(date);
        let player_id = self.player_id.clone();
        b.set_rec_loading(true);
        b.set_recordings(ModelRc::default());
        b.set_rec_message(SharedString::new());
        self.rt.spawn(async move {
            let result = client.hub_clips(&hub, &camera.device_id, start, end, &player_id).await;
            on_ui(move |app| app.recordings_loaded(request, result));
        });
    }

    fn recordings_loaded(&self, request: u64, result: cloud::Result<Vec<Clip>>) {
        if self.hub.borrow().rec_request != request {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_rec_loading(false);
        match result {
            Ok(mut clips) => {
                clips.reverse(); // newest first
                let total: i64 = clips.iter().map(Clip::seconds).sum();
                b.set_rec_message(match clips.len() {
                    0 => SharedString::new(),
                    n => format!("{n} recording(s) · {}", fmt_duration(total as u64)).into(),
                });
                self.hub.borrow_mut().clips = clips;
            }
            Err(e) => {
                if e.is_auth() {
                    self.cloud_auth_failed();
                }
                b.set_rec_message(format!("Could not list the recordings: {e}").into());
            }
        }
        self.refresh_rec_rows();
    }

    fn refresh_rec_rows(&self) {
        let Some(camera) = self.selected_rec_camera() else { return };
        let playing = self.playing.borrow().as_ref().map(|p| (p.camera_id.clone(), p.clip));
        let rows: Vec<RecRow> = self
            .hub
            .borrow()
            .clips
            .iter()
            .map(|clip| RecRow {
                time: format!("{} – {}", local_time(clip.start).format("%H:%M:%S"), local_time(clip.end).format("%H:%M:%S"))
                    .into(),
                kind: cloud::clip_kind(clip.kind).into(),
                duration: fmt_duration(clip.seconds() as u64).into(),
                playing: playing.as_ref().is_some_and(|(id, c)| *id == camera.device_id && c == clip),
                saved: is_saved(&clip_path(&camera, clip)),
            })
            .collect();
        self.ui().global::<Bridge>().set_recordings(ModelRc::new(VecModel::from(rows)));
    }

    fn clip_request(&self, index: usize) -> Option<ClipRequest> {
        let hub = self.hub.borrow();
        Some(ClipRequest {
            hub: hub.hub.clone()?,
            camera: self.selected_rec_camera()?,
            clip: *hub.clips.get(index)?,
            player_id: self.player_id.clone(),
        })
    }

    /// Plays a recording in the app: the saved copy if there is one, otherwise
    /// through the cloud, saving it as it comes.
    fn play_clip(&self, index: usize) {
        let Some(request) = self.clip_request(index) else { return };
        self.start_player(request);
    }

    fn start_player(&self, request: ClipRequest) {
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            self.status("ffmpeg not found: playing recordings needs it (see Settings › Video & display).");
            return;
        };
        let path = clip_path(&request.camera, &request.clip);
        let source = if is_saved(&path) {
            Source::File(path.clone())
        } else {
            let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
                self.status("Sign in to your Tapo account first.");
                return;
            };
            Source::Cloud { client, request: Box::new(request.clone()), path: path.clone() }
        };
        let title = format!("{} · {}", request.camera.name, local_time(request.clip.start).format("%a %d %b, %H:%M:%S"));
        // Stop the previous recording before starting this one.
        *self.playing.borrow_mut() = None;
        let player = Player::start(&self.rt, ffmpeg, source, || on_ui(|app| app.player_changed()));
        *self.playing.borrow_mut() = Some(Playing {
            player,
            camera_id: request.camera.device_id.clone(),
            clip: request.clip,
            path,
            request,
            shown_version: 0,
            started: None,
            shown_position: String::new(),
        });
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_player_open(true);
        b.set_player_has_frame(false);
        b.set_player_frame(Image::default());
        b.set_player_title(title.into());
        b.set_player_position(SharedString::new());
        self.player_changed();
        self.refresh_rec_rows();
    }

    /// The player's download or playback moved on.
    pub fn player_changed(&self) {
        let (state, total, path) = {
            let playing = self.playing.borrow();
            let Some(p) = playing.as_ref() else { return };
            let state = p.player.state.lock().unwrap().clone();
            (state, p.clip.seconds(), p.path.clone())
        };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let mut status = state.message.clone();
        if state.downloading && state.downloaded > 0.0 {
            status += &format!(" {} of {}", fmt_duration(state.downloaded as u64), fmt_duration(total as u64));
        }
        b.set_player_status(status.into());
        b.set_player_busy(state.downloading);
        b.set_player_ended(state.ended);
        let saved = state.saved.is_some() || is_saved(&path);
        let was_saved = !b.get_player_file().is_empty();
        b.set_player_file(if saved { path.display().to_string().into() } else { SharedString::new() });
        if saved != was_saved {
            self.refresh_rec_rows();
        }
    }

    fn pump_player(&self, b: &Bridge) {
        let mut playing = self.playing.borrow_mut();
        let Some(p) = playing.as_mut() else { return };
        {
            let slot = p.player.slot.lock().unwrap();
            if slot.version != p.shown_version {
                if let Some(frame) = slot.frame.clone() {
                    b.set_player_frame(Image::from_rgb8(frame));
                    b.set_player_has_frame(true);
                }
                p.shown_version = slot.version;
                p.started.get_or_insert_with(Instant::now);
            }
        }
        let ended = p.player.state.lock().unwrap().ended;
        let Some(started) = p.started else { return };
        if ended {
            return; // keep the last position
        }
        let total = p.clip.seconds();
        let position = format!("{} / {}", clock(started.elapsed().as_secs().min(total as u64)), clock(total as u64));
        if position != p.shown_position {
            b.set_player_position(position.as_str().into());
            p.shown_position = position;
        }
    }

    fn player_stop(&self) {
        *self.playing.borrow_mut() = None;
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_player_open(false);
        b.set_player_frame(Image::default());
        b.set_player_has_frame(false);
        b.set_player_file(SharedString::new());
        self.refresh_rec_rows();
    }

    fn player_replay(&self) {
        let request = self.playing.borrow().as_ref().map(|p| p.request.clone());
        if let Some(request) = request {
            self.start_player(request);
        }
    }

    /// Saves a recording without playing it.
    fn download(&self, index: usize) {
        let Some(request) = self.clip_request(index) else { return };
        let path = clip_path(&request.camera, &request.clip);
        let label = format!("{} · {}", request.camera.name, local_time(request.clip.start).format("%d %b %H:%M:%S"));
        if is_saved(&path) {
            self.push_download(label, "Already saved".into(), &path, true);
            return;
        }
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            self.status("Sign in to your Tapo account first.");
            return;
        };
        let row = self.push_download(label, "Asking the hub for the recording…".into(), &path, false);
        let total = request.clip.seconds() as u64;
        self.rt.spawn(async move {
            let mut last = Instant::now();
            let mut progress = |p: clip::Progress| {
                if last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    let text = format!("Downloading… {} of {}", fmt_duration(p.seconds as u64), fmt_duration(total));
                    on_ui(move |app| app.set_download_status(row, text, false));
                }
            };
            let partial = crate::player::partial_path(&path);
            let result = crate::player::save_clip(&client, &request, &path, &partial, &mut progress).await;
            let (text, done) = match &result {
                Ok(()) => {
                    let mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                    (format!("Saved · {mb:.1} MB"), true)
                }
                Err(e) => (format!("Failed: {e:#}"), false),
            };
            let auth = result.as_ref().err().and_then(|e| e.downcast_ref::<cloud::CloudError>()).is_some_and(|e| e.is_auth());
            on_ui(move |app| {
                app.set_download_status(row, text, done);
                if auth {
                    app.cloud_auth_failed();
                }
                app.refresh_rec_rows();
            });
        });
    }

    fn push_download(&self, label: String, status: String, path: &Path, done: bool) -> usize {
        self.downloads.push(DownloadRow {
            label: label.into(),
            status: status.into(),
            path: path.display().to_string().into(),
            done,
        });
        self.downloads.row_count() - 1
    }

    fn set_download_status(&self, row: usize, status: String, done: bool) {
        let Some(mut d) = self.downloads.row_data(row) else { return };
        if d.done && !done {
            return; // a late progress update
        }
        d.status = status.into();
        d.done = done;
        self.downloads.set_row_data(row, d);
    }
}

fn mfa_index(method: MfaMethod) -> i32 {
    match method {
        MfaMethod::App => 1,
        MfaMethod::Email => 2,
    }
}

/// Camera names compare loosely: settings from the RTSP version used the
/// Tapo app's nickname, which is also what the cloud list has.
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

/// "H500" for the Home card and the list.
fn hub_detail(hub: &CloudCamera) -> String {
    match hub.model.trim() {
        "" => "Hub".into(),
        model => model.split('(').next().unwrap_or(model).trim().to_string(),
    }
}

/// Monday of `date`'s week.
fn week_start(date: NaiveDate) -> NaiveDate {
    date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64)
}

/// The day's first and last second (this PC's time zone), as Unix time.
fn local_day(date: NaiveDate) -> (i64, i64) {
    let midnight = |d: NaiveDate| {
        Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp())
    };
    let start = midnight(date).unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp());
    let end = midnight(date + chrono::Duration::days(1)).unwrap_or(start + 86_400) - 1;
    (start, end)
}

fn local_time(unix: i64) -> chrono::DateTime<Local> {
    Local.timestamp_opt(unix, 0).single().unwrap_or_else(Local::now)
}

/// Where a recording is saved: `Videos\TapoViewer\<camera>_<start>.ts`.
fn clip_path(camera: &HubCamera, clip: &Clip) -> PathBuf {
    let start = local_time(clip.start).format("%Y-%m-%d_%H-%M-%S");
    config::recordings_dir().join(format!("{}_{start}.ts", file_safe(&camera.name)))
}

fn is_saved(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}

/// "1:05", "1:02:03"
fn clock(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60),
        s => format!("{}:{:02}", s / 60, s % 60),
    }
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
        assert_eq!(fmt_duration(65), "1m 05s");
        assert_eq!(clock(65), "1:05");
        assert_eq!(clock(3723), "1:02:03");
        let monday = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        assert_eq!(week_start(NaiveDate::from_ymd_opt(2026, 11, 22).unwrap()), monday);
        assert_eq!(week_start(monday), monday);
        let (start, end) = local_day(monday);
        assert!((82_799..=90_000).contains(&(end - start)), "{}", end - start);
        let hub = CloudCamera { model: "H500(EU)".into(), ..Default::default() };
        assert_eq!(hub_detail(&hub), "H500");
        assert_eq!(fmt_duration(3700), "1h 01m");
        assert_eq!(file_safe("Front door/1"), "Front_door_1");
    }

    #[test]
    fn links_account_cameras() {
        let cloud = |id: &str, name: &str| CloudCamera { device_id: id.into(), name: name.into(), ..Default::default() };
        let local = |id: u32, name: &str| CameraConfig { id, name: name.into(), ..Default::default() };
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
        assert_eq!(office.id, 7);
        // Running it again changes nothing.
        let (linked, added) = link_cloud_cameras(&mut cameras, &account);
        assert!(linked.is_empty() && added == 0);
    }
}
