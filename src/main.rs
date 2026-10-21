//! Trickle ICE (RFC 8840 / RFC 9725 section 4.3.2) against gst-plugins-rs'
//! `whipserversrc` and `whepserversink`.
//!
//! The client is a plain webrtcbin in the same process. It sends its offer
//! right away, before it has gathered any candidate, and PATCHes each
//! candidate to the session URL once it has it, the way browsers do. The
//! program prints the status of each PATCH, every `handle-ice` the server's
//! signaller emits, and which of the client's candidates the server's
//! webrtcbin knows in the end.
//!
//! cargo run --features upstream -- whip|whep
//! cargo run --features fork -- whip|whep

#[cfg(all(feature = "upstream", feature = "fork"))]
compile_error!("enable only one of the features `upstream` and `fork`");
#[cfg(not(any(feature = "upstream", feature = "fork")))]
compile_error!("enable one of the features `upstream` and `fork`");

use anyhow::{Context, Result, anyhow, bail};
use gst::glib;
use gst::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

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

enum Event {
    Offer(gst_sdp::SDPMessage),
    Candidate(u32, String),
}

static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

macro_rules! log {
    ($who:literal, $($arg:tt)*) => {
        println!("{:>6.3}s [{}] {}", START.elapsed().as_secs_f64(), $who, format!($($arg)*))
    };
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
    let _ = *START;

    println!("gst-plugin-webrtc: {PLUGIN}, {}", gst::version_string());

    // The signallers block on their own runtime when stopping, so tear down
    // outside of ours
    let (server, client) = tokio::runtime::Runtime::new()?.block_on(run(mode))?;
    client.set_state(gst::State::Null)?;
    server.set_state(gst::State::Null)?;
    Ok(())
}

