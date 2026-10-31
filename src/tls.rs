//! TLS identity: the given certificate, or a self-signed one generated once and kept.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use ironrdp_server::TlsIdentityCtx;
use tracing::info;

pub fn identity(cert: Option<&Path>, key: Option<&Path>) -> anyhow::Result<TlsIdentityCtx> {
    let (cert, key) = match (cert, key) {
        (Some(cert), Some(key)) => (cert.to_owned(), key.to_owned()),
        (None, None) => self_signed()?,
        _ => bail!("--cert and --key go together"),
    };
    TlsIdentityCtx::init_from_paths(&cert, &key)
        .with_context(|| format!("load TLS identity {}", cert.display()))
}

/// `~/Library/Application Support/openrdp`: the TLS identity and the signing keychain.
pub fn support_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/openrdp"))
}

fn self_signed() -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = support_dir()?;
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if cert.exists() && key.exists() {
        return Ok((cert, key));
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let host = hostname();
    // RSA: Windows App drops the TLS handshake on an ECDSA certificate.
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)?;
    let generated = rcgen::CertificateParams::new(vec![host.clone(), "localhost".into()])?
        .self_signed(&key_pair)?;
    std::fs::write(&cert, generated.pem())?;
    write_private(&key, key_pair.serialize_pem().as_bytes())?;
    info!(path = %cert.display(), %host, "generated self-signed TLS certificate");
    Ok((cert, key))
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(data)
}

fn hostname() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| format!("{}.local", s.trim()))
        .filter(|s| s.len() > ".local".len())
        .unwrap_or_else(|| "openrdp".into())
}
