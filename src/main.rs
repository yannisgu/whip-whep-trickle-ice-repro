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

use anyhow::{Result, bail};
use gst::prelude::*;
use std::collections::BTreeMap;
use std::time::Duration;
use whip_whep_trickle_ice_repro::{Mode, PLUGIN, client, server};

fn main() -> Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("whip") => Mode::Whip,
        Some("whep") => Mode::Whep,
        _ => bail!("usage: whip-whep-trickle-ice-repro whip|whep"),
    };

    whip_whep_trickle_ice_repro::init()?;

    let server = match mode {
        Mode::Whip => server::whip()?,
        Mode::Whep => server::whep()?,
    };
    server.pipeline.set_state(gst::State::Playing)?;

    let mut client = client::Client::new(mode)?;
    client.pipeline.set_state(gst::State::Playing)?;

    let runtime = tokio::runtime::Runtime::new()?;
    let trickled = runtime.block_on(client.trickle(&server.base))?;
    let mut candidates = BTreeMap::<u32, usize>::new();
    for trickled in server.trickled.lock().unwrap().iter() {
        *candidates.entry(trickled.mline).or_default() += 1;
    }
    let edge_cases = runtime.block_on(async {
        let edge_cases = client.edge_cases(&server.base, &server.trickled).await?;
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
    for case in edge_cases {
        println!(
            "{}: {}, handed to m-line(s) {:?}",
            case.what, case.status, case.mlines
        );
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