async fn run(mode: Mode) -> Result<(gst::Pipeline, gst::Pipeline)> {
    let (server, server_webrtcbin, base) = match mode {
        Mode::Whip => whip_server()?,
        Mode::Whep => whep_server()?,
    };
    server.set_state(gst::State::Playing)?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (client, webrtcbin) = client(mode, tx)?;
    client.set_state(gst::State::Playing)?;

    let http = reqwest::Client::new();

    // The offer, without candidates
    let Some(Event::Offer(offer)) = rx.recv().await else {
        bail!("no offer");
    };
    let endpoint = format!("{base}/endpoint");
    log!(
        "client",
        "POST {endpoint}: offer with {} candidate(s)",
        count_candidates(&offer)
    );
    let resp = post_offer(&http, &endpoint, &offer).await?;
    let (resource, answer) = resp;
    log!("client", "201 Created, session {resource}");
    let answer = gst_sdp::SDPMessage::parse_buffer(answer.as_bytes())?;
    webrtcbin.emit_by_name::<()>(
        "set-remote-description",
        &[
            &gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, answer),
            &None::<gst::Promise>,
        ],
    );

    // Trickle each candidate in its own PATCH, as browsers do
    let mut statuses = vec![];
    let deadline = tokio::time::sleep(Duration::from_secs(4));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                // webrtcbin's end of gathering
                Some(Event::Candidate(mline, candidate)) if candidate.is_empty() => {
                    let frag = sdpfrag(&offer, mline, &["end-of-candidates"], None);
                    let status = patch(&http, &resource, &frag).await?;
                    log!("client", "PATCH m-line {mline} (mid {}) end-of-candidates -> {status}", mid(&offer, mline));
                }
                Some(Event::Candidate(mline, candidate)) => {
                    let frag = sdpfrag(&offer, mline, &[&candidate], None);
                    let status = patch(&http, &resource, &frag).await?;
                    log!("client", "PATCH m-line {mline} (mid {}) {candidate} -> {status}", mid(&offer, mline));
                    statuses.push(status);
                }
                Some(Event::Offer(_)) => (),
                None => break,
            },
            _ = &mut deadline => break,
        }
    }

    // Edge cases: a fragment with only the candidates of the last media
    // section, an ICE restart, and an unknown session
    let last = offer.medias_len() - 1;
    let fake = "candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host";
    let mut extra = vec![];
    if last > 0 {
        let frag = sdpfrag(&offer, last, &[fake], None);
        let status = patch(&http, &resource, &frag).await?;
        extra.push((
            format!(
                "fragment with only mid {} (m-line {last})",
                mid(&offer, last)
            ),
            status,
        ));
    }
    let frag = sdpfrag(
        &offer,
        0,
        &[fake],
        Some(("restart", "restartrestartrestartrestart")),
    );
    extra.push((
        "ICE restart (new ice-ufrag)".into(),
        patch(&http, &resource, &frag).await?,
    ));
    let frag = sdpfrag(&offer, 0, &[fake], None);
    let unknown = format!("{base}/resource/does-not-exist");
    extra.push((
        "unknown session".into(),
        patch(&http, &unknown, &frag).await?,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    println!();
    println!(
        "=== {} with {PLUGIN}",
        if mode == Mode::Whip { "WHIP" } else { "WHEP" }
    );
    let ok = statuses.iter().filter(|s| s.is_success()).count();
    println!("trickled candidates accepted: {ok}/{}", statuses.len());
    for (what, status) in extra {
        println!("{what}: {status}");
    }
    println!(
        "client ice-connection-state: {:?}",
        webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state")
    );
    match server_webrtcbin.lock().unwrap().clone() {
        Some(server_webrtcbin) => print_remote_candidates(&server_webrtcbin),
        None => println!("server webrtcbin: none"),
    }

    Ok((server, client))
}

type ServerWebrtcbin = Arc<Mutex<Option<gst::Element>>>;

/// `whipserversrc` receiving into fakesinks
fn whip_server() -> Result<(gst::Pipeline, ServerWebrtcbin, String)> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("whipserversrc")
        .property("stun-server", None::<String>)
        .build()?;
    let signaller = src.property::<glib::Object>("signaller");
    signaller.set_property("host-addr", "http://127.0.0.1:8190");
    tap_handle_ice(&signaller);
    pipeline.add(&src)?;

    let pipeline_weak = pipeline.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
    });

    let server_webrtcbin = ServerWebrtcbin::default();
    let found = server_webrtcbin.clone();
    pipeline.connect_deep_element_added(move |_, _, element| {
        if element.factory().is_some_and(|f| f.name() == "webrtcbin") {
            watch_ice_state("server", element);
            *found.lock().unwrap() = Some(element.clone());
        }
    });

    Ok((
        pipeline,
        server_webrtcbin,
        "http://127.0.0.1:8190/whip".into(),
    ))
}

/// `whepserversink` sending a video and an audio test stream
///
/// Congestion control is disabled: with one ICE transport per m-line,
/// webrtcsink's `request-aux-sender` handler deadlocks on its settings lock
/// (see README).
fn whep_server() -> Result<(gst::Pipeline, ServerWebrtcbin, String)> {
    let pipeline = gst::parse::launch(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=15/1 ! ws. \
         audiotestsrc is-live=true ! ws. \
         whepserversink name=ws video-caps=video/x-vp8 audio-caps=audio/x-opus \
         congestion-control=disabled",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    let sink = pipeline.by_name("ws").context("whepserversink")?;
    sink.set_property("stun-server", None::<String>);
    let signaller = sink.property::<glib::Object>("signaller");
    signaller.set_property("host-addr", "http://127.0.0.1:8191");
    tap_handle_ice(&signaller);

    let server_webrtcbin = ServerWebrtcbin::default();
    let found = server_webrtcbin.clone();
    sink.connect("consumer-added", false, move |args| {
        let webrtcbin = args[2].get::<gst::Element>().unwrap();
        watch_ice_state("server", &webrtcbin);
        *found.lock().unwrap() = Some(webrtcbin);
        None
    });

    Ok((
        pipeline,
        server_webrtcbin,
        "http://127.0.0.1:8191/whep".into(),
    ))
}

/// Logs what the server's signaller hands to webrtcbin
fn tap_handle_ice(signaller: &glib::Object) {
    signaller.connect("handle-ice", false, |args| {
        let mline = args[2].get::<u32>().unwrap();
        let mid = args[3].get::<Option<String>>().unwrap();
        let candidate = args[4].get::<String>().unwrap();
        log!(
            "server",
            "handle-ice m-line {mline} mid {mid:?}: {candidate}"
        );
        None
    });
}

fn watch_ice_state(who: &'static str, webrtcbin: &gst::Element) {
    webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _| {
        let state =
            webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        match who {
            "server" => log!("server", "ice-connection-state {state:?}"),
            _ => log!("client", "ice-connection-state {state:?}"),
        }
    });
}

