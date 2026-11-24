//! Application state and the glue between the Slint UI and the background
//! work (streams, PTZ, hub recordings). All of it lives on the UI thread;
//! background tasks report back through [`on_ui`].

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};

use crate::clip::{self, ClipRequest};
use crate::cloud::{
    self, Clip, CloudCamera, CloudClient, Detection, EventFilter, FaceFilter, FaceInfo, FaceTag, FaceTrackingEntry,
    Homes, HubCamera, HubInfo, Login, MfaChallenge, MfaMethod, NightVisionMode, PatrolAxis, Session,
};
use crate::config::{self, AppConfig, CameraConfig, Language, Quality, Renderer, Theme};
use crate::i18n;
use crate::playback::{Playback, SPEEDS, Source};
use crate::ptz::{Ptz, PtzCmd};
use crate::secret::Secret;
use crate::stream::{Frame, StreamHandle, StreamSpec};
use crate::{
    Bridge, CameraRow, DayChip, DownloadRow, FaceRow, FaceTrackRow, HomeCard, HourBar, HubRow, MainWindow, Page,
    PresetRow, RecRow, Tile, TlSeg, TlTick,
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
    crate::audio::shutdown();
    if let Some(app) = APP.with(|a| a.borrow_mut().take()) {
        app.timer.stop();
        app.batch_cancel();
        app.stop_live_recording(true);
        app.stop_face_photos();
        app.stop_event_pictures();
        app.watching.borrow_mut().take();
        app.streams.borrow_mut().clear();
        app.ptz.borrow_mut().clear();
        crate::playback::clear_cache();
    }
}

type StreamKey = (u32, Quality);

struct LiveRecording {
    key: StreamKey,
    camera_name: String,
    partial: PathBuf,
    target: PathBuf,
    started: Instant,
    shown_secs: u64,
}

struct Live {
    handle: StreamHandle,
    shown_version: u64,
    shown_status: String,
    /// When the page stopped needing it. Streams stay a little while after,
    /// so going back (e.g. from a camera to the grid) is instant.
    unused_since: Option<Instant>,
}

/// How long a stream no page needs keeps running.
const STREAM_LINGER: Duration = Duration::from_secs(20);
/// How long playback state and cached video stay warm after leaving Playback.
const PLAYBACK_LINGER: Duration = Duration::from_secs(30);
/// Maximum decoded event thumbnails kept in RAM at once (disk cache holds the rest).
const MAX_EVENT_PICTURES: usize = 100;
/// Home refreshes a camera's picture when it's older than this...
const PICTURE_FRESH: Duration = Duration::from_secs(10 * 60);
/// ...a couple of cameras at a time.
const HOME_STREAMS: usize = 2;
/// The largest picture kept per camera.
const PICTURE_WIDTH: u32 = 640;

/// A camera's latest picture: its Home card, and its tile while live view
/// connects. Also kept on disk (see `pictures`).
struct Thumb {
    frame: Frame,
    taken: SystemTime,
    /// When it was last written to disk.
    saved: Option<Instant>,
}

impl Thumb {
    fn is_fresh(&self) -> bool {
        self.taken.elapsed().is_ok_and(|age| age < PICTURE_FRESH)
    }
}

/// The Playback page: one hub's cameras and one day of a camera's recordings.
#[derive(Default)]
struct HubSession {
    /// Bumped on every (re)load so stale results are ignored.
    generation: u64,
    hub: Option<CloudCamera>,
    cameras: Vec<HubCamera>,
    /// The day's recordings, oldest first.
    clips: Vec<Clip>,
    /// What the camera detected that day, oldest first (empty if the hub
    /// doesn't list detections).
    events: Vec<Detection>,
    /// Merged detections across all cameras on the hub when "All cameras" is selected.
    all_events: Vec<(HubCamera, Detection)>,
    /// When jumping from an "All cameras" event into that camera's recordings,
    /// start playback at this Unix ms once loaded.
    pending_play_at: Option<i64>,
    /// The list's rows, newest first, from `events` if there are any, else
    /// from `clips`, under a heading for each hour.
    rows: Vec<ListRow>,
    /// Days with recordings, for the months in `months`.
    days: HashSet<NaiveDate>,
    months: HashSet<NaiveDate>,
    rec_request: u64,
}

/// A row of the Playback page's list.
#[derive(Clone, PartialEq)]
enum ListRow {
    /// The hour (Unix seconds of its start) the rows below it fall in.
    Hour(i64),
    /// (start, end, kind, optional camera when in "All cameras" view)
    Entry(i64, i64, i64, Option<HubCamera>),
}

impl ListRow {
    fn entry(&self) -> Option<(i64, i64, i64, Option<&HubCamera>)> {
        match self {
            ListRow::Entry(a, b, k, cam) => Some((*a, *b, *k, cam.as_ref())),
            ListRow::Hour(_) => None,
        }
    }
}

/// The part of the day the timeline shows (Unix ms).
#[derive(Default)]
struct TimelineView {
    start: i64,
    span: i64,
    /// Where `start` was when a drag began.
    drag_from: Option<i64>,
    /// When the user last moved it, so following the picture waits a moment.
    touched: Option<Instant>,
}

