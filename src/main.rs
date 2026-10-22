//! Trickle ICE (RFC 8840 / RFC 9725 section 4.3.2) against gst-plugins-rs'
//! `whipserversrc` and `whepserversink`.
//!
//! Server (`server.rs`) and client (`client.rs`) run in the same process.
//! The client sends its offer right away, before it has gathered any
//! candidate, and PATCHes each candidate to the session URL once it has it,
//! the way browsers do. The program prints the status of each PATCH and
//! every `handle-ice` the server's signaller emits, and counts the candidates
//! the signaller hands to webrtcbin per m-line.
//!
//! cargo run --features upstream -- whip|whep
//! cargo run --features fork -- whip|whep

#[cfg(all(feature = "upstream", feature = "fork"))]
compile_error!("enable only one of the features `upstream` and `fork`");
#[cfg(not(any(feature = "upstream", feature = "fork")))]
compile_error!("enable one of the features `upstream` and `fork`");

use anyhow::{Result, bail};
use gst::prelude::*;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

macro_rules! log {
    ($who:expr, $($arg:tt)*) => {
        println!("{:>6.3}s [{}] {}", $crate::START.elapsed().as_secs_f64(), $who, format!($($arg)*))
    };
}

mod client;
mod server;

const PLUGIN: &str = if cfg!(feature = "upstream") {
    "upstream main (9cb42da34)"
} else {
    "fork webrtc-whip-whep-trickle-ice (f9c7317b5)"
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Whip,
    Whep,
}

fn main() -> Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("whip") => Mode::Whip,
        Some("whep") => Mode::Whep,
        _ => bail!("usage: whip-whep-trickle-ice-repro whip|whep"),
    };

    gst::init()?;
    #[cfg(feature = "upstream")]
    gst_plugin_webrtc_upstream::plugin_register_static()?;
    #[cfg(feature = "fork")]
    gst_plugin_webrtc_fork::plugin_register_static()?;
    LazyLock::force(&START);

    println!("gst-plugin-webrtc: {PLUGIN}, {}", gst::version_string());

    let server = match mode {
        Mode::Whip => server::whip()?,
        Mode::Whep => server::whep()?,
    };
    server.pipeline.set_state(gst::State::Playing)?;

    let mut client = client::Client::new(mode)?;
    client.pipeline.set_state(gst::State::Playing)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let trickled = runtime.block_on(client.trickle(&server.base))?;
    let candidates = server.candidates.lock().unwrap().clone();
    let edge_cases = runtime.block_on(async {
        let edge_cases = client.edge_cases(&server.base).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        anyhow::Ok(edge_cases)
    })?;
    // The signallers block on a runtime of their own when stopping, so this
    // has to happen outside of ours
    drop(runtime);

    println!();
    println!(
        "=== {} with {PLUGIN}",
        if mode == Mode::Whip { "WHIP" } else { "WHEP" }
    );
    let ok = trickled.iter().filter(|s| s.is_success()).count();
    println!("trickled candidates accepted: {ok}/{}", trickled.len());
    println!("candidates handed to the server's webrtcbin, per m-line: {candidates:?}");
    for (what, status) in edge_cases {
        println!("{what}: {status}");
    }
    println!(
        "client ice-connection-state: {:?}",
        client
            .webrtcbin
            .property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state")
    );
    if let Some(webrtcbin) = server.webrtcbin.lock().unwrap().as_ref() {
        println!(
            "server ice-connection-state: {:?}",
            webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state")
        );
    }

    client.pipeline.set_state(gst::State::Null)?;
    server.pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn watch_ice_state(who: &'static str, webrtcbin: &gst::Element) {
    webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _| {
        let state =
            webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        log!(who, "ice-connection-state {state:?}");
    });
}
