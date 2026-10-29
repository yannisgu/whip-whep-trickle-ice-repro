//! A WHIP/WHEP client that trickles its candidates, the way browsers do: a
//! plain webrtcbin that sends its offer right away, before it has gathered
//! any candidate, and PATCHes each candidate to the session URL once it has
//! it

use crate::Mode;
use anyhow::{Context, Result, bail};
use gst::glib;
use gst::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

enum Event {
    Offer(gst_sdp::SDPMessage),
    Candidate(u32, String),
}

pub struct Client {
    pub pipeline: gst::Pipeline,
    pub webrtcbin: gst::Element,
    events: mpsc::UnboundedReceiver<Event>,
    http: reqwest::Client,
    /// The offer and the session URL, once the server answered
    session: Option<(gst_sdp::SDPMessage, String)>,
    /// The `ETag` of the server's answer, sent back in `If-Match`
    etag: Option<String>,
}

/// The `If-Match` header of a PATCH
#[derive(Clone, Copy)]
enum IfMatch<'a> {
    /// The `ETag` of the answer, if the server sent one
    Etag,
    None,
    Value(&'a str),
}

impl Client {
    /// The WHIP client sends a video stream, the WHEP client receives video
    /// and audio without BUNDLE, so that each m-line has its own ICE
    /// transport
    pub fn new(mode: Mode) -> Result<Self> {
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

        crate::watch_ice_state("client", &webrtcbin);

        let (tx, events) = mpsc::unbounded_channel();
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
                webrtcbin
                    .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
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

        Ok(Client {
            pipeline,
            webrtcbin,
            events,
            http: reqwest::Client::new(),
            session: None,
            etag: None,
        })
    }

    /// Sends the offer to the endpoint under `base`, then trickles each
    /// candidate in its own PATCH for a few seconds
    pub async fn trickle(&mut self, base: &str) -> Result<Vec<reqwest::StatusCode>> {
        // The offer, without candidates
        let Some(Event::Offer(offer)) = self.events.recv().await else {
            bail!("no offer");
        };
        let endpoint = format!("{base}/endpoint");
        log!(
            "client",
            "POST {endpoint}: offer with {} candidate(s)",
            count_candidates(&offer)
        );
        let (resource, etag, answer) = self.post_offer(&endpoint, &offer).await?;
        log!("client", "201 Created, session {resource}, ETag {etag:?}");
        self.etag = etag;
        let answer = gst_sdp::SDPMessage::parse_buffer(answer.as_bytes())?;
        self.webrtcbin.emit_by_name::<()>(
            "set-remote-description",
            &[
                &gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Answer,
                    answer,
                ),
                &None::<gst::Promise>,
            ],
        );

        let mut trickled = vec![];
        let deadline = tokio::time::sleep(Duration::from_secs(4));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                ev = self.events.recv() => match ev {
                    // webrtcbin's end of gathering
                    Some(Event::Candidate(mline, candidate)) if candidate.is_empty() => {
                        let frag = sdpfrag(&offer, mline, &["end-of-candidates"], None);
                        let status = self.patch(&resource, &frag, IfMatch::Etag).await?;
                        log!("client", "PATCH m-line {mline} (mid {}) end-of-candidates -> {status}", mid(&offer, mline));
                    }
                    Some(Event::Candidate(mline, candidate)) => {
                        let frag = sdpfrag(&offer, mline, &[&candidate], None);
                        let status = self.patch(&resource, &frag, IfMatch::Etag).await?;
                        log!("client", "PATCH m-line {mline} (mid {}) {candidate} -> {status}", mid(&offer, mline));
                        trickled.push(status);
                    }
                    Some(Event::Offer(_)) => (),
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }

