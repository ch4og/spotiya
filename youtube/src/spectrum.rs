//! Four-band levels of what the speaker is playing, for the receiver's spectrum
//! (TUI graph and the Mini 2 display). The same band filters and scaling as the
//! receiver's Spotify analyzer, so both sources look alike.
//!
//! Levels are sent as 4-byte datagrams to the socket the receiver names in
//! `SPOTIYA_YOUTUBE_LEVELS`; analysis only runs while the receiver's spectrum
//! setting (`SPOTIYA_SPECTRUM_SETTING`) is on.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const RATE: f32 = 48_000.0;
const BANDS: usize = 4;
/// One level update every 50 ms of audio, like the receiver's analyzer.
const WINDOW: u32 = 48_000 / 20;
const CENTERS: [f32; BANDS] = [100.0, 400.0, 1600.0, 6400.0];
/// How often the spectrum setting is re-read.
const SETTING_POLL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct Band {
    b0: f32,
    a1: f32,
    a2: f32,
    z1: [f32; 2],
    z2: [f32; 2],
}

impl Band {
    fn new(center: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * center / RATE;
        let alpha = omega.sin() / (2.0 * 0.7);
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            a1: -2.0 * omega.cos() / a0,
            a2: (1.0 - alpha) / a0,
            z1: [0.0; 2],
            z2: [0.0; 2],
        }
    }

    fn sample(&mut self, channel: usize, input: f32) -> f32 {
        let output = self.b0 * input + self.z1[channel];
        self.z1[channel] = -self.a1 * output + self.z2[channel];
        self.z2[channel] = -self.b0 * input - self.a2 * output;
        output
    }
}

pub struct Spectrum {
    socket: Option<(UnixDatagram, PathBuf)>,
    setting: Option<PathBuf>,
    enabled: bool,
    checked: Option<Instant>,
    bands: [Band; BANDS],
    energy: [f32; BANDS],
    envelope: [f32; BANDS],
    frames: u32,
}

impl Spectrum {
    pub fn from_env() -> Self {
        let target = std::env::var_os("SPOTIYA_YOUTUBE_LEVELS").map(PathBuf::from);
        let socket = target.and_then(|path| UnixDatagram::unbound().ok().map(|s| (s, path)));
        if let Some((socket, _)) = &socket {
            let _ = socket.set_nonblocking(true);
        }
        Self {
            socket,
            setting: std::env::var_os("SPOTIYA_SPECTRUM_SETTING").map(PathBuf::from),
            enabled: false,
            checked: None,
            bands: CENTERS.map(Band::new),
            energy: [0.0; BANDS],
            envelope: [0.0; BANDS],
            frames: 0,
        }
    }

    /// Starts a fresh analysis, e.g. after a pause or a new track.
    pub fn reset(&mut self) {
        self.bands = CENTERS.map(Band::new);
        self.energy = [0.0; BANDS];
        self.envelope = [0.0; BANDS];
        self.frames = 0;
    }

    fn active(&mut self) -> bool {
        if self.socket.is_none() {
            return false;
        }
        if self.checked.is_none_or(|at| at.elapsed() >= SETTING_POLL) {
            self.checked = Some(Instant::now());
            self.enabled = self
                .setting
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .is_some_and(|s| s.trim() == "1");
        }
        self.enabled
    }

    /// Feeds interleaved stereo samples just written to the device.
    pub fn feed(&mut self, samples: &[i16]) {
        if !self.active() {
            return;
        }
        for frame in samples.as_chunks::<2>().0 {
            for (channel, &sample) in frame.iter().enumerate() {
                let input = f32::from(sample) / 32768.0;
                for (band, energy) in self.bands.iter_mut().zip(&mut self.energy) {
                    let output = band.sample(channel, input);
                    *energy += output * output;
                }
            }
            self.frames += 1;
            if self.frames == WINDOW {
                self.frames = 0;
                let levels = self.levels();
                if let Some((socket, path)) = &self.socket {
                    // The receiver may be restarting; a dropped update is harmless.
                    let _ = socket.send_to(&levels, path);
                }
            }
        }
    }

    fn levels(&mut self) -> [u8; BANDS] {
        let mut levels = [0; BANDS];
        for ((energy, envelope), level) in self
            .energy
            .iter_mut()
            .zip(&mut self.envelope)
            .zip(&mut levels)
        {
            let rms = (*energy / (WINDOW * 2) as f32).sqrt();
            let target = ((20.0 * rms.max(1e-7).log10() + 54.0) / 48.0).clamp(0.0, 1.0);
            let speed = if target > *envelope { 0.8 } else { 0.2 };
            *envelope += (target - *envelope) * speed;
            *level = (*envelope * 100.0).round() as u8;
            *energy = 0.0;
        }
        levels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frequency: f32, seconds: f32) -> Vec<i16> {
        (0..(RATE * seconds) as usize)
            .flat_map(|n| {
                let v = (2.0 * std::f32::consts::PI * frequency * n as f32 / RATE).sin() * 0.25;
                let s = (v * 32767.0) as i16;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_tone_lights_its_own_band_and_levels_reach_the_receiver() {
        let dir = std::env::temp_dir().join(format!("yt-spectrum-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("levels");
        let _ = std::fs::remove_file(&path);
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let setting = dir.join("spectrum-enabled");
        std::fs::write(&setting, "1\n").unwrap();
        let mut spectrum = Spectrum {
            socket: Some((UnixDatagram::unbound().unwrap(), path.clone())),
            setting: Some(setting.clone()),
            enabled: false,
            checked: None,
            bands: CENTERS.map(Band::new),
            energy: [0.0; BANDS],
            envelope: [0.0; BANDS],
            frames: 0,
        };
        spectrum.feed(&tone(400.0, 0.5));
        let mut last = [0u8; BANDS];
        let mut got = 0;
        while let Ok(n) = receiver.recv(&mut last) {
            assert_eq!(n, BANDS);
            got += 1;
            if got == 10 {
                break;
            }
        }
        assert_eq!(got, 10, "0.5 s of audio is ten 50 ms windows");
        let loudest = (0..BANDS).max_by_key(|&i| last[i]).unwrap();
        assert_eq!(loudest, 1, "400 Hz belongs to the second band: {last:?}");

        // Turned off in the receiver: nothing is analysed or sent.
        std::fs::write(&setting, "0\n").unwrap();
        spectrum.checked = None;
        spectrum.feed(&tone(400.0, 0.2));
        receiver
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(receiver.recv(&mut last).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
