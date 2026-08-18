use std::{
    env, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LATEST_RELEASE: &str =
    "https://api.github.com/repos/EmbrasureAI/embrasure-cli/releases/latest";
const RELEASE_DOWNLOADS: &str = "https://github.com/EmbrasureAI/embrasure-cli/releases/download";

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: String,
    latest: String,
}

pub fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("embrasure/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not initialize the HTTP client")
}

pub async fn run(check_only: bool) -> Result<String> {
    let latest = latest_version().await?;
    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(&latest, current)? {
        return Ok(format!("Embrasure {current} is up to date."));
    }
    if check_only {
        return Ok(format!(
            "Embrasure {latest} is available; current version is {current}."
        ));
    }
    let executable = env::current_exe().context("could not locate the current executable")?;
    if installed_by_brew(&executable) {
        let status = Command::new("brew")
            .args(["upgrade", "embrasureai/tap/embrasure"])
            .status()
            .context("could not run Homebrew; run `brew upgrade embrasureai/tap/embrasure`")?;
        if !status.success() {
            bail!("Homebrew upgrade failed; run `brew upgrade embrasureai/tap/embrasure`");
        }
        return Ok(format!("Updated Embrasure to {latest} with Homebrew."));
    }
    self_replace(&executable, &latest).await?;
    Ok(format!("Updated Embrasure to {latest}."))
}

pub async fn doctor_notice() -> Option<String> {
    if !std::io::stderr().is_terminal()
        || env::var_os("CI").is_some()
        || env::var_os("NO_UPDATE_NOTIFIER").is_some()
    {
        return None;
    }
    if let Ok(cache) = read_cache()
        && cache_is_fresh(&cache.checked_at)
    {
        return is_newer(&cache.latest, env!("CARGO_PKG_VERSION"))
            .ok()
            .filter(|newer| *newer)
            .map(|_| {
                format!(
                    "Embrasure {} is available; run `embrasure update`.",
                    cache.latest
                )
            });
    }
    let latest = latest_version().await.ok()?;
    let _ = write_cache(&UpdateCache {
        checked_at: Utc::now().to_rfc3339(),
        latest: latest.clone(),
    });
    is_newer(&latest, env!("CARGO_PKG_VERSION"))
        .ok()
        .filter(|newer| *newer)
        .map(|_| format!("Embrasure {latest} is available; run `embrasure update`."))
}

async fn latest_version() -> Result<String> {
    let release: LatestRelease = http_client()?
        .get(LATEST_RELEASE)
        .send()
        .await
        .context("could not check GitHub for updates")?
        .error_for_status()
        .context("GitHub update check failed")?
        .json()
        .await
        .context("GitHub returned an invalid release response")?;
    Ok(release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_owned())
}

async fn self_replace(executable: &Path, version: &str) -> Result<()> {
    let target = release_target()?;
    let archive_name = format!("embrasure-{version}-{target}.tar.gz");
    let base = format!("{RELEASE_DOWNLOADS}/v{version}");
    let client = http_client()?;
    let archive = download(&client, &format!("{base}/{archive_name}")).await?;
    let checksums = download(&client, &format!("{base}/SHA256SUMS")).await?;
    verify_checksum(&archive_name, &archive, &checksums)?;

    let scratch = tempfile::tempdir().context("could not create update directory")?;
    let archive_path = scratch.path().join(&archive_name);
    fs::write(&archive_path, archive)?;
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(scratch.path())
        .status()
        .context("could not extract the update; install `tar` and retry")?;
    if !status.success() {
        bail!("could not extract {archive_name}");
    }
    let downloaded = scratch
        .path()
        .join(format!("embrasure-{version}-{target}"))
        .join("embrasure");
    let replacement = replacement_path(executable)?;
    fs::copy(&downloaded, &replacement).with_context(|| {
        format!(
            "could not write next to {}; check directory permissions",
            executable.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&replacement, executable).with_context(|| {
        format!(
            "could not replace {}; check directory permissions",
            executable.display()
        )
    })?;
    Ok(())
}

async fn download(client: &Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec())
}

fn verify_checksum(name: &str, archive: &[u8], checksums: &[u8]) -> Result<()> {
    let checksums = std::str::from_utf8(checksums).context("SHA256SUMS is not UTF-8")?;
    let expected = checksums
        .lines()
        .find_map(|line| {
            let (checksum, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start_matches([' ', '*']) == name).then_some(checksum)
        })
        .with_context(|| format!("SHA256SUMS does not contain {name}"))?;
    let actual = hex(&Sha256::digest(archive));
    if actual != expected {
        bail!("checksum verification failed for {name}");
    }
    Ok(())
}

fn release_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        (os, arch) => bail!("updates are not available for {os}/{arch}"),
    }
}

fn installed_by_brew(executable: &Path) -> bool {
    let path = executable.to_string_lossy();
    path.contains("/Cellar/")
        || path.contains("/opt/homebrew/")
        || path.contains("/home/linuxbrew/")
}

fn replacement_path(executable: &Path) -> Result<PathBuf> {
    let name = executable
        .file_name()
        .context("current executable has no file name")?;
    Ok(executable.with_file_name(format!("{}.new", name.to_string_lossy())))
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    Ok(parse_version(candidate)? > parse_version(current)?)
}

fn parse_version(value: &str) -> Result<(u64, u64, u64)> {
    let numbers = value.split('.').collect::<Vec<_>>();
    if numbers.len() != 3 {
        bail!("invalid release version {value}");
    }
    Ok((
        numbers[0].parse()?,
        numbers[1].parse()?,
        numbers[2].parse()?,
    ))
}

fn cache_path() -> Result<PathBuf> {
    Ok(ProjectDirs::from("ai", "Embrasure", "embrasure-cli")
        .context("could not resolve the OS cache directory")?
        .cache_dir()
        .join("update-check.json"))
}

fn read_cache() -> Result<UpdateCache> {
    Ok(serde_json::from_slice(&fs::read(cache_path()?)?)?)
}

fn write_cache(cache: &UpdateCache) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec(cache)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn cache_is_fresh(value: &str) -> bool {
    let Ok(checked) = DateTime::parse_from_rfc3339(value) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(checked.with_timezone(&Utc));
    age >= chrono::Duration::zero() && age <= chrono::Duration::hours(24)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_checksums_are_strict() {
        assert!(is_newer("0.5.0", "0.4.9").unwrap());
        assert!(!is_newer("0.4.0", "0.4.0").unwrap());
        assert!(parse_version("0.4").is_err());
        let archive = b"archive";
        let sums = format!("{}  release.tar.gz\n", hex(&Sha256::digest(archive)));
        assert!(verify_checksum("release.tar.gz", archive, sums.as_bytes()).is_ok());
    }
}
