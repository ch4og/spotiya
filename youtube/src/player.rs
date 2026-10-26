//! Plays one YouTube track at a time: WebM/Opus arrives from `web`, is decoded
//! here, and is written to ALSA by a dedicated thread. Pausing silences its dmix
//! client; stopping or finishing closes it before handing the speaker back.
//!
//! The decoder runs at download speed, so the queue to the device is short and
//! bounded: decoding waits for the device instead of holding a whole track of
//! PCM (37 MB for three minutes) in memory.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::watch;

use crate::handover::Handover;
use crate::web::{Track, Web};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Opus decodes at most 120 ms per frame: 5760 samples per channel at 48 kHz.
const MAX_FRAME: usize = 5760;
/// Latency the reported position is pulled back by, so the seekbar matches the sound.
const LATENCY_MS: u64 = 400;
/// Decoded frames queued for the device: Opus frames are 20 ms, so about 0.3 s.
const QUEUE: usize = 16;
/// Silence after which the device is closed; short gaps between tracks keep it open.
const LINGER: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Stop,
    Play,
    Pause,
}

struct Shared {
    uri: Option<String>,
    duration_secs: u64,
    state: State,
    position_ms: u64,
    volume: u32,
    /// Bumped on every stop/seek so a stale download task knows to exit.
    generation: u64,
}

/// Snapshot handed to the Lounge reporter.
pub struct Status {
    pub state: State,
    pub position_ms: u64,
    pub volume: u32,
}

pub struct Loaded {
    track: Track,
    body: tokio::sync::mpsc::Receiver<Bytes>,
    video_id: String,
    start_ms: u64,
}

pub struct Player {
    web: Arc<Web>,
    handover: Arc<Handover>,
    shared: Arc<Mutex<Shared>>,
    events: watch::Sender<u64>,
    pcm: SyncSender<PcmMsg>,
}

/// Messages from the decode task to the ALSA writer thread.
enum PcmMsg {
    /// Decoded interleaved S16LE samples for the current generation.
    Samples(u64, Vec<i16>),
    /// Silence the device while keeping it open for a quick resume.
    Release,
    /// Close the device; acknowledged once the PCM is closed.
    Stop(tokio::sync::oneshot::Sender<()>),
    /// Play the queued tail and close the device after a natural end.
    Finish(tokio::sync::oneshot::Sender<()>),
    /// Close the device and stop the writer.
    Close(tokio::sync::oneshot::Sender<()>),
}

