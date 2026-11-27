use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");
    bundle_ffmpeg();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Statically link C++ / GCC runtimes (needed by openh264) on mingw-w64
        // so Tapas.exe depends only on built-in Windows system DLLs.
        // Because openh264-sys2 emits an explicit `-lstdc++`, staging only the
        // static `libstdc++.a` (without `libstdc++.dll.a`) ahead on the linker
        // search path guarantees `libstdc++-6.dll` is never imported.
        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            stage_static_mingw_libs();
            println!("cargo::rustc-link-arg=-static-libstdc++");
            println!("cargo::rustc-link-arg=-static-libgcc");
            println!("cargo::rustc-link-arg=-Wl,-Bstatic");
            println!("cargo::rustc-link-arg=-l:libwinpthread.a");
            println!("cargo::rustc-link-arg=-Wl,-Bdynamic");
        }
        // The exe's icon (Explorer, the taskbar, shortcuts).
        println!("cargo::rerun-if-changed=assets/app.ico");
        embed_resource::compile("assets/app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("could not embed the app icon");
    }
}

fn stage_static_mingw_libs() {
    let dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("static-mingw");
    let _ = fs::create_dir_all(&dir);
    for lib in ["libstdc++.a", "libwinpthread.a"] {
        let out = std::process::Command::new("x86_64-w64-mingw32-g++")
            .arg(format!("-print-file-name={lib}"))
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            let path = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
            if path.is_file() {
                let _ = fs::copy(&path, dir.join(lib));
            }
        }
    }
    println!("cargo::rustc-link-search=native={}", dir.display());
}

/// Embeds ffmpeg into the executable when `vendor/ffmpeg.exe` (Windows) or
/// `vendor/ffmpeg` (other targets) exists, or when `TAPAS_FFMPEG` points
/// to a binary. Build it with `tools/build-ffmpeg.sh`. Without it, the app
/// looks for ffmpeg next to the exe and on PATH instead.
fn bundle_ffmpeg() {
    println!("cargo::rustc-check-cfg=cfg(bundled_ffmpeg)");
    println!("cargo::rerun-if-env-changed=TAPAS_FFMPEG");
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let default = if windows { "vendor/ffmpeg.exe" } else { "vendor/ffmpeg" };
    println!("cargo::rerun-if-changed={default}");

    let src = env::var_os("TAPAS_FFMPEG").map(PathBuf::from).unwrap_or_else(|| default.into());
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
