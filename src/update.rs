//! Updates from GitHub releases. The newest release's installer
//! (`TapoViewer-Setup-<version>.exe`, from `tools/build-installer.sh`) is
//! downloaded, checked against the SHA-256 GitHub publishes for it and run
//! silently (`/S /RELAUNCH`): it installs over this version once the app has
//! closed and starts the new one.
//!
//! The releases must be public: the app has no GitHub sign-in.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Where releases are published unless the settings name another repository.
pub const DEFAULT_REPO: &str = "chaika1h/tapas";

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    /// The installer.
    pub url: String,
    pub size: u64,
    /// Hex, from the asset's `digest`.
    pub sha256: Option<String>,
}

fn client(timeout: Option<Duration>) -> Result<reqwest::Client> {
    let tls = crate::cloud::tls_config().map_err(|e| anyhow!("{e}"))?;
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured((*tls).clone())
        .user_agent(format!("TapoViewer/{CURRENT}"))
        .connect_timeout(Duration::from_secs(15));
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    builder.build().context("HTTP client setup failed")
}

fn latest_url(repo: &str) -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("TAPO_VIEWER_UPDATE_URL") {
        return url;
    }
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// The newest release, if it's newer than this version.
pub async fn check(repo: &str) -> Result<Option<Release>> {
    let response = client(Some(Duration::from_secs(30)))?
        .get(latest_url(repo))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("Could not reach GitHub")?;
    match response.status().as_u16() {
        200 => {}
        404 => bail!("No releases at github.com/{repo} (none yet, or the repository is private)"),
        403 | 429 => bail!("GitHub is limiting requests; try again in an hour"),
        code => bail!("GitHub answered HTTP {code}"),
    }
    let bytes = response.bytes().await.context("Could not read GitHub's answer")?;
    let release: Value = serde_json::from_slice(&bytes).context("Unexpected answer from GitHub")?;
    parse_release(&release)
}

fn parse_release(release: &Value) -> Result<Option<Release>> {
    let tag = release.get("tag_name").and_then(Value::as_str).unwrap_or("");
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if !is_newer(&version, CURRENT) {
        return Ok(None);
    }
    let asset = release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|a| {
            let name = a.get("name").and_then(Value::as_str).unwrap_or("");
            name.starts_with("TapoViewer-Setup") && name.ends_with(".exe")
        })
        .ok_or_else(|| anyhow!("Release {tag} has no installer"))?;
    let sha256 = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    Ok(Some(Release {
        version,
        notes: release.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string(),
        url: asset.get("browser_download_url").and_then(Value::as_str).unwrap_or("").to_string(),
        size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
        sha256,
    }))
}

/// Whether version `a` is newer than `b` ("0.10.0" > "0.9.3").
pub fn is_newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect() };
    let (a, b) = (parts(a), parts(b));
    !a.is_empty() && a > b
}

/// Downloads the installer to the temp folder, reporting (bytes, of bytes).
pub async fn download(release: &Release, progress: &mut (dyn FnMut(u64, u64) + Send)) -> Result<PathBuf> {
    use tokio::io::AsyncWriteExt;
    let path = std::env::temp_dir().join(format!("TapoViewer-Setup-{}.exe", release.version));
    let mut response = client(None)?.get(&release.url).send().await.context("Could not download the update")?;
    if !response.status().is_success() {
        bail!("Could not download the update (HTTP {})", response.status().as_u16());
    }
    let total = response.content_length().unwrap_or(release.size);
    let mut file = tokio::fs::File::create(&path).await.context("Could not save the update")?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| anyhow!("The download stalled"))?
            .context("The download broke off")?;
        let Some(chunk) = chunk else { break };
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        got += chunk.len() as u64;
        progress(got, total);
    }
    file.flush().await?;
    drop(file);
    let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    match &release.sha256 {
        Some(expected) if *expected != digest => {
            let _ = tokio::fs::remove_file(&path).await;
            bail!("The download is damaged (its SHA-256 doesn't match the release); try again")
        }
        Some(_) => {}
        None => log::warn!("update: the release has no SHA-256 to check the download against"),
    }
    Ok(path)
}

/// Starts the installer silently; it waits for this app to close, installs
/// and starts the new version. The caller then quits.
pub fn install(installer: &std::path::Path) -> Result<()> {
    if cfg!(windows) {
        std::process::Command::new(installer)
            .args(["/S", "/RELAUNCH"])
            .spawn()
            .context("Could not start the installer")?;
        Ok(())
    } else {
        bail!("Updates install on Windows only (the installer is at {})", installer.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("", "0.2.0"));
    }

    #[test]
    fn reads_a_github_release() {
        let release = json!({
            "tag_name": "v99.1.0",
            "body": "Faster playback.\n",
            "assets": [
                {"name": "TapoViewer-99.1.0-portable.zip", "browser_download_url": "https://x/zip", "size": 5},
                {"name": "TapoViewer-Setup-99.1.0.exe", "browser_download_url": "https://x/setup", "size": 12,
                 "digest": "sha256:ABCDEF"}
            ]
        });
        let r = parse_release(&release).unwrap().unwrap();
        assert_eq!((r.version.as_str(), r.url.as_str(), r.size), ("99.1.0", "https://x/setup", 12));
        assert_eq!(r.sha256.as_deref(), Some("abcdef"));
        assert_eq!(r.notes, "Faster playback.");
        // Not newer: nothing to do.
        assert!(parse_release(&json!({"tag_name": "v0.0.1", "assets": []})).unwrap().is_none());
        // Newer but without an installer.
        assert!(parse_release(&json!({"tag_name": "v99.0.0", "assets": []})).is_err());
    }
}
