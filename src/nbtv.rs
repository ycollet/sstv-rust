//! NBTV Association club format: vertical 32-line monochrome, 12.5 frames/s.

pub mod media;
pub mod saving;

use std::collections::VecDeque;

use anyhow::{Result, ensure};

pub const WIDTH: usize = 32;
pub const HEIGHT: usize = 48;
pub const FRAME_PIXELS: usize = WIDTH * HEIGHT;
pub const LINE_RATE: f64 = 400.0;
pub const FPS: f64 = 12.5;
pub const SYNC_LEVEL: f32 = -0.32;
pub const WHITE_LEVEL: f32 = 0.8;
pub const DEFAULT_THRESHOLD: f32 = -0.12;
const SYNC_SECONDS: f64 = 0.0002;
const PORCH_SECONDS: f64 = 0.00005;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    #[default]
    Classic,
    SeparateSync,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic (embedded sync)",
            Self::SeparateSync => "Separate sync (custom)",
        }
    }

    pub fn from_separate(separate: bool) -> Self {
        if separate {
            Self::SeparateSync
        } else {
            Self::Classic
        }
    }
}

pub fn sync_sample(position: usize, sample_rate: u32) -> f32 {
    let line_position = position as f64 * LINE_RATE / f64::from(sample_rate);
    if !(line_position.floor() as usize).is_multiple_of(WIDTH)
        && line_position.fract() / LINE_RATE < SYNC_SECONDS
    {
        SYNC_LEVEL
    } else {
        0.0
    }
}

pub struct SyncDelay {
    samples: Vec<f32>,
    position: usize,
}

impl SyncDelay {
    pub fn new(samples: usize) -> Self {
        Self {
            samples: vec![0.0; samples],
            position: 0,
        }
    }

    pub fn push(&mut self, sample: f32) -> f32 {
        if self.samples.is_empty() {
            return sample;
        }
        let delayed = self.samples[self.position];
        self.samples[self.position] = sample;
        self.position = (self.position + 1) % self.samples.len();
        delayed
    }
}

#[derive(Debug)]
pub struct Clip {
    pixels: Vec<u8>,
}

impl Clip {
    pub fn new(pixels: Vec<u8>) -> Result<Self> {
        ensure!(
            !pixels.is_empty() && pixels.len().is_multiple_of(FRAME_PIXELS),
            "NBTV source must contain complete 32x48 grayscale frames"
        );
        Ok(Self { pixels })
    }

    pub fn frames(&self) -> usize {
        self.pixels.len() / FRAME_PIXELS
    }

    pub fn samples(&self, sample_rate: u32) -> usize {
        frame_samples(self.frames(), sample_rate)
    }

    pub fn sample(&self, position: usize, sample_rate: u32) -> f32 {
        let frame = frame_index(position, sample_rate).min(self.frames() - 1);
        sample_frame(
            &self.pixels[frame * FRAME_PIXELS..(frame + 1) * FRAME_PIXELS],
            position,
            sample_rate,
        )
    }
}

pub(crate) fn frame_samples(frames: usize, sample_rate: u32) -> usize {
    // Ceil the total duration, not individual line durations (110.25 samples at 44.1 kHz).
    (frames as u64 * u64::from(sample_rate) * 2).div_ceil(25) as usize
}

pub(crate) fn frame_index(position: usize, sample_rate: u32) -> usize {
    (position as f64 * LINE_RATE / f64::from(sample_rate)).floor() as usize / WIDTH
}

pub(crate) fn sample_frame(pixels: &[u8], position: usize, sample_rate: u32) -> f32 {
    let line_position = position as f64 * LINE_RATE / f64::from(sample_rate);
    let line_index = line_position.floor() as usize;
    let column = line_index % WIDTH;
    let phase = line_position.fract() / LINE_RATE;
    // The omitted pulse is the boundary between line 32 and line 1.
    if phase < SYNC_SECONDS {
        return if column == 0 { 0.0 } else { SYNC_LEVEL };
    }
    let active_start = SYNC_SECONDS + PORCH_SECONDS;
    let active_end = 1.0 / LINE_RATE - PORCH_SECONDS;
    if phase < active_start || phase >= active_end {
        return 0.0;
    }
    let y = (1.0 - (phase - active_start) / (active_end - active_start)) * (HEIGHT - 1) as f64;
    let y0 = y.floor() as usize;
    let y1 = (y0 + 1).min(HEIGHT - 1);
    let x = WIDTH - 1 - column;
    let a = f32::from(pixels[y0 * WIDTH + x]);
    let b = f32::from(pixels[y1 * WIDTH + x]);
    (a + (b - a) * y.fract() as f32) / 255.0 * WHITE_LEVEL
}

