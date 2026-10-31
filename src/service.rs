//! `openrdp install` / `openrdp uninstall`: run the server at login.
//!
//! The binary is copied into `~/Applications/OpenRDP.app` so macOS grants Screen
//! Recording and Accessibility to "OpenRDP" itself rather than to a terminal, and
//! a LaunchAgent starts it in the user's session and restarts it if it exits.
//! The password goes into the agent's plist, readable only by the user.
//!
//! A previous install under the old names is deleted here: the agent, the app,
//! the plist, the log and `~/Library/Application Support/macrdp`. Nothing is
//! carried over. The new app is a different identity, so macOS asks for the
//! permissions again.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

const LABEL: &str = "com.openrdp.server";
const OLD_LABEL: &str = "com.macrdp.server";
const BUNDLE_NAME: &str = "OpenRDP";
const EXE_NAME: &str = "openrdp";
/// Shown in Finder and in the Privacy & Security lists. Rebuilt from
/// `AppIcon.svg` by `scripts/make-icon.swift`.
const ICON: &[u8] = include_bytes!("../assets/icon/AppIcon.icns");

struct Paths {
    app: PathBuf,
    /// Keeps the TLS identity and the signing keychain.
    support: PathBuf,
    binary: PathBuf,
    plist: PathBuf,
    log: PathBuf,
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn paths() -> anyhow::Result<Paths> {
    let home = home_dir()?;
    let app = home.join("Applications/OpenRDP.app");
    Ok(Paths {
        binary: app.join(format!("Contents/MacOS/{EXE_NAME}")),
        app,
        support: crate::tls::support_dir()?,
        plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
        log: home.join("Library/Logs/openrdp.log"),
    })
}

/// Installs and starts the agent with `server_args` (already validated); the
/// password comes separately so it stays out of the process list.
pub fn install(server_args: &[String], password: &str) -> anyhow::Result<()> {
    let paths = paths()?;
    let removed = bootout(OLD_LABEL);
    let removed = removed || remove_legacy(&home_dir()?)?;
    stop();

    let exe = std::env::current_exe().context("locate this binary")?;
    stage_binary(&paths.binary, &exe)?;
    std::fs::write(
        paths.app.join("Contents/Info.plist"),
        info_plist(env!("CARGO_PKG_VERSION")),
    )?;
    let resources = paths.app.join("Contents/Resources");
    std::fs::create_dir_all(&resources)?;
    std::fs::write(resources.join("AppIcon.icns"), ICON)?;
    sign(&paths)?;

    let mut program = vec![paths.binary.display().to_string()];
    program.extend(server_args.iter().cloned());
    write_private(&paths.plist, &agent_plist(&program, password, &paths.log))?;
    run(Command::new("launchctl")
        .args(["bootstrap", &domain()])
        .arg(&paths.plist))
    .context("start the agent")?;

    println!("Installed {} and started it at login.", paths.app.display());
    println!("Log: {}", paths.log.display());
    if removed {
        println!(
            "Removed the previous install. macOS treats \"{BUNDLE_NAME}\" as a new app, so \
             Screen Recording and Accessibility have to be granted again at the Mac (System \
             Settings cannot be changed remotely). Then restart it:\n  \
             launchctl kickstart -k {}/{LABEL}",
            domain()
        );
    } else {
        println!(
            "On the first install macOS asks for Screen Recording and Accessibility for \
             \"{BUNDLE_NAME}\": allow both at the Mac (System Settings cannot be changed \
             remotely), then restart it:\n  launchctl kickstart -k {}/{LABEL}\n\
             Later installs keep the grants.",
            domain()
        );
    }
    Ok(())
}

/// Replaces the installed binary with `new_binary`, signs it again and restarts
/// the agent. The agent's arguments and password stay as they are.
pub fn apply_update(new_binary: &Path, version: &str) -> anyhow::Result<()> {
    let paths = paths()?;
    if !paths.app.is_dir() {
        bail!("{} is not installed", paths.app.display());
    }
    stage_binary(&paths.binary, new_binary)?;
    std::fs::write(paths.app.join("Contents/Info.plist"), info_plist(version))?;
    sign(&paths)?;
    run(Command::new("launchctl").args(["kickstart", "-k", &format!("{}/{LABEL}", domain())]))
        .context("restart the agent")
}

/// Whether this process is the installed app, rather than a copy run by hand.
pub fn running_from_app() -> bool {
    let Ok(paths) = paths() else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    same_file(&exe, &paths.binary)
}

/// Replace rather than overwrite in place: the old file may be mapped by a
/// process that is still exiting.
fn stage_binary(dest: &Path, source: &Path) -> anyhow::Result<()> {
    let macos = dest.parent().context("bundle executable path")?;
    std::fs::create_dir_all(macos).with_context(|| format!("create {}", macos.display()))?;
    let staged = dest.with_extension("new");
    std::fs::copy(source, &staged).with_context(|| format!("copy to {}", staged.display()))?;
    std::fs::rename(&staged, dest)?;
    let _ = Command::new("xattr")
        .args(["-d", "com.apple.quarantine"])
        .arg(dest)
        .status();
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Signs with the local identity, so grants carry over to the next build; ad
/// hoc when that cannot be set up, which costs the grants on every update.
fn sign(paths: &Paths) -> anyhow::Result<()> {
    match crate::signing::Identity::load_or_create(&paths.support)
        .and_then(|identity| identity.sign(&paths.app, LABEL))
    {
        Ok(()) => Ok(()),
        Err(err) => {
            eprintln!(
                "warning: local signing failed ({err:#}); signing ad hoc, so macOS will ask \
                 for permissions again after every update"
            );
            run(Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&paths.app))
            .context("sign OpenRDP.app")
        }
    }
}

pub fn uninstall() -> anyhow::Result<()> {
    let paths = paths()?;
    stop();
    if paths.plist.exists() {
        std::fs::remove_file(&paths.plist)
            .with_context(|| format!("remove {}", paths.plist.display()))?;
    }
    if paths.app.exists() {
        std::fs::remove_dir_all(&paths.app)
            .with_context(|| format!("remove {}", paths.app.display()))?;
    }
    println!("Removed the agent and {}.", paths.app.display());
    Ok(())
}

/// Stops the agent if it is loaded; quiet when it is not.
fn stop() {
    let _ = bootout(LABEL);
}

/// Boots the agent out. `true` when it was loaded.
fn bootout(label: &str) -> bool {
    Command::new("launchctl")
        .args(["bootout", &format!("{}/{label}", domain())])
        .status()
        .is_ok_and(|status| status.success())
}

/// Deletes the previous install outright: app, agent plist, log and support
/// directory. `true` when any of it was there.
fn remove_legacy(home: &Path) -> anyhow::Result<bool> {
    let old_app = home.join("Applications/macRDP.app");
    let old_plist = home.join(format!("Library/LaunchAgents/{OLD_LABEL}.plist"));
    let old_support = home.join("Library/Application Support/macrdp");
    let old_log = home.join("Library/Logs/macrdp.log");
    let present =
        old_app.exists() || old_plist.exists() || old_support.exists() || old_log.exists();
    for path in [&old_plist, &old_log] {
        if path.exists() {
            std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    for path in [&old_app, &old_support] {
        if path.exists() {
            std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(present)
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc_getuid() })
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

pub(crate) fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command
        .output()
        .with_context(|| format!("run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn write_private(path: &Path, data: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    // An existing file keeps its mode on open; tighten it either way.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(data.as_bytes())?;
    Ok(())
}

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>{LABEL}</string>
    <key>CFBundleName</key><string>{BUNDLE_NAME}</string>
    <key>CFBundleExecutable</key><string>{EXE_NAME}</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
"#
    )
}

fn agent_plist(program: &[String], password: &str, log: &Path) -> String {
    let arguments: String = program
        .iter()
        .map(|arg| format!("        <string>{}</string>\n", escape(arg)))
        .collect();
    let log = escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>OPENRDP_PASSWORD</key><string>{password}</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>ProcessType</key><string>Interactive</string>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        password = escape(password)
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_plist_escapes() {
        let plist = agent_plist(
            &["/a/openrdp".into(), "--user".into(), "ivan".into()],
            "p<&>",
            Path::new("/tmp/log"),
        );
        assert!(plist.contains("<string>p&lt;&amp;&gt;</string>"));
        assert!(plist.contains("<string>--user</string>"));
        assert!(plist.contains("<key>OPENRDP_PASSWORD</key>"));
        assert!(plist.contains("<string>com.openrdp.server</string>"));
    }

    #[test]
    fn remove_legacy_deletes_the_previous_install() {
        let home = std::env::temp_dir().join(format!("openrdp-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let old_bin = home.join("Applications/macRDP.app/Contents/MacOS");
        std::fs::create_dir_all(&old_bin).unwrap();
        std::fs::write(old_bin.join("macrdp"), b"old").unwrap();
        let old_plist = home.join("Library/LaunchAgents/com.macrdp.server.plist");
        std::fs::create_dir_all(old_plist.parent().unwrap()).unwrap();
        std::fs::write(&old_plist, b"plist").unwrap();
        let old_support = home.join("Library/Application Support/macrdp");
        std::fs::create_dir_all(&old_support).unwrap();
        std::fs::write(old_support.join("cert.pem"), b"cert").unwrap();
        let old_log = home.join("Library/Logs/macrdp.log");
        std::fs::create_dir_all(old_log.parent().unwrap()).unwrap();
        std::fs::write(&old_log, b"log").unwrap();

        assert!(remove_legacy(&home).unwrap());
        assert!(!home.join("Applications/macRDP.app").exists());
        assert!(!old_plist.exists());
        assert!(!old_support.exists());
        assert!(!old_log.exists());
        assert!(!remove_legacy(&home).unwrap());

        let _ = std::fs::remove_dir_all(&home);
    }
}
