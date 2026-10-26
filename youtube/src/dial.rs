use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct DialState {
    /// Whether the "YouTube" DIAL app is currently running.
    pub running: AtomicBool,
    /// Channel to deliver pairing codes for the "cl" (YouTube) session.
    pub pairing_tx: mpsc::Sender<String>,
    /// Channel to deliver pairing codes for the "m" (YouTube Music) session.
    pub pairing_tx_m: mpsc::Sender<String>,
    /// Human-readable name shown during discovery.
    pub device_name: String,
    /// UPnP UUID for this device (without the "uuid:" prefix).
    pub uuid: String,
    /// Local IP for building absolute Application-URL.
    pub local_ip: std::net::Ipv4Addr,
    /// Port this DIAL server listens on.
    pub port: u16,
    /// Screen ID for the "cl" (YouTube) Lounge session, exposed in DIAL additionalData.
    pub screen_id: String,
    /// When set (YOUTUBE_DEBUG=1), exposes `/debug/*` to drive playback without a phone.
    pub debug_tx: Option<mpsc::Sender<crate::lounge::PlayerCommand>>,
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the DIAL HTTP server on the given port.
///
/// This serves the UPnP device description and the `/apps/YouTube` DIAL
/// endpoints that the YouTube Music app talks to after SSDP discovery.
pub async fn run_dial_server(
    port: u16,
    state: Arc<DialState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut app = Router::new()
        .route("/ssdp/device-desc.xml", get(device_description))
        .route("/apps/YouTube", get(app_status))
        .route("/apps/YouTube", post(app_launch))
        .route("/apps/YouTube/run", delete(app_stop));
    if state.debug_tx.is_some() {
        app = app
            .route("/debug/play/{video_id}", get(debug_play))
            .route("/debug/pause", get(debug_pause))
            .route("/debug/resume", get(debug_resume))
            .route("/debug/stop", get(debug_stop));
    }
    let app = app.with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("DIAL server listening on 0.0.0.0:{port}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        // Wait until shutdown is signalled
        loop {
            if *shutdown.borrow() {
                return;
            }
            if shutdown.changed().await.is_err() {
                return;
            }
        }
    })
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// GET /ssdp/device-desc.xml
// ---------------------------------------------------------------------------

async fn device_description(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let base_url = format!("http://{}:{}", state.local_ip, state.port);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion>
    <major>1</major>
    <minor>0</minor>
  </specVersion>
  <URLBase>{base_url}</URLBase>
  <device>
    <deviceType>urn:dial-multiscreen-org:device:dial:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>Spotiya</manufacturer>
    <modelName>Spotiya YouTube</modelName>
    <UDN>uuid:{uuid}</UDN>
    <iconList>
      <icon>
        <mimetype>image/png</mimetype>
        <width>48</width>
        <height>48</height>
        <depth>24</depth>
        <url>/icon.png</url>
      </icon>
    </iconList>
    <serviceList>
      <service>
        <serviceType>urn:dial-multiscreen-org:service:dial:1</serviceType>
        <serviceId>urn:dial-multiscreen-org:serviceId:dial</serviceId>
        <controlURL>/ssdp/notfound</controlURL>
        <eventSubURL>/ssdp/notfound</eventSubURL>
        <SCPDURL>/ssdp/notfound</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        base_url = base_url,
        name = xml_escape(&state.device_name),
        uuid = state.uuid,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    // The Application-URL header MUST be an absolute URL per DIAL spec Section 5.4.
    // No trailing slash -- matches peer-dial Node.js reference implementation.
    let app_url = format!("http://{}:{}/apps", state.local_ip, state.port);
    headers.insert(
        HeaderName::from_static("application-url"),
        app_url.parse().unwrap(),
    );

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// GET /apps/YouTube -- app status
// ---------------------------------------------------------------------------

async fn app_status(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let running = state.running.load(Ordering::Relaxed);
    let state_str = if running { "running" } else { "stopped" };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="1.7">
  <name>YouTube</name>
  <options allowStop="true"/>
  <state>{state_str}</state>
  <link rel="run" href="run"/>
  <additionalData>
    <screenId>{screen_id}</screenId>
  </additionalData>
</service>"#,
        screen_id = state.screen_id,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// POST /apps/YouTube -- launch
// ---------------------------------------------------------------------------

async fn app_launch(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
    body: String,
) -> Response {
    tracing::debug!("DIAL {} {} from {} body={}", method, uri, addr, body);

    // Body is form-urlencoded: pairingCode=XXX&theme=m&v=VIDEO_ID&t=0
    let params: Vec<(String, String)> = serde_urlencoded::from_str(&body).unwrap_or_default();

    let theme = params
        .iter()
        .find(|(k, _)| k == "theme")
        .map(|(_, v)| v.as_str())
        .unwrap_or("cl");

    if let Some((_, code)) = params.iter().find(|(k, _)| k == "pairingCode") {
        if !code.is_empty() {
            let tx = if theme == "m" {
                &state.pairing_tx_m
            } else {
                &state.pairing_tx
            };
            tracing::info!(
                "DIAL launch: theme={}, pairing code routing to {} session",
                theme,
                theme
            );
            if let Err(e) = tx.send(code.clone()).await {
                tracing::error!("failed to send pairing code: {e}");
            }
        }
    }

    state.running.store(true, Ordering::Relaxed);

    // DIAL spec: 201 Created with LOCATION pointing to the running instance.
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, "/apps/YouTube/run".parse().unwrap());

    (StatusCode::CREATED, headers).into_response()
}

// ---------------------------------------------------------------------------
// DELETE /apps/YouTube/run -- stop
// ---------------------------------------------------------------------------

async fn app_stop(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> StatusCode {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);
    state.running.store(false, Ordering::Relaxed);
    StatusCode::OK
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Debug endpoints (only mounted with YOUTUBE_DEBUG=1)
// ---------------------------------------------------------------------------

async fn debug_play(
    axum::extract::Path(video_id): axum::extract::Path<String>,
    State(state): State<Arc<DialState>>,
) -> Response {
    use crate::lounge::PlayerCommand;
    let Some(tx) = &state.debug_tx else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let _ = tx
        .send(PlayerCommand::RemoteConnected { theme: "cl".into() })
        .await;
    let _ = tx
        .send(PlayerCommand::SetVideo {
            video_id: video_id.clone(),
            ctt: None,
            current_time: 0.0,
        })
        .await;
    (StatusCode::OK, format!("playing {video_id}")).into_response()
}

async fn debug_pause(State(state): State<Arc<DialState>>) -> Response {
    debug_send(&state, crate::lounge::PlayerCommand::Pause, "paused").await
}

async fn debug_resume(State(state): State<Arc<DialState>>) -> Response {
    debug_send(&state, crate::lounge::PlayerCommand::Play, "resumed").await
}

async fn debug_stop(State(state): State<Arc<DialState>>) -> Response {
    debug_send(&state, crate::lounge::PlayerCommand::Stop, "stopped").await
}

async fn debug_send(
    state: &Arc<DialState>,
    cmd: crate::lounge::PlayerCommand,
    ok: &str,
) -> Response {
    let Some(tx) = &state.debug_tx else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let _ = tx.send(cmd).await;
    (StatusCode::OK, ok.to_string()).into_response()
}
