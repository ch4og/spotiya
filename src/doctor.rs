use crate::{
    control::Status,
    installation::{self, error},
    profiles,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
struct Check {
    name: &'static str,
    result: &'static str,
    detail: String,
}
#[derive(Serialize)]
struct LogSummary {
    source: &'static str,
    counts: BTreeMap<&'static str, u32>,
}
#[derive(Default, Serialize)]
struct Receiver {
    running: bool,
    supervisor_running: bool,
    status_current: bool,
    connected: bool,
    active: bool,
    loading: bool,
    volume: u16,
    retry_required: bool,
    failed_tracks: u8,
    has_error: bool,
}
#[derive(Serialize)]
struct Report {
    format: u32,
    version: &'static str,
    unix_time: u64,
    profile: String,
    receiver: Receiver,
    checks: Vec<Check>,
    logs: Vec<LogSummary>,
}
impl Report {
    fn new() -> Self {
        Self {
            format: 1,
            version: env!("CARGO_PKG_VERSION"),
            unix_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            profile: "unresolved".into(),
            receiver: Receiver::default(),
            checks: vec![],
            logs: vec![],
        }
    }
    fn add(&mut self, name: &'static str, result: &'static str, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            result,
            detail: detail.into(),
        });
    }
    fn status(&mut self, running: Option<u32>, supervisor: bool, status: Option<Status>) {
        self.receiver.running = running.is_some();
        self.receiver.supervisor_running = supervisor;
        let status = status.filter(|s| running == Some(s.receiver_pid));
        self.receiver.status_current = status.is_some();
        if let Some(s) = status {
            self.receiver.connected = s.connected;
            self.receiver.active = s.active;
            self.receiver.loading = s.loading;
            self.receiver.volume = s.volume;
            self.receiver.retry_required = s.retry_required;
            self.receiver.failed_tracks = s.failed_tracks;
            self.receiver.has_error = !s.error.is_empty();
            self.add(
                "Spotify session",
                if s.connected { "OK" } else { "WARN" },
                if s.connected {
                    "Connected"
                } else {
                    "Disconnected; login or recovery may be needed"
                },
            );
            if s.retry_required {
                self.add(
                    "Playback recovery",
                    "WARN",
                    "Repeated track failures; inspect error categories before using retry",
                );
            } else if !s.error.is_empty() {
                self.add(
                    "Playback state",
                    "WARN",
                    "Receiver reports an error; inspect error categories below",
                );
            }
        } else {
            self.add(
                "Receiver status",
                "WARN",
                if running.is_some() {
                    "Status is missing or stale"
                } else {
                    "Receiver is stopped; doctor does not start it"
                },
            );
        }
    }
}
fn log_summary(source: &'static str, path: &Path) -> io::Result<LogSummary> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(65536)))?;
    let mut reader = BufReader::new(file.take(65536));
    let mut line = Vec::new();
    let mut counts = BTreeMap::new();
    if length > 65536 {
        reader.read_until(b'\n', &mut line)?;
        line.clear();
    }
    while reader.read_until(b'\n', &mut line)? > 0 {
        let text = String::from_utf8_lossy(&line);
        let category = category(&text);
        if let Some(category) = category {
            *counts.entry(category).or_insert(0) += 1;
        }
        line.clear();
    }
    Ok(LogSummary { source, counts })
}
fn category(line: &str) -> Option<&'static str> {
    let lower = line.trim_end().to_ascii_lowercase();
    // Spotiya's own failure lines; everything else it prints is progress, not an error.
    if lower.starts_with("receiver: connect failed") {
        return Some(
            // Builds before 0.9.4 printed only the error kind, as `PermissionDenied`.
            if lower.contains("permission denied")
                || lower.contains("permissiondenied")
                || lower.contains("unauthenticated")
                || lower.contains("no valid authentication credentials")
            {
                "authentication"
            } else {
                "network"
            },
        );
    }
    if lower.starts_with("receiver: connect timeout") {
        return Some("network");
    }
    if lower.starts_with("supervisor: receiver exited") {
        return Some("receiver_restart");
    }
    if lower.starts_with("alsa lib ") {
        return Some("audio_device");
    }
    // librespot logs success at INFO, often naming the decoder or ALSA sink.
    if !lower.starts_with("[warn ") && !lower.starts_with("[error ") {
        return None;
    }
    Some(
        if lower.contains("audio_key") || lower.contains("audio key") {
            "audio_key"
        } else if lower.contains("alsa") || lower.contains("audio_backend") {
            "audio_device"
        } else if lower.contains("decoder") || lower.contains("decoding") {
            "decoder"
        } else if lower.contains("unauthenticated")
            || lower.contains("invalid credentials")
            || lower.contains("access token")
            || lower.contains("authentication failed")
            || lower.contains("login failed")
        {
            "authentication"
        } else if lower.contains("http_client")
            || lower.contains("cdn_url")
            || lower.contains("network")
            || lower.contains("websocket")
            || lower.contains("connection to server closed")
            || lower.contains("timed out")
            || lower.contains("connection refused")
        {
            "network"
        } else {
            "other_warning_or_error"
        },
    )
}
fn mixer(audio: &profiles::Audio) -> io::Result<(i64, i64, i64, Option<bool>)> {
    let mixer = alsa::mixer::Mixer::new(&audio.mixer_device, true)
        .map_err(|_| error("Cannot open configured ALSA mixer"))?;
    let control = mixer
        .find_selem(&alsa::mixer::SelemId::new(
            &audio.mixer_control,
            audio.mixer_index,
        ))
        .ok_or_else(|| error("Configured mixer control does not exist"))?;
    let value = control
        .get_playback_volume(audio.mixer_channel.alsa())
        .map_err(|_| error("Configured mixer channel is unavailable"))?;
    let (min, max) = control.get_playback_volume_range();
    let enabled = if control.has_playback_switch() {
        Some(
            control
                .get_playback_switch(audio.mixer_channel.alsa())
                .map_err(|_| error("Cannot read mixer mute switch"))?
                != 0,
        )
    } else {
        None
    };
    Ok((value, min, max, enabled))
}
async fn network(report: &mut Report) {
    for host in [
        "apresolve.spotify.com",
        "accounts.spotify.com",
        "api.github.com",
    ] {
        let resolved =
            tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, 443)))
                .await;
        let success = resolved
            .ok()
            .and_then(Result::ok)
            .is_some_and(|mut addresses| addresses.next().is_some());
        report.add(
            match host {
                "apresolve.spotify.com" => "DNS Spotify",
                "accounts.spotify.com" => "DNS login",
                _ => "DNS updates",
            },
            if success { "OK" } else { "FAIL" },
            if success {
                "Resolved"
            } else {
                "Resolution failed or timed out"
            },
        );
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("spotiya/", env!("CARGO_PKG_VERSION")))
        .build();
    let Ok(client) = client else {
        report.add("HTTPS", "FAIL", "Cannot initialize TLS client");
        return;
    };
    for (name, url) in [
        ("HTTPS Spotify", "https://apresolve.spotify.com/"),
        ("HTTPS login", "https://accounts.spotify.com/"),
        ("HTTPS updates", "https://api.github.com/"),
    ] {
        match client.get(url).send().await {
            Ok(response) => report.add(
                name,
                "OK",
                format!("TLS verified; HTTP {}", response.status().as_u16()),
            ),
            Err(e) => report.add(
                name,
                "FAIL",
                if e.is_timeout() {
                    "Request timed out"
                } else {
                    "Connection or TLS failed; check DNS, routing, certificates and system clock"
                },
            ),
        }
    }
}
fn collect(state: &Path) -> Report {
    let mut report = Report::new();
    let base = Path::new(installation::BASE);
    let pid = installation::live_pid(base, Path::new(installation::RECEIVER_PID));
    let supervisor =
        installation::live_pid(base, Path::new(installation::SUPERVISOR_PID)).is_some();
    let status = fs::read(crate::control::STATUS)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    report.status(pid, supervisor, status);
    match profiles::load(None,state) {
        Ok(resolved)=>{
            report.profile=if matches!(resolved.profile.id.as_str(),"mini1"|"mini2"|"cucumber"|"generic") { resolved.profile.id.clone() } else { "custom".into() };
            report.add("Device profile","OK",format!("{}; buttons={}, ring={}, display={}, voice={}",report.profile,resolved.capabilities.buttons,resolved.capabilities.ring,resolved.capabilities.display,resolved.capabilities.voice));
            match mixer(&resolved.profile.audio) {
                Ok((value,min,max,enabled))=>{
                    report.add("ALSA mixer",if value<=min || enabled==Some(false) { "WARN" } else { "OK" },format!("Volume {value}, range {min}..{max}, muted={}",enabled==Some(false)));
                }
                Err(e)=>report.add("ALSA mixer","FAIL",e.to_string()),
            }
            report.add("ALSA PCM","INFO","Playback device is not opened; doctor does not claim or play audio");
            if let Some(voice)=resolved.profile.voice {
                let connected=std::net::TcpStream::connect_timeout(&voice.address,Duration::from_secs(1)).is_ok();
                report.add("Quasar IPC",if connected { "OK" } else { "WARN" },if connected { "Local endpoint accepts connections" } else { "Local endpoint is unavailable" });
            }
        }
        Err(_)=>report.add("Device profile","FAIL","Invalid or unreadable saved profile; use profile or profile select to inspect and repair"),
    }
    match installation::free_bytes(state) {
        Ok(bytes) => report.add(
            "Free disk",
            if bytes < 12 * 1024 * 1024 {
                "WARN"
            } else {
                "OK"
            },
            format!(
                "{} KiB available; updates need staging and rollback space",
                bytes / 1024
            ),
        ),
        Err(_) => report.add("Free disk", "WARN", "Cannot read state filesystem capacity"),
    }
    if let Ok(text) = fs::read_to_string("/proc/meminfo")
        && let Some(line) = text.lines().find(|l| l.starts_with("MemAvailable:"))
        && let Some(kib) = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u64>().ok())
    {
        report.add(
            "Available RAM",
            if kib < 16 * 1024 { "WARN" } else { "OK" },
            format!("{kib} KiB"),
        );
    }
    report.add(
        "Saved login",
        if state.join("credentials.json").is_file() {
            "OK"
        } else {
            "WARN"
        },
        if state.join("credentials.json").is_file() {
            "Credentials file exists; contents are not read"
        } else {
            "No saved credentials; login may be needed"
        },
    );
    for (source, path) in [
        ("receiver", crate::supervisor::LOG),
        ("supervisor", "/tmp/spotify-connect-supervisor.log"),
    ] {
        if let Ok(summary) = log_summary(source, Path::new(path)) {
            report.logs.push(summary);
        }
    }
    report
}
fn save(report: &Report, path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(report)?)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })();
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}
pub fn run(state: &Path, output: Option<&Path>) -> io::Result<()> {
    let mut report = collect(state);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(3)
        .build()?;
    runtime.block_on(network(&mut report));
    runtime.shutdown_timeout(Duration::from_millis(100));
    println!("Spotiya {} diagnostics", env!("CARGO_PKG_VERSION"));
    for check in &report.checks {
        println!("[{}] {}: {}", check.result, check.name, check.detail);
    }
    for log in &report.logs {
        for (category, count) in &log.counts {
            println!("[LOG] {}: {category}={count}", log.source);
        }
    }
    println!(
        "Log summaries cover the last 64 KiB per file; historical errors may already be resolved."
    );
    if let Some(path) = output {
        save(&report, path)?;
        println!(
            "Report saved: {} (mode 0600; no raw logs, account data or tokens)",
            path.display()
        );
    }
    if report.checks.iter().any(|c| c.result == "FAIL") {
        Err(error("Doctor found failed checks; see results above"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_never_copy_account_metadata_error_payloads_or_raw_log_lines() {
        let temp =
            installation::Temporary::new(&std::env::temp_dir(), "spotiya-doctor-test").unwrap();
        let path = temp.0.join("log");
        fs::write(&path,b"[ERROR librespot_core::audio_key] Bearer SECRET_TOKEN\n[WARN librespot_core::http_client] https://private/?token=SECRET_TOKEN\nreceiver: private-user@example.com\n").unwrap();
        let mut report = Report::new();
        report.logs.push(log_summary("receiver", &path).unwrap());
        report.status(
            Some(123),
            true,
            Some(Status {
                receiver_pid: 123,
                title: "SECRET_TITLE".into(),
                artist: "SECRET_ARTIST".into(),
                name: "PRIVATE_HOSTNAME".into(),
                error: "SECRET_TOKEN".into(),
                ..Default::default()
            }),
        );
        let json = serde_json::to_string(&report).unwrap();
        for secret in [
            "SECRET_",
            "private-user",
            "PRIVATE_HOSTNAME",
            "https://private",
        ] {
            assert!(!json.contains(secret));
        }
        assert_eq!(report.logs[0].counts["audio_key"], 1);
        assert_eq!(report.logs[0].counts["network"], 1);
    }
    #[test]
    fn log_categories_count_failures_but_not_progress() {
        for (line, expected) in [
            (
                "receiver: connect failed (Permission denied { Login failed with reason: Bad credentials }); discovery remains available",
                Some("authentication"),
            ),
            (
                "receiver: connect failed (No valid authentication credentials { token expired }); discovery remains available",
                Some("authentication"),
            ),
            (
                "receiver: connect failed (Unauthenticated); discovery remains available",
                Some("authentication"),
            ),
            (
                "receiver: connect failed (PermissionDenied); discovery remains available",
                Some("authentication"),
            ),
            (
                "[WARN librespot_core::dealer] Websocket connection failed: IO error",
                Some("network"),
            ),
            (
                "receiver: connect failed (Service unavailable { transport returned no data })",
                Some("network"),
            ),
            (
                "supervisor: receiver exited (Ok(ExitStatus(unix_wait_status(256)))); retry in 2s",
                Some("receiver_restart"),
            ),
            (
                "ALSA lib pcm.c:8526:(snd_pcm_recover) underrun occurred",
                Some("audio_device"),
            ),
            (
                "[WARN librespot_playback::audio_backend::alsa] Error writing from AlsaSink buffer",
                Some("audio_device"),
            ),
            (
                "[WARN librespot_core::session] Try another access point...",
                Some("other_warning_or_error"),
            ),
            (
                "[INFO librespot_audio::playplay] PlayPlay decoder ready",
                None,
            ),
            (
                "[INFO librespot_playback::audio_backend::alsa] Using AlsaSink with format: S16",
                None,
            ),
            (
                "[INFO librespot_core::playplay] AP audio key refused; trying PlayPlay",
                None,
            ),
            ("receiver: PlayPlay decoder configured on demand", None),
            ("supervisor: receiver started (pid 31719)", None),
        ] {
            assert_eq!(category(line), expected, "{line}");
        }
    }
    #[test]
    fn saved_reports_are_private_and_existing_files_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let temp =
            installation::Temporary::new(&std::env::temp_dir(), "spotiya-report-test").unwrap();
        let path = temp.0.join("report.json");
        save(&Report::new(), &path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, b"existing").unwrap();
        assert!(save(&Report::new(), &path).is_err());
        assert_eq!(fs::read(path).unwrap(), b"existing");
    }
    #[test]
    fn status_from_another_receiver_is_not_reported_as_current() {
        let mut report = Report::new();
        report.status(
            Some(123),
            false,
            Some(Status {
                receiver_pid: 456,
                connected: true,
                ..Default::default()
            }),
        );
        assert!(!report.receiver.status_current);
        assert!(!report.receiver.connected);
    }
}
