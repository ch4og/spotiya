//! Supervises the optional YouTube add-on binary and relays its speaker handover.
//!
//! The add-on (`spotiya-youtube`) is a separate process that receives casts and
//! plays their audio to ALSA. It shares the speaker with Spotify and Alice through
//! the same [`Gate`](crate::airplay::Gate) AirPlay uses: when it starts a track it
//! sends `YOUTUBE BEGIN` over the control socket, and the receiver stops Spotify and
//! pauses Alice. When Spotify reclaims, the Gate asks the add-on to drain (never
//! kills it) so the shared dmix is handed back cleanly.

use crate::{airplay::Gate, control, profiles};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::Instant};

const LOG: &str = "/tmp/spotiya-youtube.log";
/// Datagram socket where the add-on sends four spectrum levels per 50 ms.
const LEVELS: &str = "/run/spotiya-youtube.levels";
const BIN: &str = "spotiya-youtube";

#[derive(Debug, PartialEq)]
pub enum Action {
    /// Hooks from the add-on (token-authenticated): it took the speaker,
    /// released it, changed volume, started a track or paused/resumed it.
    Begin,
    End,
    Volume(u16),
    Title(String),
    Paused(bool),
    /// Position and duration of the current track, in milliseconds.
    Position(u64, u32),
    /// From the TUI, CLI or buttons (trusted local socket, no token).
    Play(String),
    Control(&'static str),
}
pub struct Request {
    pub action: Action,
    pub token: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// Parses an add-on hook: `YOUTUBE BEGIN|END <token>`, `YOUTUBE VOLUME <token> 0..100`,
/// `YOUTUBE STATE <token> paused|playing` or `YOUTUBE TITLE <token> <text>`.
pub fn parse(command: &str) -> Option<(Action, String)> {
    let mut parts = command.trim().splitn(4, ' ');
    if parts.next()? != "YOUTUBE" {
        return None;
    }
    // ON/OFF/URL/STOP carry no token and are handled elsewhere.
    let kind = parts.next()?;
    let token = parts.next()?;
    if uuid::Uuid::parse_str(token).is_err() {
        return None;
    }
    let rest = parts.next();
    let action = match (kind, rest) {
        ("BEGIN", None) => Action::Begin,
        ("END", None) => Action::End,
        ("VOLUME", Some(value)) => Action::Volume(value.parse().ok().filter(|v| *v <= 100)?),
        ("POSITION", Some(value)) => {
            let (position, duration) = value.split_once(' ')?;
            Action::Position(position.parse().ok()?, duration.parse().ok()?)
        }
        ("STATE", Some("paused")) => Action::Paused(true),
        ("STATE", Some("playing")) => Action::Paused(false),
        ("TITLE", Some(title)) => {
            let title: String = title
                .chars()
                .filter(|c| !c.is_control())
                .take(300)
                .collect();
            if title.trim().is_empty() {
                return None;
            }
            Action::Title(title.trim().to_owned())
        }
        _ => return None,
    };
    Some((action, token.into()))
}

pub struct Runtime {
    base: PathBuf,
    bin: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    control: PathBuf,
    setting: PathBuf,
    spectrum_setting: PathBuf,
    name: String,
    device: String,
    child: Option<tokio::process::Child>,
    token: String,
    retry_at: Instant,
    trimmed: Instant,
    pub enabled: bool,
    pub available: bool,
    pub error: String,
}

impl Runtime {
    pub fn new(base: &Path, state: &Path, name: &str, audio: &profiles::Audio) -> Self {
        let available = super::addons::youtube_available(base);
        let setting = state.join("youtube-enabled");
        let enabled = available && fs::read_to_string(&setting).is_ok_and(|s| s.trim() == "1");
        Self {
            base: base.to_path_buf(),
            bin: base.join("youtube/bin").join(BIN),
            data: state.join("youtube"),
            cache: state.join("youtube/cache"),
            control: PathBuf::from("/run/spotiya-youtube.control"),
            setting,
            spectrum_setting: state.join("spectrum-enabled"),
            name: name.into(),
            device: audio.device.clone(),
            child: None,
            token: String::new(),
            retry_at: Instant::now(),
            trimmed: Instant::now(),
            enabled,
            available,
            error: String::new(),
        }
    }

    pub fn control_path(&self) -> PathBuf {
        self.control.clone()
    }

    pub fn valid(&self, token: &str) -> bool {
        self.enabled && !self.token.is_empty() && self.token == token
    }

    pub fn running(&self) -> bool {
        self.child.is_some() && self.error.is_empty()
    }

    pub fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.available = super::addons::youtube_available(&self.base);
        if enabled && !self.available {
            return Err(io::Error::other(
                "Install the optional YouTube package first",
            ));
        }
        let temp = self.setting.with_extension("new");
        fs::write(&temp, if enabled { "1\n" } else { "0\n" })?;
        fs::rename(temp, &self.setting)?;
        self.enabled = enabled;
        self.retry_at = Instant::now();
        Ok(())
    }

