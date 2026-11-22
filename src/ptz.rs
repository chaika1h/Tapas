//! Camera controls through the Tapo cloud, so they work from anywhere like
//! live view: pan/tilt, presets and privacy mode. One task per camera handles
//! its commands in order.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::cloud::{CloudCamera, CloudClient, CloudError, MAX_MOVE};

pub enum PtzCmd {
    /// Relative move: positive pans right / tilts up.
    Move(i32, i32),
    Goto(String),
    Save(String),
    Privacy(bool),
    /// Re-reads what the camera supports, its privacy state and presets.
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
            match client.camera_overview(&camera).await {
                Ok((ptz, privacy)) => {
                    let mut s = state.lock().unwrap();
                    s.supported = Some(ptz);
                    s.privacy = privacy;
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
                    Ok(if now { "Privacy mode is on." } else { "Privacy mode is off." }.to_string())
                }
                Err(e) => Err(e),
            },
            PtzCmd::Refresh if supported => load_presets(&client, &camera, &state).await.map(|_| String::new()),
            PtzCmd::Refresh => Ok(String::new()),
            _ if !supported => Ok("This camera has no pan/tilt.".to_string()),
            PtzCmd::Move(x, y) => client.move_motor(&camera, x, y).await.map(|_| String::new()),
            PtzCmd::Goto(id) => client.goto_preset(&camera, &id).await.map(|_| "Moving to preset…".into()),
            PtzCmd::Save(name) => match client.save_preset(&camera, &name).await {
                Ok(()) => load_presets(&client, &camera, &state).await.map(|_| format!("Saved preset “{name}”.")),
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
