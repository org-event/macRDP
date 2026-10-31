//! The login window.
//!
//! A LaunchAgent runs only in a logged-in Aqua session, so the port is closed
//! while the login window is up. `com.openrdp.broker` is a LaunchDaemon. It
//! starts at boot as root and never captures the screen or posts events itself.
//! It watches the owner of `/dev/console`: root means the login window. Then it
//! starts this server inside that window's GUI session with `launchctl bsexec`
//! on the `loginwindow` process, and the server drops to the user who ran
//! `install`.
//!
//! Screen Recording and Accessibility are granted per user, to the signed app.
//! Running as that user inside the login window's session makes those grants
//! apply. A process that stayed root would be checked against root's database,
//! which System Settings never offers a switch for.
//!
//! The stream is the physical display. When somebody logs in, the daemon stops
//! this process and the LaunchAgent — launchd starts it with the Aqua session —
//! binds the port. The client connects again. Logging out runs the same path
//! back. The FileVault screen before macOS boots is not a session this daemon
//! can join; RDP starts once the disk is unlocked and the login window is up.
//!
//! The password sits in `/Library/Application Support/openrdp/broker.conf`,
//! mode 600, root-owned. The daemon plist itself has no secret.

use std::ffi::{CString, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::service::{escape, run as run_command, write_private};

const LABEL: &str = "com.openrdp.broker";
const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.openrdp.broker.plist";
const SYSTEM_DIR: &str = "/Library/Application Support/openrdp";
const CONFIG_PATH: &str = "/Library/Application Support/openrdp/broker.conf";
const PID_PATH: &str = "/Library/Application Support/openrdp/login-window.pid";
const BROKER_LOG: &str = "/Library/Logs/openrdp-broker.log";
const FORMAT_VERSION: &str = "openrdp-broker 1";

const SIGTERM: i32 = 15;
const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;

/// Polled often enough that a login wins the port before the agent's first bind retry gives up.
const POLL: Duration = Duration::from_millis(200);
/// A login-window server that dies at once (the port is taken, `loginwindow` is missing) waits
/// this long, so a fast-user-switch does not respawn it in a tight loop.
const RESTART_BACKOFF: Duration = Duration::from_secs(30);
const RESTART_SOON: Duration = Duration::from_secs(2);

static STOP: AtomicBool = AtomicBool::new(false);
/// Pid of the login-window server, so a signal can stop it without taking a lock.
static LOGIN_PID: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" {
    fn getuid() -> u32;
    fn geteuid() -> u32;
    fn getgid() -> u32;
    fn setuid(uid: u32) -> i32;
    fn setgid(gid: u32) -> i32;
    fn initgroups(user: *const i8, group: u32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn dup2(src: i32, dst: i32) -> i32;
    fn signal(sig: i32, handler: unsafe extern "C" fn(i32)) -> usize;
}

unsafe extern "C" fn request_stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
    let pid = LOGIN_PID.load(Ordering::Relaxed);
    if pid > 1 {
        unsafe {
            kill(-pid, SIGTERM);
            kill(pid, SIGTERM);
        }
    }
}

/// What the broker should do for this console owner. Root owns `/dev/console` at the login window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    Serve,
    Stop,
}

fn watch(console_uid: u32) -> Watch {
    if console_uid == 0 {
        Watch::Serve
    } else {
        Watch::Stop
    }
}

struct Config {
    uid: u32,
    gid: u32,
    user: String,
    home: PathBuf,
    binary: PathBuf,
    args: Vec<String>,
    password: String,
}

