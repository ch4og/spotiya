//! Spotiya's YouTube add-on: a DIAL/Lounge cast receiver that plays casts from
//! the YouTube and YouTube Music apps. Audio is fetched as the mobile web client
//! (PO tokens minted locally; see `web`), decoded and sent to ALSA by `player`,
//! and the speaker is shared with Spotify and Alice through `handover`.
//!
//! It runs as a separate binary so its engine and HTTP stack cost nothing until
//! the add-on is enabled. The receiver spawns it; standalone, it just plays.

mod config;
mod dial;
mod handover;
mod lounge;
mod messages;
mod player;
mod spectrum;
mod ssdp;
mod web;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{future::Future, pin::Pin};

use lounge::PlayerCommand;
use messages::{fmt_time, OutgoingMessage, PlayerState};
use player::{Player, State};

struct Playlist {
    video_ids: Vec<String>,
    index: usize,
    list_id: String,
    ctt: Option<String>,
    params: Option<String>,
    title: String,
    duration_secs: u64,
    cpn: String,
    last_state: PlayerState,
    loading: Option<Loading>,
}

struct Loading {
    job: Pin<Box<dyn Future<Output = Result<player::Loaded, String>> + Send>>,
    paused: bool,
}

async fn loaded(loading: &mut Option<Loading>) -> Result<player::Loaded, String> {
    match loading {
        Some(loading) => loading.job.as_mut().await,
        None => std::future::pending().await,
    }
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            video_ids: Vec::new(),
            index: 0,
            list_id: String::new(),
            ctt: None,
            params: None,
            title: String::new(),
            duration_secs: 0,
            cpn: messages::generate_cpn(),
            last_state: PlayerState::Idle,
            loading: None,
        }
    }
}

impl Playlist {
    fn current(&self) -> String {
        self.video_ids.get(self.index).cloned().unwrap_or_default()
    }
}

struct Options {
    name: String,
    state_dir: PathBuf,
    cache_dir: PathBuf,
    device: String,
}

