//! The servers under test: `whipserversrc` and `whepserversink` on 127.0.0.1

use anyhow::{Context, Result, anyhow};
use gst::glib;
use gst::prelude::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub struct Server {
    pub pipeline: gst::Pipeline,
    /// The session's webrtcbin, once the client has connected
    pub webrtcbin: Arc<Mutex<Option<gst::Element>>>,
    /// How many candidates the signaller handed to webrtcbin, per m-line
    pub candidates: Arc<Mutex<BTreeMap<u32, usize>>>,
    /// The URL the WHIP/WHEP paths are relative to
    pub base: String,
}

/// `whipserversrc` receiving into fakesinks
pub fn whip() -> Result<Server> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("whipserversrc")
        .property("stun-server", None::<String>)
        .build()?;
    let signaller = src.property::<glib::Object>("signaller");
    signaller.set_property("host-addr", "http://127.0.0.1:8190");
    let candidates = tap_handle_ice(&signaller);
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

    let webrtcbin = Arc::new(Mutex::new(None));
    let found = webrtcbin.clone();
    pipeline.connect_deep_element_added(move |_, _, element| {
        if element.factory().is_some_and(|f| f.name() == "webrtcbin") {
            crate::watch_ice_state("server", element);
            *found.lock().unwrap() = Some(element.clone());
        }
    });

    Ok(Server {
        pipeline,
        webrtcbin,
        candidates,
        base: "http://127.0.0.1:8190/whip".into(),
    })
}

/// `whepserversink` sending a video and an audio test stream
///
/// Congestion control is disabled: with one ICE transport per m-line,
/// webrtcsink's `request-aux-sender` handler deadlocks on its settings lock
/// (see README).
pub fn whep() -> Result<Server> {
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
    let candidates = tap_handle_ice(&signaller);

    // webrtcsink runs each session's webrtcbin in a pipeline of its own
    let webrtcbin = Arc::new(Mutex::new(None));
    let found = webrtcbin.clone();
    sink.connect("consumer-added", false, move |args| {
        let webrtcbin = args[2].get::<gst::Element>().unwrap();
        crate::watch_ice_state("server", &webrtcbin);
        *found.lock().unwrap() = Some(webrtcbin);
        None
    });

    Ok(Server {
        pipeline,
        webrtcbin,
        candidates,
        base: "http://127.0.0.1:8191/whep".into(),
    })
}

/// Logs and counts what the server's signaller hands to webrtcbin
fn tap_handle_ice(signaller: &glib::Object) -> Arc<Mutex<BTreeMap<u32, usize>>> {
    let candidates = Arc::new(Mutex::new(BTreeMap::new()));
    let counts = candidates.clone();
    signaller.connect("handle-ice", false, move |args| {
        let mline = args[2].get::<u32>().unwrap();
        *counts.lock().unwrap().entry(mline).or_default() += 1;
        let mid = args[3].get::<Option<String>>().unwrap();
        let candidate = args[4].get::<String>().unwrap();
        log!(
            "server",
            "handle-ice m-line {mline} mid {mid:?}: {candidate}"
        );
        None
    });
    candidates
}
