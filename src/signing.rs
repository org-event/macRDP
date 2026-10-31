//! A local code signing identity for OpenRDP.app.
//!
//! macOS ties Screen Recording and Accessibility grants to an app's designated
//! requirement. An ad hoc signature makes that the hash of one build, so every
//! update lost the grants. Signed with a fixed self-signed certificate the
//! requirement becomes "identifier com.openrdp.server and this certificate",
//! which every build meets. No trust setting is needed for that.
//!
//! The certificate lives in its own keychain with a random password kept next
//! to it (mode 600): the login keychain and the user's password stay out of it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use tracing::info;

use crate::service::{run, write_private};

const NAME: &str = "OpenRDP Local Signing";

pub struct Identity {
    keychain: PathBuf,
    password: String,
}

impl Identity {
    /// The identity in `dir`, created on first use.
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let keychain = dir.join("signing.keychain-db");
        let password_file = dir.join("signing.password");
        if keychain.exists() && password_file.exists() {
            let password = std::fs::read_to_string(&password_file)?.trim().to_owned();
            return Ok(Self { keychain, password });
        }
        std::fs::create_dir_all(dir)?;
        let identity = Self {
            keychain,
            password: random_hex()?,
        };
        identity
            .create(dir)
            .context("create the signing identity")?;
        write_private(&password_file, &identity.password)?;
        info!(keychain = %identity.keychain.display(), "created a local signing identity");
        Ok(identity)
    }

    fn create(&self, dir: &Path) -> anyhow::Result<()> {
        let work = tempdir(dir)?;
        let (key, cert, p12) = (
            work.join("key.pem"),
            work.join("cert.pem"),
            work.join("id.p12"),
        );
        let result = (|| {
            run(Command::new("openssl")
                .args([
                    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
                ])
                .args(["-subj", &format!("/CN={NAME}")])
                .args(["-addext", "extendedKeyUsage=critical,codeSigning"])
                .args(["-addext", "keyUsage=critical,digitalSignature"])
                .args(["-addext", "basicConstraints=critical,CA:false"])
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&cert))?;
            let pass = format!("pass:{}", self.password);
            run(Command::new("openssl")
                .args(["pkcs12", "-export", "-name", NAME, "-passout", &pass])
                .arg("-inkey")
                .arg(&key)
                .arg("-in")
                .arg(&cert)
                .arg("-out")
                .arg(&p12))?;
            if self.keychain.exists() {
                std::fs::remove_file(&self.keychain)?;
            }
            // create-keychain leaves the search list alone, so the identity
            // does not show up next to the user's own.
            run(self
                .security(["create-keychain", "-p", &self.password])
                .arg(&self.keychain))?;
            // No auto-lock: the agent's updates sign unattended.
            run(Command::new("security")
                .arg("set-keychain-settings")
                .arg(&self.keychain))?;
            self.unlock()?;
            run(self
                .security(["import"])
                .arg(&p12)
                .args(["-P", &self.password, "-T", "/usr/bin/codesign", "-k"])
                .arg(&self.keychain))?;
            // Lets codesign use the key without a confirmation dialog.
            run(self
                .security([
                    "set-key-partition-list",
                    "-S",
                    "apple-tool:,apple:,codesign:",
                    "-s",
                ])
                .args(["-k", &self.password])
                .arg(&self.keychain))
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    /// Signs `bundle` so it keeps its privacy grants across builds.
    pub fn sign(&self, bundle: &Path, identifier: &str) -> anyhow::Result<()> {
        self.unlock()?;
        run(Command::new("codesign")
            .args([
                "--force",
                "--sign",
                NAME,
                "--identifier",
                identifier,
                "--keychain",
            ])
            .arg(&self.keychain)
            .arg(bundle))
    }

    fn unlock(&self) -> anyhow::Result<()> {
        run(self
            .security(["unlock-keychain", "-p", &self.password])
            .arg(&self.keychain))
    }

    fn security<const N: usize>(&self, args: [&str; N]) -> Command {
        let mut command = Command::new("security");
        command.args(args);
        command
    }
}

fn random_hex() -> anyhow::Result<String> {
    let output = Command::new("openssl")
        .args(["rand", "-hex", "24"])
        .output()
        .context("run openssl rand")?;
    let hex = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(hex.len() == 48, "openssl rand gave no output");
    Ok(hex)
}

/// A private scratch directory for the key material.
fn tempdir(parent: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = parent.join(format!("signing-{}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}
