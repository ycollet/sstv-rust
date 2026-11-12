use std::sync::{Arc, atomic::Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow};
use eframe::egui;
use sstv::modes;
use sstv_jack::{
    Playback, RunState,
    transmit::{AudioExchange, Controls},
};

use super::{Args, encode};

pub(super) struct EncoderApp {
    image: Arc<image::DynamicImage>,
    image_path: String,
    protocol: usize,
    sample_rate: u32,
    controls: Arc<Controls>,
    exchange: AudioExchange,
    state: Arc<RunState>,
    monitor: jack::Port<jack::Unowned>,
    preparing: Option<JoinHandle<Result<Playback>>>,
    waiting_for_audio: bool,
    error: Option<String>,
    xruns: usize,
    underruns: usize,
}

impl EncoderApp {
    pub(super) fn new(
        args: Args,
        image: image::DynamicImage,
        sample_rate: u32,
        controls: Arc<Controls>,
        exchange: AudioExchange,
        state: Arc<RunState>,
        monitor: jack::Port<jack::Unowned>,
    ) -> Self {
        let mut app = Self {
            image: Arc::new(image),
            image_path: args.image.display().to_string(),
            protocol: args.protocol,
            sample_rate,
            controls,
            exchange,
            state,
            monitor,
            preparing: None,
            waiting_for_audio: false,
            error: None,
            xruns: 0,
            underruns: 0,
        };
        app.prepare();
        app
    }

    fn prepare(&mut self) {
        self.controls.ready.store(false, Ordering::Release);
        self.error = None;
        self.waiting_for_audio = true;
        let image = Arc::clone(&self.image);
        let protocol = self.protocol;
        let sample_rate = self.sample_rate;
        let state = Arc::clone(&self.state);
        self.preparing = Some(std::thread::spawn(move || {
            encode(&image, protocol, sample_rate, &state)
        }));
    }

    fn poll(&mut self) {
        if let Err(error) = self.controls.check_playback(&mut self.underruns)
            && self.error.is_none()
        {
            eprintln!("{error:#}");
            self.error = Some(format!("{error:#}"));
        }
        // Returning audio here, rather than freeing it in JACK's callback,
        // keeps protocol switches real-time safe.
        while let Ok(previous) = self.exchange.retired.pop() {
            drop(previous);
        }
        if self.preparing.as_ref().is_some_and(|job| job.is_finished()) {
            let result = self
                .preparing
                .take()
                .expect("preparation worker exists")
                .join()
                .map_err(|_| anyhow!("Image encoding worker panicked"))
                .and_then(|result| result)
                .and_then(|playback| {
                    self.exchange
                        .pending
                        .push(playback)
                        .map_err(|_| anyhow!("Audio preparation queue is full"))
                });
            if let Err(error) = result {
                eprintln!("{error:#}");
                self.error = Some(format!("{error:#}"));
                self.waiting_for_audio = false;
            }
        }
        if self.waiting_for_audio && self.controls.ready.load(Ordering::Acquire) {
            self.waiting_for_audio = false;
        }
        let xruns = self.state.xruns.load(Ordering::Relaxed);
        if xruns != self.xruns {
            eprintln!("Warning: JACK xrun count {xruns}; the image may be damaged.");
            self.xruns = xruns;
        }
    }
}

impl eframe::App for EncoderApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll();
        if !self.state.is_running() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let playing = self.controls.playing.load(Ordering::Acquire);
        let ready = self.controls.ready.load(Ordering::Acquire) && !self.waiting_for_audio;
        let mut follow = self.controls.follow_transport.load(Ordering::Acquire);
        let mut repeat = self.controls.repeat.load(Ordering::Relaxed);
        let connections = match self.monitor.connected_count() {
            Ok(count) => count,
            Err(error) => {
                if self.error.is_none() {
                    eprintln!("Cannot query JACK connections: {error}");
                    self.error = Some(format!("Cannot query JACK connections: {error}"));
                }
                0
            }
        };
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SSTV encoder");
            ui.label(&self.image_path);
            ui.label(format!(
                "{} Hz | {connections} output connection(s)",
                self.sample_rate
            ));
            ui.separator();
            let previous = self.protocol;
            ui.add_enabled_ui(!playing && !self.waiting_for_audio, |ui| {
                egui::ComboBox::from_label("Protocol")
                    .selected_text(format!("{:?}", modes::ALL[self.protocol]))
                    .show_ui(ui, |ui| {
                        for (index, mode) in modes::ALL.iter().enumerate() {
                            ui.selectable_value(&mut self.protocol, index, format!("{mode:?}"));
                        }
                    });
            });
            if self.protocol != previous {
                self.prepare();
            }
            let (width, height) = modes::ALL[self.protocol].resolution();
            ui.label(format!("Image is resized to {width} x {height}"));
            ui.checkbox(&mut repeat, "Repeat");
            ui.add_enabled_ui(ready && self.protocol == previous, |ui| {
                ui.checkbox(&mut follow, "Follow JACK transport");
            });
            self.controls.repeat.store(repeat, Ordering::Relaxed);
            self.controls
                .follow_transport
                .store(follow, Ordering::Release);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        ready
                            && self.protocol == previous
                            && !playing
                            && !follow
                            && connections > 0,
                        egui::Button::new("Play"),
                    )
                    .clicked()
                {
                    self.controls.play();
                }
                if ui.add_enabled(playing, egui::Button::new("Stop")).clicked() {
                    self.controls.stop();
                    self.state.stop();
                }
            });
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
            } else if self.waiting_for_audio {
                ui.label("Preparing audio...");
            } else if playing {
                ui.label("Transmitting");
            } else if follow {
                ui.label("Following JACK transport; waiting for the next start");
            } else {
                ui.label("Ready - connect the effects chain, then press Play");
            }
            let position = self.controls.position.load(Ordering::Relaxed);
            let length = self.controls.length.load(Ordering::Relaxed);
            if ready && length > 0 {
                let elapsed = position as f64 / f64::from(self.sample_rate);
                let duration = length as f64 / f64::from(self.sample_rate);
                ui.add(
                    egui::ProgressBar::new(position as f32 / length as f32)
                        .text(format!("{elapsed:.1} / {duration:.1} s")),
                );
            }
            ui.separator();
            ui.label(format!(
                "JACK transport: {} | xruns: {}",
                if self.controls.rolling.load(Ordering::Relaxed) {
                    "rolling"
                } else {
                    "stopped / starting"
                },
                self.xruns,
            ));
            ui.small("Play/Stop never change global JACK transport.");
            ui.small("Follow starts from the image header, not the timeline position.");
            if connections == 0 {
                ui.colored_label(egui::Color32::YELLOW, "No JACK output connection.");
            }
        });
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl Drop for EncoderApp {
    fn drop(&mut self) {
        self.controls.stop();
        if let Some(job) = self.preparing.take() {
            match job.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("Image encoding failed: {error:#}"),
                Err(_) => eprintln!("Image encoding worker panicked"),
            }
        }
    }
}

pub(super) fn run(app: EncoderApp) -> Result<()> {
    eframe::run_native(
        "SSTV encoder",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([520.0, 410.0])
                .with_min_inner_size([420.0, 360.0]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(app))),
    )
    .map_err(|error| anyhow!("Cannot run encoder GUI: {error}"))
}