/// The WHIP client sends a video stream, the WHEP client receives video and
/// audio without BUNDLE, so that each m-line has its own ICE transport
fn client(mode: Mode, tx: mpsc::UnboundedSender<Event>) -> Result<(gst::Pipeline, gst::Element)> {
    let pipeline = gst::Pipeline::new();
    let webrtcbin = gst::ElementFactory::make("webrtcbin").build()?;
    webrtcbin
        .property::<glib::Object>("ice-agent")
        .set_property("ice-tcp", false);
    pipeline.add(&webrtcbin)?;

    match mode {
        Mode::Whip => {
            webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
            let src = gst::parse::bin_from_description(
                "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=15/1 \
                 ! vp8enc deadline=1 ! rtpvp8pay pt=96 \
                 ! capsfilter caps=\"application/x-rtp,media=video,encoding-name=VP8,payload=96,clock-rate=90000\"",
                true,
            )?;
            pipeline.add(&src)?;
            src.link(&webrtcbin)?;
            let transceiver = webrtcbin
                .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                    "get-transceiver",
                    &[&0i32],
                )
                .context("transceiver")?;
            transceiver.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );
        }
        Mode::Whep => {
            webrtcbin.set_property_from_str("bundle-policy", "none");
            for caps in [
                "application/x-rtp,media=video,encoding-name=VP8,payload=96,clock-rate=90000",
                "application/x-rtp,media=audio,encoding-name=OPUS,payload=111,clock-rate=48000,encoding-params=(string)2",
            ] {
                webrtcbin.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
                    "add-transceiver",
                    &[
                        &gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                        &caps.parse::<gst::Caps>()?,
                    ],
                );
            }
            webrtcbin.connect_pad_added(|webrtcbin, pad| {
                let pipeline = webrtcbin.parent().and_downcast::<gst::Pipeline>().unwrap();
                let sink = gst::ElementFactory::make("fakesink")
                    .property("async", false)
                    .build()
                    .unwrap();
                pipeline.add(&sink).unwrap();
                sink.sync_state_with_parent().unwrap();
                pad.link(&sink.static_pad("sink").unwrap()).unwrap();
            });
        }
    }

    watch_ice_state("client", &webrtcbin);

    let offered = Arc::new(Mutex::new(false));
    let tx_offer = tx.clone();
    webrtcbin.connect("on-negotiation-needed", false, move |args| {
        let webrtcbin = args[0].get::<gst::Element>().unwrap();
        if std::mem::replace(&mut *offered.lock().unwrap(), true) {
            return None;
        }
        let tx = tx_offer.clone();
        let webrtcbin_weak = webrtcbin.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let Some(webrtcbin) = webrtcbin_weak.upgrade() else {
                return;
            };
            let offer = reply
                .unwrap()
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                .unwrap();
            webrtcbin.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            let _ = tx.send(Event::Offer(offer.sdp().to_owned()));
        });
        webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        None
    });

    webrtcbin.connect("on-ice-candidate", false, move |args| {
        let mline = args[1].get::<u32>().unwrap();
        let candidate = args[2].get::<String>().unwrap();
        let _ = tx.send(Event::Candidate(mline, candidate));
        None
    });

    Ok((pipeline, webrtcbin))
}