impl Config {
    fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut text = String::new();
        text.push_str(FORMAT_VERSION);
        text.push('\n');
        push_field(&mut text, "uid", &self.uid.to_string())?;
        push_field(&mut text, "gid", &self.gid.to_string())?;
        push_field(&mut text, "user", &self.user)?;
        push_field(&mut text, "home", path_str(&self.home)?)?;
        push_field(&mut text, "binary", path_str(&self.binary)?)?;
        for arg in &self.args {
            push_field(&mut text, "arg", arg)?;
        }
        let password = self.password.as_bytes();
        text.push_str(&format!("password {}\n", password.len()));
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice(password);
        Ok(bytes)
    }

    fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut offset = 0;
        let mut password_at = None;
        let mut header = Vec::new();
        while offset < bytes.len() {
            let rest = &bytes[offset..];
            let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
                bail!("truncated broker config");
            };
            let line =
                std::str::from_utf8(&rest[..newline]).context("broker config is not utf-8")?;
            offset += newline + 1;
            if let Some(length) = line.strip_prefix("password ") {
                password_at = Some((offset, length.parse::<usize>().context("password length")?));
                break;
            }
            header.push(line.to_owned());
        }
        let Some((start, length)) = password_at else {
            bail!("broker config has no password");
        };
        let rest = &bytes[start..];
        anyhow::ensure!(
            rest.len() == length,
            "password is {} bytes, config says {length}",
            rest.len()
        );
        let password = String::from_utf8(rest.to_vec()).context("password is not utf-8")?;

        let mut lines = header.into_iter();
        let version = lines.next().context("empty broker config")?;
        anyhow::ensure!(
            version == FORMAT_VERSION,
            "unknown broker config: {version}"
        );

        let mut uid = None;
        let mut gid = None;
        let mut user = None;
        let mut home = None;
        let mut binary = None;
        let mut args = Vec::new();
        for line in lines {
            let Some((key, value)) = line.split_once(' ') else {
                bail!("broker config line has no value: {line}");
            };
            match key {
                "uid" => uid = Some(value.parse()?),
                "gid" => gid = Some(value.parse()?),
                "user" => user = Some(value.to_owned()),
                "home" => home = Some(PathBuf::from(value)),
                "binary" => binary = Some(PathBuf::from(value)),
                "arg" => args.push(value.to_owned()),
                other => bail!("unknown broker config field {other}"),
            }
        }
        Ok(Self {
            uid: uid.context("uid missing")?,
            gid: gid.context("gid missing")?,
            user: user.context("user missing")?,
            home: home.context("home missing")?,
            binary: binary.context("binary missing")?,
            args,
            password,
        })
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, self.to_bytes()?)
    }

    fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("parse {}", path.display()))
    }
}

fn push_field(text: &mut String, key: &str, value: &str) -> anyhow::Result<()> {
    if value.contains('\n') {
        bail!("{key} contains a newline");
    }
    text.push_str(key);
    text.push(' ');
    text.push_str(value);
    text.push('\n');
    Ok(())
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().context("path is not utf-8")
}

fn daemon_plist(binary: &Path) -> anyhow::Result<String> {
    let binary = escape(path_str(binary)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>broker</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>{BROKER_LOG}</string>
    <key>StandardErrorPath</key><string>{BROKER_LOG}</string>
</dict>
</plist>
"#
    ))
}

/// Writes the broker config and installs the daemon. Asks for an administrator
/// password. A failure here leaves the per-user agent in place.
pub(crate) fn request_install(
    binary: &Path,
    server_args: &[String],
    password: &str,
) -> anyhow::Result<()> {
    // SAFETY: getuid and getgid have no preconditions.
    let (uid, gid) = unsafe { (getuid(), getgid()) };
    if uid == 0 {
        eprintln!(
            "Skipping the login-window service: run install as the user who logs in at this Mac, not as root."
        );
        return Ok(());
    }
    let config = Config {
        uid,
        gid,
        user: std::env::var("USER").context("USER is not set")?,
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?,
        binary: binary.to_owned(),
        args: server_args.to_vec(),
        password: password.to_owned(),
    };
    let staged = config
        .home
        .join("Library/Application Support/openrdp/broker.staged");
    config.save(&staged)?;
    println!("Installing the login-window service (administrator password).");
    let status = Command::new("/usr/bin/sudo")
        .arg("--")
        .arg(binary)
        .arg("install-system")
        .arg(&staged)
        .status()
        .context("ask for an administrator password")?;
    let _ = std::fs::remove_file(&staged);
    if status.success() {
        println!(
            "The login window is served by {LABEL}. Log out and connect as usual; after you log in, connect again."
        );
    } else {
        eprintln!(
            "The login-window service was not installed. The server still starts after you log in.\n\
             Run this install again in a terminal and enter the administrator password when asked."
        );
    }
    Ok(())
}

