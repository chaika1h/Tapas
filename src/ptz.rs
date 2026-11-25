//! Camera controls through the Tapo cloud, so they work from anywhere like
//! live view: pan/tilt, presets and privacy mode. One task per camera handles
//! its commands in order.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::cloud::{CloudCamera, CloudClient, CloudError, MAX_MOVE, NightVisionMode, PatrolAxis};

pub enum PtzCmd {
    /// Relative move: positive pans right / tilts up.
    Move(i32, i32),
    Goto(String),
    Save(String),
    DeletePreset(String),
    Privacy(bool),
    Spotlight(bool),
    NightVision(NightVisionMode),
    AutoTrack(bool),
    PatrolStart(PatrolAxis),
    PatrolStop,
    Siren(bool),
    Microphone(bool),
    Reboot,
    /// Re-reads what the camera supports, its toggle states and presets.
    Refresh,
}

#[derive(Default)]
pub struct PtzState {
    pub busy: bool,
    pub message: String,
    /// (id, name)
    pub presets: Vec<(String, String)>,
    /// Whether the camera has pan/tilt; `None` until it has answered.
    pub supported: Option<bool>,
    /// Privacy mode; `None` until known, or if the camera has none.
    pub privacy: Option<bool>,
    /// White spotlight state; `None` if unsupported/unknown.
    pub spotlight: Option<bool>,
    /// Night vision mode; `None` if unsupported/unknown.
    pub night_vision: Option<NightVisionMode>,
    /// Motion auto-tracking; `None` if unsupported/unknown.
    pub auto_track: Option<bool>,
    /// Manual siren/alarm state; `None` if unsupported/unknown.
    pub siren_on: Option<bool>,
    /// Hardware camera microphone state (`Some(false)` = muted/disabled on camera).
    pub mic_enabled: Option<bool>,
    /// Whether the user started patrol from this session.
    pub patrolling: bool,
}

