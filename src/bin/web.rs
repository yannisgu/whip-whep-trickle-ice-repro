//! Browser repro: runs `whipserversrc` and `whepserversink` and serves a test
//! page on the same origin as their WHIP/WHEP endpoints.
//!
//! cargo run --features upstream --bin web [-- 127.0.0.1:8189]
//! cargo run --features fork --bin web
//!
//! The signallers send no CORS headers, so `/whip/*` and `/whep/*` are proxied
//! to them. `/repro/session/{id}` lists the candidates the signaller handed to
//! webrtcbin for a session.

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{any, get};
use gst::prelude::*;
use std::sync::Arc;
use whip_whep_trickle_ice_repro::{PLUGIN, server};

const PAGE: &str = include_str!("../../web/index.html");

struct App {
    whip: server::Server,
    whep: server::Server,
    http: reqwest::Client,
}

fn main() -> Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8189".into());

    whip_whep_trickle_ice_repro::init()?;
    let whip = server::whip()?;
    let whep = server::whep()?;
    whip.pipeline.set_state(gst::State::Playing)?;
    whep.pipeline.set_state(gst::State::Playing)?;

    let app = Arc::new(App {
        whip,
        whep,
        http: reqwest::Client::new(),
    });
    let router = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(
            "/repro/info",
            get(|| async { Json(serde_json::json!({ "plugin": PLUGIN })) }),
        )
        .route("/repro/session/{id}", get(session))
        .route("/whip/{*rest}", any(proxy))
        .route("/whep/{*rest}", any(proxy))
        .with_state(app);

    tokio::runtime::Runtime::new()?.block_on(async {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("open http://{addr}/");
        axum::serve(listener, router).await?;
        anyhow::Ok(())
    })
}

/// The candidates the server's signallers handed to webrtcbin for session `id`
async fn session(State(app): State<Arc<App>>, Path(id): Path<String>) -> impl IntoResponse {
    let mut trickled = vec![];
    for server in [&app.whip, &app.whep] {
        trickled.extend(
            server
                .trickled
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.session == id)
                .cloned(),
        );
    }
    Json(trickled)
}

/// Forwards a WHIP/WHEP request to the signaller's own HTTP server
async fn proxy(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let base = if uri.path().starts_with("/whip/") {
        &app.whip.base
    } else {
        &app.whep.base
    };
    // `base` ends with /whip or /whep, as does the start of the path
    let origin = base.rsplit_once('/').unwrap().0;
    let mut req = app
        .http
        .request(method, format!("{origin}{}", uri.path()))
        .body(body);
    for name in ["content-type", "if-match"] {
        if let Some(value) = headers.get(name) {
            req = req.header(name, value);
        }
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(err) => return (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    };

    let mut out = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if !matches!(
            name.as_str(),
            "content-length" | "transfer-encoding" | "connection"
        ) {
            out = out.header(name, value);
        }
    }
    let body = resp.bytes().await.unwrap_or_default();
    out.body(body.into()).unwrap()
}
