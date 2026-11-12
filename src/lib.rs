use std::fs::File;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Result, bail};
use rtrb::Consumer;

mod cache;
pub mod nbtv;
pub mod transmit;

pub const POLL_INTERVAL: Duration = Duration::from_millis(2);

pub struct RunState {
    pub running: AtomicBool,
    server_stopped: AtomicBool,
    rate_changed: AtomicBool,
    pub xruns: AtomicUsize,
}

impl RunState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            server_stopped: AtomicBool::new(false),
            rate_changed: AtomicBool::new(false),
            xruns: AtomicUsize::new(0),
        })
    }

    pub fn install_interrupt_handler(self: &Arc<Self>) -> Result<()> {
        let state = Arc::clone(self);
        ctrlc::set_handler(move || state.stop())?;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn check(&self) -> Result<()> {
        if self.server_stopped.load(Ordering::Relaxed) {
            bail!("JACK server shut down");
        }
        if self.rate_changed.load(Ordering::Relaxed) {
            bail!("JACK sample rate changed; restart the application");
        }
        Ok(())
    }
}

pub struct Notifications {
    pub state: Arc<RunState>,
    pub sample_rate: u32,
}

impl jack::NotificationHandler for Notifications {
    unsafe fn shutdown(&mut self, _: jack::ClientStatus, _: &str) {
        self.state.server_stopped.store(true, Ordering::Relaxed);
        self.state.stop();
    }

    fn sample_rate(&mut self, _: &jack::Client, rate: jack::Frames) -> jack::Control {
        if rate != self.sample_rate {
            self.state.rate_changed.store(true, Ordering::Relaxed);
            self.state.stop();
            return jack::Control::Quit;
        }
        jack::Control::Continue
    }

    fn xrun(&mut self, _: &jack::Client) -> jack::Control {
        self.state.xruns.fetch_add(1, Ordering::Relaxed);
        jack::Control::Continue
    }
}

/// Empty queues are temporary starvation, not the end of a transmission.
pub struct LiveSamples {
    consumer: Consumer<i16>,
    state: Arc<RunState>,
    interrupt: Option<Arc<AtomicBool>>,
}

impl LiveSamples {
    pub fn new(consumer: Consumer<i16>, state: Arc<RunState>) -> Self {
        Self {
            consumer,
            state,
            interrupt: None,
        }
    }

    pub fn with_interrupt(mut self, interrupt: Arc<AtomicBool>) -> Self {
        self.interrupt = Some(interrupt);
        self
    }

    pub fn discard_buffered(&mut self) {
        let buffered = self.consumer.slots();
        for _ in 0..buffered {
            let _ = self.consumer.pop();
        }
    }
}