pub struct Ptz {
    tx: mpsc::UnboundedSender<PtzCmd>,
    pub state: Arc<Mutex<PtzState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Ptz {
    pub fn spawn(rt: &tokio::runtime::Handle, camera_id: u32, client: Arc<CloudClient>, camera: CloudCamera) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PtzState::default()));
        let task = rt.spawn(run(camera_id, client, camera, rx, state.clone()));
        Self { tx, state, task }
    }

    pub fn send(&self, cmd: PtzCmd) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for Ptz {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    camera_id: u32,
    client: Arc<CloudClient>,
    camera: CloudCamera,
    mut rx: mpsc::UnboundedReceiver<PtzCmd>,
    state: Arc<Mutex<PtzState>>,
) {
    let notify = move || crate::app::on_ui(move |app| app.ptz_changed(camera_id));
    let set = |busy: bool, message: Option<String>| {
        let mut s = state.lock().unwrap();
        s.busy = busy;
        if let Some(m) = message {
            s.message = m;
        }
    };

    let mut next: Option<PtzCmd> = None;
    loop {
        let cmd = match next.take() {
            Some(cmd) => cmd,
            None => match rx.recv().await {
                Some(cmd) => cmd,
                None => break,
            },
        };
        // Each move is a round trip through the cloud, so arrow presses that
        // queued up meanwhile become one bigger move.
        let cmd = match cmd {
            PtzCmd::Move(mut x, mut y) => {
                while let Ok(more) = rx.try_recv() {
                    match more {
                        PtzCmd::Move(dx, dy) => (x, y) = (x + dx, y + dy),
                        other => {
                            next = Some(other);
                            break;
                        }
                    }
                }
                PtzCmd::Move(x.clamp(-MAX_MOVE, MAX_MOVE), y.clamp(-MAX_MOVE, MAX_MOVE))
            }
            other => other,
        };

        set(true, None);
        notify();

        // What the camera can do, before the first command (and on refresh).
        let known = state.lock().unwrap().supported.is_some();
        if !known || matches!(cmd, PtzCmd::Refresh) {
            match client.camera_controls(&camera).await {
                Ok(ctrl) => {
                    let mut s = state.lock().unwrap();
                    s.supported = Some(ctrl.ptz);
                    s.privacy = ctrl.privacy;
                    s.spotlight = ctrl.spotlight;
                    s.night_vision = ctrl.night_vision;
                    s.auto_track = ctrl.auto_track;
                    s.siren_on = ctrl.siren_on;
                    s.mic_enabled = ctrl.mic.is_enabled();
                }
                Err(e) => {
                    report(&e, &set);
                    notify();
                    continue;
                }
            }
        }
        let supported = state.lock().unwrap().supported == Some(true);

        let result = match cmd {
            PtzCmd::Privacy(on) => match client.set_privacy_mode(&camera, on).await {
                Ok(now) => {
                    state.lock().unwrap().privacy = Some(now);
                    // The relay stops sending while the lens is covered;
                    // reconnect so the picture comes back when it's uncovered.
                    crate::app::on_ui(move |app| app.privacy_changed(camera_id));
                    Ok(if now {
                        crate::i18n::pick("Privacy mode is on.", "Gizlilik modu açık.")
                    } else {
                        crate::i18n::pick("Privacy mode is off.", "Gizlilik modu kapalı.")
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Spotlight(on) => match client.set_spotlight(&camera, on).await {
                Ok(now) => {
                    state.lock().unwrap().spotlight = Some(now);
                    Ok(if now {
                        crate::i18n::pick("Spotlight turned on.", "Spot ışığı açıldı.")
                    } else {
                        crate::i18n::pick("Spotlight turned off.", "Spot ışığı kapatıldı.")
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::NightVision(mode) => match client.set_night_vision(&camera, mode).await {
                Ok(now) => {
                    state.lock().unwrap().night_vision = Some(now);
                    Ok(format!("{}: {}.", crate::i18n::pick("Night vision", "Gece görüşü"), now.label()))
                }
                Err(e) => Err(e),
            },
            PtzCmd::AutoTrack(on) => match client.set_auto_track(&camera, on).await {
                Ok(now) => {
                    state.lock().unwrap().auto_track = Some(now);
                    Ok(if now {
                        crate::i18n::pick("Auto-tracking is on.", "Otomatik takip açık.")
                    } else {
                        crate::i18n::pick("Auto-tracking is off.", "Otomatik takip kapalı.")
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Siren(on) => match client.set_camera_alarm(&camera, on).await {
                Ok(now) => {
                    state.lock().unwrap().siren_on = Some(now);
                    Ok(if now {
                        crate::i18n::pick("Siren triggered.", "Siren çalıyor.")
                    } else {
                        crate::i18n::pick("Siren stopped.", "Siren durduruldu.")
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Microphone(on) => match client.set_mic_enabled(&camera, on).await {
                Ok((now, record_audio_was_off)) => {
                    state.lock().unwrap().mic_enabled = Some(now);
                    if now && record_audio_was_off {
                        crate::app::on_ui(move |app| app.reconnect_camera_stream(camera_id));
                    }
                    Ok(if now {
                        crate::i18n::pick("Camera microphone turned on.", "Kamera mikrofonu açıldı.")
                    } else {
                        crate::i18n::pick("Camera microphone muted.", "Kamera mikrofonu kapatıldı.")
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Reboot => match client.reboot_device(&camera).await {
                Ok(()) => Ok(crate::i18n::pick("Rebooting camera…", "Kamera yeniden başlatılıyor…").to_string()),
                Err(e) => Err(e),
            },
            PtzCmd::Refresh if supported => load_presets(&client, &camera, &state).await.map(|_| String::new()),
            PtzCmd::Refresh => Ok(String::new()),
            _ if !supported => Ok(crate::i18n::pick("This camera has no pan/tilt.", "Bu kamerada yönlendirme yok.").to_string()),
            PtzCmd::Move(x, y) => {
                state.lock().unwrap().patrolling = false;
                client.move_motor(&camera, x, y).await.map(|_| String::new())
            }
            PtzCmd::PatrolStart(axis) => match client.start_patrol(&camera, axis).await {
                Ok(()) => {
                    state.lock().unwrap().patrolling = true;
                    Ok(match axis {
                        PatrolAxis::Horizontal => crate::i18n::pick("Horizontal patrol started.", "Yatay tarama başlatıldı."),
                        PatrolAxis::Vertical => crate::i18n::pick("Vertical patrol started.", "Dikey tarama başlatıldı."),
                    }
                    .to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::PatrolStop => match client.stop_patrol(&camera).await {
                Ok(()) => {
                    state.lock().unwrap().patrolling = false;
                    Ok(crate::i18n::pick("Patrol stopped.", "Tarama durduruldu.").to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Goto(id) => {
                state.lock().unwrap().patrolling = false;
                client
                    .goto_preset(&camera, &id)
                    .await
                    .map(|_| crate::i18n::pick("Moving to preset…", "Konuma gidiliyor…").into())
            }
            PtzCmd::Save(name) => match client.save_preset(&camera, &name).await {
                Ok(()) => load_presets(&client, &camera, &state).await.map(|_| {
                    if crate::i18n::is_tr() {
                        format!("“{name}” konumu kaydedildi.")
                    } else {
                        format!("Saved preset “{name}”.")
                    }
                }),
                Err(e) => Err(e),
            },
            PtzCmd::DeletePreset(id) => match client.delete_preset(&camera, &id).await {
                Ok(()) => load_presets(&client, &camera, &state)
                    .await
                    .map(|_| crate::i18n::pick("Preset deleted.", "Konum silindi.").into()),
                Err(e) => Err(e),
            },
        };
        match result {
            Ok(msg) => set(false, Some(msg)),
            Err(e) => report(&e, &set),
        }
        notify();
    }
}

fn report(e: &CloudError, set: &impl Fn(bool, Option<String>)) {
    if e.is_auth() {
        crate::app::on_ui(|app| app.cloud_auth_failed());
    }
    set(false, Some(e.to_string()));
}

async fn load_presets(
    client: &CloudClient,
    camera: &CloudCamera,
    state: &Arc<Mutex<PtzState>>,
) -> Result<(), CloudError> {
    let presets = client.presets(camera).await?;
    state.lock().unwrap().presets = presets;
    Ok(())
}