        self.session = Some((offer, resource));
        Ok(trickled)
    }

    /// PATCHes the cases besides plain trickling, one at a time, and returns
    /// each one's status and the m-lines of the candidates the server's
    /// signaller handed to webrtcbin for it
    pub async fn edge_cases(
        &self,
        base: &str,
        trickled: &Mutex<Vec<crate::server::Trickled>>,
    ) -> Result<Vec<EdgeCase>> {
        let (offer, resource) = self.session.as_ref().context("no session")?;
        let last = offer.medias_len() - 1;
        let fake = "candidate:9999 1 UDP 2122260223 192.0.2.1 50000 typ host";
        let restart = Some(("restart", "restartrestartrestartrestart"));
        let unknown = format!("{base}/resource/does-not-exist");

        let mut cases = vec![];
        if last > 0 {
            cases.push((
                format!(
                    "fragment with only mid {} (m-line {last})",
                    mid(offer, last)
                ),
                resource.as_str(),
                sdpfrag(offer, last, &[fake], None),
                IfMatch::Etag,
            ));
        }
        cases.extend([
            (
                "candidate before the first m= line, no m= line".to_string(),
                resource.as_str(),
                session_sdpfrag(offer, &[fake]),
                IfMatch::Etag,
            ),
            (
                "If-Match: *".into(),
                resource.as_str(),
                sdpfrag(offer, 0, &[fake], None),
                IfMatch::Value("*"),
            ),
            (
                "no If-Match".into(),
                resource.as_str(),
                sdpfrag(offer, 0, &[fake], None),
                IfMatch::None,
            ),
            (
                "wrong If-Match".into(),
                resource.as_str(),
                sdpfrag(offer, 0, &[fake], None),
                IfMatch::Value("\"wrong\""),
            ),
            (
                "ICE restart (new ice-ufrag)".into(),
                resource.as_str(),
                sdpfrag(offer, 0, &[fake], restart),
                IfMatch::Etag,
            ),
            (
                "unknown session".into(),
                unknown.as_str(),
                sdpfrag(offer, 0, &[fake], None),
                IfMatch::Etag,
            ),
        ]);

        let mut edge_cases = vec![];
        for (what, url, frag, if_match) in cases {
            let before = trickled.lock().unwrap().len();
            let status = self.patch(url, &frag, if_match).await?;
            // The signaller emits handle-ice before it answers
            let mlines = trickled.lock().unwrap()[before..]
                .iter()
                .map(|t| t.mline)
                .collect();
            edge_cases.push(EdgeCase {
                what,
                status,
                mlines,
            });
        }
        Ok(edge_cases)
    }

    /// POSTs `offer` and returns the session URL and the answer
    async fn post_offer(
        &self,
        endpoint: &str,
        offer: &gst_sdp::SDPMessage,
    ) -> Result<(String, Option<String>, String)> {
        // The server's HTTP listener comes up with its pipeline
        for _ in 0..50 {
            let resp = match self
                .http
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
            let etag = resp
                .headers()
                .get("etag")
                .map(|etag| etag.to_str())
                .transpose()?
                .map(str::to_string);
            return Ok((resource, etag, resp.text().await?));
        }
        bail!("server not reachable")
    }

    async fn patch(
        &self,
        url: &str,
        frag: &str,
        if_match: IfMatch<'_>,
    ) -> Result<reqwest::StatusCode> {
        let mut req = self
            .http
            .patch(url)
            .header("Content-Type", "application/trickle-ice-sdpfrag")
            .body(frag.to_string());
        let if_match = match if_match {
            IfMatch::Etag => self.etag.as_deref(),
            IfMatch::None => None,
            IfMatch::Value(value) => Some(value),
        };
        if let Some(if_match) = if_match {
            req = req.header("If-Match", if_match);
        }
        Ok(req.send().await?.status())
    }
}

/// A PATCH besides plain trickling
pub struct EdgeCase {
    pub what: String,
    pub status: reqwest::StatusCode,
    /// The m-lines the server's signaller handed its candidates to
    pub mlines: Vec<u32>,
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

/// An `application/trickle-ice-sdpfrag` body with `candidates` in the
/// session part, before any `m=` line, and the session-level ICE
/// credentials, or those of the first media section
fn session_sdpfrag(offer: &gst_sdp::SDPMessage, candidates: &[&str]) -> String {
    let media = offer.media(0).unwrap();
    let attr = |name| {
        offer
            .attribute_val(name)
            .or(media.attribute_val(name))
            .unwrap()
    };
    let mut frag = format!(
        "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\n",
        attr("ice-ufrag"),
        attr("ice-pwd")
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
