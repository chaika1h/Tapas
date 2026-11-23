// No console window in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod clip;
mod cloud;
mod config;
mod ffmpeg;
mod pictures;
mod playback;
mod ptz;
mod relay;
mod secret;
mod stream;
mod update;

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
    let wgpu_config = |backends: slint::wgpu_30::wgpu::Backends| {
        use slint::wgpu_30::{WGPUConfiguration, WGPUSettings, wgpu};
        let mut settings = WGPUSettings::default();
        settings.backends = backends;
        // Prefer the integrated GPU so a discrete GPU driver isn't spun up just
        // for a 2D UI + video viewer.
        settings.power_preference = wgpu::PowerPreference::LowPower;
        // Keep gpu-allocator D3D12/Vulkan heap blocks small (4..16 MiB device,
        // 4..8 MiB host) instead of growing up to 64 MiB and holding them idle.
        const MB: u64 = 1024 * 1024;
        settings.device_memory_hints = wgpu::MemoryHints::Manual {
            suballocated_device_memory_block_size: (4 * MB)..(16 * MB),
        };
        WGPUConfiguration::Automatic(settings)
    };
    let selector = match renderer {
        Renderer::Direct3D if cfg!(windows) => base()
            .renderer_name("femtovg-wgpu".into())
            .require_wgpu_30(wgpu_config(slint::wgpu_30::wgpu::Backends::DX12)),
        Renderer::Direct3D => base()
            .renderer_name("femtovg-wgpu".into())
            .require_wgpu_30(wgpu_config(slint::wgpu_30::wgpu::Backends::PRIMARY)),
        Renderer::Vulkan => base()
            .renderer_name("femtovg-wgpu".into())
            .require_wgpu_30(wgpu_config(slint::wgpu_30::wgpu::Backends::VULKAN)),
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

/// Logs to stderr and to `tapas.log` next to the settings file, since
/// release builds have no console.
fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,wgpu=warn,naga=warn"));
    let log_path = config::config_path().with_file_name("tapas.log");
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