impl Player {
    pub fn new(web: Arc<Web>, handover: Arc<Handover>, device: String) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            uri: None,
            duration_secs: 0,
            state: State::Stop,
            position_ms: 0,
            volume: 100,
            generation: 0,
        }));
        let (events, _) = watch::channel(0);
        let writer_shared = shared.clone();
        let (pcm, rx) = mpsc::sync_channel(QUEUE);
        std::thread::Builder::new()
            .name("yt-alsa".into())
            .spawn(move || writer(device, rx, writer_shared))
            .expect("spawn ALSA writer");
        Self {
            web,
            handover,
            shared,
            events,
            pcm,
        }
    }

    /// A receiver that fires whenever playback state changes (for the Lounge reporter).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.events.subscribe()
    }

    pub fn status(&self) -> Status {
        let s = self.shared.lock().unwrap();
        Status {
            state: s.state,
            position_ms: s.position_ms,
            volume: s.volume,
        }
    }

    /// Loading has no playback side effects and can be cancelled by dropping it.
    pub async fn load(&self, video_id: &str, position_secs: f64) -> Result<Loaded, String> {
        let (track, body) = self.web.open(video_id).await?;
        Ok(Loaded {
            track,
            body,
            video_id: video_id.to_owned(),
            start_ms: (position_secs.max(0.0) * 1000.0) as u64,
        })
    }

    pub fn start(&self, loaded: Loaded, paused: bool) -> Track {
        let Loaded {
            track,
            body,
            video_id,
            start_ms,
        } = loaded;
        self.handover.title(&track.title);
        self.handover.progress(start_ms, track.duration_secs * 1000);
        let generation = {
            let mut s = self.shared.lock().unwrap();
            s.generation += 1;
            s.uri = Some(video_id);
            s.duration_secs = track.duration_secs;
            s.position_ms = start_ms;
            s.state = if paused { State::Pause } else { State::Play };
            s.generation
        };
        self.events.send_modify(|n| *n += 1);
        // A new track or a seek can follow a pause; the receiver must not keep
        // thinking it is paused while this plays.
        self.handover.paused(paused);
        self.spawn_decode(generation, body, start_ms);
        track
    }

    pub fn pause(&self) {
        let mut s = self.shared.lock().unwrap();
        if s.state == State::Play {
            s.state = State::Pause;
            // Best effort: if the queue is full the writer still closes after LINGER.
            let _ = self.pcm.try_send(PcmMsg::Release);
            let (position, duration) = (s.position_ms, s.duration_secs * 1000);
            drop(s);
            self.events.send_modify(|n| *n += 1);
            self.handover.progress(position, duration);
            self.handover.paused(true);
        }
    }

    pub fn resume(&self) {
        let mut s = self.shared.lock().unwrap();
        if s.state == State::Pause {
            s.state = State::Play;
            let (position, duration) = (s.position_ms, s.duration_secs * 1000);
            drop(s);
            self.events.send_modify(|n| *n += 1);
            self.handover.progress(position, duration);
            self.handover.paused(false);
        }
    }

    pub async fn stop(&self) {
        self.close_output(false, false).await;
    }

    pub async fn finish(&self) {
        self.close_output(false, true).await;
    }

    /// Re-anchors the receiver's progress bar; called periodically while playing.
    pub fn report_progress(&self) {
        let s = self.shared.lock().unwrap();
        if s.state == State::Play {
            let (position, duration) = (s.position_ms, s.duration_secs * 1000);
            drop(s);
            self.handover.progress(position, duration);
        }
    }

    /// Stops playback and waits (bounded) until the writer has closed the PCM.
    ///
    /// A process that exits while it holds the shared dmix leaves its IPC state
    /// behind, and the next client to open the speaker can then hang, Alice
    /// included. Every exit path must come through here first.
    pub async fn shutdown(&self) {
        self.close_output(true, false).await;
    }

    async fn close_output(&self, shutdown: bool, drain: bool) {
        {
            let mut s = self.shared.lock().unwrap();
            if !drain {
                s.generation += 1;
            }
            s.state = State::Stop;
            s.position_ms = 0;
            s.uri = None;
        }
        self.events.send_modify(|n| *n += 1);
        let (ack, done) = tokio::sync::oneshot::channel();
        let mut msg = if shutdown {
            PcmMsg::Close(ack)
        } else if drain {
            PcmMsg::Finish(ack)
        } else {
            PcmMsg::Stop(ack)
        };
        let closed = tokio::time::timeout(STALL + Duration::from_secs(1), async {
            loop {
                match self.pcm.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(back)) => {
                        msg = back;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(TrySendError::Disconnected(_)) => return true,
                }
            }
            done.await.is_ok()
        })
        .await
        .unwrap_or(false);
        if closed {
            self.handover.end();
        } else {
            tracing::warn!("[yt-player] ALSA writer did not close in time");
        }
    }

    pub fn set_volume(&self, volume: u32) {
        let volume = volume.min(100);
        self.shared.lock().unwrap().volume = volume;
        // With a receiver, its hardware mixer owns volume; standalone, decode()
        // attenuates in software from the stored value.
        self.handover.volume(volume);
    }

    /// Streams and decodes `body` on a task that stops as soon as its generation is superseded.
    fn spawn_decode(
        &self,
        generation: u64,
        body: tokio::sync::mpsc::Receiver<Bytes>,
        start_ms: u64,
    ) {
        let pcm = self.pcm.clone();
        let shared = self.shared.clone();
        let events = self.events.clone();
        let wake = events.subscribe();
        let handover = self.handover.clone();
        tokio::spawn(async move {
            if let Err(e) = decode(
                generation,
                body,
                start_ms,
                pcm,
                shared.clone(),
                handover,
                wake,
            )
            .await
            {
                tracing::warn!("[yt-player] gen {generation} ended: {e}");
            }
            let mut s = shared.lock().unwrap();
            if s.generation == generation && s.state != State::Pause {
                s.state = State::Stop;
                drop(s);
                events.send_modify(|n| *n += 1);
            }
        });
    }
}

