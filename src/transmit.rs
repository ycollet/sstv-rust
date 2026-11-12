use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use anyhow::{Result, bail};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::Playback;

#[derive(Default)]
pub struct Controls {
    command: AtomicUsize,
    pub repeat: AtomicBool,
    pub follow_transport: AtomicBool,
    pub ready: AtomicBool,
    pub playing: AtomicBool,
    pub position: AtomicUsize,
    pub length: AtomicUsize,
    pub rolling: AtomicBool,
    pub transport_failed: AtomicBool,
    pub cache_failed: AtomicBool,
    underruns: AtomicUsize,
}

#[allow(deprecated, reason = "fetch_update supports the Rust 1.88 MSRV")]
impl Controls {
    pub fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn check_playback(&self, last_underruns: &mut usize) -> Result<()> {
        let underruns = self.underruns();
        if underruns != *last_underruns {
            eprintln!(
                "Warning: playback cache underrun; {underruns} samples replaced with silence. Storage read-ahead could not keep up."
            );
            *last_underruns = underruns;
        }
        if self.cache_failed.load(Ordering::Acquire) {
            bail!("Playback cache worker failed; see stderr for details");
        }
        Ok(())
    }

    pub fn play(&self) {
        self.command
            .fetch_update(Ordering::Release, Ordering::Relaxed, |previous| {
                Some((previous.wrapping_add(2)) | 1)
            })
            .expect("command update always succeeds");
    }

    pub fn stop(&self) {
        self.command
            .fetch_update(Ordering::Release, Ordering::Relaxed, |previous| {
                Some((previous.wrapping_add(2)) & !1)
            })
            .expect("command update always succeeds");
    }
}

pub struct AudioExchange {
    pub pending: Producer<Playback>,
    pub retired: Consumer<Playback>,
}

/// All sample ownership is returned to the UI before it can be freed.
pub struct Transmitter {
    pending: Consumer<Playback>,
    retired: Producer<Playback>,
    playback: Option<Playback>,
    controls: Arc<Controls>,
    last_command: usize,
    following: bool,
    rolling: bool,
    playing: bool,
}

impl Transmitter {
    pub fn new(controls: Arc<Controls>) -> (Self, AudioExchange) {
        let (send, pending) = RingBuffer::new(1);
        let (retired, receive) = RingBuffer::new(1);
        (
            Self {
                pending,
                retired,
                playback: None,
                controls,
                last_command: 0,
                following: false,
                rolling: false,
                playing: false,
            },
            AudioExchange {
                pending: send,
                retired: receive,
            },
        )
    }

    pub fn process(&mut self, output: &mut [f32], rolling: bool) {
        self.process_channels(output, None, rolling, false);
    }

    pub fn process_with_sync(
        &mut self,
        output: &mut [f32],
        sync: &mut [f32],
        rolling: bool,
        separate: bool,
    ) {
        self.process_channels(output, Some(sync), rolling, separate);
    }