fn options() -> Options {
    // The receiver's add-on installer runs the binary with --version to check it loads.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("spotiya-youtube {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    let mut name = std::env::var("DEVICE_NAME").unwrap_or_else(|_| "Spotiya".into());
    let mut state_dir =
        PathBuf::from(std::env::var("YOUTUBE_STATE_DIR").unwrap_or_else(|_| ".".into()));
    let mut cache_dir = PathBuf::from(
        std::env::var("YOUTUBE_CACHE_DIR").unwrap_or_else(|_| "/tmp/spotiya-youtube".into()),
    );
    let mut device = std::env::var("YOUTUBE_ALSA_DEVICE").unwrap_or_else(|_| "default".into());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" => name = args.next().unwrap_or(name),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or(state_dir),
            "--cache-dir" => cache_dir = args.next().map(PathBuf::from).unwrap_or(cache_dir),
            "--device" => device = args.next().unwrap_or(device),
            other => tracing::warn!("[init] ignoring unknown argument {other}"),
        }
    }
    Options {
        name,
        state_dir,
        cache_dir,
        device,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt::init();
    // QuickJS churns the allocator hard; on 32-bit glibc the default per-thread
    // arenas fragment to 100 MB+ under a fast autoplay queue. One arena plus an
    // eager trim threshold keeps the add-on near its real ~40 MB footprint.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 20);
    }
    let options = options();
    std::fs::create_dir_all(&options.state_dir)?;
    std::fs::create_dir_all(&options.cache_dir)?;

    // Commands queue here until the player loop starts, so the receiver's control
    // socket is usable at once, before the network setup below has finished.
    let (player_cmd_tx, mut player_cmd_rx) = tokio::sync::mpsc::channel::<PlayerCommand>(32);
    if let Some(path) = std::env::var_os("SPOTIYA_YOUTUBE_CONTROL") {
        tokio::spawn(local_control(PathBuf::from(path), player_cmd_tx.clone()));
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let mut config = config::Config::load(&options.state_dir)?;
    tracing::info!("[init] device \"{}\", uuid {}", options.name, config.uuid);

    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    // The station's Wi-Fi drops now and then; wait for it instead of exiting.
    while config.screen_id.is_empty() || config.screen_id_m.is_empty() {
        let result = tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
            result = lounge::generate_screen_id(&http) => result,
        };
        match result {
            Ok(id) => {
                if config.screen_id.is_empty() {
                    config.screen_id = id;
                } else {
                    config.screen_id_m = id;
                }
                config.save()?;
            }
            Err(e) => {
                tracing::warn!("[init] screen id: {e}; retrying");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = terminate.recv() => return Ok(()),
                }
            }
        }
    }

    let web = web::Web::new(http.clone(), options.cache_dir.clone());
    let handover = Arc::new(handover::Handover::new());
    let player = Arc::new(Player::new(web, handover, options.device.clone()));

    let local_ip = ssdp::get_local_ip()?;
    tracing::info!("[net] local IP {local_ip}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Player state is routed to whichever Lounge session a remote last connected on.
    let (state_tx, mut state_router_rx) = tokio::sync::mpsc::channel::<OutgoingMessage>(32);
    let (state_cl_tx, state_cl_rx) = tokio::sync::mpsc::channel::<OutgoingMessage>(32);
    let (state_m_tx, state_m_rx) = tokio::sync::mpsc::channel::<OutgoingMessage>(32);
    let (pairing_cl_tx, pairing_cl_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (pairing_m_tx, pairing_m_rx) = tokio::sync::mpsc::channel::<String>(8);
    let active_theme = Arc::new(std::sync::RwLock::new("cl".to_string()));

    let router_theme = active_theme.clone();
    tokio::spawn(async move {
        while let Some(msg) = state_router_rx.recv().await {
            let tx = if *router_theme.read().unwrap() == "m" {
                &state_m_tx
            } else {
                &state_cl_tx
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let dial_state = Arc::new(dial::DialState {
        running: std::sync::atomic::AtomicBool::new(false),
        pairing_tx: pairing_cl_tx,
        pairing_tx_m: pairing_m_tx,
        device_name: options.name.clone(),
        uuid: config.uuid.clone(),
        local_ip,
        port: 8008,
        screen_id: config.screen_id.clone(),
        debug_tx: std::env::var("YOUTUBE_DEBUG")
            .is_ok()
            .then(|| player_cmd_tx.clone()),
    });

    spawn_services(
        &options.name,
        &config,
        &http,
        local_ip,
        dial_state.clone(),
        player_cmd_tx.clone(),
        state_cl_rx,
        state_m_rx,
        pairing_cl_rx,
        pairing_m_rx,
        shutdown_rx.clone(),
    );

    tracing::info!("[youtube] \"{}\" is now discoverable", options.name);

    let mut playlist = Playlist::default();
    let mut has_remote = false;
    let mut playing = false;
    let mut changes = player.subscribe();
    let mut report = tokio::time::interval(Duration::from_secs(3));

    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            cmd = player_cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match &cmd {
                    PlayerCommand::RemoteConnected { theme } => {
                        has_remote = true;
                        *active_theme.write().unwrap() = theme.clone();
                        continue;
                    }
                    PlayerCommand::RemoteDisconnected { theme } => {
                        if *theme != *active_theme.read().unwrap() {
                            continue;
                        }
                        has_remote = false;
                    }
                    PlayerCommand::SetPlaylist { .. } | PlayerCommand::SetVideo { .. } if !has_remote => {
                        // Replayed from the initial bind before any remote connected.
                        continue;
                    }
                    _ => {}
                }
                if let Err(e) = handle(cmd, &player, &state_tx, &mut playlist, &mut playing).await {
                    tracing::error!("[player] {e}");
                }
            }
            result = loaded(&mut playlist.loading) => {
                let paused = playlist.loading.take().unwrap().paused;
                match result {
                    Ok(loaded) => {
                        let track = player.start(loaded, paused);
                        playlist.title = track.title;
                        playlist.duration_secs = track.duration_secs;
                        playing = true;
                        let state = if paused { PlayerState::Paused } else { PlayerState::Playing };
                        send_state(&player, &state_tx, &mut playlist, state).await;
                    }
                    Err(e) => {
                        tracing::error!("[player] {} failed: {e}", playlist.current());
                        player.stop().await;
                        send_state(&player, &state_tx, &mut playlist, PlayerState::Stopped).await;
                    }
                }
            }
            _ = changes.changed() => {
                // A track that ended on its own leaves the player stopped while we
                // still think it is playing: advance to the next one.
                if playing && player.status().state == State::Stop {
                    playing = false;
                    track_ended(&player, &state_tx, &mut playlist, &mut playing).await;
                }
            }
            _ = report.tick() => {
                player.report_progress();
                if playing {
                    let status = player.status();
                    if status.state == State::Play {
                        let dur = fmt_time(playlist.duration_secs as f64);
                        state_tx.try_send(
                            OutgoingMessage::new("onStateChange")
                                .with_arg("state", "1")
                                .with_arg("currentTime", fmt_time(status.position_ms as f64 / 1000.0))
                                .with_arg("duration", &dur)
                                .with_arg("loadedTime", &dur)
                                .with_arg("seekableStartTime", "0")
                                .with_arg("seekableEndTime", &dur)
                                .with_arg("cpn", &playlist.cpn),
                        ).ok();
                    }
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    playlist.loading = None;
    player.shutdown().await;
    tracing::info!("[youtube] shutdown");
    Ok(())
}

type CmdResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn handle(
    cmd: PlayerCommand,
    player: &Arc<Player>,
    state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>,
    playlist: &mut Playlist,
    playing: &mut bool,
) -> CmdResult {
    match cmd {
        PlayerCommand::SetPlaylist {
            video_id,
            video_ids,
            list_id,
            index,
            ctt,
            params,
            current_time,
        } => {
            playlist.video_ids = video_ids;
            playlist.index = index;
            playlist.list_id = list_id;
            playlist.ctt = ctt;
            playlist.params = params;
            playlist.last_state = PlayerState::Idle;
            start(player, state_tx, playlist, &video_id, current_time, playing).await;
        }
        PlayerCommand::SetVideo {
            video_id,
            ctt,
            current_time,
        } => {
            playlist.video_ids = vec![video_id.clone()];
            playlist.index = 0;
            playlist.list_id.clear();
            playlist.ctt = ctt;
            playlist.last_state = PlayerState::Idle;
            start(player, state_tx, playlist, &video_id, current_time, playing).await;
        }
        PlayerCommand::Play => {
            if let Some(loading) = &mut playlist.loading {
                loading.paused = false;
                return Ok(());
            }
            player.resume();
            *playing = true;
            send_state(player, state_tx, playlist, PlayerState::Playing).await;
        }
        PlayerCommand::Pause => {
            if let Some(loading) = &mut playlist.loading {
                loading.paused = true;
            }
            player.pause();
            send_state(player, state_tx, playlist, PlayerState::Paused).await;
        }
        // The station's buttons and the TUI toggle; deciding here, by the real
        // player state, keeps them working even if the receiver's view is stale.
        PlayerCommand::Toggle => {
            if let Some(loading) = &mut playlist.loading {
                loading.paused = !loading.paused;
                return Ok(());
            }
            if player.status().state == State::Play {
                player.pause();
                send_state(player, state_tx, playlist, PlayerState::Paused).await;
            } else {
                player.resume();
                *playing = true;
                send_state(player, state_tx, playlist, PlayerState::Playing).await;
            }
        }
        PlayerCommand::SeekTo { position } => {
            let video_id = playlist.current();
            if !video_id.is_empty() {
                // Streams are not randomly seekable; restart and skip to the target.
                start(player, state_tx, playlist, &video_id, position, playing).await;
            }
        }
        PlayerCommand::SetVolume { volume } => {
            player.set_volume(volume);
            send_volume(state_tx, volume).await;
        }
        PlayerCommand::GetVolume => {
            send_volume(state_tx, player.status().volume).await;
        }
        PlayerCommand::GetNowPlaying => {
            playlist.last_state = PlayerState::Idle;
            let state = if playlist.loading.is_some() {
                PlayerState::Loading
            } else {
                match player.status().state {
                    State::Play => PlayerState::Playing,
                    State::Pause => PlayerState::Paused,
                    State::Stop => PlayerState::Stopped,
                }
            };
            send_state(player, state_tx, playlist, state).await;
        }
        PlayerCommand::GetPlaylist => {
            state_tx
                .try_send(
                    OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .ok();
        }
        PlayerCommand::Stop | PlayerCommand::RemoteDisconnected { .. } => {
            playlist.loading = None;
            *playing = false;
            player.stop().await;
            playlist.video_ids.clear();
            playlist.index = 0;
            playlist.list_id.clear();
            playlist.duration_secs = 0;
            send_state(player, state_tx, playlist, PlayerState::Stopped).await;
        }
        PlayerCommand::Previous => {
            if playlist.index > 0 {
                playlist.index -= 1;
                let video_id = playlist.current();
                playlist.last_state = PlayerState::Idle;
                start(player, state_tx, playlist, &video_id, 0.0, playing).await;
            }
        }
        PlayerCommand::Next => {
            if playlist.index + 1 < playlist.video_ids.len() {
                playlist.index += 1;
                let video_id = playlist.current();
                playlist.last_state = PlayerState::Idle;
                start(player, state_tx, playlist, &video_id, 0.0, playing).await;
            } else {
                playlist.loading = None;
                *playing = false;
                player.stop().await;
                send_state(player, state_tx, playlist, PlayerState::Stopped).await;
            }
        }
        PlayerCommand::UpdatePlaylist {
            video_ids,
            list_id,
            current_index,
        } => {
            let current = playlist.current();
            playlist.index = current_index
                .map(|i| i.min(video_ids.len().saturating_sub(1)))
                .unwrap_or_else(|| video_ids.iter().position(|v| *v == current).unwrap_or(0));
            playlist.video_ids = video_ids;
            if !list_id.is_empty() {
                playlist.list_id = list_id;
            }
            send_nav(state_tx, playlist).await;
            state_tx
                .try_send(
                    OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .ok();
        }
        PlayerCommand::RemoteConnected { .. } => {}
    }
    Ok(())
}

/// Resolves and starts a track, reporting Loading then Playing (or Stopped on failure).
async fn start(
    player: &Arc<Player>,
    state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>,
    playlist: &mut Playlist,
    video_id: &str,
    position: f64,
    playing: &mut bool,
) {
    playlist.loading = None;
    player.stop().await;
    *playing = false;
    playlist.duration_secs = 0;
    playlist.title.clear();
    send_state(player, state_tx, playlist, PlayerState::Loading).await;
    let player = player.clone();
    let video_id = video_id.to_owned();
    playlist.loading = Some(Loading {
        job: Box::pin(async move { player.load(&video_id, position).await }),
        paused: false,
    });
}

/// Reports the finished track, then advances to the next one if there is one.
async fn track_ended(
    player: &Arc<Player>,
    state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>,
    playlist: &mut Playlist,
    playing: &mut bool,
) {
    let dur = fmt_time(playlist.duration_secs as f64);
    playlist.last_state = PlayerState::Stopped;
    state_tx
        .try_send(
            OutgoingMessage::new("onStateChange")
                .with_arg("state", "0")
                .with_arg("currentTime", &dur)
                .with_arg("duration", &dur)
                .with_arg("loadedTime", &dur)
                .with_arg("seekableStartTime", "0")
                .with_arg("seekableEndTime", &dur)
                .with_arg("cpn", &playlist.cpn),
        )
        .ok();
    if playlist.index + 1 < playlist.video_ids.len() {
        playlist.index += 1;
        let video_id = playlist.current();
        playlist.last_state = PlayerState::Idle;
        start(player, state_tx, playlist, &video_id, 0.0, playing).await;
    } else {
        player.finish().await;
        tracing::info!("[player] playlist finished");
    }
}

/// Sends `onStateChange` always, and `nowPlaying` only on the transitions the
/// YouTube app expects it (a changed track, or a status change out of Playing).
async fn send_state(
    player: &Arc<Player>,
    state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>,
    playlist: &mut Playlist,
    state: PlayerState,
) {
    let status = player.status();
    let elapsed = status.position_ms as f64 / 1000.0;
    let duration = playlist.duration_secs as f64;
    let loaded = match state {
        PlayerState::Playing | PlayerState::Paused | PlayerState::Loading => duration,
        _ => 0.0,
    };
    let prev = playlist.last_state;
    let now_playing = state != prev && prev != PlayerState::Playing;
    playlist.last_state = state;

    state_tx
        .try_send(
            OutgoingMessage::new("onStateChange")
                .with_arg("state", state.as_str())
                .with_arg("currentTime", fmt_time(elapsed))
                .with_arg("duration", fmt_time(duration))
                .with_arg("loadedTime", fmt_time(loaded))
                .with_arg("seekableStartTime", "0")
                .with_arg("seekableEndTime", fmt_time(duration))
                .with_arg("cpn", &playlist.cpn),
        )
        .ok();

    if now_playing {
        let mut np = OutgoingMessage::new("nowPlaying")
            .with_arg("currentTime", fmt_time(elapsed))
            .with_arg("duration", fmt_time(duration))
            .with_arg("cpn", &playlist.cpn)
            .with_arg("loadedTime", fmt_time(loaded))
            .with_arg("videoId", playlist.current())
            .with_arg("state", state.as_str())
            .with_arg("seekableStartTime", "0")
            .with_arg("seekableEndTime", fmt_time(duration))
            .with_arg("currentIndex", playlist.index.to_string());
        if !playlist.list_id.is_empty() {
            np = np.with_arg("listId", &playlist.list_id);
        }
        if let Some(ctt) = &playlist.ctt {
            np = np.with_arg("ctt", ctt);
        }
        if let Some(params) = &playlist.params {
            np = np.with_arg("params", params);
        }
        state_tx.try_send(np).ok();
        send_nav(state_tx, playlist).await;
    }
}

async fn send_nav(state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>, playlist: &Playlist) {
    state_tx
        .try_send(
            OutgoingMessage::new("onHasPreviousNextChanged")
                .with_arg(
                    "hasPrevious",
                    if playlist.index > 0 { "true" } else { "false" },
                )
                .with_arg(
                    "hasNext",
                    if playlist.index + 1 < playlist.video_ids.len() {
                        "true"
                    } else {
                        "false"
                    },
                ),
        )
        .ok();
}

async fn send_volume(state_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>, volume: u32) {
    state_tx
        .try_send(
            OutgoingMessage::new("onVolumeChanged")
                .with_arg("volume", volume.to_string())
                .with_arg("muted", if volume == 0 { "true" } else { "false" }),
        )
        .ok();
}

/// Receiver-driven control: `PLAY <videoId>`, `PAUSE`, `RESUME`, `TOGGLE`, `NEXT`,
/// `PREVIOUS` and `STOP`, for links from the TUI and the station's buttons.
async fn local_control(path: PathBuf, tx: tokio::sync::mpsc::Sender<PlayerCommand>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("[control] cannot bind {}: {e}", path.display());
            return;
        }
    };
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.trim().splitn(2, ' ');
            let ok = match (parts.next(), parts.next()) {
                (Some("PLAY"), Some(id))
                    if id.len() == 11
                        && id
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == '_') =>
                {
                    // Fake a remote so the command passes the receiver's no-remote guard.
                    let _ = tx
                        .send(PlayerCommand::RemoteConnected { theme: "cl".into() })
                        .await;
                    tx.send(PlayerCommand::SetVideo {
                        video_id: id.into(),
                        ctt: None,
                        current_time: 0.0,
                    })
                    .await
                    .is_ok()
                }
                (Some("STOP"), None) => tx.send(PlayerCommand::Stop).await.is_ok(),
                (Some("PAUSE"), None) => tx.send(PlayerCommand::Pause).await.is_ok(),
                (Some("TOGGLE"), None) => tx.send(PlayerCommand::Toggle).await.is_ok(),
                (Some("RESUME"), None) => tx.send(PlayerCommand::Play).await.is_ok(),
                (Some("NEXT"), None) => tx.send(PlayerCommand::Next).await.is_ok(),
                (Some("PREVIOUS"), None) => tx.send(PlayerCommand::Previous).await.is_ok(),
                _ => false,
            };
            let _ = reader
                .into_inner()
                .write_all(if ok { b"OK\n" } else { b"ERROR\n" })
                .await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_services(
    name: &str,
    config: &config::Config,
    http: &reqwest::Client,
    local_ip: std::net::Ipv4Addr,
    dial_state: Arc<dial::DialState>,
    player_cmd_tx: tokio::sync::mpsc::Sender<PlayerCommand>,
    state_cl_rx: tokio::sync::mpsc::Receiver<OutgoingMessage>,
    state_m_rx: tokio::sync::mpsc::Receiver<OutgoingMessage>,
    pairing_cl_rx: tokio::sync::mpsc::Receiver<String>,
    pairing_m_rx: tokio::sync::mpsc::Receiver<String>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let uuid = config.uuid.clone();
    let ssdp_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = ssdp::run_ssdp(local_ip, 8008, &uuid, ssdp_shutdown).await {
            tracing::error!("[ssdp] {e}");
        }
    });

    let dial_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = dial::run_dial_server(8008, dial_state, dial_shutdown).await {
            tracing::error!("[dial] {e}");
        }
    });

    // "cl" is the YouTube theme, "m" is YouTube Music; each is its own Lounge device.
    let cl = (
        http.clone(),
        name.to_string(),
        config.uuid.clone(),
        config.screen_id.clone(),
    );
    let cl_tx = player_cmd_tx.clone();
    let cl_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            cl.0,
            cl.1,
            cl.2,
            cl.3,
            "cl".into(),
            cl_tx,
            state_cl_rx,
            pairing_cl_rx,
            cl_shutdown,
        )
        .await
        {
            tracing::error!("[lounge:cl] {e}");
        }
    });

    let m = (
        http.clone(),
        name.to_string(),
        uuid::Uuid::new_v4().to_string(),
        config.screen_id_m.clone(),
    );
    let m_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            m.0,
            m.1,
            m.2,
            m.3,
            "m".into(),
            player_cmd_tx,
            state_m_rx,
            pairing_m_rx,
            m_shutdown,
        )
        .await
        {
            tracing::error!("[lounge:m] {e}");
        }
    });
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn player(handover: Arc<handover::Handover>) -> Arc<Player> {
        let web = web::Web::new(reqwest::Client::new(), std::env::temp_dir());
        Arc::new(Player::new(web, handover, "null".into()))
    }

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    async fn pending_load(playlist: &mut Playlist) -> Arc<AtomicBool> {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        playlist.loading = Some(Loading {
            job: Box::pin(async move {
                let _guard = guard;
                std::future::pending().await
            }),
            paused: false,
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(5), loaded(&mut playlist.loading))
                .await
                .is_err()
        );
        dropped
    }

    #[tokio::test]
    async fn stop_cancels_loading_without_waiting_for_the_network() {
        let player = player(Arc::new(handover::Handover::testing(|_| true)));
        let mut playlist = Playlist::default();
        let dropped = pending_load(&mut playlist).await;
        let (state, _messages) = tokio::sync::mpsc::channel(1);
        state.try_send(OutgoingMessage::new("busy")).unwrap();
        let mut playing = false;
        tokio::time::timeout(
            Duration::from_secs(1),
            handle(
                PlayerCommand::Stop,
                &player,
                &state,
                &mut playlist,
                &mut playing,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(dropped.load(Ordering::Acquire));
        assert!(playlist.loading.is_none());
        assert_eq!(player.status().state, State::Stop);
        player.shutdown().await;
    }

    #[tokio::test]
    async fn a_new_video_replaces_a_pending_load() {
        let player = player(Arc::new(handover::Handover::testing(|_| true)));
        let mut playlist = Playlist::default();
        let dropped = pending_load(&mut playlist).await;
        let (state, _messages) = tokio::sync::mpsc::channel(32);
        let mut playing = false;
        handle(
            PlayerCommand::SetVideo {
                video_id: "abcdefghijk".into(),
                ctt: None,
                current_time: 0.0,
            },
            &player,
            &state,
            &mut playlist,
            &mut playing,
        )
        .await
        .unwrap();
        assert!(dropped.load(Ordering::Acquire));
        assert!(playlist.loading.is_some());
        assert_eq!(playlist.current(), "abcdefghijk");
        // Do not poll the replacement: this test makes no network requests.
        handle(
            PlayerCommand::Stop,
            &player,
            &state,
            &mut playlist,
            &mut playing,
        )
        .await
        .unwrap();
        player.shutdown().await;
    }

    #[tokio::test]
    async fn finishing_the_last_track_releases_the_speaker() {
        let (observed, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let handover = Arc::new(handover::Handover::testing(move |command| {
            let _ = observed.send(command.split_whitespace().nth(1).unwrap().to_owned());
            true
        }));
        assert!(handover.begin().await);
        assert_eq!(commands.recv().await.as_deref(), Some("BEGIN"));
        let player = player(handover);
        let mut playlist = Playlist {
            video_ids: vec!["abcdefghijk".into()],
            ..Playlist::default()
        };
        let (state, _messages) = tokio::sync::mpsc::channel(32);
        let mut playing = false;
        track_ended(&player, &state, &mut playlist, &mut playing).await;
        let command = tokio::time::timeout(Duration::from_secs(1), commands.recv())
            .await
            .unwrap();
        assert_eq!(command.as_deref(), Some("END"));
        assert!(!playing);
        player.shutdown().await;
    }
}