/// A sync-edge receiver. A double line interval identifies the missing pulse.
pub struct Decoder {
    sample_rate: u32,
    history: VecDeque<f32>,
    position: usize,
    pulse_start: Option<usize>,
    previous_sync: Option<usize>,
    line: Option<usize>,
    pixels: Vec<u8>,
}

impl Decoder {
    pub fn new(sample_rate: u32) -> Result<Self> {
        ensure!(
            sample_rate >= 32_000,
            "NBTV requires a JACK sample rate of at least 32000 Hz"
        );
        Ok(Self {
            sample_rate,
            history: VecDeque::new(),
            position: 0,
            pulse_start: None,
            previous_sync: None,
            line: None,
            pixels: vec![0; FRAME_PIXELS],
        })
    }

    pub fn locked(&self) -> bool {
        self.line.is_some()
    }

    fn read_line(&mut self, start: f64, duration: f64, line: usize) {
        let oldest = self.position + 1 - self.history.len();
        let sync_fraction = SYNC_SECONDS * LINE_RATE;
        let porch_fraction = PORCH_SECONDS * LINE_RATE;
        for y in 0..HEIGHT {
            let fraction = sync_fraction
                + porch_fraction
                + (1.0 - (y as f64 + 0.5) / HEIGHT as f64)
                    * (1.0 - sync_fraction - 2.0 * porch_fraction);
            // Sample inside the active interval, not its black porch edge.
            let at = (start + fraction * duration)
                .min(start + (1.0 - porch_fraction) * duration - 1.0)
                .round() as usize;
            if let Some(sample) = at.checked_sub(oldest).and_then(|i| self.history.get(i)) {
                self.pixels[y * WIDTH + WIDTH - 1 - line] =
                    (sample / WHITE_LEVEL * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    pub fn push(&mut self, sample: f32, threshold: f32) -> Option<Vec<u8>> {
        self.push_channels(sample, sample, threshold)
    }

    pub fn push_channels(&mut self, picture: f32, sync: f32, threshold: f32) -> Option<Vec<u8>> {
        self.history.push_back(picture);
        let period = f64::from(self.sample_rate) / LINE_RATE;
        while self.history.len() > (period * 3.0).ceil() as usize + 32 {
            self.history.pop_front();
        }
        let mut frame = None;
        if sync < threshold {
            if self.pulse_start.is_none() {
                self.pulse_start = Some(self.position);
            }
        } else if let Some(start) = self.pulse_start.take() {
            let width = (self.position - start) as f64 / f64::from(self.sample_rate);
            if (0.00008..=0.0004).contains(&width) {
                if let Some(previous) = self.previous_sync {
                    let spacing = (start - previous) as f64;
                    if (1.85 * period..=2.15 * period).contains(&spacing) {
                        if self.line == Some(WIDTH - 1) {
                            self.read_line(previous as f64, spacing / 2.0, WIDTH - 1);
                            frame = Some(self.pixels.clone());
                        }
                        self.pixels.fill(0);
                        self.read_line(previous as f64 + spacing / 2.0, spacing / 2.0, 0);
                        self.line = Some(1);
                    } else if (0.85 * period..=1.15 * period).contains(&spacing) {
                        if let Some(line) = self.line {
                            if line < WIDTH - 1 {
                                self.read_line(previous as f64, spacing, line);
                                self.line = Some(line + 1);
                            } else {
                                self.line = None;
                            }
                        }
                    } else {
                        self.line = None;
                    }
                }
                self.previous_sync = Some(start);
            }
        }
        if self
            .previous_sync
            .is_some_and(|last| (self.position - last) as f64 > 3.0 * period)
        {
            self.line = None;
            self.previous_sync = None;
        }
        self.position += 1;
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(frames: usize) -> Clip {
        Clip::new(
            (0..frames)
                .flat_map(|frame| {
                    (0..HEIGHT).flat_map(move |y| {
                        (0..WIDTH).map(move |x| ((x * 4 + y * 2 + frame * 20) % 256) as u8)
                    })
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn raster_round_trip_preserves_orientation_at_fractional_and_integer_line_rates() {
        for sample_rate in [44_100, 48_000, 96_000] {
            let clip = chart(5);
            let mut decoder = Decoder::new(sample_rate).unwrap();
            let mut received = Vec::new();
            for position in 0..clip.samples(sample_rate) {
                if let Some(frame) =
                    decoder.push(clip.sample(position, sample_rate), DEFAULT_THRESHOLD)
                {
                    received.push(frame);
                }
            }
            assert!(received.len() >= 3);
            for (index, frame) in received.iter().enumerate() {
                let expected = &clip.pixels[(index + 1) * FRAME_PIXELS..(index + 2) * FRAME_PIXELS];
                let error: f64 = frame
                    .iter()
                    .zip(expected)
                    .map(|(a, b)| f64::from(a.abs_diff(*b)))
                    .sum::<f64>()
                    / FRAME_PIXELS as f64;
                assert!(error < 6.0, "{sample_rate} Hz: mean error {error}");
            }
        }
    }

    #[test]
    fn silence_loses_lock_and_a_new_stream_reacquires() {
        let clip = chart(5);
        let mut decoder = Decoder::new(48_000).unwrap();
        let feed = |decoder: &mut Decoder| {
            (0..clip.samples(48_000))
                .filter_map(|position| {
                    decoder.push(clip.sample(position, 48_000), DEFAULT_THRESHOLD)
                })
                .count()
        };
        assert!(feed(&mut decoder) >= 3);
        for _ in 0..4800 {
            assert!(decoder.push(0.0, DEFAULT_THRESHOLD).is_none());
        }
        assert!(!decoder.locked());
        assert!(feed(&mut decoder) >= 3);
    }

    #[test]
    fn line_pulses_are_narrow_and_the_frame_boundary_pulse_is_missing() {
        let clip = chart(1);
        let samples: Vec<_> = (0..3840).map(|i| clip.sample(i, 48_000)).collect();
        let starts = samples
            .windows(2)
            .filter(|pair| pair[0] >= 0.0 && pair[1] < 0.0)
            .count();
        assert_eq!(starts, 31);
        assert!(samples[..9].iter().all(|sample| *sample == 0.0));
        assert_eq!(samples[120], SYNC_LEVEL);
        assert!(samples[130] >= 0.0);
    }

    #[test]
    fn independently_constructed_missing_pulse_raster_decodes() {
        let mut decoder = Decoder::new(48_000).unwrap();
        let mut frames = Vec::new();
        // 2.5 ms lines at 48 kHz: 120 samples, ten sync samples,
        // three back-porch samples, 105 active samples, two front-porch samples.
        // Each vertical column has a distinct constant brightness.
        for _ in 0..4 {
            for line in 0..32 {
                for sample in 0..120 {
                    let value = match sample {
                        0..10 if line != 0 => -0.32,
                        13..118 => (31 - line) as f32 / 31.0 * 0.8,
                        _ => 0.0,
                    };
                    if let Some(frame) = decoder.push(value, DEFAULT_THRESHOLD) {
                        frames.push(frame);
                    }
                }
            }
        }
        assert_eq!(frames.len(), 2);
        for frame in frames {
            for row in frame.as_chunks::<32>().0 {
                for (x, pixel) in row.iter().enumerate() {
                    assert!(pixel.abs_diff((x * 255 / 31) as u8) <= 1);
                }
            }
        }
    }

    #[test]
    fn clean_separate_sync_locks_even_when_the_entire_picture_is_below_sync_threshold() {
        let clip = chart(5);
        for sample_rate in [44_100, 48_000] {
            let mut separate = Decoder::new(sample_rate).unwrap();
            let mut embedded = Decoder::new(sample_rate).unwrap();
            let mut frames = 0;
            for position in 0..clip.samples(sample_rate) {
                let picture = clip.sample(position, sample_rate).max(0.0) * 0.1 - 0.8;
                let sync = sync_sample(position, sample_rate);
                assert!(embedded.push(picture, DEFAULT_THRESHOLD).is_none());
                if let Some(frame) = separate.push_channels(picture, sync, DEFAULT_THRESHOLD) {
                    frames += 1;
                    assert!(frame.iter().all(|pixel| *pixel == 0));
                }
            }
            assert!(frames >= 3);
            assert!(separate.locked());
            assert!(!embedded.locked());
        }
    }

    #[test]
    fn matching_sync_delay_preserves_the_delayed_picture_and_missing_sync_loses_lock() {
        let clip = chart(6);
        let rate = 48_000;
        let mut picture_delay = SyncDelay::new(737);
        let mut sync_delay = SyncDelay::new(737);
        let mut baseline = Decoder::new(rate).unwrap();
        let mut delayed = Decoder::new(rate).unwrap();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        for position in 0..clip.samples(rate) + 737 {
            let (picture, sync) = if position < clip.samples(rate) {
                (
                    clip.sample(position, rate).max(0.0),
                    sync_sample(position, rate),
                )
            } else {
                (0.0, 0.0)
            };
            if let Some(frame) = baseline.push_channels(picture, sync, DEFAULT_THRESHOLD) {
                expected.push(frame);
            }
            if let Some(frame) = delayed.push_channels(
                picture_delay.push(picture),
                sync_delay.push(sync),
                DEFAULT_THRESHOLD,
            ) {
                actual.push(frame);
            }
        }
        assert!(actual.len() >= 4);
        assert_eq!(actual, expected);
        for _ in 0..4800 {
            assert!(
                delayed
                    .push_channels(-1.0, 0.0, DEFAULT_THRESHOLD)
                    .is_none()
            );
        }
        assert!(!delayed.locked());
    }
}