    /// Asks the running add-on to start a video by id, or stop. Best effort.
    pub fn send(&self, line: &str) -> io::Result<()> {
        let mut socket = std::os::unix::net::UnixStream::connect(&self.control)?;
        // Called on the receiver loop; the add-on answers as soon as it queued the command.
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        writeln!(socket, "{line}")?;
        let mut reply = String::new();
        socket.take(64).read_to_string(&mut reply)?;
        if reply.trim() == "OK" {
            Ok(())
        } else {
            Err(io::Error::other("YouTube add-on rejected the command"))
        }
    }

    fn start(&mut self) -> io::Result<()> {
        fs::create_dir_all(&self.data)?;
        fs::create_dir_all(&self.cache)?;
        self.token = uuid::Uuid::new_v4().to_string();
        let _ = fs::remove_file(&self.control);
        let output = crate::supervisor::open_log(Path::new(LOG))?;
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--name")
            .arg(&self.name)
            .env("YOUTUBE_STATE_DIR", &self.data)
            .env("YOUTUBE_CACHE_DIR", &self.cache)
            .env("YOUTUBE_ALSA_DEVICE", &self.device)
            .env("SPOTIYA_YOUTUBE_TOKEN", &self.token)
            .env("SPOTIYA_YOUTUBE_CONTROL", &self.control)
            .env("SPOTIYA_YOUTUBE_LEVELS", LEVELS)
            .env("SPOTIYA_SPECTRUM_SETTING", &self.spectrum_setting)
            // One malloc arena keeps the engine's 32-bit footprint from ballooning.
            .env("MALLOC_ARENA_MAX", "1")
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .kill_on_drop(true)
            .process_group(0);
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    return Err(io::Error::other("Receiver exited"));
                }
                Ok(())
            });
        }
        self.child = Some(cmd.spawn()?);
        Ok(())
    }

    /// Stops the add-on gracefully (SIGTERM so it drains ALSA), then forces it.
    pub async fn stop(&mut self, gate: &Gate) {
        self.token.clear();
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
            // The add-on closes ALSA on SIGTERM. SIGKILL is the last resort: a client
            // killed while it holds the shared dmix can leave the speaker silent.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if !matches!(child.try_wait(), Ok(None)) {
                    break;
                }
                if Instant::now() >= deadline {
                    if let Some(pid) = child.id() {
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child.wait().await;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        gate.release_youtube();
    }

    pub async fn poll(&mut self, gate: &Gate) {
        if self.child.is_some() && self.trimmed.elapsed() >= Duration::from_secs(30) {
            self.trimmed = Instant::now();
            let _ = crate::supervisor::trim_log(Path::new(LOG));
        }
        if !self.available {
            self.available = super::addons::youtube_available(&self.base);
        }
        if let Some(child) = &mut self.child
            && !matches!(child.try_wait(), Ok(None))
        {
            self.child = None;
            gate.release_youtube();
            self.error = "YouTube add-on exited; retrying".into();
            self.retry_at = Instant::now() + Duration::from_secs(3);
        }
        if !self.enabled && self.child.is_some() {
            self.stop(gate).await;
            self.error.clear();
        }
        if self.enabled && self.child.is_none() && Instant::now() >= self.retry_at {
            self.error.clear();
            if let Err(e) = self.start() {
                self.error = e.to_string();
                self.retry_at = Instant::now() + Duration::from_secs(10);
                log::warn!("youtube: {}", self.error);
            }
        }
    }

    pub fn status(&self, status: &mut control::Status, gate: &Gate) {
        status.youtube_available = self.available;
        status.youtube_enabled = self.enabled;
        status.youtube_running = self.running();
        status.youtube_active =
            gate.active.load(std::sync::atomic::Ordering::Acquire) && gate.holds_youtube();
        status.youtube_error.clone_from(&self.error);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SIGTERM, not SIGKILL: the add-on drains its ALSA device on the way out.
        if let Some(pid) = self.child.as_ref().and_then(|c| c.id()) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
    }
}

/// A video id only when `input` is unmistakably a YouTube link, so a Spotify search
/// that happens to be eleven letters is never taken for one.
pub fn link(input: &str) -> Option<String> {
    let lower = input.trim().to_ascii_lowercase();
    let host = ["youtube.com/", "youtu.be/", "youtube-nocookie.com/"];
    if lower.contains(char::is_whitespace) || !host.iter().any(|h| lower.contains(h)) {
        return None;
    }
    video_id(input)
}

/// Receives the add-on's spectrum levels and shows them while YouTube holds the
/// speaker, so the TUI graph and the station display work as they do for Spotify.
pub fn receive_levels(
    gate: std::sync::Arc<Gate>,
    spectrum: std::sync::Arc<crate::spectrum::Spectrum>,
) -> io::Result<()> {
    let _ = fs::remove_file(LEVELS);
    let socket = std::os::unix::net::UnixDatagram::bind(LEVELS)?;
    fs::set_permissions(LEVELS, fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("youtube-levels".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut levels = [0u8; 4];
            while let Ok(n) = socket.recv(&mut levels) {
                if n == levels.len() && gate.holds_youtube() && spectrum.enabled() {
                    spectrum.publish(levels.map(|l| l.min(100)));
                }
            }
        })?;
    Ok(())
}

