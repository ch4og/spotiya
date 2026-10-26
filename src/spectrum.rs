use librespot::playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    time::Instant,
};

const BANDS: usize = 4;
const WINDOW: u32 = SAMPLE_RATE / 20;
const CENTERS: [f32; BANDS] = [100.0, 400.0, 1600.0, 6400.0];

pub struct Spectrum {
    enabled: AtomicBool,
    levels: [AtomicU8; BANDS],
    updated: AtomicU32,
    origin: Instant,
}
impl Spectrum {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            levels: std::array::from_fn(|_| AtomicU8::new(0)),
            updated: AtomicU32::new(0),
            origin: Instant::now(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        self.clear();
    }
    fn clear(&self) {
        for level in &self.levels {
            level.store(0, Ordering::Relaxed);
        }
        self.updated.store(0, Ordering::Release);
    }
    pub(crate) fn publish(&self, levels: [u8; BANDS]) {
        for (slot, level) in self.levels.iter().zip(levels) {
            slot.store(level, Ordering::Relaxed);
        }
        self.updated.store(
            (self.origin.elapsed().as_millis() as u32).wrapping_add(1),
            Ordering::Release,
        );
    }
    pub fn frame(&self) -> Option<[u8; BANDS]> {
        let updated = self.updated.load(Ordering::Acquire);
        if !self.enabled()
            || updated == 0
            || (self.origin.elapsed().as_millis() as u32)
                .wrapping_add(1)
                .wrapping_sub(updated)
                > 300
        {
            return None;
        }
        Some(std::array::from_fn(|i| {
            self.levels[i].load(Ordering::Relaxed)
        }))
    }
}

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
        let omega = 2.0 * std::f32::consts::PI * center / SAMPLE_RATE as f32;
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
struct Analyzer {
    bands: [Band; BANDS],
    energy: [f32; BANDS],
    envelope: [f32; BANDS],
    frames: u32,
}
impl Analyzer {
    fn new() -> Self {
        Self {
            bands: CENTERS.map(Band::new),
            energy: [0.0; BANDS],
            envelope: [0.0; BANDS],
            frames: 0,
        }
    }
    fn feed(&mut self, samples: &[f64], spectrum: &Spectrum) {
        for frame in samples.chunks_exact(usize::from(NUM_CHANNELS)) {
            for (channel, sample) in frame.iter().enumerate() {
                let input = if sample.is_finite() {
                    (*sample as f32).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                for (band, energy) in self.bands.iter_mut().zip(&mut self.energy) {
                    let output = band.sample(channel, input);
                    *energy += output * output;
                }
            }
            self.frames += 1;
            if self.frames == WINDOW {
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
                spectrum.publish(levels);
                self.frames = 0;
            }
        }
    }
}

pub struct VisualSink {
    inner: Box<dyn Sink>,
    spectrum: Arc<Spectrum>,
    analyzer: Analyzer,
    analyzing: bool,
    ducking: Arc<crate::ducking::Ducking>,
    gain: crate::ducking::Gain,
    gate: Arc<crate::airplay::Gate>,
}
impl VisualSink {
    pub fn new(
        inner: Box<dyn Sink>,
        spectrum: Arc<Spectrum>,
        ducking: Arc<crate::ducking::Ducking>,
        gate: Arc<crate::airplay::Gate>,
    ) -> Self {
        Self {
            inner,
            spectrum,
            analyzer: Analyzer::new(),
            analyzing: false,
            ducking,
            gain: crate::ducking::Gain::new(),
            gate,
        }
    }
}
impl Sink for VisualSink {
    fn start(&mut self) -> SinkResult<()> {
        self.analyzer = Analyzer::new();
        self.spectrum.clear();
        self.analyzing = false;
        self.gate.spotify_open.store(true, Ordering::Release);
        let result = self
            .gate
            .reclaim()
            .map_err(|e| {
                librespot::playback::audio_backend::SinkError::ConnectionRefused(e.to_string())
            })
            .and_then(|()| self.inner.start());
        if result.is_err() {
            self.gate.close_spotify();
        }
        result?;
        Ok(())
    }
    fn stop(&mut self) -> SinkResult<()> {
        self.spectrum.clear();
        let result = self.inner.stop();
        self.gate.close_spotify();
        result
    }
    fn write(&mut self, mut packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let enabled = self.spectrum.enabled();
        if enabled {
            if !self.analyzing {
                self.analyzer = Analyzer::new();
            }
            if let Ok(samples) = packet.samples() {
                self.analyzer.feed(samples, &self.spectrum);
            }
        }
        self.analyzing = enabled;
        if let AudioPacket::Samples(samples) = &mut packet {
            self.gain.apply(samples, self.ducking.active());
        }
        self.inner.write(packet, converter)
    }
}

impl Drop for VisualSink {
    fn drop(&mut self) {
        let _ = self.inner.stop();
        self.gate.close_spotify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(frequency: f64, antiphase: bool) -> Vec<f64> {
        (0..SAMPLE_RATE / 2)
            .flat_map(|n| {
                let value = (2.0 * std::f64::consts::PI * frequency * f64::from(n)
                    / f64::from(SAMPLE_RATE))
                .sin()
                    * 0.25;
                [value, if antiphase { -value } else { value }]
            })
            .collect()
    }
    #[test]
    fn detects_frequency_bands_including_antiphase_stereo() {
        for (wanted, frequency) in CENTERS.into_iter().enumerate() {
            let spectrum = Spectrum::new(true);
            let mut analyzer = Analyzer::new();
            analyzer.feed(&tone(f64::from(frequency), true), &spectrum);
            let levels = spectrum.frame().unwrap();
            assert!(levels[wanted] > 40, "{levels:?}");
            assert_eq!(
                levels
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, value)| *value)
                    .unwrap()
                    .0,
                wanted,
                "{levels:?}"
            );
        }
    }
    #[test]
    fn silence_disable_and_packet_boundaries() {
        let samples = tone(400.0, false);
        let a = Spectrum::new(true);
        let b = Spectrum::new(true);
        Analyzer::new().feed(&samples, &a);
        let mut split = Analyzer::new();
        for packet in samples.chunks(512) {
            split.feed(packet, &b);
        }
        assert_eq!(a.frame(), b.frame());
        a.set_enabled(false);
        assert!(a.frame().is_none());
        b.clear();
        Analyzer::new().feed(&vec![0.0; WINDOW as usize * 2], &b);
        assert_eq!(b.frame(), Some([0; BANDS]));
    }
    #[test]
    fn analysis_leaves_pcm_unchanged() {
        use std::{cell::RefCell, rc::Rc};
        struct Capture(Rc<RefCell<Vec<f64>>>);
        impl Sink for Capture {
            fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
                self.0
                    .borrow_mut()
                    .extend_from_slice(packet.samples().unwrap());
                Ok(())
            }
        }
        let output = Rc::new(RefCell::new(Vec::new()));
        let spectrum = Arc::new(Spectrum::new(true));
        let mut sink = VisualSink::new(
            Box::new(Capture(output.clone())),
            spectrum.clone(),
            Arc::new(crate::ducking::Ducking::new()),
            Arc::new(crate::airplay::Gate::new()),
        );
        let samples = tone(100.0, false);
        let mut converter = Converter::new(None);
        sink.write(AudioPacket::Samples(samples.clone()), &mut converter)
            .unwrap();
        assert_eq!(*output.borrow(), samples);
        assert!(spectrum.frame().is_some());
        sink.stop().unwrap();
        assert!(spectrum.frame().is_none());
    }
    #[test]
    fn ducking_changes_only_spotify_pcm_in_place_and_restores_it() {
        use std::{cell::RefCell, rc::Rc};
        struct Capture(Rc<RefCell<Vec<f64>>>);
        impl Sink for Capture {
            fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
                *self.0.borrow_mut() = match packet {
                    AudioPacket::Samples(samples) => samples,
                    _ => panic!("expected PCM"),
                };
                Ok(())
            }
        }
        let output = Rc::new(RefCell::new(Vec::new()));
        let ducking = Arc::new(crate::ducking::Ducking::new());
        let mut sink = VisualSink::new(
            Box::new(Capture(output.clone())),
            Arc::new(Spectrum::new(false)),
            ducking.clone(),
            Arc::new(crate::airplay::Gate::new()),
        );
        let samples = vec![0.5; 10000];
        let pointer = samples.as_ptr();
        ducking.set_busy(true);
        let mut converter = Converter::new(None);
        sink.write(AudioPacket::Samples(samples), &mut converter)
            .unwrap();
        assert_eq!(output.borrow().as_ptr(), pointer);
        assert_eq!(output.borrow()[9999], 0.1);
        ducking.set_busy(false);
        sink.write(AudioPacket::Samples(vec![0.5; 30000]), &mut converter)
            .unwrap();
        assert_eq!(output.borrow()[29999], 0.5);
    }
}