async fn decode(
    generation: u64,
    mut body: tokio::sync::mpsc::Receiver<Bytes>,
    start_ms: u64,
    pcm: SyncSender<PcmMsg>,
    shared: Arc<Mutex<Shared>>,
    handover: Arc<Handover>,
    mut wake: watch::Receiver<u64>,
) -> Result<(), String> {
    let mut demux = Demux::default();
    let mut decoder =
        opus::Decoder::new(RATE, opus::Channels::Stereo).map_err(|e| e.to_string())?;
    let mut out = vec![0i16; MAX_FRAME * CHANNELS];
    let mut ended = false;
    loop {
        let Some((at, frame)) = demux.next() else {
            if ended {
                return Ok(());
            }
            // Wake on state changes too, so a stalled download cannot outlive its track.
            tokio::select! {
                chunk = body.recv() => match chunk {
                    Some(chunk) => demux.push(&chunk),
                    None => ended = true,
                },
                _ = wake.changed() => {
                    if shared.lock().unwrap().generation != generation {
                        return Ok(());
                    }
                }
            }
            continue;
        };
        // Pause here instead of buffering ahead, so a pause takes effect immediately.
        loop {
            let (state, current) = {
                let s = shared.lock().unwrap();
                (s.state, s.generation)
            };
            if current != generation {
                return Ok(());
            }
            if state != State::Pause {
                break;
            }
            let _ = pcm.try_send(PcmMsg::Release);
            let _ = wake.changed().await;
        }
        if at < start_ms {
            continue;
        }
        let samples = decoder
            .decode(&frame, &mut out, false)
            .map_err(|e| e.to_string())?;
        let volume = if handover.drives_volume() {
            100
        } else {
            shared.lock().unwrap().volume
        };
        let scaled = apply_gain(&out[..samples * CHANNELS], volume);
        // The first audio of a track reclaims the speaker from Spotify/AirPlay.
        if !handover.begin().await {
            return Err("speaker handover was refused".into());
        }
        if !queue(&pcm, &shared, generation, scaled).await {
            return Ok(());
        }
        let mut s = shared.lock().unwrap();
        if s.generation == generation {
            s.position_ms = at.saturating_sub(LATENCY_MS).max(start_ms);
        }
    }
}

