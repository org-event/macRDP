//! Checks GitHub for a newer release and installs it.
//!
//! The default is to ask with a dialog and install only after confirmation.
//! `--update auto` installs without asking. `--update off` (or `--no-update`)
//! does not check. A postponed version is asked about again the next day, and
//! a newer tag is asked about straight away.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use tracing::{info, warn};

const LATEST: &str = "https://api.github.com/repos/org-event/OpenRDP/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/org-event/OpenRDP/releases/download/";
const FIRST_WAIT: Duration = Duration::from_secs(20);
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const ASK_AGAIN_AFTER: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Auto,
    Off,
}

pub fn mode(disabled: bool, flag: Option<&str>) -> anyhow::Result<Mode> {
    if disabled {
        return Ok(Mode::Off);
    }
    match flag {
        Some("ask") => Ok(Mode::Ask),
        Some("auto") => Ok(Mode::Auto),
        Some("off") => Ok(Mode::Off),
        Some(other) => bail!("unknown --update {other} (ask, auto, off)"),
        None => Ok(mode_from_env()),
    }
}

fn mode_from_env() -> Mode {
    match std::env::var("OPENRDP_UPDATE").ok().as_deref() {
        Some("auto") => Mode::Auto,
        Some("off" | "0") => Mode::Off,
        _ => Mode::Ask,
    }
}

/// Checks soon after startup, then every six hours, until the process exits.
pub fn watch(mode: Mode) {
    if mode == Mode::Off {
        return;
    }
    sleep(FIRST_WAIT);
    loop {
        if let Err(err) = check_once(mode) {
            warn!(%err, "update check failed");
        }
        sleep(INTERVAL);
    }
}

fn check_once(mode: Mode) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let release = fetch_latest()?;
    let Some(release) = release else {
        return Ok(());
    };
    if !is_newer(&release.tag, current) {
        info!(current, latest = %release.tag, "no update");
        return Ok(());
    }
    let state = State::load()?;
    let now = unix_now();
    if mode == Mode::Ask
        && state.skipped.as_deref() == Some(release.tag.as_str())
        && now.saturating_sub(state.skipped_at) < ASK_AGAIN_AFTER
    {
        info!(version = %release.tag, "update postponed");
        return Ok(());
    }
    info!(version = %release.tag, current, "update available");
    let install = match mode {
        Mode::Auto => true,
        Mode::Off => false,
        Mode::Ask => match ask(&release.tag, current)? {
            Some(true) => true,
            Some(false) => {
                State {
                    skipped: Some(release.tag),
                    skipped_at: now,
                }
                .save()?;
                info!("update postponed from the dialog");
                return Ok(());
            }
            None => {
                warn!("no dialog, so the update was not installed");
                return Ok(());
            }
        },
    };
    if install {
        install_release(&release)?;
    }
    Ok(())
}

struct Release {
    tag: String,
    archive: String,
    checksum: String,
}

fn fetch_latest() -> anyhow::Result<Option<Release>> {
    let body = curl(LATEST).context("fetch the latest release")?;
    let Some(tag) = json_string(&body, "tag_name").map(str::to_owned) else {
        bail!("latest release has no tag");
    };
    let Some((archive, checksum)) = asset_urls(&body) else {
        warn!(%tag, "latest release has no Apple Silicon archive");
        return Ok(None);
    };
    if !archive.starts_with(DOWNLOAD_PREFIX) || !checksum.starts_with(DOWNLOAD_PREFIX) {
        bail!("release download URL is not from this repository");
    }
    Ok(Some(Release {
        tag,
        archive,
        checksum,
    }))
}

fn install_release(release: &Release) -> anyhow::Result<()> {
    let version = release.tag.trim_start_matches('v');
    let dir = crate::tls::support_dir()?.join("update");
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let archive = dir.join("openrdp.tar.gz");
    curl_to(&release.archive, &archive).context("download the release")?;
    let expected = curl(&release.checksum).context("download the checksum")?;
    let expected = checksum_hex(&expected).context("read the checksum")?;
    let actual = sha256(&archive)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("checksum mismatch for {}", archive.display());
    }
    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    crate::service::run(
        Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked),
    )
    .context("unpack the release")?;
    let binary = find_binary(&unpacked)?;
    let _ = Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&binary)
        .status();
    if crate::service::running_from_app() {
        info!(version, "installing the update into OpenRDP.app");
        crate::service::apply_update(&binary, version)?;
    } else {
        info!(version, "installing the update and restarting");
        reinstall_detached(&binary)?;
        std::process::exit(0);
    }
    Ok(())
}

/// Starts `binary install <this process's arguments>` in its own session, after
/// a pause so this process can release the port.
fn reinstall_detached(binary: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("sleep 2; exec \"$0\" \"$@\"")
        .arg(binary)
        .arg("install");
    for arg in std::env::args().skip(1) {
        cmd.arg(arg);
    }
    cmd.env("OPENRDP_UPDATE", "off");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.process_group(0);
    cmd.spawn().context("start the installer")?;
    Ok(())
}