/// Removes the daemon. Quiet when it was never installed.
pub(crate) fn request_uninstall(binary: &Path) -> anyhow::Result<()> {
    if !Path::new(DAEMON_PLIST).exists() {
        return Ok(());
    }
    println!("Removing the login-window service (administrator password).");
    let status = Command::new("/usr/bin/sudo")
        .arg("--")
        .arg(binary)
        .arg("uninstall-system")
        .status()
        .context("ask for an administrator password")?;
    if !status.success() {
        eprintln!(
            "The login-window service is still installed. Remove it with:\n  sudo {} uninstall-system",
            binary.display()
        );
    }
    Ok(())
}

/// Copies the staged config into place and bootstraps the daemon. Root only.
pub(crate) fn install_system(staged: &Path) -> anyhow::Result<()> {
    require_root("install-system")?;
    let config = Config::load(staged)?;
    anyhow::ensure!(
        config.uid != 0,
        "the login-window server cannot run as root"
    );
    anyhow::ensure!(
        config.binary.is_file(),
        "missing {}",
        config.binary.display()
    );
    std::fs::create_dir_all(SYSTEM_DIR).with_context(|| format!("create {SYSTEM_DIR}"))?;
    config.save(Path::new(CONFIG_PATH))?;
    let _ = std::fs::remove_file(staged);

    let plist = daemon_plist(&config.binary)?;
    std::fs::write(DAEMON_PLIST, &plist).with_context(|| format!("write {DAEMON_PLIST}"))?;
    std::fs::set_permissions(DAEMON_PLIST, std::fs::Permissions::from_mode(0o644))?;

    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    run_command(Command::new("/bin/launchctl").args(["enable", &format!("system/{LABEL}")]))
        .context("enable the login-window service")?;
    run_command(Command::new("/bin/launchctl").args(["bootstrap", "system", DAEMON_PLIST]))
        .context("start the login-window service")?;
    println!("Installed {DAEMON_PLIST}.");
    Ok(())
}

/// Stops the daemon and deletes its config. Root only.
pub(crate) fn uninstall_system() -> anyhow::Result<()> {
    require_root("uninstall-system")?;
    stop_login_server();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    for path in [DAEMON_PLIST, CONFIG_PATH, PID_PATH] {
        if Path::new(path).exists() {
            std::fs::remove_file(path).with_context(|| format!("remove {path}"))?;
        }
    }
    println!("Removed the login-window service.");
    Ok(())
}

fn require_root(command: &str) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { geteuid() } == 0 {
        Ok(())
    } else {
        bail!("{command} must run as root")
    }
}

/// Broker process: root, for the life of the boot. Launchd restarts it.
pub(crate) fn run() -> anyhow::Result<()> {
    require_root("broker")?;
    let config = Config::load(Path::new(CONFIG_PATH))?;
    // SAFETY: the handler only stores a flag and signals the login-window pid.
    unsafe {
        signal(SIGTERM, request_stop);
        signal(SIGINT, request_stop);
    }
    tracing::info!(user = %config.user, "watching the console");

    let mut child: Option<Child> = None;
    let mut started = Instant::now();
    let mut next_spawn = Instant::now();
    let mut missing_window = false;
    let mut last_uid = None;

    while !STOP.load(Ordering::Relaxed) {
        if let Some(running) = child.as_mut()
            && let Some(status) = running.try_wait().context("wait for launchctl")?
        {
            let ran = started.elapsed();
            child = None;
            if !status.success() {
                tracing::warn!(?status, "login-window server exited");
            }
            next_spawn = Instant::now()
                + if ran < RESTART_BACKOFF {
                    RESTART_BACKOFF
                } else {
                    RESTART_SOON
                };
        }

        let uid = console_uid();
        if last_uid != uid {
            tracing::info!(?uid, "console user");
            last_uid = uid;
        }
        match uid.map(watch) {
            Some(Watch::Serve) => {
                note_login_pid();
                if !login_server_running() && child.is_none() && Instant::now() >= next_spawn {
                    match spawn_login_server(&config) {
                        Ok(running) => {
                            missing_window = false;
                            started = Instant::now();
                            next_spawn = started + RESTART_SOON;
                            child = Some(running);
                        }
                        Err(err) => {
                            if !missing_window {
                                tracing::info!("{err:#}");
                                missing_window = true;
                            }
                            next_spawn = Instant::now() + RESTART_SOON;
                        }
                    }
                }
            }
            Some(Watch::Stop) => {
                if child.is_some() || login_server_running() {
                    tracing::info!("a user is at the console; stopping the login-window server");
                }
                if let Some(mut running) = child.take() {
                    let _ = running.kill();
                    let _ = running.wait();
                }
                stop_login_server();
                missing_window = false;
            }
            // A failed stat must not tear the login-window server down.
            None => {}
        }

        for _ in 0..5 {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            sleep(POLL);
        }
    }

    if let Some(mut running) = child.take() {
        let _ = running.kill();
        let _ = running.wait();
    }
    stop_login_server();
    tracing::info!("broker stopped");
    Ok(())
}

