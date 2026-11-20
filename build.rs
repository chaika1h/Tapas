use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");
    bundle_ffmpeg();
}

/// Embeds ffmpeg into the executable when `vendor/ffmpeg.exe` (Windows) or
/// `vendor/ffmpeg` (other targets) exists, or when `TAPO_VIEWER_FFMPEG` points
/// to a binary. Build it with `tools/build-ffmpeg.sh`. Without it, the app
/// looks for ffmpeg next to the exe and on PATH instead.
fn bundle_ffmpeg() {
    println!("cargo::rustc-check-cfg=cfg(bundled_ffmpeg)");
    println!("cargo::rerun-if-env-changed=TAPO_VIEWER_FFMPEG");
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let default = if windows { "vendor/ffmpeg.exe" } else { "vendor/ffmpeg" };
    println!("cargo::rerun-if-changed={default}");

    let src = env::var_os("TAPO_VIEWER_FFMPEG").map(PathBuf::from).unwrap_or_else(|| default.into());
    let Ok(bytes) = fs::read(&src) else {
        if windows && env::var("PROFILE").as_deref() == Ok("release") {
            println!("cargo::warning=ffmpeg is not bundled; run tools/build-ffmpeg.sh to include it");
        }
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ffmpeg.bin");
    fs::write(&out, &bytes).expect("copy ffmpeg into OUT_DIR");

    // FNV-1a of the binary: names the extracted copy, so a new build replaces it.
    let id = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3));
    println!("cargo::rustc-env=BUNDLED_FFMPEG_ID={id:016x}");
    println!("cargo::rustc-cfg=bundled_ffmpeg");
}