/// `Some(true)` install, `Some(false)` later, `None` when there is no dialog.
fn ask(tag: &str, current: &str) -> anyhow::Result<Option<bool>> {
    let version = tag.trim_start_matches('v');
    let (message, later, install) = if russian() {
        (
            format!(
                "Доступен OpenRDP {version} (сейчас {current}). Подключение на минуту прервётся. Установить?"
            ),
            "Позже",
            "Установить",
        )
    } else {
        (
            format!(
                "OpenRDP {version} is available (this one is {current}). The connection drops for a moment. Install?"
            ),
            "Later",
            "Install",
        )
    };
    let script = format!(
        "display dialog {} buttons {{{}, {}}} default button {} with title \"OpenRDP\"",
        apple(&message),
        apple(later),
        apple(install),
        apple(later),
    );
    let output = Command::new("osascript").arg("-e").arg(script).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Ok(Some(answer.contains(install)))
}

fn russian() -> bool {
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output();
    if let Ok(output) = output
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with("ru")
    {
        return true;
    }
    std::env::var("LANG").is_ok_and(|lang| lang.starts_with("ru"))
}

fn apple(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn curl(url: &str) -> anyhow::Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: openrdp",
            url,
        ])
        .output()
        .context("run curl")?;
    if !output.status.success() {
        bail!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn curl_to(url: &str, dest: &Path) -> anyhow::Result<()> {
    crate::service::run(
        Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "180",
                "-H",
                "User-Agent: openrdp",
                "-o",
            ])
            .arg(dest)
            .arg(url),
    )
}

fn sha256(path: &Path) -> anyhow::Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .context("run shasum")?;
    if !output.status.success() {
        bail!("shasum failed");
    }
    checksum_hex(&String::from_utf8(output.stdout)?).context("read shasum")
}

fn find_binary(root: &Path) -> anyhow::Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "openrdp") {
                return Ok(path);
            }
        }
    }
    bail!("release archive has no openrdp binary")
}

struct State {
    skipped: Option<String>,
    skipped_at: u64,
}

impl State {
    fn load() -> anyhow::Result<Self> {
        let path = state_path()?;
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut state = Self {
            skipped: None,
            skipped_at: 0,
        };
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("skipped ") {
                state.skipped = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("skipped_at ") {
                state.skipped_at = value.parse().unwrap_or(0);
            }
        }
        Ok(state)
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = state_path()?;
        let skipped = self.skipped.as_deref().unwrap_or("");
        let body = format!("skipped {skipped}\nskipped_at {}\n", self.skipped_at);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, body)?;
        Ok(())
    }
}

fn state_path() -> anyhow::Result<PathBuf> {
    Ok(crate::tls::support_dir()?.join("update.state"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let rest = json.get(json.find(&pattern)? + pattern.len()..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn asset_urls(json: &str) -> Option<(String, String)> {
    let mut archive = None;
    let mut checksum = None;
    let key = "\"browser_download_url\"";
    let mut rest = json;
    while let Some(index) = rest.find(key) {
        rest = rest.get(index + key.len()..)?;
        let value = rest.trim_start().strip_prefix(':')?.trim_start();
        let value = value.strip_prefix('"')?;
        let end = value.find('"')?;
        let url = value[..end].replace("\\/", "/");
        if url.contains("aarch64-apple-darwin.tar.gz.sha256") {
            checksum = Some(url);
        } else if url.contains("aarch64-apple-darwin.tar.gz") {
            archive = Some(url);
        }
        rest = value.get(end..)?;
    }
    Some((archive?, checksum?))
}

fn checksum_hex(text: &str) -> Option<String> {
    let hex = text.split_whitespace().next()?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_owned())
}

/// `tag` is the release (`v1.2.0` or `1.2.0`), `current` is `CARGO_PKG_VERSION`.
fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_release_compares_by_numbers() {
        assert!(is_newer("v1.2.0", "1.0.0"));
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.2.0"));
        assert!(!is_newer("not-a-version", "1.0.0"));
    }

    #[test]
    fn release_json_yields_the_apple_silicon_archive() {
        let json = r#"{"tag_name":"v1.2.0","assets":[
            {"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/v1.2.0/openrdp-v1.2.0-aarch64-apple-darwin.tar.gz.sha256"},
            {"browser_download_url":"https://github.com/org-event/OpenRDP/releases/download/v1.2.0/openrdp-v1.2.0-aarch64-apple-darwin.tar.gz"}
        ]}"#;
        assert_eq!(json_string(json, "tag_name"), Some("v1.2.0"));
        let (archive, checksum) = asset_urls(json).unwrap();
        assert!(archive.ends_with("aarch64-apple-darwin.tar.gz"));
        assert!(checksum.ends_with(".sha256"));
        assert_eq!(
            checksum_hex(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  name\n"
            )
            .as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn update_mode_prefers_the_flag() {
        assert_eq!(mode(true, Some("auto")).unwrap(), Mode::Off);
        assert_eq!(mode(false, Some("auto")).unwrap(), Mode::Auto);
        assert_eq!(mode(false, Some("ask")).unwrap(), Mode::Ask);
        assert!(mode(false, Some("sometimes")).is_err());
    }
}