impl Iterator for LiveSamples {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        while self.state.is_running() {
            if self
                .interrupt
                .as_ref()
                .is_some_and(|interrupt| interrupt.load(Ordering::Acquire))
            {
                return None;
            }
            if let Ok(sample) = self.consumer.pop() {
                return Some(sample);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        None
    }
}

pub fn pcm_sample(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// Precomputed audio keeps synthesis and allocation out of the JACK callback.
#[derive(Debug)]
pub struct Playback {
    content: PlaybackContent,
    position: usize,
    repeat: bool,
    underruns: usize,
}

#[derive(Debug)]
enum PlaybackContent {
    Pcm(Vec<f32>),
    Nbtv {
        clip: Arc<nbtv::Clip>,
        sample_rate: u32,
    },
    CachedPcm {
        cache: cache::ReadAhead,
        length: usize,
    },
    CachedNbtv {
        cache: cache::ReadAhead,
        frames: usize,
        sample_rate: u32,
    },
}

impl Playback {
    pub fn new(samples: Vec<f32>, repeat: bool) -> Self {
        assert!(!samples.is_empty());
        Self {
            content: PlaybackContent::Pcm(samples),
            position: 0,
            repeat,
            underruns: 0,
        }
    }

    pub fn nbtv(clip: Arc<nbtv::Clip>, sample_rate: u32) -> Self {
        Self {
            content: PlaybackContent::Nbtv { clip, sample_rate },
            position: 0,
            repeat: false,
            underruns: 0,
        }
    }

    pub fn cached_pcm(file: File, length: usize, sample_rate: u32) -> Result<Self> {
        Ok(Self {
            content: PlaybackContent::CachedPcm {
                cache: cache::ReadAhead::new(
                    file,
                    length
                        .checked_mul(4)
                        .ok_or_else(|| anyhow::anyhow!("Audio cache is too large"))?,
                    (sample_rate as usize / 2).max(1) * 4,
                )?,
                length,
            },
            position: 0,
            repeat: false,
            underruns: 0,
        })
    }

    pub fn cached_nbtv(file: File, frames: usize, sample_rate: u32) -> Result<Self> {
        Ok(Self {
            content: PlaybackContent::CachedNbtv {
                cache: cache::ReadAhead::new(
                    file,
                    frames
                        .checked_mul(nbtv::FRAME_PIXELS)
                        .ok_or_else(|| anyhow::anyhow!("Video cache is too large"))?,
                    nbtv::FRAME_PIXELS * 8,
                )?,
                frames,
                sample_rate,
            },
            position: 0,
            repeat: false,
            underruns: 0,
        })
    }

    pub fn underruns(&self) -> usize {
        self.underruns
    }

    pub fn failed(&self) -> bool {
        match &self.content {
            PlaybackContent::CachedPcm { cache, .. }
            | PlaybackContent::CachedNbtv { cache, .. } => cache.failed(),
            _ => false,
        }
    }

    /// Returns true only after the final sample has left a previous callback.
    pub fn fill(&mut self, output: &mut [f32]) -> bool {
        self.fill_channels(output, None, false)
    }

    pub fn fill_with_sync(&mut self, output: &mut [f32], sync: &mut [f32], separate: bool) -> bool {
        assert_eq!(output.len(), sync.len());
        self.fill_channels(output, Some(sync), separate)
    }

    fn fill_channels(
        &mut self,
        output: &mut [f32],
        mut sync: Option<&mut [f32]>,
        separate: bool,
    ) -> bool {
        if let Some(sync) = &mut sync {
            sync.fill(0.0);
        }
        let length = self.len();
        if !self.repeat && self.position == length {
            output.fill(0.0);
            return true;
        }
        for (index, sample) in output.iter_mut().enumerate() {
            if self.position == length && self.repeat {
                self.restart();
            }
            if self.position < length {
                *sample = match &mut self.content {
                    PlaybackContent::Pcm(samples) => samples[self.position],
                    PlaybackContent::Nbtv { clip, sample_rate } => {
                        let picture = clip.sample(self.position, *sample_rate);
                        if separate {
                            if let Some(sync) = &mut sync {
                                sync[index] = nbtv::sync_sample(self.position, *sample_rate);
                            }
                            picture.max(0.0)
                        } else {
                            picture
                        }
                    }
                    PlaybackContent::CachedPcm { cache, .. } => {
                        if let Some(bytes) = cache.bytes_at(self.position * 4, 4) {
                            f32::from_le_bytes(
                                bytes.try_into().expect("An audio sample is four bytes"),
                            )
                        } else {
                            self.underruns += 1;
                            0.0
                        }
                    }
                    PlaybackContent::CachedNbtv {
                        cache,
                        frames,
                        sample_rate,
                    } => {
                        let frame = nbtv::frame_index(self.position, *sample_rate).min(*frames - 1);
                        if let Some(pixels) =
                            cache.bytes_at(frame * nbtv::FRAME_PIXELS, nbtv::FRAME_PIXELS)
                        {
                            let picture = nbtv::sample_frame(pixels, self.position, *sample_rate);
                            if separate {
                                if let Some(sync) = &mut sync {
                                    sync[index] = nbtv::sync_sample(self.position, *sample_rate);
                                }
                                picture.max(0.0)
                            } else {
                                picture
                            }
                        } else {
                            self.underruns += 1;
                            if separate && let Some(sync) = &mut sync {
                                sync[index] = nbtv::sync_sample(self.position, *sample_rate);
                            }
                            0.0
                        }
                    }
                };
                self.position += 1;
            } else {
                *sample = 0.0;
            }
        }
        false
    }

    pub fn restart(&mut self) {
        self.position = 0;
        match &mut self.content {
            PlaybackContent::CachedPcm { cache, .. }
            | PlaybackContent::CachedNbtv { cache, .. } => cache.restart(),
            _ => {}
        }
    }

    pub fn set_repeat(&mut self, repeat: bool) {
        self.repeat = repeat;
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn len(&self) -> usize {
        match &self.content {
            PlaybackContent::Pcm(samples) => samples.len(),
            PlaybackContent::Nbtv { clip, sample_rate } => clip.samples(*sample_rate),
            PlaybackContent::CachedPcm { length, .. } => *length,
            PlaybackContent::CachedNbtv {
                frames,
                sample_rate,
                ..
            } => nbtv::frame_samples(*frames, *sample_rate),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn playback_preserves_samples_across_callbacks_and_drains_before_finishing() {
        let mut playback = Playback::new(vec![0.1, 0.2, 0.3], false);
        let mut block = [0.0; 2];
        assert!(!playback.fill(&mut block));
        assert_eq!(block, [0.1, 0.2]);
        assert!(!playback.fill(&mut block));
        assert_eq!(block, [0.3, 0.0]);
        assert!(playback.fill(&mut block));
        assert_eq!(block, [0.0, 0.0]);
    }

    #[test]
    fn repeated_playback_wraps_inside_a_callback() {
        let mut playback = Playback::new(vec![0.1, 0.2], true);
        let mut block = [0.0; 5];
        assert!(!playback.fill(&mut block));
        assert_eq!(block, [0.1, 0.2, 0.1, 0.2, 0.1]);
    }

    #[test]
    fn cached_pcm_repeats_and_restarts_with_exact_samples() {
        let samples: Vec<f32> = (0..256).map(|i| i as f32 / 256.0).collect();
        let mut file = tempfile::tempfile().unwrap();
        for sample in &samples {
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
        let mut playback = Playback::cached_pcm(file, samples.len(), 32).unwrap();
        playback.set_repeat(true);
        let mut output = [0.0; 8];
        for _ in 0..2 {
            playback.restart();
            for block in 0..70 {
                playback.fill(&mut output);
                for (i, sample) in output.iter().enumerate() {
                    assert_eq!(*sample, samples[(block * 8 + i) % samples.len()]);
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        assert_eq!(playback.underruns(), 0);
    }

    #[test]
    fn cached_nbtv_matches_the_in_memory_raster_through_repeat_and_mode_changes() {
        let pixels: Vec<u8> = (0..nbtv::FRAME_PIXELS * 100)
            .map(|i| (i / 32 % 256) as u8)
            .collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&pixels).unwrap();
        let clip = nbtv::Clip::new(pixels).unwrap();
        let rate = 32_000;
        let length = clip.samples(rate);
        let mut playback = Playback::cached_nbtv(file, clip.frames(), rate).unwrap();
        playback.set_repeat(true);
        let mut picture = [0.0; 2048];
        let mut sync = [0.0; 2048];
        for separate in [false, true] {
            playback.restart();
            for block in 0..(length / 2048 + 4) {
                playback.fill_with_sync(&mut picture, &mut sync, separate);
                for index in 0..2048 {
                    let position = (block * 2048 + index) % length;
                    let expected = clip.sample(position, rate);
                    assert_eq!(
                        picture[index],
                        if separate {
                            expected.max(0.0)
                        } else {
                            expected
                        }
                    );
                    assert_eq!(
                        sync[index],
                        if separate {
                            nbtv::sync_sample(position, rate)
                        } else {
                            0.0
                        }
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        assert_eq!(playback.underruns(), 0);
    }

    #[test]
    fn live_samples_wait_for_input_and_stop_without_a_producer() {
        let state = RunState::new();
        let (mut producer, consumer) = rtrb::RingBuffer::new(8);
        let worker_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            let mut samples = LiveSamples::new(consumer, worker_state);
            assert_eq!(samples.next(), Some(123));
            samples.next()
        });
        std::thread::sleep(POLL_INTERVAL);
        producer.push(123).unwrap();
        // Wait until the worker has consumed the sample before stopping it.
        while producer.slots() != 8 {
            std::thread::sleep(POLL_INTERVAL);
        }
        state.stop();
        assert_eq!(worker.join().unwrap(), None);
    }

    #[test]
    fn jack_input_is_clamped_to_pcm_range() {
        assert_eq!(pcm_sample(2.0), i16::MAX);
        assert_eq!(pcm_sample(-2.0), -i16::MAX);
        assert_eq!(pcm_sample(0.0), 0);
    }

    #[test]
    fn interrupted_live_samples_discard_old_audio_before_resuming() {
        let state = RunState::new();
        let interrupt = Arc::new(AtomicBool::new(true));
        let (mut producer, consumer) = rtrb::RingBuffer::new(8);
        producer.push(123).unwrap();
        let mut samples = LiveSamples::new(consumer, state).with_interrupt(Arc::clone(&interrupt));
        assert_eq!(samples.next(), None);
        samples.discard_buffered();
        producer.push(456).unwrap();
        interrupt.store(false, Ordering::Release);
        assert_eq!(samples.next(), Some(456));
    }
}