fn console_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/dev/console")
        .ok()
        .map(|meta| meta.uid())
}

fn spawn_login_server(config: &Config) -> anyhow::Result<Child> {
    let pid = loginwindow_pid()?;
    tracing::info!(pid, "starting the server at the login window");
    let _ = std::fs::remove_file(PID_PATH);
    Command::new("/bin/launchctl")
        .args(["bsexec", &pid.to_string()])
        .arg(&config.binary)
        .arg("login-window")
        .stdin(Stdio::null())
        .spawn()
        .context("launchctl bsexec")
}

fn loginwindow_pid() -> anyhow::Result<u32> {
    let output = Command::new("/usr/bin/pgrep")
        .args(["-x", "loginwindow"])
        .output()
        .context("look up loginwindow")?;
    let text = String::from_utf8(output.stdout).context("loginwindow pid")?;
    text.split_whitespace()
        .filter_map(|line| line.parse().ok())
        .min()
        .context("loginwindow is not running yet")
}

/// Enter the login window's session as the installing user and return the server arguments.
///
/// Called before any other thread exists. `bsexec` has already joined `loginwindow`'s
/// bootstrap namespace; dropping uid afterwards keeps that namespace and makes TCC
/// check the installing user's grants.
pub(crate) fn prepare_login_window() -> anyhow::Result<Vec<OsString>> {
    require_root("login-window")?;
    let config = Config::load(Path::new(CONFIG_PATH))?;
    // Our own process group, so the broker can signal the server without signaling launchctl's group.
    unsafe { setpgid(0, 0) };
    write_private(Path::new(PID_PATH), std::process::id().to_string())?;

    let home = path_str(&config.home)?.to_owned();
    let user = config.user.clone();
    let password = config.password.clone();
    // SAFETY: this runs at process start, before the runtime or any other thread.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USER", &user);
        std::env::set_var("LOGNAME", &user);
        std::env::set_var("OPENRDP_PASSWORD", &password);
        std::env::set_var("OPENRDP_LOGIN_WINDOW", "1");
    }
    drop_privileges(config.uid, config.gid, &user)?;
    if let Err(err) = std::env::set_current_dir(&home) {
        eprintln!("cannot enter {home}: {err}");
    }
    redirect_log(Path::new(&home));
    Ok(config.args.into_iter().map(OsString::from).collect())
}

pub(crate) fn login_window_requested() -> bool {
    std::env::var_os("OPENRDP_LOGIN_WINDOW").is_some_and(|value| value == "1")
}

fn drop_privileges(uid: u32, gid: u32, user: &str) -> anyhow::Result<()> {
    let name = CString::new(user).context("user name")?;
    // SAFETY: `name` is a valid C string; the ids come from the config written at install.
    let failed = unsafe {
        if initgroups(name.as_ptr(), gid) != 0 {
            Some("initgroups")
        } else if setgid(gid) != 0 {
            Some("setgid")
        } else if setuid(uid) != 0 {
            Some("setuid")
        } else {
            None
        }
    };
    if let Some(call) = failed {
        bail!("{call}: {}", std::io::Error::last_os_error());
    }
    // SAFETY: geteuid has no preconditions. setuid(0) must fail once the privileges are gone.
    unsafe {
        anyhow::ensure!(geteuid() == uid, "still running as uid {}", geteuid());
        anyhow::ensure!(setuid(0) != 0, "dropped privileges did not stick");
    }
    Ok(())
}

