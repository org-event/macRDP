//! `macrdp install` / `macrdp uninstall`: run the server at login.
//!
//! The binary is copied into `~/Applications/macRDP.app` so macOS grants Screen
//! Recording and Accessibility to "macRDP" itself rather than to a terminal, and
//! a LaunchAgent starts it in the user's session and restarts it if it exits.
//! The password goes into the agent's plist, readable only by the user.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

const LABEL: &str = "com.macrdp.server";

struct Paths {
    app: PathBuf,
    binary: PathBuf,
    plist: PathBuf,
    log: PathBuf,
}

fn paths() -> anyhow::Result<Paths> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let app = home.join("Applications/macRDP.app");
    Ok(Paths {
        binary: app.join("Contents/MacOS/macrdp"),
        app,
        plist: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
        log: home.join("Library/Logs/macrdp.log"),
    })
}

/// Installs and starts the agent with `server_args` (already validated); the
/// password comes separately so it stays out of the process list.
pub fn install(server_args: &[String], password: &str) -> anyhow::Result<()> {
    let paths = paths()?;
    stop();

    let macos = paths.binary.parent().expect("inside the bundle");
    std::fs::create_dir_all(macos).with_context(|| format!("create {}", macos.display()))?;
    let exe = std::env::current_exe().context("locate this binary")?;
    // Replace rather than overwrite in place: the old file may be mapped by a
    // process that is still exiting.
    let staged = paths.binary.with_extension("new");
    std::fs::copy(&exe, &staged).with_context(|| format!("copy to {}", staged.display()))?;
    std::fs::rename(&staged, &paths.binary)?;
    std::fs::write(paths.app.join("Contents/Info.plist"), info_plist())?;
    run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&paths.app))
        .context("sign macRDP.app")?;

    let mut program = vec![paths.binary.display().to_string()];
    program.extend(server_args.iter().cloned());
    write_private(&paths.plist, &agent_plist(&program, password, &paths.log))?;
    run(Command::new("launchctl").args(["bootstrap", &domain()]).arg(&paths.plist))
        .context("start the agent")?;

    println!("Installed {} and started it at login.", paths.app.display());
    println!("Log: {}", paths.log.display());
    println!(
        "On first start macOS asks for Screen Recording and Accessibility for \"macRDP\": \
         allow both at the Mac (System Settings cannot be changed remotely), then the agent \
         restarts on its own within a few seconds."
    );
    Ok(())
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
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc_getuid() })
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().with_context(|| format!("run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn write_private(path: &Path, data: &str) -> anyhow::Result<()> {
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

fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>{LABEL}</string>
    <key>CFBundleName</key><string>macRDP</string>
    <key>CFBundleExecutable</key><string>macrdp</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION")
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
        <key>MACRDP_PASSWORD</key><string>{password}</string>
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
            &["/a/macrdp".into(), "--user".into(), "ivan".into()],
            "p<&>",
            Path::new("/tmp/log"),
        );
        assert!(plist.contains("<string>p&lt;&amp;&gt;</string>"));
        assert!(plist.contains("<string>--user</string>"));
    }
}