/// Waits for room in the device queue without blocking the async runtime.
/// Returns false once this track is superseded or the writer is gone.
async fn queue(
    pcm: &SyncSender<PcmMsg>,
    shared: &Mutex<Shared>,
    generation: u64,
    samples: Vec<i16>,
) -> bool {
    let mut msg = PcmMsg::Samples(generation, samples);
    loop {
        match pcm.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                if shared.lock().unwrap().generation != generation {
                    return false;
                }
                msg = back;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// Software volume as an approximate perceptual curve, like the local player did.
fn apply_gain(samples: &[i16], volume: u32) -> Vec<i16> {
    if volume >= 100 {
        return samples.to_vec();
    }
    let g = (volume as f32 / 100.0).powi(2);
    samples.iter().map(|&s| (s as f32 * g) as i16).collect()
}

/// How long the device may refuse samples before it is treated as stuck.
const STALL: Duration = Duration::from_secs(2);
/// Fresh opens tried on a stuck device before the track is given up.
const REOPENS: u32 = 3;
/// A pause this long closes the device; a shorter one keeps it open.
const PAUSE_CLOSE: Duration = Duration::from_secs(300);

/// The device, opened on the first samples and kept open across pauses.
///
/// Every wait is bounded: an earlier blocking version hung for good when a fresh
/// dmix never started (two clients bringing the shared mixer up at once), which
/// left the speaker silent until the process was killed. Pausing keeps the
/// client attached, so resuming joins the running mixer instead of starting a
/// new one; dmix mixes clients, so Alice and Spotify are not blocked meanwhile.
struct Output {
    device: String,
    pcm: Option<alsa::PCM>,
    stuck: u32,
}

impl Output {
    fn write(&mut self, mut samples: &[i16]) -> Result<bool, String> {
        use alsa::pcm::State as PcmState;
        let opened = self.pcm.is_none();
        if opened {
            self.pcm =
                Some(open_device(&self.device).map_err(|e| format!("open {}: {e}", self.device))?);
        }
        let pcm = self.pcm.as_ref().expect("device opened");
        if pcm.state() == PcmState::Setup {
            pcm.prepare().map_err(|e| format!("prepare: {e}"))?;
        }
        let io = pcm.io_i16().map_err(|e| e.to_string())?;
        let mut progress = std::time::Instant::now();
        while !samples.is_empty() {
            match io.writei(samples) {
                Ok(frames) => {
                    samples = &samples[frames * CHANNELS..];
                    if frames > 0 {
                        progress = std::time::Instant::now();
                    }
                }
                // alsa-lib reports errors as negative errno values.
                Err(e) if e.errno().abs() == libc::EAGAIN => {
                    // The buffer is full. A stream that never started will not
                    // drain it, so start it by hand rather than wait forever.
                    if pcm.state() == PcmState::Prepared {
                        let _ = pcm.start();
                    }
                    if progress.elapsed() > STALL {
                        return Err(format!("device stalled in {:?}", pcm.state()));
                    }
                    let _ = pcm.wait(Some(100));
                }
                // Underruns and suspends recover with a prepare.
                Err(e) => pcm
                    .try_recover(e, true)
                    .map_err(|e| format!("recover: {e}"))?,
            }
        }
        Ok(opened)
    }

    /// Silences the device at once but keeps it open for a quick resume.
    fn pause(&mut self) {
        if let Some(pcm) = &self.pcm {
            let _ = pcm.drop();
        }
    }

    /// Lets what is buffered play out (bounded), then closes the device.
    fn finish(&mut self) {
        use alsa::pcm::State as PcmState;
        if let Some(pcm) = &self.pcm {
            if pcm.state() == PcmState::Running {
                let _ = pcm.drain();
                let deadline = std::time::Instant::now() + STALL;
                while pcm.state() == PcmState::Draining && std::time::Instant::now() < deadline {
                    let _ = pcm.wait(Some(100));
                }
            }
        }
        self.close();
    }

    fn close(&mut self) {
        if let Some(pcm) = self.pcm.take() {
            let _ = pcm.drop();
        }
    }
}

/// Owns the ALSA device and writes only the current track's samples to it.
fn writer(device: String, rx: Receiver<PcmMsg>, shared: Arc<Mutex<Shared>>) {
    let mut out = Output {
        device,
        pcm: None,
        stuck: 0,
    };
    // Analysed here, as samples go to the device, so the levels follow the sound.
    let mut spectrum = crate::spectrum::Spectrum::from_env();
    let mut given_up = None;
    let mut paused_for = Duration::ZERO;
    loop {
        match rx.recv_timeout(LINGER) {
            Ok(PcmMsg::Samples(generation, samples)) => {
                paused_for = Duration::ZERO;
                // A skip leaves the old track's tail queued; it must not play.
                if shared.lock().unwrap().generation != generation || given_up == Some(generation) {
                    continue;
                }
                match out.write(&samples) {
                    Ok(opened) => {
                        if opened {
                            spectrum.reset();
                        }
                        out.stuck = 0;
                        spectrum.feed(&samples);
                    }
                    Err(e) => {
                        out.close();
                        out.stuck += 1;
                        tracing::warn!("[yt-alsa] {e}; reopening ({}/{REOPENS})", out.stuck);
                        if out.stuck >= REOPENS {
                            tracing::error!(
                                "[yt-alsa] the speaker does not take audio; skipping this track"
                            );
                            given_up = Some(generation);
                            out.stuck = 0;
                        }
                    }
                }
            }
            Ok(PcmMsg::Release) => out.pause(),
            Ok(PcmMsg::Stop(ack)) => {
                out.close();
                let _ = ack.send(());
            }
            Ok(PcmMsg::Finish(ack)) => {
                out.finish();
                let _ = ack.send(());
            }
            Ok(PcmMsg::Close(ack)) => {
                out.close();
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                if shared.lock().unwrap().state == State::Pause {
                    paused_for += LINGER;
                    if paused_for >= PAUSE_CLOSE {
                        out.close();
                    }
                } else {
                    // The track ended or the download stalled.
                    out.finish();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    out.finish();
}

fn open_device(device: &str) -> Result<alsa::PCM, alsa::Error> {
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::Direction;
    // Non-blocking, so no call into ALSA can wait without a timeout.
    let pcm = PCM::new(device, Direction::Playback, true)?;
    {
        let hw = HwParams::any(&pcm)?;
        hw.set_channels(CHANNELS as u32)?;
        hw.set_rate(RATE, alsa::ValueOr::Nearest)?;
        hw.set_format(Format::s16())?;
        hw.set_access(Access::RWInterleaved)?;
        // ~0.5 s buffer keeps dmix fed without adding much seek-to-sound latency.
        hw.set_buffer_time_near(500_000, alsa::ValueOr::Nearest)?;
        hw.set_period_time_near(100_000, alsa::ValueOr::Nearest)?;
        pcm.hw_params(&hw)?;
    }
    pcm.prepare()?;
    Ok(pcm)
}

// ---------------------------------------------------------------------------
// Streaming WebM/Opus reader: yields (timestamp ms, Opus frame) per block.
// Descends Segment/Cluster/BlockGroup/Info; reads Timecode, TimecodeScale and
// SimpleBlock/Block; skips everything else. Validated against captured streams.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Demux {
    buf: Vec<u8>,
    pos: usize,
    skip: u64,
    cluster: u64,
    scale_ns: u64,
}

fn vint(d: &[u8], keep_marker: bool) -> Option<(u64, usize)> {
    let first = *d.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || d.len() < len {
        return None;
    }
    let mut v = if keep_marker {
        first as u64
    } else {
        (first as u64) & ((1u64 << (8 - len)) - 1)
    };
    for &b in &d[1..len] {
        v = (v << 8) | b as u64;
    }
    Some((v, len))
}

fn uint(d: &[u8]) -> u64 {
    d.iter().fold(0, |v, &b| (v << 8) | b as u64)
}

impl Demux {
    fn push(&mut self, data: &[u8]) {
        if self.pos > 1 << 16 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
    }

    fn next(&mut self) -> Option<(u64, Vec<u8>)> {
        loop {
            if self.skip > 0 {
                let n = self.skip.min((self.buf.len() - self.pos) as u64);
                self.pos += n as usize;
                self.skip -= n;
                if self.skip > 0 {
                    return None;
                }
            }
            let d = &self.buf[self.pos..];
            let (id, il) = vint(d, true)?;
            let (size, sl) = vint(&d[il..], false)?;
            let header = il + sl;
            let unknown = size == (1u64 << (7 * sl)) - 1;
            match id {
                // Segment, Cluster, BlockGroup, Info: descend into children.
                0x18538067 | 0x1F43B675 | 0xA0 | 0x1549A966 => {
                    self.pos += header;
                    continue;
                }
                0xE7 | 0x2AD7B1 | 0xA3 | 0xA1 if !unknown => {
                    let size = size as usize;
                    if d.len() < header + size {
                        return None;
                    }
                    let body = &d[header..header + size];
                    self.pos += header + size;
                    match id {
                        0xE7 => self.cluster = uint(body),
                        0x2AD7B1 => self.scale_ns = uint(body),
                        _ => {
                            let (_, tl) = vint(body, false)?;
                            let relative = i16::from_be_bytes([body[tl], body[tl + 1]]) as i64;
                            let scale = if self.scale_ns == 0 {
                                1_000_000
                            } else {
                                self.scale_ns
                            };
                            let ticks = (self.cluster as i64 + relative).max(0) as u64;
                            return Some((ticks * scale / 1_000_000, body[tl + 3..].to_vec()));
                        }
                    }
                }
                _ => {
                    self.pos += header;
                    self.skip = if unknown { 0 } else { size };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_reports_end_only_after_the_device_is_closed() {
        let (observed, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let handover = Arc::new(Handover::testing(move |command| {
            let _ = observed.send(command.split_whitespace().nth(1).unwrap().to_owned());
            true
        }));
        assert!(handover.begin().await);
        assert_eq!(commands.recv().await.as_deref(), Some("BEGIN"));
        let (pcm, writer) = mpsc::sync_channel(QUEUE);
        let (events, _) = watch::channel(0);
        let player = Player {
            web: Web::new(reqwest::Client::new(), std::env::temp_dir()),
            handover,
            shared: Arc::new(Mutex::new(Shared {
                uri: None,
                duration_secs: 0,
                state: State::Play,
                position_ms: 0,
                volume: 100,
                generation: 1,
            })),
            events,
            pcm,
        };
        let stopping = tokio::spawn(async move { player.stop().await });
        let message = tokio::task::spawn_blocking(move || {
            writer.recv_timeout(Duration::from_secs(5)).unwrap()
        })
        .await
        .unwrap();
        let PcmMsg::Stop(closed) = message else {
            panic!("expected device close")
        };
        assert!(
            commands.try_recv().is_err(),
            "END arrived before the PCM closed"
        );
        closed.send(()).unwrap();
        stopping.await.unwrap();
        assert_eq!(commands.recv().await.as_deref(), Some("END"));
    }

    #[tokio::test]
    async fn refused_handover_never_queues_pcm() {
        let shared = Arc::new(Mutex::new(Shared {
            uri: None,
            duration_secs: 1,
            state: State::Play,
            position_ms: 0,
            volume: 100,
            generation: 1,
        }));
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let mut packet = vec![0; 256];
        let n = encoder
            .encode(&vec![0; 960 * CHANNELS], &mut packet)
            .unwrap();
        packet.truncate(n);
        assert!(packet.len() + 4 < 127);
        let mut webm = vec![0xa3, 0x80 | (packet.len() + 4) as u8, 0x81, 0, 0, 0x80];
        webm.extend(packet);
        let (body_tx, body) = tokio::sync::mpsc::channel(1);
        body_tx.send(Bytes::from(webm)).await.unwrap();
        drop(body_tx);
        let (pcm, output) = mpsc::sync_channel(QUEUE);
        let (_events, wake) = watch::channel(0);
        let handover = Arc::new(Handover::testing(|_| false));
        let result = decode(1, body, 0, pcm, shared, handover, wake).await;
        assert_eq!(result.unwrap_err(), "speaker handover was refused");
        assert!(
            output.try_recv().is_err(),
            "PCM was sent without speaker ownership"
        );
    }
}
