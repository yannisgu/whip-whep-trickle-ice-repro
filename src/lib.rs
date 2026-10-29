//! Trickle ICE (RFC 8840 / RFC 9725 section 4.3.2) against gst-plugins-rs'
//! `whipserversrc` and `whepserversink`: what the binaries share.

#[cfg(all(feature = "upstream", feature = "fork"))]
compile_error!("enable only one of the features `upstream` and `fork`");
#[cfg(not(any(feature = "upstream", feature = "fork")))]
compile_error!("enable one of the features `upstream` and `fork`");

use gst::prelude::*;
use std::sync::LazyLock;
use std::time::Instant;

#[doc(hidden)]
pub static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Prints a line with the time since start and who logs it
#[macro_export]
macro_rules! log {
    ($who:expr, $($arg:tt)*) => {
        println!("{:>6.3}s [{}] {}", $crate::START.elapsed().as_secs_f64(), $who, format!($($arg)*))
    };
}

pub mod client;
pub mod server;

pub const PLUGIN: &str = if cfg!(feature = "upstream") {
    "upstream main"
} else {
    "fork webrtc-whip-whep-trickle-ice"
};

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Whip,
    Whep,
}

/// Initializes GStreamer and registers the gst-plugin-webrtc under test
pub fn init() -> anyhow::Result<()> {
    gst::init()?;
    #[cfg(feature = "upstream")]
    gst_plugin_webrtc_upstream::plugin_register_static()?;
    #[cfg(feature = "fork")]
    gst_plugin_webrtc_fork::plugin_register_static()?;
    LazyLock::force(&START);
    println!("gst-plugin-webrtc: {PLUGIN}, {}", gst::version_string());
    Ok(())
}

pub fn watch_ice_state(who: &'static str, webrtcbin: &gst::Element) {
    webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _| {
        let state =
            webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        log!(who, "ice-connection-state {state:?}");
    });
}