/// Watching recordings along the day.
struct Watching {
    playback: Playback,
    camera_id: String,
    date: NaiveDate,
    shown_version: u64,
    /// What the UI was last given, so it's only told about changes.
    shown: (String, String, bool, bool, bool, Option<usize>),
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
    watching: RefCell<Option<Watching>>,
    rec_date: RefCell<NaiveDate>,
    /// The first day of the month the calendar shows.
    rec_month: Cell<NaiveDate>,
    timeline: RefCell<TimelineView>,
    speed: Cell<u32>,
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
    /// The latest picture of each camera, by camera id.
    thumbnails: RefCell<HashMap<u32, Thumb>>,
    /// Cameras Home stopped asking for a picture this visit (offline, etc.).
    home_skip: RefCell<HashSet<u32>>,
    /// When unused streams were last looked at.
    last_sweep: Cell<Option<Instant>>,
    /// When the memory in use was last logged.
    last_memory_log: Cell<Option<Instant>>,
    /// Home card badges, e.g. "Offline".
    home_states: RefCell<HashMap<u32, String>>,
    home_cards: Rc<VecModel<HomeCard>>,
    /// Where the camera view's back button goes.
    return_page: Cell<Page>,
    /// The list's rows as shown, updated in place so it keeps its scroll.
    rec_rows: Rc<VecModel<RecRow>>,
    /// Event pictures by (camera id, event start).
    event_pictures: RefCell<HashMap<(String, i64), Frame>>,
    /// Fetching event pictures, while nothing plays.
    picture_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Recording what's watched: from when (ms), in the day being watched.
    recording_from: Cell<Option<i64>>,
    /// A newer version, once found.
    update: RefCell<Option<crate::update::Release>>,
    /// The Tapo app's homes and rooms, from the account.
    homes: RefCell<Homes>,
    /// What each hub (by device id) says about itself.
    hub_info: RefCell<HashMap<String, HubInfo>>,
    /// Whether the hubs have been asked (whatever they answered).
    hub_info_tried: Cell<bool>,
    /// Hub device ids currently sounding their alarm (toggled by user).
    hub_alarms: RefCell<HashSet<String>>,
    /// Hub device ids with an in-flight control request.
    hub_busy_ids: RefCell<HashSet<String>>,
    /// Live-view camera id currently unmuted (if any).
    tile_audio_cam: Cell<Option<u32>>,
    /// Whether Single camera view audio is unmuted.
    single_audio_on: Cell<bool>,
    /// Whether Playback player audio is unmuted.
    player_audio_on: Cell<bool>,
    /// When the user last left Playback, so cached recordings and thumbnails
    /// are freed once idle elsewhere.
    playback_left_at: Cell<Option<Instant>>,
    /// Whether the UI pump timer is currently running at its fast (30 ms) rate.
    timer_fast: Cell<bool>,
    /// A camera (cloud device id, name, optional seek Unix ms) to select once
    /// the hub's cameras are listed.
    pending_rec_camera: RefCell<Option<(String, String, Option<i64>)>>,
    /// Selected clips in Playback batch export mode, keyed by (camera_device_id, start_unix_s).
    batch_selected: RefCell<HashSet<(String, i64)>>,
    /// Cancellation flag for an active sequential batch MP4 export.
    batch_cancel: RefCell<Option<Arc<AtomicBool>>>,
    /// Recognized faces cached from the selected H500 hub.
    faces: RefCell<Vec<FaceInfo>>,
    /// Device id of the hub `faces` were fetched from.
    faces_loaded_hub: RefCell<Option<String>>,
    faces_request: Cell<u64>,
    selected_face: Cell<Option<i64>>,
    face_track_date: Cell<NaiveDate>,
    face_track_request: Cell<u64>,
    face_tracks: RefCell<Vec<FaceTrackingEntry>>,
    /// Decoded H500 face avatars in memory, keyed by (hub_id, face_id).
    face_photos: RefCell<HashMap<(String, i64), Frame>>,
    /// Background task fetching missing H500 face photos.
    face_photo_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Active live MP4 recording on the Single camera view.
    live_recording: RefCell<Option<LiveRecording>>,
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
    let today = Local::now().date_naive();
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
        watching: RefCell::default(),
        rec_date: RefCell::new(today),
        rec_month: Cell::new(first_of_month(today)),
        timeline: RefCell::default(),
        speed: Cell::new(1),
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
        home_skip: RefCell::default(),
        last_sweep: Cell::default(),
        last_memory_log: Cell::default(),
        home_states: RefCell::default(),
        home_cards: Rc::new(VecModel::default()),
        return_page: Cell::new(Page::Home),
        pending_rec_camera: RefCell::default(),
        batch_selected: RefCell::default(),
        batch_cancel: RefCell::default(),
        faces: RefCell::default(),
        faces_loaded_hub: RefCell::default(),
        faces_request: Cell::new(0),
        selected_face: Cell::new(None),
        face_track_date: Cell::new(today),
        face_track_request: Cell::new(0),
        face_tracks: RefCell::default(),
        face_photos: RefCell::default(),
        face_photo_task: RefCell::default(),
        live_recording: RefCell::default(),
        homes: RefCell::default(),
        hub_info: RefCell::default(),
        hub_info_tried: Cell::default(),
        hub_alarms: RefCell::default(),
        hub_busy_ids: RefCell::default(),
        tile_audio_cam: Cell::default(),
        single_audio_on: Cell::default(),
        player_audio_on: Cell::default(),
        playback_left_at: Cell::default(),
        timer_fast: Cell::new(true),
        rec_rows: Rc::new(VecModel::default()),
        event_pictures: RefCell::default(),
        picture_task: RefCell::default(),
        recording_from: Cell::default(),
        update: RefCell::default(),
    });
    APP.with(|a| *a.borrow_mut() = Some(app.clone()));
    if new_terminal {
        app.persist();
    }

    let b = ui.global::<Bridge>();
    b.set_tiles(app.tiles.clone().into());
    b.set_home_cameras(app.home_cards.clone().into());
    let theme_index = app.cfg.borrow().theme.index();
    ui.invoke_set_theme(theme_index);
    crate::chrome::install(ui, theme_index);
    let lang = i18n::set(app.cfg.borrow().language);
    i18n::apply_to_ui(ui, lang);
    b.set_downloads(app.downloads.clone().into());
    b.set_quality_labels(string_model(Quality::LABELS.iter().copied()));

    b.on_navigate(cb!(&app, |a, page| a.navigate(page)));
    b.on_toggle_fullscreen(cb!(&app, |a| a.toggle_fullscreen()));
    b.on_escape(cb!(&app, |a| a.escape()));
    b.on_window_minimize(|| crate::chrome::minimize());
    b.on_window_toggle_maximize(|| crate::chrome::toggle_maximize());
    b.on_window_close(|| crate::chrome::close());
    b.on_open_camera(cb!(&app, |a, id| a.open_camera(id as u32)));
    b.on_open_camera_slot(cb!(&app, |a, slot| a.open_camera_slot(slot as usize)));
    b.on_toggle_tile_audio(cb!(&app, |a, id| a.toggle_tile_audio(id as u32)));
    b.on_open_hub(cb!(&app, |a, i| a.open_hub(i)));
    b.on_hub_toggle_loop(cb!(&app, |a, i| a.hub_toggle_loop(i as usize)));
    b.on_hub_toggle_alarm(cb!(&app, |a, i| a.hub_toggle_alarm(i as usize)));
    b.on_single_back(cb!(&app, |a| a.single_back()));
    b.on_single_record(cb!(&app, |a| a.single_record()));
    b.on_single_toggle_audio(cb!(&app, |a| a.single_toggle_audio()));
    b.on_reset_zoom(cb!(&app, |a| a.reset_zoom()));
    b.on_player_skip(cb!(&app, |a, secs| a.player_skip(secs)));
    b.on_player_step_speed(cb!(&app, |a, delta| a.player_step_speed(delta)));
    b.on_privacy_toggle(cb!(&app, |a| a.privacy_toggle()));
    b.on_spotlight_toggle(cb!(&app, |a| a.spotlight_toggle()));
    b.on_night_vision_changed(cb!(&app, |a, i| a.night_vision_changed(i)));
    b.on_auto_track_toggle(cb!(&app, |a| a.auto_track_toggle()));
    b.on_patrol_start(cb!(&app, |a, axis| a.patrol_start(axis)));
    b.on_patrol_stop(cb!(&app, |a| a.ptz_send(PtzCmd::PatrolStop)));
    b.on_siren_toggle(cb!(&app, |a| a.siren_toggle()));
    b.on_camera_reboot(cb!(&app, |a| a.ptz_send(PtzCmd::Reboot)));
    b.on_open_playback(cb!(&app, |a| a.open_playback()));
    b.on_open_camera_playback(cb!(&app, |a, id| a.open_camera_playback(id as u32)));
    b.on_quality_changed(cb!(&app, |a, i| a.quality_changed(i)));
    b.on_snapshot(cb!(&app, |a| a.snapshot()));
    b.on_ptz_move(cb!(&app, |a, pan, tilt| a.ptz_send(PtzCmd::Move(pan, tilt))));
    b.on_ptz_goto(cb!(&app, |a, id| a.ptz_send(PtzCmd::Goto(id.into()))));
    b.on_ptz_save(cb!(&app, |a, name| {
        a.ptz_send(PtzCmd::Save(name.trim().into()));
        a.ui().invoke_focus_keys(); // so the arrow keys pan again
    }));
    b.on_ptz_delete(cb!(&app, |a, id| a.ptz_send(PtzCmd::DeletePreset(id.into()))));
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
    b.on_rec_filter_changed(cb!(&app, |a, _i| a.rec_filter_changed()));
    b.on_change_month(cb!(&app, |a, d| a.change_month(d)));
    b.on_go_today(cb!(&app, |a| a.set_rec_date(Local::now().date_naive())));
    b.on_pick_day(cb!(&app, |a, i| a.pick_day(i)));
    b.on_play_clip(cb!(&app, |a, i| a.play_row(i as usize)));
    b.on_download(cb!(&app, |a, i| a.download(i as usize)));
    b.on_jump_hour(cb!(&app, |a, h| a.jump_hour(h)));
    b.on_batch_toggle_mode(cb!(&app, |a| a.batch_toggle_mode()));
    b.on_batch_toggle_row(cb!(&app, |a, i| a.batch_toggle_row(i as usize)));
    b.on_batch_select_all(cb!(&app, |a| a.batch_select_all()));
    b.on_batch_clear(cb!(&app, |a| a.batch_clear()));
    b.on_batch_export(cb!(&app, |a| a.batch_export()));
    b.on_batch_cancel(cb!(&app, |a| a.batch_cancel()));
    b.on_faces_refresh(cb!(&app, |a| a.load_faces()));
    b.on_face_filter_changed(cb!(&app, |a, _i| a.face_filter_changed()));
    b.on_face_select(cb!(&app, |a, id| a.face_select(id.as_str())));
    b.on_face_save(cb!(&app, |a| a.face_save()));
    b.on_face_track_day(cb!(&app, |a, d| a.face_track_day(d)));
    b.on_face_track_play(cb!(&app, |a, i| a.face_track_play(i as usize)));
    b.on_player_stop(cb!(&app, |a| a.player_stop()));
    b.on_player_toggle_pause(cb!(&app, |a| a.player_toggle_pause()));
    b.on_player_toggle_audio(cb!(&app, |a| a.player_toggle_audio()));
    b.on_player_speed(cb!(&app, |a, i| a.player_speed(i)));
    b.on_player_snapshot(cb!(&app, |a| a.player_snapshot()));
    b.on_player_record(cb!(&app, |a| a.player_record()));
    b.on_check_update(cb!(&app, |a| a.check_update(true)));
    b.on_install_update(cb!(&app, |a| a.install_update()));
    b.on_check_updates_changed(cb!(&app, |a| {
        let on = a.ui().global::<Bridge>().get_check_updates();
        a.cfg.borrow_mut().skip_update_checks = !on;
        a.persist();
    }));
    b.set_app_version(crate::update::CURRENT.into());
    b.set_check_updates(!app.cfg.borrow().skip_update_checks);
    b.set_recordings(app.rec_rows.clone().into());
    b.on_tl_click(cb!(&app, |a, f| a.tl_click(f)));
    let weak = Rc::downgrade(&app);
    b.on_tl_time_at(move |at| weak.upgrade().map(|a| a.tl_time_at(at)).unwrap_or_default());
    b.on_tl_drag(cb!(&app, |a, f| a.tl_drag(f)));
    b.on_tl_drag_end(cb!(&app, |a| a.timeline.borrow_mut().drag_from = None));
    b.on_tl_zoom(cb!(&app, |a, factor, at| a.tl_zoom(factor, at)));
    b.on_tl_whole_day(cb!(&app, |a| a.reset_timeline()));
    b.set_speed_labels(string_model(SPEEDS.iter().map(|s| format!("{s}×"))));
    b.on_open_file(|p| open_path(Path::new(p.as_str())));
    b.on_show_in_folder(|p| show_in_folder(Path::new(p.as_str())));

    app.load_settings_into_ui();
    app.refresh_cloud_ui();
    app.load_saved_pictures();
    app.refresh_camera_lists();
    app.rt.spawn_blocking(crate::pictures::prune_events);
    app.refresh_hub_lists();
    app.update_rec_date_label();
    app.update_calendar();
    app.update_face_track_date_label();
    if app.signed_in_now() {
        // The hubs as the account has them now (names, new hubs), then their
        // details, homes and rooms.
        if let Some(client) = app.cloud_client() {
            app.rt.spawn(async move {
                let hubs = client.list_hubs().await;
                on_ui(move |app| app.cloud_hubs_loaded(hubs));
            });
        }
    }
    app.reset_timeline();
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

    keep_app_accent(ui);
    app.timer.start(slint::TimerMode::Repeated, Duration::from_millis(30), cb!(&app, |a| a.pump()));
    app.sync_timer();
    if !app.cfg.borrow().skip_update_checks {
        slint::Timer::single_shot(Duration::from_secs(4), cb!(&app, |a| a.check_update(false)));
    }
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

    /// Runs the UI pump fast (30 ms) while video streams or playback are
    /// active, and slow (500 ms) while idle so the app doesn't wake 33x/sec
    /// on Home or Settings.
    fn sync_timer(&self) {
        let active_playback = self
            .watching
            .borrow()
            .as_ref()
            .is_some_and(|w| !w.playback.state.lock().unwrap().paused);
        let fast = !self.streams.borrow().is_empty() || active_playback;
        if self.timer_fast.replace(fast) != fast {
            self.timer.set_interval(Duration::from_millis(if fast { 30 } else { 500 }));
        }
    }

    // ---------------------------------------------------------------- frames

    /// Runs every ~30 ms while active (500 ms while idle): moves new frames
    /// and status into the UI, and sweeps unused streams and caches.
    fn pump(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let b = ui.global::<Bridge>();
        let page = b.get_page();
        match page {
            Page::Home => self.pump_home(),
            Page::Live => {
                self.update_columns(&ui);
                self.pump_tiles();
            }
            Page::Single => self.pump_single(&b),
            Page::Recordings => self.pump_player(&b),
            _ => {}
        }
        if self.last_memory_log.get().is_none_or(|t| t.elapsed() >= Duration::from_secs(120)) {
            self.last_memory_log.set(Some(Instant::now()));
            self.log_memory(page);
        }
        if self.last_sweep.get().is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.last_sweep.set(Some(Instant::now()));
            keep_app_accent(&ui);
            self.streams.borrow_mut().retain(|_, l| l.unused_since.is_none_or(|t| t.elapsed() < STREAM_LINGER));
            // Release playback cache, player frame and event thumbnails after
            // lingering off the Playback page, so sitting on Home/Settings
            // holds no recording buffers in RAM.
            if page != Page::Recordings
                && self.playback_left_at.get().is_some_and(|t| t.elapsed() >= PLAYBACK_LINGER)
            {
                self.playback_left_at.set(None);
                self.release_playback_memory();
            }
            self.sync_timer();
        }
        let expired = self.status_until.borrow().is_some_and(|t| Instant::now() >= t);
        if expired {
            *self.status_until.borrow_mut() = None;
            b.set_status_text(SharedString::new());
        }
    }

    /// Drops cached recordings, parked playback and decoded event thumbnails
    /// after leaving the Playback page (event pictures stay on disk as JPEGs).
    fn release_playback_memory(&self) {
        self.stop_event_pictures();
        self.player_stop();
        crate::playback::clear_cache();
        if !self.event_pictures.borrow().is_empty() {
            self.event_pictures.borrow_mut().clear();
            self.refresh_rec_rows();
        }
    }

    /// Logs the memory in use and what's running, to tell what costs what
    /// (e.g. comparing renderers).
    fn log_memory(&self, page: Page) {
        let Some((private, working)) = process_memory_mb() else { return };
        let streams = self.streams.borrow().len();
        let watching = self.watching.borrow().is_some();
        log::info!(
            "memory: {private} MB in use (working set {working} MB) · {:?} renderer · {page:?} page · {streams} video stream(s) · playback {} · {} MB of recordings cached · {} event picture(s)",
            self.cfg.borrow().renderer,
            if watching { "open" } else { "closed" },
            crate::playback::cached_bytes() >> 20,
            self.event_pictures.borrow().len(),
        );
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
                tile.frame = Image::from_rgba8_premultiplied(frame);
                tile.has_frame = true;
                live.shown_version = version;
            }
            tile.status = status.as_str().into();
            tile.live = is_live;
            // Not live (yet, or any more): the picture is dimmed, with why.
            tile.stale = !is_live;
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
                b.set_single_frame(Image::from_rgba8_premultiplied(frame));
                b.set_single_has_frame(true);
            }
            shown.0 = slot.version;
        }
        if b.get_single_stale() == slot.is_live() {
            b.set_single_stale(!slot.is_live());
        }
        let status = slot.status_text();
        if status != shown.1 {
            b.set_single_status(status.as_str().into());
            shown.1 = status;
        }
        drop(slot);
        drop(shown);
        drop(streams);
        if let Some(rec) = self.live_recording.borrow_mut().as_mut() {
            let secs = rec.started.elapsed().as_secs();
            if secs != rec.shown_secs {
                rec.shown_secs = secs;
                b.set_single_rec_time(format!("{}:{:02}", secs / 60, secs % 60).into());
            }
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
        if !self.signed_in_now() {
            self.streams.borrow_mut().clear(); // nothing to show
            return;
        }
        let page = self.ui().global::<Bridge>().get_page();
        let cfg = self.cfg.borrow();
        let wanted: Vec<StreamKey> = match page {
            // Home needs a picture per camera for its card: a short stream
            // for those without a recent one, a couple at a time.
            Page::Home => {
                let thumbs = self.thumbnails.borrow();
                let skip = self.home_skip.borrow();
                cfg.cameras
                    .iter()
                    .filter(|c| c.cloud.is_some() && !skip.contains(&c.id))
                    .filter(|c| !thumbs.get(&c.id).is_some_and(Thumb::is_fresh))
                    .take(HOME_STREAMS)
                    .map(|c| (c.id, Quality::Sd))
                    .collect()
            }
            Page::Live => cfg.cameras.iter().map(|c| (c.id, c.grid_quality)).collect(),
            Page::Single => self.single.borrow().iter().copied().collect(),
            _ => Vec::new(),
        };
        let mut streams = self.streams.borrow_mut();
        let now = Instant::now();
        for (key, live) in streams.iter_mut() {
            if wanted.contains(key) {
                live.unused_since = None;
            } else {
                live.unused_since.get_or_insert(now);
            }
        }
        for key in wanted {
            if streams.contains_key(&key) {
                continue;
            }
            // Cameras not linked to the account have nothing to stream; their
            // tile says so (see `tile_status`).
            if let Some(spec) = cfg.camera(key.0).and_then(|cam| self.spec(cam, key.1)) {
                let handle = StreamHandle::spawn(&self.rt, spec);
                streams.insert(key, Live { handle, shown_version: 0, shown_status: String::new(), unused_since: None });
            }
        }
        drop(streams);
        self.sync_timer();
        self.sync_audio();
    }

    /// Routes audio to whichever single source should currently be audible
    /// based on the active page and mute toggles, and syncs the UI icons.
    fn sync_audio(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let page = b.get_page();
        let active_id = match page {
            Page::Live => self.tile_audio_cam.get().and_then(|cam_id| {
                let quality = self.cfg.borrow().camera(cam_id)?.grid_quality;
                self.streams.borrow().get(&(cam_id, quality)).map(|l| l.handle.audio_id)
            }),
            Page::Single if self.single_audio_on.get() => {
                let key = *self.single.borrow();
                key.and_then(|k| self.streams.borrow().get(&k).map(|l| l.handle.audio_id))
            }
            Page::Recordings if self.player_audio_on.get() => {
                self.watching.borrow().as_ref().map(|w| w.playback.audio_id)
            }
            _ => None,
        };
        crate::audio::set_active(active_id);

        let single_on = page == Page::Single && self.single_audio_on.get();
        if b.get_single_audio_on() != single_on {
            b.set_single_audio_on(single_on);
        }
        let player_on = page == Page::Recordings && self.player_audio_on.get() && self.watching.borrow().is_some();
        if b.get_player_audio_on() != player_on {
            b.set_player_audio_on(player_on);
        }
        let tile_cam = if page == Page::Live { self.tile_audio_cam.get() } else { None };
        for i in 0..self.tiles.row_count() {
            let Some(mut tile) = self.tiles.row_data(i) else { continue };
            let want = tile_cam == Some(tile.id as u32);
            if tile.audio_on != want {
                tile.audio_on = want;
                self.tiles.set_row_data(i, tile);
            }
        }
    }

    fn toggle_tile_audio(&self, camera_id: u32) {
        let next = if self.tile_audio_cam.get() == Some(camera_id) { None } else { Some(camera_id) };
        self.tile_audio_cam.set(next);
        self.sync_audio();
    }

    fn single_toggle_audio(&self) {
        self.single_audio_on.set(!self.single_audio_on.get());
        self.sync_audio();
    }

    fn player_toggle_audio(&self) {
        self.player_audio_on.set(!self.player_audio_on.get());
        self.sync_audio();
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

    /// Releases Live-view tile textures while away from the Live page
    /// (`tiles_from_pictures` restores them immediately when returning).
    fn clear_tile_frames(&self) {
        for i in 0..self.tiles.row_count() {
            let Some(mut tile) = self.tiles.row_data(i) else { continue };
            if tile.has_frame {
                tile.frame = Image::default();
                tile.has_frame = false;
                self.tiles.set_row_data(i, tile);
            }
        }
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
        if from == Page::Live && page != Page::Live {
            self.clear_tile_frames();
        }
        if from == Page::Single && page != Page::Single {
            self.stop_live_recording(false);
        }
        if from == Page::People && page != Page::People {
            self.stop_face_photos();
        }
        if page != from {
            self.reset_zoom();
        }
        // Leaving Playback pauses the recording and starts the linger timer
        // so its cache and thumbnails are freed once idle elsewhere.
        if from == Page::Recordings && page != Page::Recordings {
            if let Some(w) = self.watching.borrow().as_ref() {
                w.playback.set_paused(true);
            }
            self.stop_event_pictures();
            self.playback_left_at.set(Some(Instant::now()));
        }
        if page == Page::Recordings {
            self.playback_left_at.set(None);
        }
        if page != Page::Single {
            *self.single.borrow_mut() = None;
            b.set_reboot_confirm(false);
            if b.get_single_has_frame() {
                b.set_single_frame(Image::default());
                b.set_single_has_frame(false);
            }
        }
        if page == Page::Home {
            self.home_skip.borrow_mut().clear();
            self.refresh_home_cards();
        }
        if page == Page::Live {
            self.tiles_from_pictures();
        }
        b.set_page(page);
        ui.invoke_focus_keys();
        self.sync_streams();
        if page == Page::Recordings {
            if self.hub.borrow().hub.is_none() && !b.get_hub_busy() {
                self.connect_hub();
            } else if self.event_pictures.borrow().is_empty() && !self.hub.borrow().events.is_empty() {
                // Reload cached event JPEGs from disk if they were cleared while away.
                self.fetch_event_pictures();
            }
        }
        if page == Page::People {
            let hub_id = self.selected_hub().map(|h| h.device_id);
            if self.faces_loaded_hub.borrow().as_ref() != hub_id.as_ref() && !b.get_faces_loading() {
                self.load_faces();
            } else {
                self.update_face_track_date_label();
                self.refresh_faces_ui();
                self.refresh_face_tracks_ui();
                self.fetch_face_photos();
            }
        }
        self.sync_timer();
        self.sync_audio();
    }

    fn reset_zoom(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if (b.get_video_zoom() - 1.0).abs() > 0.001 || b.get_video_pan_x() != 0.0 || b.get_video_pan_y() != 0.0 {
            b.set_video_zoom(1.0);
            b.set_video_pan_x(0.0);
            b.set_video_pan_y(0.0);
        }
    }

    fn open_camera_slot(&self, slot: usize) {
        let id = self.cfg.borrow().cameras.get(slot).map(|c| c.id);
        if let Some(id) = id {
            self.open_camera(id);
        }
    }

    fn set_fullscreen(&self, on: bool) {
        let ui = self.ui();
        crate::chrome::set_fullscreen(on);
        ui.window().set_fullscreen(on);
        ui.global::<Bridge>().set_fullscreen(on);
        if !on {
            crate::chrome::refresh_frame();
        }
    }

    fn toggle_fullscreen(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let watching = b.get_page() == Page::Recordings && b.get_player_open();
        if matches!(b.get_page(), Page::Live | Page::Single) || watching || b.get_fullscreen() {
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
        self.stop_live_recording(false);
        self.reset_zoom();
        let Some(cam) = self.cfg.borrow().camera(id).cloned() else { return };
        let quality = Quality::Hd;
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_page() != Page::Single {
            self.return_page.set(b.get_page());
            if b.get_page() == Page::Live {
                self.capture_thumbnails();
                self.clear_tile_frames();
            }
        }
        *self.single.borrow_mut() = Some((id, quality));
        *self.single_shown.borrow_mut() = (0, String::new());

        b.set_single_name(cam.name.as_str().into());
        b.set_single_quality(quality.index());
        // Until the HD video comes: the grid's picture, or the last one.
        let placeholder = self.picture_of(id);
        b.set_single_has_frame(placeholder.is_some());
        b.set_single_stale(true);
        b.set_single_frame(placeholder.map(Image::from_rgba8_premultiplied).unwrap_or_default());
        b.set_single_status(self.tile_status(&cam).into());
        // The pan/tilt and control tiles update once the camera has answered.
        b.set_single_ptz(false);
        b.set_ptz_open(false);
        b.set_single_privacy(false);
        b.set_single_privacy_known(false);
        b.set_single_spotlight(false);
        b.set_single_spotlight_known(false);
        b.set_single_nv_known(false);
        b.set_single_nv_index(0);
        b.set_single_track(false);
        b.set_single_track_known(false);
        b.set_single_patrolling(false);
        b.set_single_siren(false);
        b.set_reboot_confirm(false);
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
        self.stop_live_recording(false);
        self.reset_zoom();
        let Some((id, _)) = *self.single.borrow() else { return };
        let quality = Quality::from_index(index);
        *self.single.borrow_mut() = Some((id, quality));
        *self.single_shown.borrow_mut() = (0, String::new());
        // The current picture stays, dimmed, until the other stream's comes.
        self.ui().global::<Bridge>().set_single_stale(true);
        self.sync_streams();
    }

    fn single_record(&self) {
        if self.live_recording.borrow().is_some() {
            self.stop_live_recording(false);
            return;
        }
        let Some(key) = *self.single.borrow() else { return };
        let Some(ffmpeg_exists) = self.ffmpeg.borrow().as_ref().map(|_| ()) else {
            self.status(i18n::pick(
                "FFmpeg is missing; cannot record MP4.",
                "FFmpeg bulunamadı; MP4 kaydedilemiyor.",
            ));
            return;
        };
        let _ = ffmpeg_exists;
        let name = self.cfg.borrow().camera(key.0).map(|c| c.name.clone()).unwrap_or_else(|| "camera".into());
        let when = Local::now();
        let target = config::recordings_dir().join(format!(
            "{}_live_{}.mp4",
            file_safe(&name),
            when.format("%Y-%m-%d_%H-%M-%S")
        ));
        let partial = clip::partial_path(&target);
        let started = {
            let streams = self.streams.borrow();
            let Some(live) = streams.get(&key) else {
                self.status(i18n::pick("Camera is not streaming yet.", "Kamera henüz yayın yapmıyor."));
                return;
            };
            live.handle.start_recording(&partial)
        };
        match started {
            Ok(()) => {
                *self.live_recording.borrow_mut() = Some(LiveRecording {
                    key,
                    camera_name: name,
                    partial,
                    target,
                    started: Instant::now(),
                    shown_secs: 0,
                });
                let ui = self.ui();
                let b = ui.global::<Bridge>();
                b.set_single_recording(true);
                b.set_single_rec_time("0:00".into());
                self.status(i18n::pick("Recording live video…", "Canlı video kaydediliyor…"));
            }
            Err(e) => {
                self.status(if i18n::is_tr() {
                    format!("Canlı kayıt başlatılamadı: {e}")
                } else {
                    format!("Could not start live recording: {e}")
                });
            }
        }
    }

    fn stop_live_recording(&self, shutting_down: bool) {
        let Some(rec) = self.live_recording.borrow_mut().take() else { return };
        if let Some(live) = self.streams.borrow().get(&rec.key) {
            live.handle.stop_recording();
        }
        if let Some(ui) = self.ui.upgrade() {
            let b = ui.global::<Bridge>();
            b.set_single_recording(false);
            b.set_single_rec_time(SharedString::new());
        }
        let bytes = std::fs::metadata(&rec.partial).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            let _ = std::fs::remove_file(&rec.partial);
            if !shutting_down {
                self.status(i18n::pick(
                    "Live recording stopped: no video data received yet.",
                    "Canlı kayıt durduruldu: henüz video verisi gelmedi.",
                ));
            }
            return;
        }
        if shutting_down {
            // Runtime is exiting; keep the raw MPEG-TS beside the target so nothing is lost.
            let fallback = rec.target.with_extension("ts");
            let _ = std::fs::rename(&rec.partial, &fallback);
            return;
        }
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            let _ = std::fs::remove_file(&rec.partial);
            return;
        };
        let label = format!("{} · {}", rec.camera_name, i18n::pick("Live", "Canlı"));
        let row = self.push_download(
            label,
            i18n::pick("Converting to MP4…", "MP4'e dönüştürülüyor…").into(),
            &rec.target,
            false,
        );
        self.status(i18n::pick("Finalizing live MP4 recording…", "Canlı MP4 kaydı hazırlanıyor…"));
        let partial = rec.partial;
        let target = rec.target;
        self.rt.spawn(async move {
            let res = crate::ffmpeg::remux_ts_to_mp4(&ffmpeg, &partial, &target).await;
            let _ = std::fs::remove_file(&partial);
            let (row_text, done, toast) = match res {
                Ok(()) => {
                    let mb = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                    let toast = if i18n::is_tr() {
                        format!("Canlı kayıt kaydedildi · {mb:.1} MB (Videolar\\Tapas)")
                    } else {
                        format!("Saved live recording · {mb:.1} MB (Videos\\Tapas)")
                    };
                    (format!("{} · {mb:.1} MB", i18n::pick("Saved", "Kaydedildi")), true, toast)
                }
                Err(e) => {
                    let toast = if i18n::is_tr() {
                        format!("Canlı kayıt kaydedilemedi: {e:#}")
                    } else {
                        format!("Could not save live recording: {e:#}")
                    };
                    (format!("{}: {e:#}", i18n::pick("Failed", "Başarısız")), false, toast)
                }
            };
            on_ui(move |app| {
                app.set_download_status(row, row_text, done);
                app.status(toast);
            });
        });
    }

    fn snapshot(&self) {
        let Some(key) = *self.single.borrow() else { return };
        let frame = self.streams.borrow().get(&key).and_then(|l| l.handle.latest_frame());
        let Some(frame) = frame else {
            self.status("No picture yet.");
            return;
        };
        let name = self.cfg.borrow().camera(key.0).map(|c| c.name.clone()).unwrap_or_default();
        self.save_snapshot(frame, &name, Local::now());
    }

    /// Saves a picture to `Pictures\Tapas`, named after the camera and
    /// when it was taken.
    fn save_snapshot(&self, frame: Frame, name: &str, when: chrono::DateTime<Local>) {
        let path = config::snapshot_dir().join(format!("{}_{}.png", file_safe(name), when.format("%Y-%m-%d_%H-%M-%S")));
        self.rt.spawn_blocking(move || {
            let result = (|| -> anyhow::Result<()> {
                std::fs::create_dir_all(path.parent().unwrap())?;
                let img = image::RgbaImage::from_raw(frame.width(), frame.height(), frame.as_bytes().to_vec())
                    .ok_or_else(|| anyhow::anyhow!("bad frame"))?;
                image::DynamicImage::ImageRgba8(img).to_rgb8().save(&path)?;
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
        let homes = self.homes.borrow();
        let title = match homes.names.as_slice() {
            [name, ..] => name.clone(),
            [] => i18n::pick("Home", "Ana sayfa").to_string(),
        };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_home_title(title.into());
        let mut subtitle = vec![if i18n::is_tr() {
            format!("{} kamera", cfg.cameras.len())
        } else if cfg.cameras.len() == 1 {
            "1 camera".to_string()
        } else {
            format!("{} cameras", cfg.cameras.len())
        }];
        if !cfg.hubs.is_empty() {
            subtitle.push(if i18n::is_tr() {
                format!("{} hub", cfg.hubs.len())
            } else if cfg.hubs.len() == 1 {
                "1 hub".to_string()
            } else {
                format!("{} hubs", cfg.hubs.len())
            });
        }
        b.set_home_subtitle(subtitle.join(" · ").into());
        let has_hub = !cfg.hubs.is_empty();
        let cards: Vec<HomeCard> = cfg
            .cameras
            .iter()
            .map(|c| {
                let thumb = thumbs.get(&c.id).map(|t| t.frame.clone());
                let room = c.cloud.as_ref().and_then(|l| homes.room_of(&l.device_id)).cloned().unwrap_or_default();
                HomeCard {
                    id: c.id as i32,
                    name: c.name.as_str().into(),
                    detail: match &c.cloud {
                        Some(link) if !link.model.is_empty() => link.model.as_str().into(),
                        Some(_) => "Tapo camera".into(),
                        None => "Not linked to your Tapo account".into(),
                    },
                    room: room.into(),
                    has_thumb: thumb.is_some(),
                    thumb: thumb.map(Image::from_rgba8_premultiplied).unwrap_or_default(),
                    state: states.get(&c.id).map(String::as_str).unwrap_or("").into(),
                    playback: has_hub && c.cloud.is_some(),
                }
            })
            .collect();
        self.home_cards.set_vec(cards);
    }

    /// Keeps the newest picture of each streaming camera.
    fn capture_thumbnails(&self) {
        let frames: Vec<(u32, Frame)> = self
            .streams
            .borrow()
            .iter()
            .filter_map(|(key, live)| Some((key.0, live.handle.latest_frame()?)))
            .collect();
        for (id, frame) in frames {
            self.keep_picture(id, frame);
        }
    }

    /// A camera's newest picture: kept for its card and tile, and written to
    /// disk now and then. Big ones are made smaller first, off the UI thread.
    fn keep_picture(&self, id: u32, frame: Frame) {
        let device_id = self.cfg.borrow().camera(id).and_then(|c| c.cloud.as_ref()).map(|l| l.device_id.clone());
        let save = {
            let thumbs = self.thumbnails.borrow();
            thumbs.get(&id).is_none_or(|t| t.saved.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)))
        };
        if frame.width() <= PICTURE_WIDTH {
            self.thumbnails.borrow_mut().insert(
                id,
                Thumb { frame: frame.clone(), taken: SystemTime::now(), saved: save.then(Instant::now) },
            );
            if let (true, Some(device_id)) = (save, device_id) {
                self.rt.spawn_blocking(move || crate::pictures::save_camera(&device_id, &frame));
            }
            return;
        }
        self.rt.spawn_blocking(move || {
            let small = shrink(&frame, PICTURE_WIDTH);
            if let (true, Some(device_id)) = (save, &device_id) {
                crate::pictures::save_camera(device_id, &small);
            }
            on_ui(move |app| {
                app.thumbnails.borrow_mut().insert(
                    id,
                    Thumb { frame: small, taken: SystemTime::now(), saved: save.then(Instant::now) },
                );
                app.refresh_home_cards();
            });
        });
    }

    /// The cameras' pictures from the last run, so Home and the tiles have
    /// something to show at once.
    fn load_saved_pictures(&self) {
        let cameras: Vec<(u32, String)> = {
            let thumbs = self.thumbnails.borrow();
            self.cfg
                .borrow()
                .cameras
                .iter()
                .filter(|c| !thumbs.contains_key(&c.id))
                .filter_map(|c| Some((c.id, c.cloud.as_ref()?.device_id.clone())))
                .collect()
        };
        let mut thumbs = self.thumbnails.borrow_mut();
        for (id, device_id) in cameras {
            if let Some((frame, taken)) = crate::pictures::load_camera(&device_id) {
                thumbs.insert(id, Thumb { frame, taken, saved: Some(Instant::now()) });
            }
        }
    }

    /// What to show for a camera until its video comes: a stream's picture
    /// (e.g. the grid's), else its last one.
    fn picture_of(&self, id: u32) -> Option<Frame> {
        let streams = self.streams.borrow();
        let streamed = streams.iter().filter(|(k, _)| k.0 == id).find_map(|(_, l)| l.handle.latest_frame());
        streamed.or_else(|| self.thumbnails.borrow().get(&id).map(|t| t.frame.clone()))
    }

    /// Live view's tiles: each camera's live picture if its stream is still
    /// running, else its last picture, dimmed, while it connects.
    fn tiles_from_pictures(&self) {
        for live in self.streams.borrow_mut().values_mut() {
            live.shown_version = 0; // the tiles pick up the latest frame again
            live.shown_status.clear();
        }
        let qualities: HashMap<u32, Quality> = self.cfg.borrow().cameras.iter().map(|c| (c.id, c.grid_quality)).collect();
        for i in 0..self.tiles.row_count() {
            let Some(mut tile) = self.tiles.row_data(i) else { continue };
            let id = tile.id as u32;
            let live_frame = qualities.get(&id).and_then(|q| {
                self.streams.borrow().get(&(id, *q)).and_then(|l| l.handle.latest_frame())
            });
            if let Some(frame) = live_frame {
                tile.frame = Image::from_rgba8_premultiplied(frame);
                tile.has_frame = true;
                self.tiles.set_row_data(i, tile);
                continue;
            }
            let picture = self.thumbnails.borrow().get(&id).map(|t| t.frame.clone());
            tile.has_frame = picture.is_some();
            tile.frame = picture.map(Image::from_rgba8_premultiplied).unwrap_or_default();
            tile.stale = true;
            tile.live = false;
            self.tiles.set_row_data(i, tile);
        }
    }

    /// On Home, cameras without a recent picture stream until their first
    /// frame, which becomes their card's picture; cameras that can't be
    /// reached get a badge instead. Running streams (e.g. the grid's, a
    /// moment after leaving it) keep the pictures fresh too.
    fn pump_home(&self) {
        let mut captured = Vec::new();
        let mut gave_up = Vec::new();
        let mut changed = false;
        {
            let streams = self.streams.borrow();
            let thumbs = self.thumbnails.borrow();
            let mut states = self.home_states.borrow_mut();
            for (key, live) in streams.iter() {
                let slot = live.handle.slot.lock().unwrap();
                if let Some(frame) = slot.frame.clone() {
                    let recent = thumbs.get(&key.0).is_some_and(|t| t.taken.elapsed().is_ok_and(|a| a < Duration::from_secs(30)));
                    if !recent {
                        captured.push((key.0, frame));
                    }
                    if states.remove(&key.0).is_some() {
                        changed = true;
                    }
                    continue;
                }
                let status = slot.status_text();
                if matches!(slot.status, crate::stream::Status::Retrying(_)) && live.unused_since.is_none() {
                    gave_up.push(key.0);
                }
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
        let resync = !captured.is_empty() || !gave_up.is_empty();
        for (id, frame) in captured {
            self.keep_picture(id, frame);
            changed = true;
        }
        // A camera that can't stream now waits for the next visit, so the
        // others get their turn.
        self.home_skip.borrow_mut().extend(gave_up);
        if changed {
            self.refresh_home_cards();
        }
        if resync {
            self.sync_streams();
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

    // -------------------------------------------------------------- updates

    fn update_repo(&self) -> String {
        match self.cfg.borrow().update_repo.trim() {
            "" => crate::update::DEFAULT_REPO.to_string(),
            repo => repo.to_string(),
        }
    }

    /// Looks for a newer version. `asked`: from the button, so the answer is
    /// shown however it turns out.
    fn check_update(&self, asked: bool) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_update_busy() {
            return;
        }
        b.set_update_busy(asked);
        if asked {
            b.set_update_status(i18n::pick("Checking for updates…", "Güncellemeler denetleniyor…").into());
        }
        let repo = self.update_repo();
        self.rt.spawn(async move {
            let result = crate::update::check(&repo).await;
            on_ui(move |app| app.update_checked(asked, result));
        });
    }

    fn update_checked(&self, asked: bool, result: anyhow::Result<Option<crate::update::Release>>) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_update_busy(false);
        match result {
            Ok(Some(release)) => {
                log::info!("update: version {} is available", release.version);
                b.set_update_available(true);
                b.set_update_version(release.version.as_str().into());
                let notes = release.notes.lines().next().unwrap_or("").to_string();
                b.set_update_status(
                    if notes.is_empty() {
                        if i18n::is_tr() {
                            format!("Sürüm {} mevcut.", release.version)
                        } else {
                            format!("Version {} is available.", release.version)
                        }
                    } else if i18n::is_tr() {
                        format!("Sürüm {} mevcut: {notes}", release.version)
                    } else {
                        format!("Version {} is available: {notes}", release.version)
                    }
                    .into(),
                );
                *self.update.borrow_mut() = Some(release);
            }
            Ok(None) => {
                b.set_update_available(false);
                b.set_update_status(i18n::pick("This is the newest version.", "En güncel sürümü kullanıyorsunuz.").into());
            }
            Err(e) => {
                log::info!("update check: {e:#}");
                if asked {
                    b.set_update_status(
                        format!("{}: {e:#}", i18n::pick("Couldn't check for updates", "Güncellemeler denetlenemedi"))
                            .into(),
                    );
                }
            }
        }
    }

    /// Downloads the new version's installer and runs it; the app closes so
    /// it can be replaced, and the installer starts the new version.
    fn install_update(&self) {
        let Some(release) = self.update.borrow().clone() else { return };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_update_busy(true);
        b.set_update_failed(false);
        b.set_update_status(
            if i18n::is_tr() {
                format!("Sürüm {} indiriliyor…", release.version)
            } else {
                format!("Downloading version {}…", release.version)
            }
            .into(),
        );
        self.rt.spawn(async move {
            let mut last = Instant::now();
            let mut progress = |got: u64, total: u64| {
                if last.elapsed() >= Duration::from_millis(300) {
                    last = Instant::now();
                    let label = i18n::pick("Downloading the update…", "Güncelleme indiriliyor…");
                    let text = match (got * 100).checked_div(total) {
                        Some(percent) => format!("{label} {percent}%"),
                        None => format!("{label} {:.1} MB", got as f64 / 1_048_576.0),
                    };
                    on_ui(move |app| app.ui().global::<Bridge>().set_update_status(text.into()));
                }
            };
            let result = crate::update::download(&release, &mut progress).await;
            on_ui(move |app| app.update_downloaded(result));
        });
    }

    fn update_downloaded(&self, result: anyhow::Result<PathBuf>) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let started = result.and_then(|path| crate::update::install(&path));
        match started {
            Ok(()) => {
                b.set_update_status(
                    i18n::pick(
                        "Installing the update; the app will start again…",
                        "Güncelleme kuruluyor; uygulama yeniden başlatılacak…",
                    )
                    .into(),
                );
                // Let the installer start, then make way for it.
                slint::Timer::single_shot(Duration::from_millis(600), || {
                    let _ = slint::quit_event_loop();
                });
            }
            Err(e) => {
                b.set_update_busy(false);
                b.set_update_failed(true);
                b.set_update_status(format!("{}: {e:#}", i18n::pick("Couldn't update", "Güncelleme başarısız")).into());
            }
        }
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
        b.set_single_spotlight_known(state.spotlight.is_some());
        b.set_single_spotlight(state.spotlight == Some(true));
        b.set_single_nv_known(state.night_vision.is_some());
        if let Some(nv) = state.night_vision {
            b.set_single_nv_index(nv.index());
        }
        b.set_single_track_known(state.auto_track.is_some());
        b.set_single_track(state.auto_track == Some(true));
        b.set_single_patrolling(state.patrolling);
        b.set_single_siren(state.siren_on == Some(true));
        b.set_ptz_busy(state.busy);
        b.set_ptz_message(state.message.as_str().into());
        let rows: Vec<PresetRow> = state
            .presets
            .iter()
            .map(|(id, name)| PresetRow { id: id.into(), name: name.into() })
            .collect();
        b.set_presets(ModelRc::new(VecModel::from(rows)));
    }

    fn spotlight_toggle(&self) {
        let on = !self.ui().global::<Bridge>().get_single_spotlight();
        self.ptz_send(PtzCmd::Spotlight(on));
    }

    fn night_vision_changed(&self, index: i32) {
        self.ptz_send(PtzCmd::NightVision(NightVisionMode::from_index(index)));
    }

    fn auto_track_toggle(&self) {
        let on = !self.ui().global::<Bridge>().get_single_track();
        self.ptz_send(PtzCmd::AutoTrack(on));
    }

    fn patrol_start(&self, axis: i32) {
        let axis = if axis == 1 { PatrolAxis::Vertical } else { PatrolAxis::Horizontal };
        self.ptz_send(PtzCmd::PatrolStart(axis));
    }

    fn siren_toggle(&self) {
        let on = !self.ui().global::<Bridge>().get_single_siren();
        self.ptz_send(PtzCmd::Siren(on));
    }

    // -------------------------------------------------------------- settings

    fn load_settings_into_ui(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let cfg = self.cfg.borrow();
        b.set_renderer_index(cfg.renderer.index());
        b.set_theme_index(cfg.theme.index());
        b.set_language_index(cfg.language.index());
        b.set_config_path(config::config_path().display().to_string().into());
    }

    /// Applies the Settings page's appearance, language and renderer choices as
    /// they're made.
    fn save_settings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let lang_changed = {
            let mut cfg = self.cfg.borrow_mut();
            let renderer = Renderer::from_index(b.get_renderer_index());
            if renderer != cfg.renderer {
                cfg.renderer = renderer;
                self.status(i18n::pick(
                    "The new renderer is used once Tapas restarts.",
                    "Yeni grafik motoru Tapas yeniden başlatılınca uygulanır.",
                ));
            }
            cfg.theme = Theme::from_index(b.get_theme_index());
            let language = Language::from_index(b.get_language_index());
            let lang_changed = language != cfg.language;
            cfg.language = language;
            lang_changed
        };
        self.persist();
        let theme_index = self.cfg.borrow().theme.index();
        ui.invoke_set_theme(theme_index);
        crate::chrome::set_theme(theme_index);
        if lang_changed {
            let lang = i18n::set(self.cfg.borrow().language);
            i18n::apply_to_ui(&ui, lang);
            if !b.get_update_available() && !b.get_update_busy() && !b.get_update_status().is_empty() {
                b.set_update_status(i18n::pick("This is the newest version.", "En güncel sürümü kullanıyorsunuz.").into());
            }
            self.refresh_home_cards();
            self.refresh_camera_lists();
            self.refresh_hub_lists();
            self.update_rec_date_label();
            self.update_calendar();
            self.rebuild_rec_rows();
            self.refresh_rec_rows();
            self.update_face_track_date_label();
            self.refresh_faces_ui();
            self.refresh_face_tracks_ui();
        }
        self.load_settings_into_ui();
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
                    Some(link) => {
                        if !link.model.is_empty() {
                            detail.push(link.model.clone());
                        }
                        if !same_name(&link.name, &c.name) {
                            detail.push(if i18n::is_tr() {
                                format!("Tapo uygulamasında “{}”", link.name)
                            } else {
                                format!("“{}” in the Tapo app", link.name)
                            });
                        }
                    }
                    None => detail.push(
                        i18n::pick(
                            "Not linked to a camera on your Tapo account",
                            "Tapo hesabındaki bir kameraya bağlı değil",
                        )
                        .into(),
                    ),
                }
                detail.push(if i18n::is_tr() {
                    format!("Canlı izlemede {}", Quality::LABELS[c.grid_quality.index() as usize])
                } else {
                    format!("{} in live view", Quality::LABELS[c.grid_quality.index() as usize])
                });
                CameraRow { id: c.id as i32, name: c.name.as_str().into(), detail: detail.join("  ·  ").into() }
            })
            .collect();
        b.set_cameras(ModelRc::new(VecModel::from(rows)));

        let tile_cam = self.tile_audio_cam.get();
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
                stale: true,
                audio_on: tile_cam == Some(c.id),
            })
            .collect();
        self.tiles.set_vec(tiles);
        self.load_saved_pictures();
        self.tiles_from_pictures();
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
            .map(|(i, h)| {
                let known = self.hub_info.borrow().get(&h.device_id).cloned();
                let loaded = known.is_some() || self.hub_info_tried.get();
                let info = known.unwrap_or_default();
                let mut detail = hub_detail(h);
                if !info.firmware.is_empty() {
                    detail += &format!(" · firmware {}", info.firmware.split_whitespace().next().unwrap_or(""));
                }
                let mut storage = info.storage.join("\n");
                if !info.storage_status.is_empty() {
                    storage += &format!("\n{}", info.storage_status);
                }
                let room = self.homes.borrow().room_of(&h.device_id).cloned().unwrap_or_default();
                // The main (largest) disk gets the bar; the rest, and any
                // problem, a line under it.
                let main = info.disks.first();
                let disk = main.map(|d| disk_name(&d.label)).unwrap_or_default();
                let health = main
                    .map(|d| {
                        let mut parts = Vec::new();
                        if let Some(t) = d.temperature_c {
                            parts.push(format!("{t}°C"));
                        }
                        if let Some(h) = d.power_on_hours.filter(|&h| h > 0) {
                            if h >= 48 {
                                parts.push(if i18n::is_tr() {
                                    format!("{} gün açık", h / 24)
                                } else {
                                    format!("{} days on", h / 24)
                                });
                            } else {
                                parts.push(if i18n::is_tr() {
                                    format!("{h} saat açık")
                                } else {
                                    format!("{h} h on")
                                });
                            }
                        }
                        parts.join(" · ")
                    })
                    .unwrap_or_default();
                let looping = info.loop_recording == Some(true);
                let full = main.and_then(cloud::Disk::used).is_some_and(|u| u > 0.97);
                let space = match main {
                    // As the Tapo app: a full disk recording in a loop is fine.
                    Some(d) if full && looping => if i18n::is_tr() {
                        format!("{} · dolu, en eskinin üzerine yazılıyor", d.total)
                    } else {
                        format!("{} · full, recording over the oldest", d.total)
                    },
                    Some(d) if !d.free.is_empty() => if i18n::is_tr() {
                        format!("{} / {} boş", d.free, d.total)
                    } else {
                        format!("{} free of {}", d.free, d.total)
                    },
                    Some(d) => d.total.clone(),
                    None => String::new(),
                };
                let mut note: Vec<String> = info
                    .disks
                    .iter()
                    .skip(1)
                    .map(|d| match d.free.is_empty() {
                        false => if i18n::is_tr() {
                            format!("{}: {} / {} boş", disk_name(&d.label), d.free, d.total)
                        } else {
                            format!("{}: {} free of {}", disk_name(&d.label), d.free, d.total)
                        },
                        true => format!("{}: {}", disk_name(&d.label), d.total),
                    })
                    .collect();
                if !info.storage_status.is_empty() {
                    note.push(info.storage_status.clone());
                }
                if known_none_after_try(loaded, main) {
                    note.push(
                        i18n::pick(
                            "The hub didn't say how its storage is.",
                            "Hub depolama durumunu bildirmedi.",
                        )
                        .into(),
                    );
                }
                let alarm_on = self.hub_alarms.borrow().contains(&h.device_id);
                let busy = self.hub_busy_ids.borrow().contains(&h.device_id);
                HubRow {
                    id: i as i32,
                    name: h.name.as_str().into(),
                    detail: detail.into(),
                    storage: storage.into(),
                    room: room.into(),
                    disk: disk.into(),
                    space: space.into(),
                    health: health.into(),
                    used: main.and_then(cloud::Disk::used).unwrap_or(-1.0),
                    warn: full && !looping,
                    note: note.join("  ·  ").into(),
                    loaded,
                    loop_known: info.loop_recording.is_some(),
                    loop_on: looping,
                    alarm_on,
                    busy,
                }
            })
            .collect();
        b.set_hubs(ModelRc::new(VecModel::from(rows)));
        b.set_hub_names(string_model(cfg.hubs.iter().map(|h| h.name.clone())));
        if b.get_rec_hub() < 0 || b.get_rec_hub() as usize >= cfg.hubs.len() {
            b.set_rec_hub(0);
        }
    }

    fn hub_toggle_loop(&self, index: usize) {
        let Some(hub) = self.cfg.borrow().hubs.get(index).cloned() else { return };
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else { return };
        let currently = self.hub_info.borrow().get(&hub.device_id).and_then(|i| i.loop_recording) == Some(true);
        let want = !currently;
        let id = hub.device_id.clone();
        self.hub_busy_ids.borrow_mut().insert(id.clone());
        self.refresh_hub_lists();
        self.rt.spawn(async move {
            let result = client.set_loop_recording(&hub, want).await;
            on_ui(move |app| {
                app.hub_busy_ids.borrow_mut().remove(&id);
                match result {
                    Ok(now) => {
                        if let Some(info) = app.hub_info.borrow_mut().get_mut(&id) {
                            info.loop_recording = Some(now);
                        }
                        app.status(if now { "Loop recording turned on." } else { "Loop recording turned off." });
                    }
                    Err(e) => {
                        if e.is_auth() {
                            app.cloud_auth_failed();
                        }
                        app.status(format!("Could not change loop recording: {e}"));
                    }
                }
                app.refresh_hub_lists();
            });
        });
    }

    fn hub_toggle_alarm(&self, index: usize) {
        let Some(hub) = self.cfg.borrow().hubs.get(index).cloned() else { return };
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else { return };
        let want = !self.hub_alarms.borrow().contains(&hub.device_id);
        let id = hub.device_id.clone();
        self.hub_busy_ids.borrow_mut().insert(id.clone());
        self.refresh_hub_lists();
        self.rt.spawn(async move {
            let result = client.set_hub_alarm(&hub, want).await;
            on_ui(move |app| {
                app.hub_busy_ids.borrow_mut().remove(&id);
                match result {
                    Ok(now) => {
                        if now {
                            app.hub_alarms.borrow_mut().insert(id);
                            app.status("Hub alarm triggered.");
                        } else {
                            app.hub_alarms.borrow_mut().remove(&id);
                            app.status("Hub alarm stopped.");
                        }
                    }
                    Err(e) => {
                        if e.is_auth() {
                            app.cloud_auth_failed();
                        }
                        app.status(format!("Could not toggle hub alarm: {e}"));
                    }
                }
                app.refresh_hub_lists();
            });
        });
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
        let initial = account.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        b.set_account_initial(initial.into());
        let server = session.as_ref().and_then(|s| url::Url::parse(&s.base_url).ok()).and_then(|u| u.host_str().map(str::to_string));
        b.set_account_server(server.unwrap_or_default().into());
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
        self.stop_live_recording(false);
        self.stop_face_photos();
        self.reset_zoom();
        self.streams.borrow_mut().clear();
        self.ptz.borrow_mut().clear();
        self.batch_cancel();
        self.batch_selected.borrow_mut().clear();
        self.faces.borrow_mut().clear();
        *self.faces_loaded_hub.borrow_mut() = None;
        self.face_tracks.borrow_mut().clear();
        self.face_photos.borrow_mut().clear();
        self.selected_face.set(None);
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_batch_mode(false);
        b.set_batch_selected_count(0);
        b.set_faces(ModelRc::default());
        b.set_face_tracks(ModelRc::default());
        b.set_selected_face_id(SharedString::new());
        b.set_selected_face_title(SharedString::new());
        self.release_playback_memory();
        self.clear_tile_frames();
        self.set_fullscreen(false);
        self.refresh_cloud_ui();
        self.sync_timer();
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
        // ⟳ on Home also takes new pictures of the cameras.
        if self.ui().global::<Bridge>().get_page() == Page::Home {
            for thumb in self.thumbnails.borrow_mut().values_mut() {
                thumb.taken = SystemTime::UNIX_EPOCH;
            }
            self.home_skip.borrow_mut().clear();
            self.sync_streams();
        }
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
        self.load_account_details(&hubs);
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

    /// The account's homes and rooms, and each hub's firmware and storage:
    /// extras, so failures are only logged.
    fn load_account_details(&self, hubs: &[CloudCamera]) {
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else { return };
        let hubs = hubs.to_vec();
        self.rt.spawn(async move {
            let homes = client.homes().await.unwrap_or_else(|e| {
                log::info!("no homes from the account: {e}");
                Homes::default()
            });
            let mut info = HashMap::new();
            for hub in &hubs {
                match client.hub_info(hub).await {
                    Ok(i) => {
                        info.insert(hub.device_id.clone(), i);
                    }
                    Err(e) => log::info!("no details from {}: {e}", hub.name),
                }
            }
            on_ui(move |app| {
                *app.homes.borrow_mut() = homes;
                *app.hub_info.borrow_mut() = info;
                app.hub_info_tried.set(true);
                app.refresh_home_cards();
                app.refresh_hub_lists();
            });
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
                // What failed with the old session: the hubs, homes and rooms.
                if !self.hub_info_tried.get()
                    && let Some(client) = self.cloud_client()
                {
                    self.rt.spawn(async move {
                        let hubs = client.list_hubs().await;
                        on_ui(move |app| app.cloud_hubs_loaded(hubs));
                    });
                }
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
        self.player_stop();
        b.set_hub_connected(false);
        b.set_hub_busy(false);
        b.set_rec_loading(false);
        self.rec_rows.set_vec(Vec::new());
        b.set_paired_names(ModelRc::default());
        self.update_calendar();
        self.refresh_timeline();
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
        let mut names = Vec::with_capacity(cameras.len() + 1);
        if cameras.len() > 1 {
            names.push(i18n::pick("All cameras (events)", "Tüm kameralar (olaylar)").to_string());
        }
        names.extend(cameras.iter().map(|c| {
            if c.hub_storage {
                c.name.clone()
            } else if i18n::is_tr() {
                format!("{} (hub'a kaydetmiyor)", c.name)
            } else {
                format!("{} (not recording to the hub)", c.name)
            }
        }));
        b.set_paired_names(string_model(names));
        // Default to the first camera that records to the hub (offset by 1 if
        // "All cameras" is listed at index 0), unless one was asked for.
        let offset = usize::from(cameras.len() > 1);
        let first = cameras.iter().position(|c| c.hub_storage).unwrap_or(0) + offset;
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
        self.open_camera_playback(id);
    }

    /// A camera's recordings on the hub, from today.
    fn open_camera_playback(&self, id: u32) {
        let wanted = self.cfg.borrow().camera(id).map(|c| {
            (c.cloud.as_ref().map(|l| l.device_id.clone()).unwrap_or_default(), c.name.clone(), None)
        });
        self.navigate(Page::Recordings);
        if self.cfg.borrow().hubs.is_empty() {
            self.status("Recordings are played from the H500 hub, and there's none on your Tapo account.");
            return;
        }
        *self.pending_rec_camera.borrow_mut() = wanted;
        // Like the Tapo app: start from today's recordings.
        *self.rec_date.borrow_mut() = Local::now().date_naive();
        self.rec_month.set(first_of_month(Local::now().date_naive()));
        self.update_rec_date_label();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_hub_connected() {
            self.select_pending_rec_camera();
        } else if !b.get_hub_busy() {
            self.connect_hub();
        }
    }

    fn rec_camera_combo_index(&self, device_id: &str, name: &str) -> Option<i32> {
        let hub = self.hub.borrow();
        let offset = usize::from(hub.cameras.len() > 1);
        hub.cameras
            .iter()
            .position(|c| !device_id.is_empty() && c.device_id == device_id)
            .or_else(|| hub.cameras.iter().position(|c| same_name(&c.name, name)))
            .map(|i| (i + offset) as i32)
    }

    fn select_pending_rec_camera(&self) {
        let Some((device_id, name, play_at)) = self.pending_rec_camera.borrow_mut().take() else { return };
        match self.rec_camera_combo_index(&device_id, &name) {
            Some(i) => self.ui().global::<Bridge>().set_rec_camera(i),
            None => self.status(format!("{name} isn't paired with this hub; pick a camera.")),
        }
        self.player_stop();
        {
            let mut session = self.hub.borrow_mut();
            session.days.clear();
            session.months.clear();
            session.pending_play_at = play_at;
        }
        self.update_calendar();
        self.load_days(self.rec_month.get());
        self.load_recordings();
    }

    fn all_cameras_mode(&self) -> bool {
        self.hub.borrow().cameras.len() > 1 && self.ui().global::<Bridge>().get_rec_camera() == 0
    }

    fn selected_rec_camera(&self) -> Option<HubCamera> {
        let hub = self.hub.borrow();
        let index = self.ui().global::<Bridge>().get_rec_camera().max(0) as usize;
        if hub.cameras.len() > 1 {
            index.checked_sub(1).and_then(|i| hub.cameras.get(i).cloned())
        } else {
            hub.cameras.get(index).cloned()
        }
    }

    fn camera_picked(&self) {
        self.player_stop();
        {
            let mut session = self.hub.borrow_mut();
            session.days.clear();
            session.months.clear();
            session.pending_play_at = None;
        }
        self.update_calendar();
        self.load_days(self.rec_month.get());
        self.load_recordings();
    }

    // ---------------------------------------------------------- the calendar

    /// ‹ › on the calendar.
    fn change_month(&self, delta: i32) {
        let month = self.rec_month.get();
        let moved = if delta < 0 {
            month.checked_sub_months(chrono::Months::new(1))
        } else {
            month.checked_add_months(chrono::Months::new(1))
        };
        let Some(moved) = moved.filter(|m| *m <= first_of_month(Local::now().date_naive())) else { return };
        self.rec_month.set(moved);
        self.update_calendar();
        self.load_days(moved);
    }

    /// A day in the calendar's grid (six weeks from the Monday before the 1st).
    fn pick_day(&self, index: i32) {
        let date = grid_start(self.rec_month.get()) + chrono::Duration::days(index as i64);
        if date <= Local::now().date_naive() {
            self.set_rec_date(date);
        }
    }

    fn set_rec_date(&self, date: NaiveDate) {
        let before = self.rec_date.replace(date);
        let month = first_of_month(date);
        if month != self.rec_month.get() {
            self.rec_month.set(month);
            self.load_days(month);
        }
        self.update_rec_date_label();
        self.update_calendar();
        if date != before {
            self.player_stop();
            self.load_recordings();
        }
    }

    fn update_rec_date_label(&self) {
        let date = *self.rec_date.borrow();
        let label = if date == Local::now().date_naive() {
            format!("{}, {:02} {}", i18n::pick("Today", "Bugün"), date.day(), i18n::short_month(date.month()))
        } else {
            format!("{:02} {} {}", date.day(), i18n::short_month(date.month()), date.year())
        };
        self.ui().global::<Bridge>().set_rec_date(label.into());
    }

    /// The month grid, marking days with recordings.
    fn update_calendar(&self) {
        let month = self.rec_month.get();
        let selected = *self.rec_date.borrow();
        let today = Local::now().date_naive();
        let start = grid_start(month);
        let session = self.hub.borrow();
        let cells: Vec<DayChip> = (0..42)
            .map(|i| {
                let day = start + chrono::Duration::days(i);
                DayChip {
                    day: day.day().to_string().into(),
                    has_video: session.days.contains(&day),
                    selected: day == selected,
                    future: day > today,
                    in_month: day.month() == month.month(),
                }
            })
            .collect();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_rec_days(ModelRc::new(VecModel::from(cells)));
        b.set_rec_month(format!("{} {}", i18n::month_name(month.month()), month.year()).into());
        b.set_rec_month_is_current(month >= first_of_month(today));
    }

    /// Which days of `month` have recordings. The hub answers for one month
    /// at a time, like the Tapo app's calendar.
    fn load_days(&self, month: NaiveDate) {
        let (Some(client), Some(hub), Some(camera)) = (
            self.cloud_client(),
            self.hub.borrow().hub.clone(),
            self.selected_rec_camera(),
        ) else {
            return;
        };
        if self.hub.borrow().months.contains(&month) {
            return;
        }
        let generation = self.hub.borrow().generation;
        let last = (month + chrono::Months::new(1) - chrono::Duration::days(1)).min(Local::now().date_naive());
        let camera_id = camera.device_id.clone();
        self.rt.spawn(async move {
            let result = client.hub_recording_days(&hub, &camera.device_id, month, last).await;
            on_ui(move |app| app.days_loaded(generation, camera_id, month, result));
        });
    }

    fn days_loaded(&self, generation: u64, camera_id: String, month: NaiveDate, result: cloud::Result<Vec<NaiveDate>>) {
        if self.hub.borrow().generation != generation
            || self.selected_rec_camera().map(|c| c.device_id) != Some(camera_id)
        {
            return;
        }
        match result {
            Ok(days) => {
                let mut session = self.hub.borrow_mut();
                session.days.extend(days);
                session.months.insert(month);
            }
            // The list says what's wrong; the calendar just has no marks.
            Err(e) => log::warn!("could not list the days with recordings: {e}"),
        }
        self.update_calendar();
    }

    // ------------------------------------------------------------- the list

    /// The day's recordings and detected events (or merged events across all
    /// cameras when "All cameras" is selected).
    fn load_recordings(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let request = {
            let mut session = self.hub.borrow_mut();
            session.rec_request += 1;
            session.clips.clear();
            session.events.clear();
            session.all_events.clear();
            session.rows.clear();
            session.rec_request
        };
        self.rec_rows.set_vec(Vec::new());
        self.stop_event_pictures();
        self.event_pictures.borrow_mut().clear();
        b.set_rec_message(SharedString::new());
        self.reset_timeline();
        let (Some(client), Some(hub)) = (self.cloud_client(), self.hub.borrow().hub.clone()) else {
            return;
        };
        let date = *self.rec_date.borrow();
        let (start, end) = local_day(date);

        if self.all_cameras_mode() {
            let cameras = self.hub.borrow().cameras.clone();
            b.set_rec_loading(true);
            self.rt.spawn(async move {
                let mut merged = Vec::new();
                for cam in cameras {
                    if let Ok(events) = client.hub_detections(&hub, &cam, start, end).await {
                        for e in events {
                            merged.push((cam.clone(), e));
                        }
                    }
                }
                merged.sort_by_key(|(_, e)| e.start);
                on_ui(move |app| app.all_events_loaded(request, merged));
            });
            return;
        }

        let Some(camera) = self.selected_rec_camera() else {
            return;
        };
        let player_id = self.player_id.clone();
        b.set_rec_loading(true);
        self.rt.spawn(async move {
            let clips = client.hub_clips(&hub, &camera.device_id, start, end, &player_id).await;
            // The events are extra: without them the list shows the recordings.
            let events = match &clips {
                Ok(_) => client.hub_detections(&hub, &camera, start, end).await.unwrap_or_else(|e| {
                    log::info!("no detected events from the hub: {e}");
                    Vec::new()
                }),
                Err(_) => Vec::new(),
            };
            on_ui(move |app| app.recordings_loaded(request, clips, events));
        });
    }

    fn all_events_loaded(&self, request: u64, merged: Vec<(HubCamera, Detection)>) {
        if self.hub.borrow().rec_request != request {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_rec_loading(false);
        let total = merged.len();
        {
            let mut session = self.hub.borrow_mut();
            session.all_events = merged;
        }
        b.set_rec_message(
            if total == 0 {
                "No detected events across your cameras on this day.".into()
            } else {
                "Click any event to jump straight to that camera's recording.".into()
            },
        );
        self.rebuild_rec_rows();
        self.refresh_rec_rows();
    }

    fn recordings_loaded(&self, request: u64, result: cloud::Result<Vec<Clip>>, events: Vec<Detection>) {
        if self.hub.borrow().rec_request != request {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_rec_loading(false);
        let mut play_at = None;
        match result {
            Ok(clips) => {
                let total: i64 = clips.iter().map(Clip::seconds).sum();
                b.set_rec_message(match clips.len() {
                    0 => SharedString::new(),
                    _ => format!("{} recorded this day", fmt_duration(total as u64)).into(),
                });
                // Days with recordings: today's are known now even if the
                // month's search came back empty.
                let date = *self.rec_date.borrow();
                let mut session = self.hub.borrow_mut();
                if !clips.is_empty() {
                    session.days.insert(date);
                }
                session.clips = clips;
                session.events = events;
                play_at = session.pending_play_at.take();
            }
            Err(e) => {
                if e.is_auth() {
                    self.cloud_auth_failed();
                }
                self.hub.borrow_mut().pending_play_at = None;
                b.set_rec_list_title(SharedString::new());
                b.set_rec_message(format!("Could not list the recordings: {e}").into());
            }
        }
        self.rebuild_rec_rows();
        self.update_calendar();
        self.refresh_rec_rows();
        self.refresh_timeline();
        if let Some(at_ms) = play_at {
            self.watch_from(at_ms);
        } else {
            self.fetch_event_pictures();
        }
    }

    fn rec_filter_changed(&self) {
        self.rebuild_rec_rows();
        self.refresh_rec_rows();
    }

    /// Recomputes `session.rows` from `all_events` / `events` / `clips` using
    /// the active AI event filter chip, and refreshes the 24-hour activity chart.
    fn rebuild_rec_rows(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let filter = EventFilter::from_index(b.get_rec_filter());
        let all_mode = self.all_cameras_mode();
        let entries: Vec<(i64, i64, i64, Option<HubCamera>)> = {
            let mut session = self.hub.borrow_mut();
            let mut entries = if all_mode {
                let list: Vec<(i64, i64, i64, Option<HubCamera>)> = session
                    .all_events
                    .iter()
                    .filter(|(_, e)| filter.matches(e))
                    .map(|(cam, e)| (e.start, e.end, e.kind, Some(cam.clone())))
                    .collect();
                let shown = list.len();
                let total = session.all_events.len();
                b.set_rec_list_title(match (shown, total) {
                    (0, 0) => SharedString::new(),
                    (s, t) if s == t => format!("{} · {s}", i18n::pick("All events", "Tüm olaylar")).into(),
                    (s, t) => format!("{} · {s}/{t}", i18n::pick("Events", "Olaylar")).into(),
                });
                list
            } else if !session.events.is_empty() {
                let list: Vec<(i64, i64, i64, Option<HubCamera>)> = session
                    .events
                    .iter()
                    .filter(|e| filter.matches(e))
                    .map(|e| (e.start, e.end, e.kind, None))
                    .collect();
                let shown = list.len();
                let total = session.events.len();
                b.set_rec_list_title(match (shown, total) {
                    (s, t) if s == t => format!("{} · {s}", i18n::pick("Detected events", "Algılanan olaylar")).into(),
                    (s, t) => format!("{} · {s}/{t}", i18n::pick("Events", "Olaylar")).into(),
                });
                list
            } else {
                let list: Vec<(i64, i64, i64, Option<HubCamera>)> = session
                    .clips
                    .iter()
                    .filter(|c| {
                        filter == EventFilter::All
                            || filter.matches(&Detection { start: c.start, end: c.end, kind: c.kind, kinds: vec![c.kind] })
                    })
                    .map(|c| (c.start, c.end, c.kind, None))
                    .collect();
                let n = list.len();
                b.set_rec_list_title(if n == 0 {
                    SharedString::new()
                } else {
                    format!("{} · {n}", i18n::pick("Recordings", "Kayıtlar")).into()
                });
                list
            };
            entries.reverse(); // newest first
            session.rows = with_hours(&entries);
            entries
        };

        // Update 24-hour activity summary chart from the filtered entries.
        let starts: Vec<i64> = entries.iter().map(|(s, _, _, _)| *s).collect();
        let (counts, peak) = hourly_activity(&starts);
        let max_count = peak.map(|(_, c)| c).unwrap_or(0);
        let bars: Vec<HourBar> = (0..24)
            .map(|h| {
                let count = counts[h];
                let ratio = if max_count > 0 { count as f32 / max_count as f32 } else { 0.0 };
                HourBar {
                    hour: h as i32,
                    count: count as i32,
                    ratio,
                    peak: max_count > 0 && count == max_count,
                }
            })
            .collect();
        b.set_hour_bars(ModelRc::new(VecModel::from(bars)));
        let summary = match peak {
            Some((h, c)) => format!("{} {:02}:00 ({c})", i18n::pick("Peak", "Zirve"), h),
            None => String::new(),
        };
        b.set_hour_summary(summary.into());

        // Drop selections for entries no longer visible after filtering/reload.
        let selected_cam = self.selected_rec_camera();
        let visible_keys: HashSet<(String, i64)> = {
            let session = self.hub.borrow();
            session
                .rows
                .iter()
                .filter_map(|r| row_selection_key(r, selected_cam.as_ref()))
                .collect()
        };
        let mut sel = self.batch_selected.borrow_mut();
        sel.retain(|k| visible_keys.contains(k));
        b.set_batch_selected_count(sel.len() as i32);
        if visible_keys.is_empty() && !b.get_batch_busy() {
            b.set_batch_mode(false);
        }
    }

    /// The row that holds the picture being watched, if any.
    fn watched_row(&self) -> Option<usize> {
        let watching = self.watching.borrow();
        let w = watching.as_ref()?;
        let position = w.playback.state.lock().unwrap().position_ms / 1000;
        let session = self.hub.borrow();
        session.rows.iter().position(|r| r.entry().is_some_and(|(start, end, _, _)| start <= position && position <= end))
    }

    fn refresh_rec_rows(&self) {
        let selected = self.selected_rec_camera();
        let all_mode = self.all_cameras_mode();
        if selected.is_none() && !all_mode {
            return;
        }
        let watched = self.watched_row();
        let session = self.hub.borrow();
        let detected = all_mode || !session.events.is_empty();
        let pictures = self.event_pictures.borrow();
        let batch_sel = self.batch_selected.borrow();
        let rows: Vec<RecRow> = session
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                ListRow::Hour(hour) => RecRow {
                    header: true,
                    time: local_time(*hour).format("%H:00").to_string().into(),
                    ..Default::default()
                },
                ListRow::Entry(start, end, kind, row_cam) => {
                    let cam = row_cam.as_ref().or(selected.as_ref());
                    let picture = cam.and_then(|c| pictures.get(&(c.device_id.clone(), *start)));
                    let saved = cam.is_some_and(|c| is_saved(&clip_path(c, *start)));
                    let is_selected = cam.is_some_and(|c| batch_sel.contains(&(c.device_id.clone(), *start)));
                    RecRow {
                        header: false,
                        time: if detected {
                            local_time(*start).format("%H:%M:%S").to_string().into()
                        } else {
                            format!("{} – {}", local_time(*start).format("%H:%M:%S"), local_time(*end).format("%H:%M:%S"))
                                .into()
                        },
                        camera: row_cam.as_ref().map(|c| c.name.as_str()).unwrap_or("").into(),
                        kind: i18n::detection_label(*kind).into(),
                        icon: kind_icon(*kind),
                        duration: fmt_duration((*end - *start).max(0) as u64).into(),
                        playing: watched == Some(i),
                        saved,
                        selected: is_selected,
                        has_thumb: picture.is_some(),
                        thumb: picture.cloned().map(Image::from_rgba8_premultiplied).unwrap_or_default(),
                    }
                }
            })
            .collect();
        // In place where the rows are the same, so the list keeps its scroll.
        if rows.len() == self.rec_rows.row_count() {
            for (i, row) in rows.into_iter().enumerate() {
                let old = self.rec_rows.row_data(i);
                let same = old.as_ref().is_some_and(|o| {
                    o.time == row.time
                        && o.camera == row.camera
                        && o.kind == row.kind
                        && o.playing == row.playing
                        && o.saved == row.saved
                        && o.selected == row.selected
                        && o.has_thumb == row.has_thumb
                        && o.header == row.header
                });
                if !same {
                    self.rec_rows.set_row_data(i, row);
                }
            }
        } else {
            self.rec_rows.set_vec(rows);
        }
    }

    /// Click on a bar in the 24-hour activity chart: jump to the first event
    /// in that hour.
    fn jump_hour(&self, hour: i32) {
        if !(0..24).contains(&hour) {
            return;
        }
        // `session.rows` is newest-first, so `rposition` finds the earliest
        // event/clip inside that hour.
        let target = {
            let session = self.hub.borrow();
            session
                .rows
                .iter()
                .rposition(|r| r.entry().is_some_and(|(s, _, _, _)| local_time(s).hour() as i32 == hour))
        };
        if let Some(idx) = target {
            self.play_row(idx);
        }
    }

    // --------------------------------------------------------- batch export

    fn batch_toggle_mode(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_batch_busy() {
            return;
        }
        let next = !b.get_batch_mode();
        b.set_batch_mode(next);
        if !next {
            self.batch_selected.borrow_mut().clear();
            b.set_batch_selected_count(0);
        }
        self.refresh_rec_rows();
    }

    fn batch_toggle_row(&self, index: usize) {
        let selected_cam = self.selected_rec_camera();
        let key = {
            let session = self.hub.borrow();
            session.rows.get(index).and_then(|r| row_selection_key(r, selected_cam.as_ref()))
        };
        let Some(key) = key else { return };
        let count = {
            let mut sel = self.batch_selected.borrow_mut();
            if !sel.remove(&key) {
                sel.insert(key);
            }
            sel.len()
        };
        self.ui().global::<Bridge>().set_batch_selected_count(count as i32);
        self.refresh_rec_rows();
    }

    fn batch_select_all(&self) {
        let selected_cam = self.selected_rec_camera();
        let keys: Vec<(String, i64)> = {
            let session = self.hub.borrow();
            session
                .rows
                .iter()
                .filter_map(|r| row_selection_key(r, selected_cam.as_ref()))
                .collect()
        };
        let count = {
            let mut sel = self.batch_selected.borrow_mut();
            sel.extend(keys);
            sel.len()
        };
        self.ui().global::<Bridge>().set_batch_selected_count(count as i32);
        self.refresh_rec_rows();
    }

    fn batch_clear(&self) {
        self.batch_selected.borrow_mut().clear();
        self.ui().global::<Bridge>().set_batch_selected_count(0);
        self.refresh_rec_rows();
    }

    fn batch_cancel(&self) {
        if let Some(flag) = self.batch_cancel.borrow().as_ref() {
            flag.store(true, Ordering::Relaxed);
            self.ui()
                .global::<Bridge>()
                .set_batch_status(i18n::pick("Cancelling…", "İptal ediliyor…").into());
        }
    }

    /// Exports all selected clips sequentially as MP4 files to `Videos\Tapas`.
    /// Runs one clip at a time because the H500 hub allows only a single active
    /// download/playback session.
    fn batch_export(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_batch_busy() {
            return;
        }
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            self.status(i18n::pick(
                "Sign in to your Tapo account first.",
                "Önce Tapo hesabınızla giriş yapın.",
            ));
            return;
        };
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            self.status(i18n::pick(
                "FFmpeg is missing; cannot export MP4.",
                "FFmpeg bulunamadı; MP4 dışa aktarılamıyor.",
            ));
            return;
        };
        let Some(hub) = self.hub.borrow().hub.clone() else { return };
        let selected_cam = self.selected_rec_camera();
        let sel = self.batch_selected.borrow().clone();
        let queue = {
            let session = self.hub.borrow();
            collect_batch_queue(&session.rows, selected_cam.as_ref(), &sel)
        };
        if queue.is_empty() {
            return;
        }
        // Free the H500's single playback/download slot before starting the queue.
        self.player_stop();
        self.stop_event_pictures();

        let total = queue.len();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.batch_cancel.borrow_mut() = Some(cancel.clone());
        b.set_batch_busy(true);
        b.set_batch_status(
            if i18n::is_tr() {
                format!("Dışa aktarılıyor 1/{total}…")
            } else {
                format!("Exporting 1/{total}…")
            }
            .into(),
        );
        let player_id = self.player_id.clone();

        self.rt.spawn(async move {
            let mut exported = 0usize;
            let mut skipped = 0usize;
            let mut failed = 0usize;
            let mut auth_failed = false;

            for (idx, (camera, clip_item)) in queue.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let n = idx + 1;
                let path = clip_path(&camera, clip_item.start);
                let label = format!("{} · {}", camera.name, local_time(clip_item.start).format("%d %b %H:%M:%S"));
                if is_saved(&path) {
                    skipped += 1;
                    let p = path.clone();
                    let l = label.clone();
                    on_ui(move |app| {
                        app.push_download(l, i18n::pick("Already saved", "Zaten kaydedildi").into(), &p, true);
                    });
                    continue;
                }

                let (tx, rx) = tokio::sync::oneshot::channel();
                let p_push = path.clone();
                let l_push = label.clone();
                on_ui(move |app| {
                    let row = app.push_download(
                        l_push,
                        i18n::pick("Asking the hub for the recording…", "Hub'dan kayıt isteniyor…").into(),
                        &p_push,
                        false,
                    );
                    let status = if i18n::is_tr() {
                        format!("Dışa aktarılıyor {n}/{total}…")
                    } else {
                        format!("Exporting {n}/{total}…")
                    };
                    app.ui().global::<Bridge>().set_batch_status(status.into());
                    let _ = tx.send(row);
                });
                let Ok(row) = rx.await else { break };

                let req = ClipRequest {
                    hub: hub.clone(),
                    camera: camera.clone(),
                    clip: clip_item,
                    player_id: player_id.clone(),
                };
                let clip_secs = clip_item.seconds().max(1) as u64;
                let mut last = Instant::now();
                let mut progress = |p: clip::Progress| {
                    if last.elapsed() >= Duration::from_millis(500) {
                        last = Instant::now();
                        let pct = ((p.seconds as u64).min(clip_secs) * 100 / clip_secs).min(99);
                        let row_text = if i18n::is_tr() {
                            format!("İndiriliyor… {} / {}", fmt_duration(p.seconds as u64), fmt_duration(clip_secs))
                        } else {
                            format!("Downloading… {} of {}", fmt_duration(p.seconds as u64), fmt_duration(clip_secs))
                        };
                        let batch_text = if i18n::is_tr() {
                            format!("Dışa aktarılıyor {n}/{total} (%{pct})…")
                        } else {
                            format!("Exporting {n}/{total} ({pct}%)…")
                        };
                        on_ui(move |app| {
                            app.set_download_status(row, row_text, false);
                            app.ui().global::<Bridge>().set_batch_status(batch_text.into());
                        });
                    }
                };
                let partial = clip::partial_path(&path);
                let result = clip::save_clip(&client, &req, &path, &partial, Some(&ffmpeg), &mut progress, move || {
                    let msg = i18n::pick("Converting to MP4…", "MP4'e dönüştürülüyor…").to_string();
                    let batch_msg = if i18n::is_tr() {
                        format!("MP4'e dönüştürülüyor {n}/{total}…")
                    } else {
                        format!("Converting {n}/{total} to MP4…")
                    };
                    on_ui(move |app| {
                        app.set_download_status(row, msg, false);
                        app.ui().global::<Bridge>().set_batch_status(batch_msg.into());
                    });
                })
                .await;

                match result {
                    Ok(()) => {
                        exported += 1;
                        let mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                        let text = format!("{} · {mb:.1} MB", i18n::pick("Saved", "Kaydedildi"));
                        on_ui(move |app| {
                            app.set_download_status(row, text, true);
                            app.refresh_rec_rows();
                        });
                    }
                    Err(e) => {
                        failed += 1;
                        if e.downcast_ref::<cloud::CloudError>().is_some_and(|ce| ce.is_auth()) {
                            auth_failed = true;
                        }
                        let text = format!("{}: {e:#}", i18n::pick("Failed", "Başarısız"));
                        on_ui(move |app| app.set_download_status(row, text, false));
                        if auth_failed {
                            break;
                        }
                    }
                }
            }

            let cancelled = cancel.load(Ordering::Relaxed);
            on_ui(move |app| {
                *app.batch_cancel.borrow_mut() = None;
                let ui = app.ui();
                let b = ui.global::<Bridge>();
                b.set_batch_busy(false);
                b.set_batch_mode(false);
                b.set_batch_status(SharedString::new());
                app.batch_selected.borrow_mut().clear();
                b.set_batch_selected_count(0);
                if auth_failed {
                    app.cloud_auth_failed();
                }
                let summary = if cancelled {
                    if i18n::is_tr() {
                        format!("Toplu dışa aktarma iptal edildi ({exported} kaydedildi).")
                    } else {
                        format!("Batch export cancelled ({exported} saved).")
                    }
                } else if failed > 0 {
                    if i18n::is_tr() {
                        format!("Toplu aktarma bitti: {exported} kaydedildi, {skipped} zaten vardı, {failed} başarısız.")
                    } else {
                        format!("Batch export finished: {exported} saved, {skipped} already saved, {failed} failed.")
                    }
                } else if skipped > 0 {
                    if i18n::is_tr() {
                        format!("{exported} MP4 kaydedildi, {skipped} zaten kayıtlıydı.")
                    } else {
                        format!("Exported {exported} MP4 clip(s); {skipped} already saved.")
                    }
                } else if i18n::is_tr() {
                    format!("{exported} MP4 klip Videolar\\Tapas klasörüne aktarıldı.")
                } else {
                    format!("Exported {exported} MP4 clip(s) to Videos\\Tapas.")
                };
                app.status(summary);
                app.refresh_rec_rows();
            });
        });
    }

    // --------------------------------------------- H500 People / Faces page

    fn update_face_track_date_label(&self) {
        let date = self.face_track_date.get();
        let today = Local::now().date_naive();
        let label = if date == today {
            format!("{}, {:02} {}", i18n::pick("Today", "Bugün"), date.day(), i18n::short_month(date.month()))
        } else {
            format!("{:02} {} {}", date.day(), i18n::short_month(date.month()), date.year())
        };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_face_track_date(label.into());
        b.set_face_track_is_today(date >= today);
    }

    fn load_faces(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            b.set_faces_message(
                i18n::pick("Sign in to your Tapo account first.", "Önce Tapo hesabınızla giriş yapın.").into(),
            );
            return;
        };
        let Some(hub) = self.selected_hub() else {
            b.set_faces_message(
                i18n::pick(
                    "No H500 HomeBase found on your Tapo account.",
                    "Tapo hesabınızda H500 HomeBase bulunamadı.",
                )
                .into(),
            );
            return;
        };
        let req = self.faces_request.get() + 1;
        self.faces_request.set(req);
        self.stop_face_photos();
        let hub_id = hub.device_id.clone();
        let need_cameras = {
            let session = self.hub.borrow();
            session.hub.as_ref().map(|h| &h.device_id) != Some(&hub_id) || session.cameras.is_empty()
        };
        b.set_faces_loading(true);
        b.set_faces_message(
            i18n::pick("Loading recognized faces from the hub…", "Hub'daki tanınan yüzler yükleniyor…").into(),
        );
        self.update_face_track_date_label();
        self.rt.spawn(async move {
            let cams = if need_cameras { client.hub_cameras(&hub).await.ok() } else { None };
            let faces = client.hub_faces(&hub).await;
            on_ui(move |app| app.faces_loaded(req, hub, cams, faces));
        });
    }

    fn stop_face_photos(&self) {
        if let Some(task) = self.face_photo_task.borrow_mut().take() {
            task.abort();
        }
    }

    fn fetch_face_photos(&self) {
        self.stop_face_photos();
        if self.ui().global::<Bridge>().get_page() != Page::People {
            return;
        }
        let (Some(client), Some(hub)) = (self.cloud_client().filter(|c| c.session().is_some()), self.selected_hub()) else {
            return;
        };
        let hub_id = hub.device_id.clone();
        let wanted: Vec<i64> = {
            let photos = self.face_photos.borrow();
            self.faces
                .borrow()
                .iter()
                .map(|f| f.face_id)
                .filter(|id| !photos.contains_key(&(hub_id.clone(), *id)))
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        let player_id = self.player_id.clone();
        let task = self.rt.spawn(async move {
            // 1. Load any face photos already cached on disk first so avatars pop in immediately.
            let mut missing = Vec::new();
            let mut from_disk = Vec::new();
            for face_id in wanted {
                let hid = hub_id.clone();
                let cached = tokio::task::spawn_blocking(move || crate::pictures::load_face(&hid, face_id)).await;
                if let Ok(Some(frame)) = cached {
                    from_disk.push((face_id, frame));
                } else {
                    missing.push(face_id);
                }
            }
            if !from_disk.is_empty() {
                let hid = hub_id.clone();
                on_ui(move |app| {
                    {
                        let mut map = app.face_photos.borrow_mut();
                        for (id, frame) in from_disk {
                            map.insert((hid.clone(), id), frame);
                        }
                    }
                    app.refresh_faces_ui();
                    app.refresh_face_tracks_ui();
                });
            }
            if missing.is_empty() {
                return;
            }
            // 2. Fetch remaining face JPEGs from the H500 over STREAM_TYPE_IMAGE in small batches.
            for chunk in missing.chunks(12) {
                match clip::face_images(&client, &hub, &player_id, chunk).await {
                    Ok(images) if !images.is_empty() => {
                        let mut decoded = Vec::new();
                        for (face_id, bytes) in images {
                            let res = tokio::task::spawn_blocking(move || crate::pictures::decode_still(&bytes, 128)).await;
                            if let Ok(Ok(frame)) = res {
                                let hid = hub_id.clone();
                                let keep = frame.clone();
                                tokio::task::spawn_blocking(move || crate::pictures::save_face(&hid, face_id, &keep));
                                decoded.push((face_id, frame));
                            }
                        }
                        if !decoded.is_empty() {
                            let hid = hub_id.clone();
                            on_ui(move |app| {
                                {
                                    let mut map = app.face_photos.borrow_mut();
                                    for (id, frame) in decoded {
                                        map.insert((hid.clone(), id), frame);
                                    }
                                }
                                app.refresh_faces_ui();
                                app.refresh_face_tracks_ui();
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::info!("face_images batch failed: {e:#}");
                        break;
                    }
                }
            }
        });
        *self.face_photo_task.borrow_mut() = Some(task);
    }

    fn faces_loaded(
        &self,
        req: u64,
        hub: CloudCamera,
        cameras: Option<Vec<HubCamera>>,
        result: cloud::Result<Vec<FaceInfo>>,
    ) {
        if self.faces_request.get() != req {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_faces_loading(false);
        if let Some(cams) = cameras
            && !cams.is_empty()
            && self.hub.borrow().hub.is_none()
        {
            let mut session = self.hub.borrow_mut();
            session.hub = Some(hub.clone());
            session.cameras = cams;
        }
        *self.faces_loaded_hub.borrow_mut() = Some(hub.device_id);
        match result {
            Ok(list) => {
                *self.faces.borrow_mut() = list;
                if let Some(sel_id) = self.selected_face.get()
                    && !self.faces.borrow().iter().any(|f| f.face_id == sel_id)
                {
                    self.selected_face.set(None);
                }
                self.refresh_faces_ui();
                self.load_face_tracks();
                self.fetch_face_photos();
            }
            Err(e) => {
                if e.is_auth() {
                    self.cloud_auth_failed();
                }
                self.faces.borrow_mut().clear();
                self.refresh_faces_ui();
                b.set_faces_message(
                    if i18n::is_tr() {
                        format!(
                            "Yüz tanıma verisi alınamadı ({e}). H500 ayarlarında Yüz Tanıma'nın açık olduğundan emin olun."
                        )
                    } else {
                        format!(
                            "Could not load faces ({e}). Make sure Facial Recognition is enabled on your H500 in the Tapo app."
                        )
                    }
                    .into(),
                );
            }
        }
    }

    fn face_display_name(face: &FaceInfo) -> String {
        if !face.name.trim().is_empty() {
            face.name.trim().to_string()
        } else if face.tag.is_familiar() {
            format!("{} #{}", i18n::pick("Person", "Kişi"), face.face_id)
        } else {
            format!("{} #{}", i18n::pick("Stranger", "Yabancı"), face.face_id)
        }
    }

    fn refresh_faces_ui(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let filter = FaceFilter::from_index(b.get_face_filter());
        let selected_id = self.selected_face.get();
        let hub_id = self.faces_loaded_hub.borrow().clone().unwrap_or_default();
        let photos = self.face_photos.borrow();
        let faces = self.faces.borrow();
        let rows: Vec<FaceRow> = faces
            .iter()
            .filter(|f| filter.matches(f))
            .map(|f| {
                let name = Self::face_display_name(f);
                let initial = name
                    .chars()
                    .find(|c| c.is_alphanumeric())
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".into());
                let ts = f.last_seen();
                let last_seen = if ts > 0 {
                    let dt = local_time(ts);
                    format!("{:02} {} {}", dt.day(), i18n::short_month(dt.month()), dt.format("%H:%M"))
                } else {
                    String::new()
                };
                let photo = photos.get(&(hub_id.clone(), f.face_id)).cloned();
                FaceRow {
                    face_id: f.face_id.to_string().into(),
                    name: name.into(),
                    initial: initial.into(),
                    familiar: f.tag.is_familiar(),
                    last_seen: last_seen.into(),
                    selected: selected_id == Some(f.face_id),
                    has_photo: photo.is_some(),
                    photo: photo.map(Image::from_rgba8_premultiplied).unwrap_or_default(),
                }
            })
            .collect();
        let empty = rows.is_empty();
        b.set_faces(ModelRc::new(VecModel::from(rows)));
        if empty {
            let msg = if faces.is_empty() {
                i18n::pick(
                    "No recognized faces yet. Enable Facial Recognition on your H500 HomeBase in the Tapo app.",
                    "Henüz tanınan yüz yok. Tapo uygulamasından H500 HomeBase için Yüz Tanıma'yı etkinleştirin.",
                )
            } else {
                i18n::pick("No people match this filter.", "Bu filtreye uyan kişi yok.")
            };
            b.set_faces_message(msg.into());
        } else {
            b.set_faces_message(SharedString::new());
        }

        if let Some(id) = selected_id
            && let Some(face) = faces.iter().find(|f| f.face_id == id)
        {
            b.set_selected_face_id(id.to_string().into());
            b.set_selected_face_title(Self::face_display_name(face).into());
        } else {
            b.set_selected_face_id(SharedString::new());
            b.set_selected_face_title(SharedString::new());
        }
    }

    fn face_filter_changed(&self) {
        self.refresh_faces_ui();
        self.load_face_tracks();
    }

    fn face_select(&self, face_id: &str) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let parsed = face_id.trim().parse::<i64>().ok();
        // Clicking the already-selected person clears selection back to "All people".
        let next = if parsed.is_some() && parsed == self.selected_face.get() { None } else { parsed };
        self.selected_face.set(next);
        if let Some(id) = next
            && let Some(face) = self.faces.borrow().iter().find(|f| f.face_id == id)
        {
            b.set_face_edit_name(face.name.as_str().into());
            b.set_face_edit_familiar(face.tag.is_familiar());
        } else {
            b.set_face_edit_name(SharedString::new());
            b.set_face_edit_familiar(false);
        }
        self.refresh_faces_ui();
        self.load_face_tracks();
    }

    fn face_save(&self) {
        let Some(face_id) = self.selected_face.get() else { return };
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else { return };
        let Some(hub) = self.selected_hub() else { return };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let name = b.get_face_edit_name().trim().to_string();
        let familiar = b.get_face_edit_familiar();
        if familiar && name.is_empty() {
            self.status(i18n::pick(
                "Enter a name before marking as Familiar.",
                "Tanıdık olarak işaretlemeden önce bir ad girin.",
            ));
            return;
        }
        b.set_face_saving(true);
        self.rt.spawn(async move {
            let result = client.set_face_info(&hub, face_id, &name, familiar).await;
            on_ui(move |app| {
                app.ui().global::<Bridge>().set_face_saving(false);
                match result {
                    Ok(()) => {
                        if let Some(f) = app.faces.borrow_mut().iter_mut().find(|f| f.face_id == face_id) {
                            f.name = name;
                            f.tag = if familiar { FaceTag::Familiar } else { FaceTag::Stranger };
                            f.user_verified = true;
                        }
                        app.refresh_faces_ui();
                        app.refresh_face_tracks_ui();
                        app.status(i18n::pick("Person updated on H500.", "Kişi bilgisi H500 üzerinde güncellendi."));
                    }
                    Err(e) => {
                        if e.is_auth() {
                            app.cloud_auth_failed();
                        }
                        app.status(if i18n::is_tr() {
                            format!("Kişi güncellenemedi: {e}")
                        } else {
                            format!("Could not update person: {e}")
                        });
                    }
                }
            });
        });
    }

    fn face_track_day(&self, delta: i32) {
        let today = Local::now().date_naive();
        let next = if delta == 0 {
            today
        } else {
            (self.face_track_date.get() + chrono::Duration::days(delta as i64)).min(today)
        };
        if next != self.face_track_date.get() {
            self.face_track_date.set(next);
            self.update_face_track_date_label();
            self.load_face_tracks();
        }
    }

    fn load_face_tracks(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let (Some(client), Some(hub)) = (self.cloud_client().filter(|c| c.session().is_some()), self.selected_hub()) else {
            return;
        };
        let req = self.face_track_request.get() + 1;
        self.face_track_request.set(req);
        let (start, end) = local_day(self.face_track_date.get());
        let face_id = self.selected_face.get();
        let filter = FaceFilter::from_index(b.get_face_filter());
        let tag = if face_id.is_some() { None } else { filter.tag_param() };

        // Candidate face IDs and hub camera MACs for the `doSearchFaceDetectionVideo`
        // fallback (used when multi-camera Face Tracking returns empty or is disabled).
        let fallback_face_ids: Vec<i64> = if let Some(id) = face_id {
            vec![id]
        } else {
            let mut list: Vec<FaceInfo> = self
                .faces
                .borrow()
                .iter()
                .filter(|f| filter.matches(f))
                .cloned()
                .collect();
            list.sort_by(|a, b| b.last_seen().cmp(&a.last_seen()));
            list.into_iter().take(24).map(|f| f.face_id).collect()
        };
        let mut camera_macs: Vec<String> = self
            .hub
            .borrow()
            .cameras
            .iter()
            .filter(|c| !c.mac.is_empty())
            .map(|c| c.mac.clone())
            .collect();
        if camera_macs.is_empty() {
            camera_macs = self
                .cfg
                .borrow()
                .cameras
                .iter()
                .filter_map(|c| c.cloud.as_ref().map(|l| l.mac.clone()))
                .filter(|m| !m.is_empty())
                .collect();
        }

        b.set_face_track_loading(true);
        b.set_face_track_message(
            i18n::pick("Loading face appearances…", "Yüz kayıtları yükleniyor…").into(),
        );
        self.rt.spawn(async move {
            let tracking = client.hub_face_tracking(&hub, start, end, face_id, tag).await;
            let result = match tracking {
                Ok(entries) if !entries.is_empty() => Ok(entries),
                Err(e) if e.is_auth() => {
                    on_ui(move |app| app.face_tracks_loaded(req, Err(e)));
                    return;
                }
                other => {
                    if let Err(ref e) = other {
                        log::info!("doSearchFaceTrackingList unavailable ({e}); trying doSearchFaceDetectionVideo");
                    }
                    match client.hub_face_videos(&hub, start, end, &fallback_face_ids, &camera_macs).await {
                        Ok(vids) if !vids.is_empty() => Ok(vids),
                        Ok(_) => other.or(Ok(Vec::new())),
                        Err(ve) => {
                            log::info!("doSearchFaceDetectionVideo failed: {ve}");
                            other.or(Err(ve))
                        }
                    }
                }
            };
            on_ui(move |app| app.face_tracks_loaded(req, result));
        });
    }

    fn face_tracks_loaded(&self, req: u64, result: cloud::Result<Vec<FaceTrackingEntry>>) {
        if self.face_track_request.get() != req {
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_face_track_loading(false);
        match result {
            Ok(entries) => {
                *self.face_tracks.borrow_mut() = entries;
                self.refresh_face_tracks_ui();
            }
            Err(e) => {
                if e.is_auth() {
                    self.cloud_auth_failed();
                }
                self.face_tracks.borrow_mut().clear();
                self.refresh_face_tracks_ui();
                b.set_face_track_message(
                    if i18n::is_tr() {
                        format!("Yüz takip geçmişi alınamadı: {e}")
                    } else {
                        format!("Could not load face timeline: {e}")
                    }
                    .into(),
                );
            }
        }
    }

    fn resolve_camera_for_track(&self, entry: &FaceTrackingEntry) -> Option<(String, String)> {
        if !entry.dev_id.is_empty() {
            if let Some(cam) = self.hub.borrow().cameras.iter().find(|c| c.device_id == entry.dev_id) {
                return Some((cam.device_id.clone(), cam.name.clone()));
            }
            if let Some(cam) = self
                .cfg
                .borrow()
                .cameras
                .iter()
                .find(|c| c.cloud.as_ref().is_some_and(|l| l.device_id == entry.dev_id))
            {
                return Some((entry.dev_id.clone(), cam.name.clone()));
            }
        }
        if entry.snapshot_mac.is_empty() {
            return None;
        }
        if let Some(cam) = self
            .hub
            .borrow()
            .cameras
            .iter()
            .find(|c| cloud::normalize_mac(&c.mac) == entry.snapshot_mac)
        {
            return Some((cam.device_id.clone(), cam.name.clone()));
        }
        if let Some(cam) = self.cfg.borrow().cameras.iter().find(|c| {
            c.cloud
                .as_ref()
                .is_some_and(|l| cloud::normalize_mac(&l.mac) == entry.snapshot_mac)
        }) {
            let id = cam.cloud.as_ref().map(|l| l.device_id.clone()).unwrap_or_default();
            return Some((id, cam.name.clone()));
        }
        None
    }

    fn refresh_face_tracks_ui(&self) {
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let tracks = self.face_tracks.borrow();
        let faces = self.faces.borrow();
        let hub_id = self.faces_loaded_hub.borrow().clone().unwrap_or_default();
        let photos = self.face_photos.borrow();
        let show_face_name = self.selected_face.get().is_none();
        let rows: Vec<FaceTrackRow> = tracks
            .iter()
            .map(|t| {
                let face = faces.iter().find(|f| f.face_id == t.face_id);
                let familiar = face.is_some_and(|f| f.tag.is_familiar());
                let face_name = if show_face_name {
                    face.map(Self::face_display_name)
                        .unwrap_or_else(|| format!("{} #{}", i18n::pick("Face", "Yüz"), t.face_id))
                } else {
                    String::new()
                };
                let resolved = self.resolve_camera_for_track(t);
                let camera = resolved
                    .as_ref()
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| {
                        if t.snapshot_mac.is_empty() {
                            i18n::pick("Camera", "Kamera").to_string()
                        } else {
                            t.snapshot_mac.clone()
                        }
                    });
                let time = if t.end_time > t.start_time {
                    format!(
                        "{} – {}",
                        local_time(t.start_time).format("%H:%M:%S"),
                        local_time(t.end_time).format("%H:%M:%S")
                    )
                } else {
                    local_time(t.start_time).format("%H:%M:%S").to_string()
                };
                let clips = if t.device_count > 1 {
                    if i18n::is_tr() {
                        format!("{} kamera · {} klip", t.device_count, t.clip_count)
                    } else {
                        format!("{} cameras · {} clips", t.device_count, t.clip_count)
                    }
                } else if t.clip_count > 1 {
                    if i18n::is_tr() {
                        format!("{} klip", t.clip_count)
                    } else {
                        format!("{} clips", t.clip_count)
                    }
                } else {
                    String::new()
                };
                let photo = photos.get(&(hub_id.clone(), t.face_id)).cloned();
                FaceTrackRow {
                    time: time.into(),
                    camera: camera.into(),
                    clips: clips.into(),
                    face_name: face_name.into(),
                    familiar,
                    playable: resolved.is_some(),
                    has_photo: photo.is_some(),
                    photo: photo.map(Image::from_rgba8_premultiplied).unwrap_or_default(),
                }
            })
            .collect();
        let count = rows.len();
        b.set_face_tracks(ModelRc::new(VecModel::from(rows)));
        b.set_face_track_message(
            if count == 0 {
                i18n::pick(
                    "No face appearances on this day.",
                    "Bu gün için yüz kaydı yok.",
                )
            } else {
                i18n::pick(
                    "Click any appearance to jump directly to Playback at that moment.",
                    "O anın kaydını izlemek için herhangi bir satıra tıklayın.",
                )
            }
            .into(),
        );
    }

    fn face_track_play(&self, index: usize) {
        let entry = self.face_tracks.borrow().get(index).cloned();
        let Some(entry) = entry else { return };
        let Some((device_id, name)) = self.resolve_camera_for_track(&entry) else {
            self.status(i18n::pick(
                "Could not match this appearance to a camera on the hub.",
                "Bu kayıt hub üzerindeki bir kamerayla eşleştirilemedi.",
            ));
            return;
        };
        let target_date = local_time(entry.start_time).date_naive();
        *self.rec_date.borrow_mut() = target_date;
        self.rec_month.set(first_of_month(target_date));
        self.update_rec_date_label();
        *self.pending_rec_camera.borrow_mut() = Some((device_id, name, Some(entry.start_time * 1000)));
        self.navigate(Page::Recordings);

        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if b.get_hub_connected() {
            self.select_pending_rec_camera();
        } else if !b.get_hub_busy() {
            self.connect_hub();
        }
    }

    /// A row of the list: watch from its start.
    fn play_row(&self, index: usize) {
        let entry = {
            let session = self.hub.borrow();
            session.rows.get(index).and_then(ListRow::entry).map(|(s, _, _, c)| (s, c.cloned()))
        };
        let Some((start, row_cam)) = entry else { return };
        if let Some(cam) = row_cam {
            // Clicked an event in the "All cameras" feed: switch to that
            // camera and jump directly to the event's timestamp once loaded.
            if let Some(idx) = self.rec_camera_combo_index(&cam.device_id, &cam.name) {
                self.player_stop();
                self.ui().global::<Bridge>().set_rec_camera(idx);
                {
                    let mut session = self.hub.borrow_mut();
                    session.days.clear();
                    session.months.clear();
                    session.pending_play_at = Some(start * 1000);
                }
                self.update_calendar();
                self.load_days(self.rec_month.get());
                self.load_recordings();
            }
            return;
        }
        self.watch_from(start * 1000);
    }

    // ----------------------------------------------------------- the player

    /// Watches the day's recordings from `at_ms`.
    fn watch_from(&self, at_ms: i64) {
        let Some(camera) = self.selected_rec_camera() else { return };
        // Past the day's last recording: its last moments, as the Tapo app
        // goes to the nearest recording. (Before or between recordings,
        // playback goes on to the next one by itself.)
        let at_ms = match self.hub.borrow().clips.iter().map(|c| (c.start, c.end)).max_by_key(|c| c.1) {
            Some((start, end)) if at_ms >= end * 1000 => (start * 1000).max(end * 1000 - 10_000),
            _ => at_ms,
        };
        let date = *self.rec_date.borrow();
        if let Some(w) = self.watching.borrow().as_ref()
            && w.camera_id == camera.device_id
            && w.date == date
        {
            self.pictures_after_video();
            w.playback.seek(at_ms);
            w.playback.set_paused(false);
            self.sync_timer();
            self.sync_audio();
            return;
        }
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            self.status("ffmpeg not found: playing recordings needs it (see Settings › Video & display).");
            return;
        };
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            self.status("Sign in to your Tapo account first.");
            return;
        };
        let (Some(hub), clips) = ({
            let session = self.hub.borrow();
            (session.hub.clone(), session.clips.clone())
        }) else {
            return;
        };
        if clips.is_empty() {
            self.status("There are no recordings on this day.");
            return;
        }
        let source = Source {
            client,
            hub,
            camera: camera.clone(),
            player_id: self.player_id.clone(),
            segments: clips,
            ffmpeg,
        };
        // The hub serves one session at a time: the picture first.
        self.pictures_after_video();
        let playback = Playback::start(&self.rt, source, at_ms, self.speed.get());
        *self.watching.borrow_mut() = Some(Watching {
            playback,
            camera_id: camera.device_id.clone(),
            date,
            shown_version: 0,
            shown: Default::default(),
        });
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_player_open(true);
        b.set_player_has_frame(false);
        b.set_player_frame(Image::default());
        b.set_player_title(format!("{} · {}", camera.name, date.format("%a %d %b %Y")).into());
        b.set_player_paused(false);
        self.timeline.borrow_mut().touched = None;
        self.sync_timer();
        self.sync_audio();
    }

    /// Runs every ~30 ms on the Playback page.
    fn pump_player(&self, b: &Bridge) {
        let (position, row_changed) = {
            let mut watching = self.watching.borrow_mut();
            let Some(w) = watching.as_mut() else { return };
            {
                let slot = w.playback.slot.lock().unwrap();
                if slot.version != w.shown_version {
                    if let Some(frame) = slot.frame.clone() {
                        b.set_player_frame(Image::from_rgba8_premultiplied(frame));
                        b.set_player_has_frame(true);
                    }
                    w.shown_version = slot.version;
                }
            }
            let st = w.playback.state.lock().unwrap().clone();
            let time = local_time(st.position_ms / 1000).format("%H:%M:%S").to_string();
            let status = st.message.clone();
            let busy = st.buffering && st.message.is_empty();
            if (&time, &status, busy, st.paused, st.ended) != (&w.shown.0, &w.shown.1, w.shown.2, w.shown.3, w.shown.4) {
                b.set_player_time(time.as_str().into());
                b.set_player_status(status.as_str().into());
                b.set_player_busy(busy);
                b.set_player_paused(st.paused);
                b.set_player_ended(st.ended);
                (w.shown.0, w.shown.1, w.shown.2, w.shown.3, w.shown.4) = (time, status, busy, st.paused, st.ended);
            }
            let paused = st.paused || st.ended;
            (Some((st.position_ms, paused)), true)
        };
        let Some((position, paused)) = position else { return };
        if let Some(from) = self.recording_from.get() {
            let secs = ((position - from).max(0) / 1000) as u64;
            let text = format!("{}:{:02}", secs / 60, secs % 60);
            if b.get_player_rec_time() != text.as_str() {
                b.set_player_rec_time(text.into());
            }
        }
        self.follow_playhead(position, paused);
        if row_changed {
            let row = self.watched_row();
            let changed = {
                let mut watching = self.watching.borrow_mut();
                let Some(w) = watching.as_mut() else { return };
                let changed = w.shown.5 != row;
                w.shown.5 = row;
                changed
            };
            if changed {
                self.refresh_rec_rows();
            }
        }
    }

    fn player_stop(&self) {
        let was = self.watching.borrow_mut().take();
        if was.is_none() {
            return;
        }
        drop(was);
        crate::playback::clear_cache();
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_player_open(false);
        b.set_player_frame(Image::default());
        b.set_player_has_frame(false);
        b.set_tl_playhead(-1.0);
        if self.recording_from.take().is_some() {
            b.set_player_recording(false);
            self.status("Recording stopped without saving: the player was closed.");
        }
        if b.get_fullscreen() {
            self.set_fullscreen(false);
        }
        self.reset_zoom();
        self.refresh_rec_rows();
        if b.get_page() == Page::Recordings {
            self.fetch_event_pictures();
        }
        self.sync_timer();
        self.sync_audio();
    }

    /// Play/pause; after the day's last recording, play from its first.
    fn player_toggle_pause(&self) {
        let first = self.hub.borrow().clips.iter().map(|c| c.start).min();
        if let Some(w) = self.watching.borrow().as_ref() {
            let (paused, ended) = {
                let s = w.playback.state.lock().unwrap();
                (s.paused, s.ended)
            };
            match (ended, first) {
                (true, Some(start)) => {
                    w.playback.seek(start * 1000);
                    w.playback.set_paused(false);
                }
                _ => w.playback.set_paused(!paused),
            }
        }
        self.pump_player(&self.ui().global::<Bridge>());
        self.sync_timer();
    }

    fn player_speed(&self, index: i32) {
        let speed = SPEEDS.get(index.max(0) as usize).copied().unwrap_or(1);
        self.speed.set(speed);
        self.ui().global::<Bridge>().set_player_speed_index(index);
        if let Some(w) = self.watching.borrow().as_ref() {
            w.playback.set_speed(speed);
        }
    }

    /// Steps playback speed down (`-1`) or up (`+1`) through `SPEEDS`.
    fn player_step_speed(&self, delta: i32) {
        let current = self.ui().global::<Bridge>().get_player_speed_index();
        let next = step_speed_index(current, delta, SPEEDS.len());
        if next != current {
            self.player_speed(next);
        }
    }

    /// Skips playback forward or backward by `delta_secs` within the current day.
    fn player_skip(&self, delta_secs: i32) {
        let pos_ms = {
            let watching = self.watching.borrow();
            let Some(w) = watching.as_ref() else { return };
            w.playback.state.lock().unwrap().position_ms
        };
        let (day_start, day_end) = local_day(*self.rec_date.borrow());
        let target = skip_target_ms(pos_ms, delta_secs, day_start * 1000, day_end * 1000);
        self.watch_from(target);
    }

    /// Take Photo in the player: the picture shown, as a PNG.
    fn player_snapshot(&self) {
        let (frame, at) = {
            let watching = self.watching.borrow();
            let Some(w) = watching.as_ref() else { return };
            (w.playback.latest_frame(), w.playback.state.lock().unwrap().position_ms)
        };
        let Some(frame) = frame else {
            self.status("No picture yet.");
            return;
        };
        let name = self.selected_rec_camera().map(|c| c.name).unwrap_or_default();
        self.save_snapshot(frame, &name, local_time(at / 1000));
    }

    // ------------------------------------------------------- event pictures

    fn insert_event_picture(&self, key: (String, i64), frame: Frame) {
        let mut pics = self.event_pictures.borrow_mut();
        if pics.len() >= MAX_EVENT_PICTURES && !pics.contains_key(&key) {
            // Drop the oldest timestamped entry to keep RAM bounded.
            if let Some(oldest) = pics.keys().min_by_key(|(_, ts)| *ts).cloned() {
                pics.remove(&oldest);
            }
        }
        pics.insert(key, frame);
    }

    /// Fetches the day's event pictures, newest first. They take the hub
    /// only while playback doesn't need it.
    fn fetch_event_pictures(&self) {
        self.stop_event_pictures();
        if self.ui().global::<Bridge>().get_page() != Page::Recordings {
            return;
        }
        let (Some(client), Some(ffmpeg), Some(hub), Some(camera)) = (
            self.cloud_client().filter(|c| c.session().is_some()),
            self.ffmpeg.borrow().clone(),
            self.hub.borrow().hub.clone(),
            self.selected_rec_camera(),
        ) else {
            return;
        };
        let wanted: Vec<(i64, i64)> = {
            let session = self.hub.borrow();
            let pictures = self.event_pictures.borrow();
            let mut events: Vec<(i64, i64)> = session.events.iter().map(|e| (e.start, e.kind)).collect();
            events.reverse(); // newest first
            events
                .into_iter()
                .filter(|(start, _)| !pictures.contains_key(&(camera.device_id.clone(), *start)))
                .take(MAX_EVENT_PICTURES)
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        let player_id = self.player_id.clone();
        let task = self.rt.spawn(async move {
            let mut misses = 0;
            for (start, kind) in wanted {
                // Pictures already fetched once come from disk.
                let device_id = camera.device_id.clone();
                let saved = tokio::task::spawn_blocking(move || crate::pictures::load_event(&device_id, start)).await;
                if let Ok(Some(frame)) = saved {
                    let key = (camera.device_id.clone(), start);
                    on_ui(move |app| {
                        app.insert_event_picture(key, frame);
                        app.refresh_rec_rows();
                    });
                    continue;
                }
                let request = ClipRequest {
                    hub: hub.clone(),
                    camera: camera.clone(),
                    clip: Clip { start, end: start, kind },
                    player_id: player_id.clone(),
                };
                let picture = match clip::event_picture(&client, &request).await {
                    // An H500 sends JPEGs; video keyframes go through ffmpeg.
                    Ok((data, kind)) if kind.starts_with("image/") && !kind.contains("avc") && !kind.contains("hevc") => {
                        tokio::task::spawn_blocking(move || crate::pictures::decode_still(&data, 256))
                            .await
                            .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")))
                    }
                    Ok((data, kind)) => crate::ffmpeg::decode_picture(&ffmpeg, &data, &kind, 256, 144).await,
                    Err(e) => Err(e),
                };
                match picture {
                    Ok(frame) => {
                        misses = 0;
                        let (device_id, keep) = (camera.device_id.clone(), frame.clone());
                        tokio::task::spawn_blocking(move || crate::pictures::save_event(&device_id, start, &keep));
                        let key = (camera.device_id.clone(), start);
                        on_ui(move |app| {
                            app.insert_event_picture(key, frame);
                            app.refresh_rec_rows();
                        });
                    }
                    Err(e) => {
                        log::info!("no picture for the event at {start}: {e:#}");
                        misses += 1;
                        if misses >= 3 {
                            log::info!("event pictures: giving up after three misses in a row");
                            return;
                        }
                    }
                }
            }
        });
        *self.picture_task.borrow_mut() = Some(task);
    }

    /// Video is wanted now: an event picture on its way stops, so the hub is
    /// free at once, and the rest come once playback has the hub.
    fn pictures_after_video(&self) {
        self.stop_event_pictures();
        self.rt.spawn(async {
            tokio::time::sleep(Duration::from_secs(4)).await;
            on_ui(|app| app.fetch_event_pictures());
        });
    }

    fn stop_event_pictures(&self) {
        if let Some(task) = self.picture_task.borrow_mut().take() {
            task.abort(); // its session guard ends the hub session
        }
    }

    // -------------------------------------------------------------- record

    /// Record: the first press marks where the picture is; the second saves
    /// the recording from there to where the picture is then (watching on,
    /// or jumping there), from memory where it can, else from the hub.
    fn player_record(&self) {
        let position = {
            let watching = self.watching.borrow();
            let Some(w) = watching.as_ref() else { return };
            w.playback.state.lock().unwrap().position_ms
        };
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        let Some(from) = self.recording_from.take() else {
            self.recording_from.set(Some(position));
            b.set_player_recording(true);
            b.set_player_rec_time("0:00".into());
            return;
        };
        b.set_player_recording(false);
        let (start, end) = (from.min(position), from.max(position));
        if end - start < 1000 {
            self.status("Nothing recorded: play on, or jump ahead, before pressing Record again.");
            return;
        }
        self.save_range(start, end);
    }

    /// Saves `start_ms..end_ms` of the day being watched to one file.
    fn save_range(&self, start_ms: i64, end_ms: i64) {
        let (Some(client), Some(hub), Some(camera)) = (
            self.cloud_client().filter(|c| c.session().is_some()),
            self.hub.borrow().hub.clone(),
            self.selected_rec_camera(),
        ) else {
            return;
        };
        let clips: Vec<Clip> = {
            let session = self.hub.borrow();
            let mut c: Vec<Clip> =
                session.clips.iter().filter(|c| c.end * 1000 > start_ms && c.start * 1000 < end_ms).copied().collect();
            c.sort_by_key(|c| c.start);
            c
        };
        if clips.is_empty() {
            self.status("There's no recording in that stretch.");
            return;
        }
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            self.status("FFmpeg is missing; cannot export MP4.");
            return;
        };
        let first = local_time(start_ms / 1000);
        let path = config::recordings_dir().join(format!(
            "{}_{}_to_{}.mp4",
            file_safe(&camera.name),
            first.format("%Y-%m-%d_%H-%M-%S"),
            local_time(end_ms / 1000).format("%H-%M-%S")
        ));
        let label = format!("{} · {} – {}", camera.name, first.format("%d %b %H:%M:%S"), local_time(end_ms / 1000).format("%H:%M:%S"));
        let row = self.push_download(label, i18n::pick("Saving…", "Kaydediliyor…").into(), &path, false);
        let player_id = self.player_id.clone();
        self.rt.spawn(async move {
            let result = save_stretch(&client, &hub, &camera, &player_id, &clips, start_ms, end_ms, &path, &ffmpeg, move || {
                let msg = i18n::pick("Converting to MP4…", "MP4'e dönüştürülüyor…").to_string();
                on_ui(move |app| app.set_download_status(row, msg, false));
            })
            .await;
            let (text, done) = match &result {
                Ok(from_memory) => {
                    let mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                    let how = if *from_memory {
                        i18n::pick(" (from what was watched)", " (izlenen bölümden)")
                    } else {
                        ""
                    };
                    (format!("{} · {mb:.1} MB{how}", i18n::pick("Saved", "Kaydedildi")), true)
                }
                Err(e) => (format!("{}: {e:#}", i18n::pick("Failed", "Başarısız")), false),
            };
            on_ui(move |app| app.set_download_status(row, text, done));
        });
    }

    // --------------------------------------------------------- the timeline

    /// The whole day.
    fn reset_timeline(&self) {
        let (start, end) = local_day(*self.rec_date.borrow());
        {
            let mut view = self.timeline.borrow_mut();
            view.start = start * 1000;
            view.span = (end + 1 - start) * 1000;
            view.drag_from = None;
        }
        self.refresh_timeline();
    }

    /// Redraws the recordings, events and hour marks in view.
    fn refresh_timeline(&self) {
        let (start, span) = {
            let view = self.timeline.borrow();
            (view.start, view.span.max(1))
        };
        let end = start + span;
        let fraction = |t: i64| (t - start) as f32 / span as f32;
        let session = self.hub.borrow();
        let mut segments = Vec::new();
        let in_view = |a: i64, b: i64| b > start && a < end;
        for c in session.clips.iter().filter(|c| in_view(c.start * 1000, c.end * 1000)) {
            let (a, b) = ((c.start * 1000).max(start), (c.end * 1000).min(end));
            segments.push(TlSeg { x: fraction(a), w: fraction(b) - fraction(a), event: c.kind != 1 });
        }
        for e in session.events.iter().filter(|e| in_view(e.start * 1000, e.end * 1000 + 1000)) {
            let (a, b) = ((e.start * 1000).max(start), (e.end * 1000).min(end));
            segments.push(TlSeg { x: fraction(a), w: (fraction(b) - fraction(a)).max(0.0), event: true });
        }
        let step = tick_step(span);
        let (day_start, _) = local_day(*self.rec_date.borrow());
        let first = day_start * 1000 + ((start - day_start * 1000) / step) * step;
        let ticks: Vec<TlTick> = (0..)
            .map(|k| first + k * step)
            .take_while(|&t| t <= end)
            // Not right at the ends, where the label wouldn't fit.
            .filter(|&t| (0.04..=0.96).contains(&fraction(t)))
            .map(|t| TlTick { x: fraction(t), label: local_time(t / 1000).format("%H:%M").to_string().into() })
            .collect();
        drop(session);
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        b.set_tl_segments(ModelRc::new(VecModel::from(segments)));
        b.set_tl_ticks(ModelRc::new(VecModel::from(ticks)));
        b.set_tl_zoomed(span < 86_000_000);
        let position = self.watching.borrow().as_ref().map(|w| w.playback.state.lock().unwrap().position_ms);
        b.set_tl_playhead(position.map_or(-1.0, fraction));
    }

    /// Keeps the playhead in view while playing, unless the user has just
    /// moved the timeline.
    fn follow_playhead(&self, position: i64, paused: bool) {
        let (start, span, recently_moved) = {
            let view = self.timeline.borrow();
            let recent = view.drag_from.is_some() || view.touched.is_some_and(|t| t.elapsed() < Duration::from_secs(5));
            (view.start, view.span.max(1), recent)
        };
        let fraction = (position - start) as f32 / span as f32;
        if !paused && !recently_moved && !(0.0..=0.95).contains(&fraction) {
            self.timeline.borrow_mut().start = position - span / 4;
            self.clamp_timeline();
            self.refresh_timeline();
            return;
        }
        let ui = self.ui();
        let b = ui.global::<Bridge>();
        if (b.get_tl_playhead() - fraction).abs() > 0.0005 {
            b.set_tl_playhead(fraction);
        }
    }

    fn clamp_timeline(&self) {
        let (day_start, day_end) = local_day(*self.rec_date.borrow());
        let mut view = self.timeline.borrow_mut();
        let latest = (day_end + 1) * 1000 - view.span;
        view.start = view.start.clamp(day_start * 1000, latest.max(day_start * 1000));
    }

    /// The time at `at` (a part of the timeline's width), for the playhead's
    /// label.
    fn tl_time_at(&self, at: f32) -> SharedString {
        let ms = {
            let view = self.timeline.borrow();
            view.start + (at.clamp(0.0, 1.0) as f64 * view.span as f64) as i64
        };
        local_time(ms / 1000).format("%H:%M:%S").to_string().into()
    }

    /// A click on the timeline: watch from there.
    fn tl_click(&self, at: f32) {
        let ms = {
            let view = self.timeline.borrow();
            view.start + (at.clamp(0.0, 1.0) as f64 * view.span as f64) as i64
        };
        self.watch_from(ms);
    }

    /// Dragging the timeline: `delta` is how far, as a part of its width.
    fn tl_drag(&self, delta: f32) {
        {
            let mut view = self.timeline.borrow_mut();
            let start = view.start;
            let from = *view.drag_from.get_or_insert(start);
            view.start = from - (delta as f64 * view.span as f64) as i64;
            view.touched = Some(Instant::now());
        }
        self.clamp_timeline();
        self.refresh_timeline();
    }

    /// Zooms by `factor` (below 1 zooms in), keeping the time at `at` (a
    /// part of the width) where it is.
    fn tl_zoom(&self, factor: f32, at: f32) {
        const MIN_SPAN: i64 = 5 * 60 * 1000;
        const MAX_SPAN: i64 = 24 * 3600 * 1000;
        {
            let mut view = self.timeline.borrow_mut();
            let at = at.clamp(0.0, 1.0) as f64;
            let pinned = view.start + (at * view.span as f64) as i64;
            let span = ((view.span as f64 * factor as f64) as i64).clamp(MIN_SPAN, MAX_SPAN);
            view.span = span;
            view.start = pinned - (at * span as f64) as i64;
            view.touched = Some(Instant::now());
        }
        self.clamp_timeline();
        self.refresh_timeline();
    }

    // ---------------------------------------------------------------- saving

    /// Saves a row's recording to `Videos\Tapas` without playing it.
    fn download(&self, index: usize) {
        let entry = {
            let session = self.hub.borrow();
            session.rows.get(index).and_then(ListRow::entry).map(|(s, e, k, c)| (s, e, k, c.cloned()))
        };
        let Some((start, end, kind, row_cam)) = entry else { return };
        let (Some(hub), Some(camera)) = (self.hub.borrow().hub.clone(), row_cam.or_else(|| self.selected_rec_camera())) else { return };
        let request = ClipRequest { hub, camera, clip: Clip { start, end, kind }, player_id: self.player_id.clone() };
        let path = clip_path(&request.camera, start);
        let label = format!("{} · {}", request.camera.name, local_time(start).format("%d %b %H:%M:%S"));
        if is_saved(&path) {
            self.push_download(label, i18n::pick("Already saved", "Zaten kaydedildi").into(), &path, true);
            return;
        }
        let Some(client) = self.cloud_client().filter(|c| c.session().is_some()) else {
            self.status(i18n::pick(
                "Sign in to your Tapo account first.",
                "Önce Tapo hesabınızla giriş yapın.",
            ));
            return;
        };
        let Some(ffmpeg) = self.ffmpeg.borrow().clone() else {
            self.status(i18n::pick(
                "FFmpeg is missing; cannot export MP4.",
                "FFmpeg bulunamadı; MP4 dışa aktarılamıyor.",
            ));
            return;
        };
        let row = self.push_download(
            label,
            i18n::pick("Asking the hub for the recording…", "Hub'dan kayıt isteniyor…").into(),
            &path,
            false,
        );
        let total = request.clip.seconds() as u64;
        self.rt.spawn(async move {
            let mut last = Instant::now();
            let mut progress = |p: clip::Progress| {
                if last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    let text = if i18n::is_tr() {
                        format!("İndiriliyor… {} / {}", fmt_duration(p.seconds as u64), fmt_duration(total))
                    } else {
                        format!("Downloading… {} of {}", fmt_duration(p.seconds as u64), fmt_duration(total))
                    };
                    on_ui(move |app| app.set_download_status(row, text, false));
                }
            };
            let partial = clip::partial_path(&path);
            let result = clip::save_clip(&client, &request, &path, &partial, Some(&ffmpeg), &mut progress, move || {
                let msg = i18n::pick("Converting to MP4…", "MP4'e dönüştürülüyor…").to_string();
                on_ui(move |app| app.set_download_status(row, msg, false));
            })
            .await;
            let (text, done) = match &result {
                Ok(()) => {
                    let mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
                    (format!("{} · {mb:.1} MB", i18n::pick("Saved", "Kaydedildi")), true)
                }
                Err(e) => (format!("{}: {e:#}", i18n::pick("Failed", "Başarısız")), false),
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

/// The app's own blue, not the Windows accent colour: Slint's Fluent style
/// tints its blues towards the system accent when it has one (set when the
/// window opens and again whenever the Windows theme changes), and uses its
/// standard blues without. This takes the system accent away; it's checked on
/// every UI tick, as a theme change brings it back. Slint has no public way to
/// do this, hence its unstable internal API (pinned with the slint version).
fn keep_app_accent(ui: &MainWindow) {
    use slint::private_unstable_api::re_exports::WindowInner;
    let context = WindowInner::from_pub(ui.window()).context();
    if context.accent_color() != slint::Color::default() {
        context.set_accent_color(slint::Color::default());
    }
}

/// Rows newest first, with a heading at each new hour.
fn with_hours(entries: &[(i64, i64, i64, Option<HubCamera>)]) -> Vec<ListRow> {
    let mut rows = Vec::new();
    let mut hour = None;
    for (start, end, kind, cam) in entries {
        let t = local_time(*start);
        let this = t.timestamp() - (t.minute() as i64 * 60 + t.second() as i64);
        if hour != Some(this) {
            rows.push(ListRow::Hour(this));
            hour = Some(this);
        }
        rows.push(ListRow::Entry(*start, *end, *kind, cam.clone()));
    }
    rows
}

/// The list's icon for an event's kind: 1 person, 2 vehicle, 3 animal,
/// 4 motion, 0 none (continuous recording).
fn kind_icon(kind: i64) -> i32 {
    match kind {
        6 | 20..=22 | 27 => 1,
        8 => 2,
        9 | 11 | 12 | 33 => 3,
        1 => 0,
        _ => 4,
    }
}

/// Writes `start_ms..end_ms` across `clips` to `path` as an `.mp4`: from
/// memory where all of a recording's part is there, else fetched from the hub.
/// Returns whether it all came from memory.
#[allow(clippy::too_many_arguments)]
async fn save_stretch(
    client: &CloudClient,
    hub: &CloudCamera,
    camera: &HubCamera,
    player_id: &str,
    clips: &[Clip],
    start_ms: i64,
    end_ms: i64,
    path: &Path,
    ffmpeg: &Path,
    on_finalize: impl FnOnce() + Send,
) -> anyhow::Result<bool> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = clip::partial_path(path);
    let mut file = std::io::BufWriter::new(std::fs::File::create(&partial)?);
    let mut all_from_memory = true;
    let result = async {
        for c in clips {
            let (a, b) = (start_ms.max(c.start * 1000), end_ms.min(c.end * 1000));
            if let Some(pieces) = crate::playback::cached_video(&camera.device_id, c.start, a, b) {
                for piece in pieces {
                    file.write_all(&piece)?;
                }
                continue;
            }
            all_from_memory = false;
            let request = ClipRequest {
                hub: hub.clone(),
                camera: camera.clone(),
                clip: Clip { start: a / 1000, end: (b + 999) / 1000, kind: c.kind },
                player_id: player_id.to_string(),
            };
            let mut media = |m: clip::Media<'_>| -> anyhow::Result<()> {
                file.write_all(m.data)?;
                Ok(())
            };
            clip::download(client, &request, &mut media, &mut |_| {}).await?;
        }
        file.flush()?;
        anyhow::Ok(())
    }
    .await;
    drop(file);
    match result {
        Ok(()) => {
            on_finalize();
            let remux = crate::ffmpeg::remux_ts_to_mp4(ffmpeg, &partial, path).await;
            let _ = std::fs::remove_file(&partial);
            remux?;
            Ok(all_from_memory)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
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

/// This process's memory in MB: (private bytes, working set).
#[cfg(windows)]
fn process_memory_mb() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the counters are sized as `cb` says; the pseudo-handle needs no closing.
    let ok = unsafe {
        GetProcessMemoryInfo(GetCurrentProcess(), &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS, counters.cb)
    };
    (ok != 0).then(|| ((counters.PrivateUsage >> 20) as u64, (counters.WorkingSetSize >> 20) as u64))
}

#[cfg(not(windows))]
fn process_memory_mb() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = |key: &str| -> Option<u64> {
        status.lines().find(|l| l.starts_with(key))?.split_whitespace().nth(1)?.parse().ok()
    };
    Some((kb("RssAnon:").unwrap_or(0) >> 10, kb("VmRSS:")? >> 10))
}

/// `frame` at most `width` wide, keeping its shape.
fn shrink(frame: &Frame, width: u32) -> Frame {
    if frame.width() <= width {
        return frame.clone();
    }
    let height = (frame.height() as u64 * width as u64 / frame.width() as u64).max(1) as u32;
    let Some(full) = image::RgbaImage::from_raw(frame.width(), frame.height(), frame.as_bytes().to_vec()) else {
        return frame.clone();
    };
    let small = image::imageops::resize(&full, width, height, image::imageops::FilterType::Triangle);
    slint::SharedPixelBuffer::clone_from_slice(small.as_raw(), width, height)
}

fn file_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "camera".into() } else { s }
}

/// "H500" for the Home card and the list.
/// "Hard disk" / "Sabit disk" for "HDD", and so on.
fn disk_name(label: &str) -> String {
    i18n::disk_label(label)
}

/// When the hub has been asked but gave nothing about its disks.
fn known_none_after_try(loaded: bool, main: Option<&cloud::Disk>) -> bool {
    loaded && main.is_none()
}

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

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// The first day of the calendar's six-week grid for `month`.
fn grid_start(month: NaiveDate) -> NaiveDate {
    week_start(first_of_month(month))
}

/// Hour marks on the timeline: about a dozen across it.
fn tick_step(span_ms: i64) -> i64 {
    const MINUTE: i64 = 60_000;
    [1, 5, 10, 15, 30, 60, 120, 180]
        .into_iter()
        .map(|m| m * MINUTE)
        .find(|step| span_ms / step <= 12)
        .unwrap_or(240 * MINUTE)
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

/// Where a recording is saved: `Videos\Tapas\<camera>_<start>.mp4`.
fn clip_path(camera: &HubCamera, start: i64) -> PathBuf {
    let start = local_time(start).format("%Y-%m-%d_%H-%M-%S");
    config::recordings_dir().join(format!("{}_{start}.mp4", file_safe(&camera.name)))
}

fn is_saved(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
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

/// Counts Unix timestamps into 24 local-hour buckets and returns the busiest
/// hour `(hour, count)` when at least one timestamp is present.
fn hourly_activity(starts: &[i64]) -> ([u32; 24], Option<(usize, u32)>) {
    let mut counts = [0u32; 24];
    for &ts in starts {
        let h = local_time(ts).hour() as usize;
        if h < 24 {
            counts[h] += 1;
        }
    }
    let peak = counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .max_by(|(h1, c1), (h2, c2)| c1.cmp(c2).then_with(|| h2.cmp(h1)))
        .map(|(h, c)| (h, *c));
    (counts, peak)
}

fn row_selection_key(row: &ListRow, fallback_cam: Option<&HubCamera>) -> Option<(String, i64)> {
    let (start, _, _, row_cam) = row.entry()?;
    let cam = row_cam.or(fallback_cam)?;
    Some((cam.device_id.clone(), start))
}

/// Collects selected clips in chronological order (oldest first), deduplicated
/// by `(camera.device_id, clip.start)`.
fn collect_batch_queue(
    rows: &[ListRow],
    fallback_cam: Option<&HubCamera>,
    selected: &HashSet<(String, i64)>,
) -> Vec<(HubCamera, Clip)> {
    let mut seen = HashSet::new();
    let mut queue = Vec::new();
    for row in rows {
        let Some((start, end, kind, row_cam)) = row.entry() else { continue };
        let Some(cam) = row_cam.or(fallback_cam) else { continue };
        let key = (cam.device_id.clone(), start);
        if selected.contains(&key) && seen.insert(key) {
            queue.push((cam.clone(), Clip { start, end: end.max(start + 1), kind }));
        }
    }
    queue.sort_by_key(|(cam, clip)| (clip.start, cam.device_id.clone()));
    queue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(fmt_duration(65), "1m 05s");
        assert_eq!(tick_step(24 * 3_600_000), 120 * 60_000);
        assert_eq!(tick_step(3_600_000), 5 * 60_000);
        assert_eq!(grid_start(NaiveDate::from_ymd_opt(2026, 11, 22).unwrap()), NaiveDate::from_ymd_opt(2026, 10, 26).unwrap());
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
    fn hourly_activity_aggregates_and_finds_peak() {
        assert_eq!(hourly_activity(&[]), ([0u32; 24], None));
        let day = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let at = |h: u32, m: u32| {
            Local.from_local_datetime(&day.and_hms_opt(h, m, 0).unwrap()).earliest().unwrap().timestamp()
        };
        let (counts, peak) = hourly_activity(&[at(9, 10), at(14, 5), at(14, 30), at(14, 55), at(22, 0)]);
        assert_eq!(counts[9], 1);
        assert_eq!(counts[14], 3);
        assert_eq!(counts[22], 1);
        assert_eq!(counts[0], 0);
        assert_eq!(peak, Some((14, 3)));
    }

    #[test]
    fn batch_queue_orders_chronologically_and_dedups() {
        let cam1 = HubCamera { device_id: "C1".into(), name: "Gate".into(), ..Default::default() };
        let cam2 = HubCamera { device_id: "C2".into(), name: "Hall".into(), ..Default::default() };
        let rows = vec![
            ListRow::Hour(2000),
            ListRow::Entry(2050, 2080, 6, Some(cam2.clone())),
            ListRow::Entry(2010, 2040, 2, Some(cam1.clone())),
            ListRow::Entry(2010, 2040, 2, Some(cam1.clone())), // duplicate
            ListRow::Entry(1900, 1920, 8, None),
        ];
        let mut selected = HashSet::new();
        selected.insert(("C2".into(), 2050));
        selected.insert(("C1".into(), 2010));
        selected.insert(("C1".into(), 1900));
        let queue = collect_batch_queue(&rows, Some(&cam1), &selected);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].1.start, 1900);
        assert_eq!(queue[1].1.start, 2010);
        assert_eq!(queue[2].1.start, 2050);
        assert_eq!(queue[2].0.device_id, "C2");
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

    #[test]
    fn nvr_shortcut_helpers_clamp_bounds() {
        assert_eq!(step_speed_index(0, -1, 4), 0);
        assert_eq!(step_speed_index(0, 1, 4), 1);
        assert_eq!(step_speed_index(3, 1, 4), 3);
        assert_eq!(step_speed_index(2, -1, 4), 1);
        assert_eq!(skip_target_ms(10_000, -30, 5_000, 50_000), 5_000);
        assert_eq!(skip_target_ms(10_000, 10, 5_000, 50_000), 20_000);
        assert_eq!(skip_target_ms(45_000, 30, 5_000, 50_000), 50_000);
    }
}

fn step_speed_index(current: i32, delta: i32, len: usize) -> i32 {
    if len == 0 {
        return 0;
    }
    current.saturating_add(delta).clamp(0, len as i32 - 1)
}

fn skip_target_ms(position_ms: i64, delta_secs: i32, day_start_ms: i64, day_end_ms: i64) -> i64 {
    position_ms.saturating_add(delta_secs as i64 * 1000).clamp(day_start_ms, day_end_ms)
}
