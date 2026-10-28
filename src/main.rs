//! macRDP: an RDP server for macOS built on IronRDP.

mod capture;
mod display;
mod encoder;
mod input;
mod tls;
mod virtual_display;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ironrdp_server::{Credentials, RdpServer};
use objc2_core_graphics::CGMainDisplayID;
use tracing::{info, warn};

use crate::display::{DisplayConfig, GfxFactory, MacDisplay, Screen, ScreenState, Shared};
use crate::input::MacInput;

const HELP: &str = "\
macrdp — RDP server for macOS

USAGE:
  macrdp --user <NAME> --pass <PASSWORD> [OPTIONS]

OPTIONS:
  --bind <ADDR>       listen address [default: 0.0.0.0:3389]
  --sec <hybrid|tls>  hybrid = NLA (CredSSP), tls = TLS only [default: hybrid]
  --fps <N>           capture frame rate cap [default: 60]
  --bitrate <MBPS>    average H.264 bitrate [default: 20]
  --fixed-size        stream the main display as it is instead of a virtual
                      display sized to the client
  --hidpi             with --fixed-size: stream in pixels instead of points
  --test-pattern      send a synthetic picture instead of the screen
  --cert <PEM> --key <PEM>
                      TLS identity [default: self-signed, kept in
                      ~/Library/Application Support/macrdp]

Password may also come from MACRDP_PASSWORD. Logs: RUST_LOG=macrdp=debug.
";

struct Args {
    bind: SocketAddr,
    hybrid: bool,
    user: String,
    pass: String,
    fps: i32,
    bitrate_mbps: u32,
    fixed_size: bool,
    hidpi: bool,
    test_pattern: bool,
    cert: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        return Ok(None);
    }
    let sec: String = args
        .opt_value_from_str("--sec")?
        .unwrap_or_else(|| "hybrid".into());
    let hybrid = match sec.as_str() {
        "hybrid" => true,
        "tls" => false,
        other => bail!("unknown --sec {other}"),
    };
    let pass = match args.opt_value_from_str("--pass")? {
        Some(pass) => pass,
        None => {
            std::env::var("MACRDP_PASSWORD").context("--pass or MACRDP_PASSWORD is required")?
        }
    };
    let parsed = Args {
        bind: args
            .opt_value_from_str("--bind")?
            .unwrap_or_else(|| "0.0.0.0:3389".parse().expect("valid address")),
        hybrid,
        user: args
            .value_from_str("--user")
            .context("--user is required")?,
        pass,
        fps: args.opt_value_from_str("--fps")?.unwrap_or(60),
        bitrate_mbps: args.opt_value_from_str("--bitrate")?.unwrap_or(20),
        fixed_size: args.contains("--fixed-size"),
        hidpi: args.contains("--hidpi"),
        test_pattern: args.contains("--test-pattern"),
        cert: args.opt_value_from_str("--cert")?,
        key: args.opt_value_from_str("--key")?,
    };
    let rest = args.finish();
    if !rest.is_empty() {
        bail!("unexpected arguments: {rest:?}");
    }
    Ok(Some(parsed))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "macrdp=info,warn".into()),
        )
        .init();

    let Some(args) = parse_args().inspect_err(|_| eprintln!("{HELP}"))? else {
        print!("{HELP}");
        return Ok(());
    };

    if !args.test_pattern {
        check_permissions();
    }

    let screen = Screen::of_display(CGMainDisplayID(), args.hidpi)?;
    let config = DisplayConfig {
        fps: args.fps,
        bitrate: args.bitrate_mbps.saturating_mul(1_000_000),
        hidpi: args.hidpi,
        match_client: !args.fixed_size,
        test_pattern: args.test_pattern,
    };
    info!(?screen, match_client = config.match_client, "display");

    let identity = tls::identity(args.cert.as_deref(), args.key.as_deref())?;
    let acceptor = identity.make_acceptor().context("TLS acceptor")?;

    let screen = Arc::new(ScreenState::new(screen));
    let shared = Arc::new(Shared::new(Arc::clone(&screen)));
    let builder = RdpServer::builder().with_addr(args.bind);
    let builder = if args.hybrid {
        builder.with_hybrid(acceptor, identity.pub_key.clone())
    } else {
        builder.with_tls(acceptor)
    };
    let mut server = builder
        .with_input_handler(MacInput::new(screen))
        .with_display_handler(MacDisplay::new(config, Arc::clone(&shared)))
        .with_gfx_factory(Some(Box::new(GfxFactory::new(shared))))
        .build();
    server.set_credentials(Some(Credentials {
        username: args.user,
        password: args.pass,
        domain: None,
    }));

    info!(bind = %args.bind, hybrid = args.hybrid, "listening");
    server.run().await.context("server")
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: Option<&objc2_core_foundation::CFDictionary>)
    -> bool;
}

fn check_permissions() {
    if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        warn!("Screen Recording permission missing; requesting it (restart after granting)");
        objc2_core_graphics::CGRequestScreenCaptureAccess();
    }
    // Asking with the prompt option also adds the app that launched us to the
    // Accessibility list, so the user only has to switch it on.
    let options = objc2_core_foundation::CFDictionary::<
        objc2_core_foundation::CFString,
        objc2_core_foundation::CFBoolean,
    >::from_slices(
        &[&*objc2_core_foundation::CFString::from_static_str(
            "AXTrustedCheckOptionPrompt",
        )],
        &[objc2_core_foundation::CFBoolean::new(true)],
    );
    // SAFETY: the dictionary holds the documented option key with a CFBoolean.
    if !unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) } {
        warn!(
            "Accessibility permission missing; requested it (restart after granting): remote keyboard and mouse will be ignored"
        );
    }
}
