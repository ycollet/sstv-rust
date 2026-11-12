use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Result, bail};
use rtrb::Consumer;

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
}

#[derive(Debug)]
enum PlaybackContent {
    Pcm(Vec<f32>),
    Nbtv {
        clip: Arc<nbtv::Clip>,
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
        }
    }

    pub fn nbtv(clip: Arc<nbtv::Clip>, sample_rate: u32) -> Self {
        Self {
            content: PlaybackContent::Nbtv { clip, sample_rate },
            position: 0,
            repeat: false,
        }
    }

    /// Returns true only after the final sample has left a previous callback.
    pub fn fill(&mut self, output: &mut [f32]) -> bool {
        let length = self.len();
        if !self.repeat && self.position == length {
            output.fill(0.0);
            return true;
        }
        for sample in output {
            if self.position == length && self.repeat {
                self.position = 0;
            }
            if self.position < length {
                *sample = match &self.content {
                    PlaybackContent::Pcm(samples) => samples[self.position],
                    PlaybackContent::Nbtv { clip, sample_rate } => {
                        clip.sample(self.position, *sample_rate)
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
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