/// Extracts an 11-character video id from a YouTube URL or a bare id.
pub fn video_id(input: &str) -> Option<String> {
    let input = input.trim();
    let valid = |id: &str| {
        id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if valid(input) {
        return Some(input.to_owned());
    }
    // youtu.be/<id>, /watch?v=<id>, /shorts/<id>, /embed/<id>, /live/<id>
    fn after<'a>(s: &'a str, pat: &str) -> Option<&'a str> {
        s.split_once(pat).map(|(_, r)| r)
    }
    let candidate = after(input, "v=")
        .or_else(|| after(input, "youtu.be/"))
        .or_else(|| after(input, "/shorts/"))
        .or_else(|| after(input, "/embed/"))
        .or_else(|| after(input, "/live/"))?;
    let id: String = candidate
        .chars()
        .take_while(|&c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        .collect();
    valid(&id).then_some(id)
}

pub fn available(base: &Path) -> bool {
    fs::metadata(base.join("youtube/bin").join(BIN))
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stopping_idle_youtube_keeps_airplay_ownership() {
        let dir = std::env::temp_dir().join(format!("youtube-idle-{}", uuid::Uuid::new_v4()));
        let profile = crate::profiles::load(Some("generic"), &dir).unwrap();
        let mut runtime = Runtime::new(&dir, &dir, "Test", &profile.profile.audio);
        runtime.child = Some(
            Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let gate = Gate::new();
        gate.active
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(!gate.holds_youtube());
        runtime.stop(&gate).await;
        assert!(gate.active.load(std::sync::atomic::Ordering::Acquire));
    }
    #[test]
    fn extracts_video_ids_from_urls_and_bare_ids() {
        for url in [
            "https://youtu.be/O5sjrLI5mSs",
            "https://www.youtube.com/watch?v=O5sjrLI5mSs&t=10",
            "https://m.youtube.com/watch?v=O5sjrLI5mSs",
            "https://www.youtube.com/shorts/O5sjrLI5mSs",
            "O5sjrLI5mSs",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("O5sjrLI5mSs"), "{url}");
        }
        assert_eq!(video_id("not a url"), None);
        assert_eq!(
            link("https://music.youtube.com/watch?v=O5sjrLI5mSs&list=RD").as_deref(),
            Some("O5sjrLI5mSs")
        );
        // Bare ids and ordinary searches stay Spotify searches.
        assert_eq!(link("O5sjrLI5mSs"), None);
        assert_eq!(link("no surprises"), None);
        assert_eq!(link("youtu.be is a site"), None);
        assert_eq!(video_id("https://youtu.be/short"), None);
    }
    #[test]
    fn addon_installed_after_startup_can_be_enabled() {
        let dir = std::env::temp_dir().join(format!("youtube-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let profile = crate::profiles::load(Some("generic"), &dir).unwrap();
        let mut runtime = Runtime::new(&dir, &dir, "Test", &profile.profile.audio);
        assert!(!runtime.available && runtime.set_enabled(true).is_err());
        let bin = dir.join("youtube/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join(BIN), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join(BIN), fs::Permissions::from_mode(0o755)).unwrap();
        runtime.set_enabled(true).unwrap();
        assert!(runtime.available && runtime.enabled);
        assert!(Runtime::new(&dir, &dir, "Test", &profile.profile.audio).enabled);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn handover_parse_accepts_only_well_formed_tokens() {
        let t = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            parse(&format!("YOUTUBE BEGIN {t}")),
            Some((Action::Begin, t.clone()))
        );
        assert_eq!(
            parse(&format!("YOUTUBE VOLUME {t} 40")),
            Some((Action::Volume(40), t.clone()))
        );
        assert!(parse(&format!("YOUTUBE VOLUME {t} 200")).is_none());
        assert!(parse("YOUTUBE BEGIN not-a-uuid").is_none());
        assert!(parse(&format!("YOUTUBE BEGIN {t} extra")).is_none());
        assert!(parse("YOUTUBE ON").is_none());
        assert_eq!(
            parse(&format!("YOUTUBE TITLE {t} linkin kla$ - given up")),
            Some((Action::Title("linkin kla$ - given up".into()), t.clone()))
        );
        assert!(parse(&format!("YOUTUBE TITLE {t}  ")).is_none());
        assert_eq!(
            parse(&format!("YOUTUBE STATE {t} paused")),
            Some((Action::Paused(true), t.clone()))
        );
        assert!(parse(&format!("YOUTUBE STATE {t} stopped")).is_none());
        assert_eq!(
            parse(&format!("YOUTUBE POSITION {t} 61500 195000")),
            Some((Action::Position(61500, 195000), t.clone()))
        );
        assert!(parse(&format!("YOUTUBE POSITION {t} 5")).is_none());
        assert!(parse(&format!("YOUTUBE POSITION {t} -1 100")).is_none());
    }
}
