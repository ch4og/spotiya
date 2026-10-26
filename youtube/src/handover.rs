//! Yields the speaker between YouTube, Spotify and Alice through the receiver's
//! control socket, the same way the AirPlay add-on does. The receiver passes a
//! one-shot token in `SPOTIYA_YOUTUBE_TOKEN`; without it (standalone testing)
//! the hooks are no-ops and YouTube simply shares the ALSA dmix.
//!
//! All hooks go through one sender thread so they reach the receiver in order:
//! an END overtaken by the next track's BEGIN would release the speaker mid-song.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

const SOCKET: &str = "/run/spotiya.control";
/// The receiver's control socket reads at most 512 bytes per command.
const TITLE_MAX: usize = 300;

type Job = (String, Option<mpsc::Sender<bool>>);

pub struct Handover {
    token: Option<String>,
    /// True once the receiver has handed us the speaker for the current run.
    active: AtomicBool,
    epoch: AtomicU64,
    /// Title of the current track, re-sent whenever the speaker is claimed.
    title: Mutex<String>,
    /// Last (position, duration) in ms, re-sent with the title on a claim.
    progress: Mutex<Option<(u64, u64)>>,
    jobs: Option<mpsc::Sender<Job>>,
}

impl Handover {
    #[cfg(test)]
    pub(crate) fn testing(exchange: impl Fn(&str) -> bool + Send + 'static) -> Self {
        Self::with_exchange(Some("test-token".into()), exchange)
    }

    pub fn new() -> Self {
        let token = std::env::var("SPOTIYA_YOUTUBE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        if token.is_none() {
            tracing::info!("[handover] no token; running without Spotify/Alice handover");
        }
        Self::with_exchange(token, exchange)
    }

    fn with_exchange(
        token: Option<String>,
        exchange: impl Fn(&str) -> bool + Send + 'static,
    ) -> Self {
        let jobs = token.as_ref().map(|_| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("yt-handover".into())
                .spawn(move || {
                    for (command, reply) in rx {
                        let ok = exchange(&command);
                        if let Some(reply) = reply {
                            let _ = reply.send(ok);
                        }
                    }
                })
                .expect("spawn handover thread");
            tx
        });
        Self {
            token,
            active: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            title: Mutex::new(String::new()),
            progress: Mutex::new(None),
            jobs,
        }
    }

    /// Whether volume should drive the receiver's hardware mixer rather than
    /// attenuate in software. True exactly when we are integrated with a receiver.
    pub fn drives_volume(&self) -> bool {
        self.token.is_some()
    }

    /// Claims the speaker before the first sound of a run. Idempotent and cheap
    /// once active; the socket exchange only happens on the transition.
    pub async fn begin(&self) -> bool {
        if self.token.is_none() || self.active.load(Ordering::Acquire) {
            return true;
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        let (reply, result) = mpsc::channel();
        if !self.send("BEGIN", Some(reply)) {
            return false;
        }
        let ok = tokio::task::spawn_blocking(move || result.recv().unwrap_or(false))
            .await
            .unwrap_or(false);
        if ok && self.epoch.load(Ordering::Acquire) == epoch {
            self.active.store(true, Ordering::Release);
            let title = self.title.lock().unwrap().clone();
            if !title.is_empty() {
                self.send(&format!("TITLE {title}"), None);
            }
            if let Some((position, duration)) = *self.progress.lock().unwrap() {
                self.send(&format!("POSITION {position} {duration}"), None);
            }
            true
        } else {
            false
        }
    }

    /// Releases the speaker when playback stops for good.
    pub fn end(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.active.store(false, Ordering::Release);
        // Also cancel an in-flight BEGIN; the worker preserves BEGIN/END order.
        self.send("END", None);
    }

    /// Mirrors the volume to the receiver's mixer while we hold the speaker.
    pub fn volume(&self, percent: u32) {
        if self.active.load(Ordering::Acquire) {
            self.send(&format!("VOLUME {}", percent.min(100)), None);
        }
    }

    /// Shows the current track in the receiver's status while we hold the speaker.
    pub fn title(&self, title: &str) {
        let title: String = title
            .chars()
            .filter(|c| !c.is_control())
            .take(TITLE_MAX)
            .collect();
        *self.title.lock().unwrap() = title.clone();
        if self.active.load(Ordering::Acquire) && !title.is_empty() {
            self.send(&format!("TITLE {title}"), None);
        }
    }

    /// Gives the receiver an anchor for its progress bar; it counts on from there
    /// while the track plays, so this is only needed on changes and now and then.
    pub fn progress(&self, position_ms: u64, duration_ms: u64) {
        *self.progress.lock().unwrap() = Some((position_ms, duration_ms));
        if self.active.load(Ordering::Acquire) {
            self.send(&format!("POSITION {position_ms} {duration_ms}"), None);
        }
    }

    /// Tells the receiver we paused (the speaker stays ours) or resumed.
    pub fn paused(&self, paused: bool) {
        if self.active.load(Ordering::Acquire) {
            let state = if paused { "paused" } else { "playing" };
            self.send(&format!("STATE {state}"), None);
        }
    }

    /// Queues `YOUTUBE <kind> <token> [args]`; false without a receiver.
    fn send(&self, kind: &str, reply: Option<mpsc::Sender<bool>>) -> bool {
        let (Some(token), Some(jobs)) = (&self.token, &self.jobs) else {
            return false;
        };
        // The token goes second, before free-form arguments such as a title.
        let (verb, rest) = kind.split_once(' ').unwrap_or((kind, ""));
        let command = if rest.is_empty() {
            format!("YOUTUBE {verb} {token}")
        } else {
            format!("YOUTUBE {verb} {token} {rest}")
        };
        jobs.send((command, reply)).is_ok()
    }
}

/// Sends one command and waits for the receiver's `OK`.
fn exchange(command: &str) -> bool {
    let Ok(mut socket) = UnixStream::connect(SOCKET) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(2)));
    if writeln!(socket, "{command}").is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply).is_ok() && reply.trim() == "OK"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn refused_handover_does_not_claim_the_speaker() {
        let handover = Handover::testing(|_| false);
        assert!(!handover.begin().await);
        assert!(!handover.active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn stop_cancels_an_in_flight_claim() {
        let (observed, mut commands) = tokio::sync::mpsc::unbounded_channel();
        let (allow, allowed) = mpsc::channel();
        let handover = Arc::new(Handover::testing(move |command| {
            let verb = command.split_whitespace().nth(1).unwrap().to_owned();
            observed.send(verb.clone()).unwrap();
            if verb == "BEGIN" {
                allowed.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            true
        }));
        let claiming = handover.clone();
        let claim = tokio::spawn(async move { claiming.begin().await });
        assert_eq!(commands.recv().await.as_deref(), Some("BEGIN"));
        handover.end();
        allow.send(()).unwrap();
        assert!(!claim.await.unwrap());
        assert!(!handover.active.load(Ordering::Acquire));
        assert_eq!(commands.recv().await.as_deref(), Some("END"));
    }
}