async fn post_offer(
    http: &reqwest::Client,
    endpoint: &str,
    offer: &gst_sdp::SDPMessage,
) -> Result<(String, String)> {
    // The server's HTTP listener comes up with the pipeline
    for _ in 0..50 {
        let resp = match http
            .post(endpoint)
            .header("Content-Type", "application/sdp")
            .body(offer.as_text()?)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if resp.status() != reqwest::StatusCode::CREATED {
            bail!("POST: {}", resp.status());
        }
        let location = resp
            .headers()
            .get("location")
            .context("no Location")?
            .to_str()?
            .to_string();
        let resource = resp.url().join(&location)?.to_string();
        return Ok((resource, resp.text().await?));
    }
    bail!("server not reachable")
}

async fn patch(http: &reqwest::Client, url: &str, frag: &str) -> Result<reqwest::StatusCode> {
    Ok(http
        .patch(url)
        .header("Content-Type", "application/trickle-ice-sdpfrag")
        .body(frag.to_string())
        .send()
        .await?
        .status())
}

/// An `application/trickle-ice-sdpfrag` body (RFC 8840) with `candidates` for
/// m-line `mline` of `offer`: the ICE credentials, then a media section with
/// only the `a=mid` of that m-line
fn sdpfrag(
    offer: &gst_sdp::SDPMessage,
    mline: u32,
    candidates: &[&str],
    credentials: Option<(&str, &str)>,
) -> String {
    let media = offer.media(mline).unwrap();
    let (ufrag, pwd) = credentials.unwrap_or_else(|| {
        (
            media
                .attribute_val("ice-ufrag")
                .or(offer.attribute_val("ice-ufrag"))
                .unwrap(),
            media
                .attribute_val("ice-pwd")
                .or(offer.attribute_val("ice-pwd"))
                .unwrap(),
        )
    });
    let mut frag = format!(
        "a=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\nm={} 9 {} {}\r\na=mid:{}\r\n",
        media.media().unwrap(),
        media.proto().unwrap(),
        media.formats().next().unwrap_or("0"),
        media.attribute_val("mid").unwrap(),
    );
    for candidate in candidates {
        frag += &format!("a={candidate}\r\n");
    }
    frag
}

fn mid(offer: &gst_sdp::SDPMessage, mline: u32) -> String {
    offer
        .media(mline)
        .and_then(|m| m.attribute_val("mid"))
        .unwrap_or("?")
        .to_string()
}

fn count_candidates(sdp: &gst_sdp::SDPMessage) -> usize {
    sdp.medias()
        .flat_map(|m| m.attributes())
        .filter(|a| a.key() == "candidate")
        .count()
}

/// The client's candidates as the server's webrtcbin knows them: `host` if
/// learned from signalling, `prflx` if only from connectivity checks
fn print_remote_candidates(webrtcbin: &gst::Element) {
    let (tx, rx) = std::sync::mpsc::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let _ = tx.send(reply.ok().flatten().map(|s| s.to_owned()));
    });
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    let Ok(Some(stats)) = rx.recv_timeout(Duration::from_secs(2)) else {
        println!("server stats: none");
        return;
    };

    println!(
        "server ice-connection-state: {:?}",
        webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state")
    );
    println!("client candidates known to the server's webrtcbin:");
    let mut lines = vec![];
    for (_, value) in stats.iter() {
        let Ok(s) = value.get::<gst::Structure>() else {
            continue;
        };
        if s.get::<gst_webrtc::WebRTCStatsType>("type").ok()
            != Some(gst_webrtc::WebRTCStatsType::RemoteCandidate)
        {
            continue;
        }
        let transport = s.get::<String>("transport-id").unwrap_or_default();
        let address = s.get::<String>("address").unwrap_or_default();
        let port = s.get::<u32>("port").unwrap_or_default();
        let typ = s.get::<String>("candidate-type").unwrap_or_default();
        lines.push(format!("  {transport}: {address}:{port} typ {typ}"));
    }
    lines.sort();
    lines.dedup();
    if lines.is_empty() {
        println!("  none");
    }
    for line in lines {
        println!("{line}");
    }
}