fn redirect_log(home: &Path) {
    let dir = home.join("Library/Logs");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("openrdp.log"))
    else {
        return;
    };
    let fd = file.as_raw_fd();
    unsafe {
        dup2(fd, 1);
        dup2(fd, 2);
    }
}

fn note_login_pid() {
    let pid = read_pid().filter(|pid| is_login_server(*pid)).unwrap_or(0);
    LOGIN_PID.store(pid, Ordering::Relaxed);
}

fn login_server_running() -> bool {
    read_pid().is_some_and(is_login_server)
}

fn read_pid() -> Option<i32> {
    let text = std::fs::read_to_string(PID_PATH).ok()?;
    let pid = text.trim().parse().ok()?;
    (pid > 1).then_some(pid)
}

fn is_login_server(pid: i32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).contains("login-window")
}

fn stop_login_server() {
    let Some(pid) = read_pid() else {
        LOGIN_PID.store(0, Ordering::Relaxed);
        return;
    };
    if !is_login_server(pid) {
        LOGIN_PID.store(0, Ordering::Relaxed);
        let _ = std::fs::remove_file(PID_PATH);
        return;
    }
    LOGIN_PID.store(pid, Ordering::Relaxed);
    unsafe {
        kill(-pid, SIGTERM);
        kill(pid, SIGTERM);
    }
    for _ in 0..20 {
        if !is_login_server(pid) {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    if is_login_server(pid) {
        unsafe {
            kill(-pid, SIGKILL);
            kill(pid, SIGKILL);
        }
    }
    LOGIN_PID.store(0, Ordering::Relaxed);
    let _ = std::fs::remove_file(PID_PATH);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            uid: 501,
            gid: 20,
            user: "me".into(),
            home: PathBuf::from("/Users/me"),
            binary: PathBuf::from("/Users/me/Applications/OpenRDP.app/Contents/MacOS/openrdp"),
            args: vec![
                "--user".into(),
                "rdp<user>".into(),
                "--bind".into(),
                "0.0.0.0:3389".into(),
            ],
            password: "p<&>\nsecret".into(),
        }
    }

    #[test]
    fn config_roundtrip_keeps_a_password_with_a_newline() {
        let config = sample();
        let parsed = Config::parse(&config.to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.uid, 501);
        assert_eq!(parsed.gid, 20);
        assert_eq!(parsed.user, "me");
        assert_eq!(parsed.home, PathBuf::from("/Users/me"));
        assert_eq!(parsed.args, config.args);
        assert_eq!(parsed.password, "p<&>\nsecret");
    }

    #[test]
    fn config_rejects_a_newline_in_an_argument() {
        let mut config = sample();
        config.args.push("bad\narg".into());
        assert!(config.to_bytes().is_err());
    }

    #[test]
    fn config_rejects_a_short_password() {
        let mut bytes = sample().to_bytes().unwrap();
        bytes.pop();
        assert!(Config::parse(&bytes).is_err());
    }

    #[test]
    fn login_window_is_console_root() {
        assert_eq!(watch(0), Watch::Serve);
        assert_eq!(watch(501), Watch::Stop);
    }

    #[test]
    fn daemon_plist_has_no_secret() {
        let plist =
            daemon_plist(Path::new("/Users/a&b/OpenRDP.app/Contents/MacOS/openrdp")).unwrap();
        assert!(plist.contains("<string>broker</string>"));
        assert!(plist.contains("com.openrdp.broker"));
        assert!(plist.contains("/Users/a&amp;b/OpenRDP.app/Contents/MacOS/openrdp"));
        assert!(!plist.contains("password"));
        assert!(!plist.contains("secret"));
    }
}
