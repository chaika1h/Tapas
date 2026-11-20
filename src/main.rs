// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod config;
mod ffmpeg;
mod mjpeg;
mod ptz;
mod secret;
mod stream;

use std::time::Duration;

use config::Renderer;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    init_logging();
    let cfg = config::load();

    let renderer = if std::env::args().any(|a| a == "--software") { Renderer::Software } else { cfg.renderer };
    select_backend(renderer);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("tapo-io")
        .enable_all()
        .build()?;

    let ui = MainWindow::new()?;
    app::install(&ui, cfg, rt.handle().clone());
    ui.run()?;

    app::shutdown();
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// Chooses the graphics stack before any window is created. `SLINT_BACKEND`
/// (e.g. `winit-software`) still overrides this for troubleshooting.
fn select_backend(renderer: Renderer) {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let base = || slint::BackendSelector::new().backend_name("winit".into());
    let selector = match renderer {
        Renderer::Direct3D if cfg!(windows) => base().renderer_name("femtovg-wgpu".into()).require_d3d(),
        Renderer::Direct3D => base().renderer_name("femtovg-wgpu".into()),
        Renderer::Vulkan => base().renderer_name("femtovg-wgpu".into()).require_vulkan(),
        Renderer::Software => base().renderer_name("software".into()),
        Renderer::OpenGL => base().renderer_name("femtovg".into()),
    };
    match selector.select() {
        Ok(()) => log::info!("renderer: {renderer:?}"),
        Err(e) => {
            log::warn!("renderer {renderer:?} unavailable ({e}); using software rendering");
            if let Err(e) = base().renderer_name("software".into()).select() {
                log::error!("software renderer unavailable too: {e}");
            }
        }
    }
}

/// Logs to stderr and to `tapo-viewer.log` next to the settings file, since
/// release builds have no console.
fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,retina=warn,wgpu=warn,naga=warn"));
    let log_path = config::config_path().with_file_name("tapo-viewer.log");
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(file) = std::fs::File::create(&log_path) {
        builder.target(env_logger::Target::Pipe(Box::new(Tee(file))));
    }
    builder.init();
}

/// Writes log lines to both a file and stderr.
struct Tee(std::fs::File);

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