    fn process_channels(
        &mut self,
        output: &mut [f32],
        mut sync: Option<&mut [f32]>,
        rolling: bool,
        separate: bool,
    ) {
        let mut loaded = false;
        // There is only one retirement producer. Reserve capacity before
        // replacing audio so no Vec is dropped on the process thread.
        if self.retired.slots() > 0
            && let Ok(next) = self.pending.pop()
        {
            if let Some(previous) = self.playback.replace(next) {
                self.retired
                    .push(previous)
                    .expect("retirement slot was reserved");
            }
            self.playing = false;
            loaded = true;
            self.controls.ready.store(true, Ordering::Release);
            self.controls.cache_failed.store(false, Ordering::Release);
        }

        let following = self.controls.follow_transport.load(Ordering::Acquire);
        if following && (loaded || !self.following || rolling != self.rolling) {
            self.playing = rolling && self.playback.is_some();
            if let Some(playback) = &mut self.playback {
                playback.restart();
            }
        } else if !following && self.following {
            self.playing = false;
        }
        self.following = following;
        self.rolling = rolling;
        self.controls.rolling.store(rolling, Ordering::Relaxed);

        let command = self.controls.command.load(Ordering::Acquire);
        if command != self.last_command {
            self.playing = command & 1 != 0 && self.playback.is_some();
            if let Some(playback) = &mut self.playback {
                playback.restart();
            }
            self.last_command = command;
        }

        if !self.controls.ready.load(Ordering::Acquire) {
            self.playing = false;
        }
        output.fill(0.0);
        if let Some(sync) = &mut sync {
            sync.fill(0.0);
        }
        if let Some(playback) = &mut self.playback {
            if playback.failed() {
                self.playing = false;
                self.controls.ready.store(false, Ordering::Release);
                self.controls.cache_failed.store(true, Ordering::Release);
            }
            playback.set_repeat(self.controls.repeat.load(Ordering::Relaxed));
            if self.playing {
                let underruns_before = playback.underruns();
                let finished = if let Some(sync) = sync {
                    playback.fill_with_sync(output, sync, separate)
                } else {
                    playback.fill(output)
                };
                if finished {
                    self.playing = false;
                }
                self.controls
                    .underruns
                    .fetch_add(playback.underruns() - underruns_before, Ordering::Relaxed);
            }
            self.controls
                .position
                .store(playback.position(), Ordering::Relaxed);
            self.controls
                .length
                .store(playback.len(), Ordering::Relaxed);
        }
        self.controls.playing.store(self.playing, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Arc<Controls>, Transmitter, AudioExchange) {
        let controls = Arc::new(Controls::default());
        let (transmitter, mut exchange) = Transmitter::new(Arc::clone(&controls));
        exchange
            .pending
            .push(Playback::new(vec![0.1, 0.2, 0.3], false))
            .unwrap();
        (controls, transmitter, exchange)
    }

    #[test]
    fn manual_play_stop_and_replay_restart_at_the_header() {
        let (controls, mut transmitter, _exchange) = setup();
        let mut output = [0.0; 2];
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0, 0.0]);
        controls.play();
        transmitter.process(&mut output, false);
        assert_eq!(output, [0.1, 0.2]);
        controls.stop();
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0, 0.0]);
        controls.play();
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.1, 0.2]);
    }

    #[test]
    fn transport_is_opt_in_and_does_not_retrigger_after_completion() {
        let (controls, mut transmitter, _exchange) = setup();
        let mut output = [0.0; 4];
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0; 4]);
        controls.follow_transport.store(true, Ordering::Release);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.1, 0.2, 0.3, 0.0]);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0; 4]);
        assert!(!controls.playing.load(Ordering::Acquire));
        transmitter.process(&mut output, false);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.1, 0.2, 0.3, 0.0]);
    }

    #[test]
    fn transport_stop_cancels_and_next_start_rewinds() {
        let (controls, mut transmitter, _exchange) = setup();
        controls.follow_transport.store(true, Ordering::Release);
        let mut output = [0.0; 2];
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.1, 0.2]);
        transmitter.process(&mut output, false);
        assert_eq!(output, [0.0, 0.0]);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.1, 0.2]);
        controls.follow_transport.store(false, Ordering::Release);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0, 0.0]);
    }

    #[test]
    fn repeat_can_be_changed_during_transmission() {
        let (controls, mut transmitter, _exchange) = setup();
        controls.repeat.store(true, Ordering::Relaxed);
        controls.play();
        let mut output = [0.0; 4];
        transmitter.process(&mut output, false);
        assert_eq!(output, [0.1, 0.2, 0.3, 0.1]);
        controls.repeat.store(false, Ordering::Relaxed);
        transmitter.process(&mut output, false);
        assert_eq!(output, [0.2, 0.3, 0.0, 0.0]);
        transmitter.process(&mut output, false);
        assert!(!controls.playing.load(Ordering::Acquire));
    }

    #[test]
    fn replacement_returns_old_audio_and_waits_for_retirement_capacity() {
        let (_controls, mut transmitter, mut exchange) = setup();
        transmitter.process(&mut [], false);
        exchange
            .pending
            .push(Playback::new(vec![0.4; 5], false))
            .unwrap();
        transmitter.process(&mut [], false);
        exchange
            .pending
            .push(Playback::new(vec![0.5; 7], false))
            .unwrap();
        transmitter.process(&mut [], false);
        assert_eq!(transmitter.playback.as_ref().unwrap().len(), 5);
        assert_eq!(exchange.retired.pop().unwrap().len(), 3);
        transmitter.process(&mut [], false);
        assert_eq!(transmitter.playback.as_ref().unwrap().len(), 7);
        assert_eq!(exchange.retired.pop().unwrap().len(), 5);
    }

    #[test]
    fn protocol_preparation_silences_old_audio_until_the_replacement_arrives() {
        let (controls, mut transmitter, mut exchange) = setup();
        controls.follow_transport.store(true, Ordering::Release);
        let mut output = [0.0; 2];
        transmitter.process(&mut output, false);
        controls.ready.store(false, Ordering::Release);
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.0, 0.0]);
        exchange
            .pending
            .push(Playback::new(vec![0.4, 0.5], false))
            .unwrap();
        transmitter.process(&mut output, true);
        assert_eq!(output, [0.4, 0.5]);
    }

    #[test]
    fn paired_nbtv_outputs_share_the_clock_across_repeat_stop_and_replay() {
        use crate::nbtv;
        let controls = Arc::new(Controls::default());
        let (mut transmitter, mut exchange) = Transmitter::new(Arc::clone(&controls));
        let clip = Arc::new(nbtv::Clip::new(vec![128; nbtv::FRAME_PIXELS]).unwrap());
        let length = clip.samples(44_100);
        exchange
            .pending
            .push(Playback::nbtv(Arc::clone(&clip), 44_100))
            .unwrap();
        controls.repeat.store(true, Ordering::Relaxed);
        controls.play();
        let mut picture = [0.0; 257];
        let mut sync = [0.0; 257];
        for block in 0..40 {
            transmitter.process_with_sync(&mut picture, &mut sync, false, true);
            for index in 0..257 {
                let position = (block * 257 + index) % length;
                assert_eq!(picture[index], clip.sample(position, 44_100).max(0.0));
                assert_eq!(sync[index], nbtv::sync_sample(position, 44_100));
            }
        }
        controls.stop();
        transmitter.process_with_sync(&mut picture, &mut sync, false, true);
        assert_eq!(picture, [0.0; 257]);
        assert_eq!(sync, [0.0; 257]);
        controls.play();
        transmitter.process_with_sync(&mut picture, &mut sync, false, true);
        for index in 0..257 {
            assert_eq!(picture[index], clip.sample(index, 44_100).max(0.0));
            assert_eq!(sync[index], nbtv::sync_sample(index, 44_100));
        }
        controls.stop();
        transmitter.process_with_sync(&mut picture, &mut sync, false, false);
        controls.play();
        transmitter.process_with_sync(&mut picture, &mut sync, false, false);
        assert!(picture.iter().any(|sample| *sample < 0.0));
        assert_eq!(sync, [0.0; 257]);
    }
}
