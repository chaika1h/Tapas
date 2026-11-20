//! Pan/tilt and presets through the camera's local Tapo API.
//!
//! This uses the TP-Link *cloud account* (the Tapo app login), not the camera
//! account used for RTSP. The login happens lazily on the first command.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tapo::{ApiClient, CameraPtzHandler};
use tokio::sync::mpsc;

pub enum PtzCmd {
    Move(i32, i32),
    Goto(String),
    Save(String),
    Refresh,
}

#[derive(Default)]
pub struct PtzState {
    pub busy: bool,
    pub message: String,
    /// (id, name)
    pub presets: Vec<(String, String)>,
}

pub struct Ptz {
    tx: mpsc::UnboundedSender<PtzCmd>,
    pub state: Arc<Mutex<PtzState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Ptz {
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        camera_id: u32,
        host: String,
        cloud_user: String,
        cloud_pass: String,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PtzState::default()));
        let task = rt.spawn(run(camera_id, host, cloud_user, cloud_pass, rx, state.clone()));
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
    host: String,
    user: String,
    pass: String,
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

    let mut handler: Option<CameraPtzHandler> = None;
    while let Some(cmd) = rx.recv().await {
        set(true, None);
        notify();

        if handler.is_none() {
            if user.is_empty() {
                set(false, Some("Enter your Tapo account in Settings to use pan/tilt.".into()));
                notify();
                continue;
            }
            let login = ApiClient::new(user.clone(), pass.clone())
                .with_timeout(Duration::from_secs(10))
                .c210(host.clone())
                .await;
            match login {
                Ok(h) => handler = Some(h),
                Err(e) => {
                    set(false, Some(format!("Could not log in to the camera: {e}")));
                    notify();
                    continue;
                }
            }
        }
        let h = handler.as_ref().expect("logged in above");

        let result = match cmd {
            PtzCmd::Move(pan, tilt) => h.pan_tilt(pan, tilt).await.map(|_| String::new()),
            PtzCmd::Goto(id) => h.goto_preset(&id).await.map(|_| "Moving to preset…".into()),
            PtzCmd::Save(name) => match h.save_preset(&name).await {
                Ok(()) => load_presets(h, &state).await.map(|_| format!("Saved preset “{name}”.")),
                Err(e) => Err(e),
            },
            PtzCmd::Refresh => load_presets(h, &state).await.map(|_| String::new()),
        };

        match result {
            Ok(msg) => set(false, Some(msg)),
            Err(e) => {
                // A device error (e.g. "reached the end of travel") keeps the
                // session; anything else forces a fresh login next time.
                if !matches!(e, tapo::Error::Tapo(tapo::TapoResponseError::DeviceError { .. })) {
                    handler = None;
                }
                set(false, Some(format!("Pan/tilt: {e}")));
            }
        }
        notify();
    }
}

async fn load_presets(h: &CameraPtzHandler, state: &Arc<Mutex<PtzState>>) -> Result<(), tapo::Error> {
    let presets = h.get_presets().await?;
    state.lock().unwrap().presets = presets.into_iter().map(|p| (p.id, p.name)).collect();
    Ok(())
}
